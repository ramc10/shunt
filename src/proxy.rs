use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{error, warn};

use crate::config::{state_path, Config, CredentialsStore};
use crate::forwarder::Forwarder;
use crate::oauth::OAuthCredential;
use crate::provider::Provider;
use crate::quota;
use crate::router;
use crate::state::StateStore;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    forwarder: Arc<Forwarder>,
    state: StateStore,
    /// Live credentials — can be refreshed at runtime without restarting.
    credentials: Arc<RwLock<HashMap<String, OAuthCredential>>>,
    /// Per-account mutex that serialises concurrent token-refresh attempts.
    ///
    /// When multiple in-flight requests hit a 401 for the same account at the
    /// same time, only one should call the upstream OAuth endpoint; the others
    /// should wait and then re-use the fresh token instead of each making their
    /// own refresh call (which would rotate the refresh_token out from under the
    /// others and cause cascading auth failures).
    refresh_locks: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Epoch-ms when this proxy instance started.
    started_ms: u64,
    /// If set, /v1/chat/completions requests are translated and forwarded here
    /// (the Anthropic proxy base URL, e.g. "http://127.0.0.1:8082").
    anthropic_base_url: Option<String>,
}

pub fn create_app(config: Config) -> anyhow::Result<Router> {
    let (app, _) = create_app_with_state(config, StateStore::load(&state_path()), None)?;
    Ok(app)
}

/// Shared live credentials map — can be written to without restarting the proxy.
pub type LiveCredentials = Arc<RwLock<HashMap<String, OAuthCredential>>>;

pub fn create_app_with_state(
    config: Config,
    state: StateStore,
    anthropic_base_url: Option<String>,
) -> anyhow::Result<(Router, LiveCredentials)> {
    let forwarder = Forwarder::new(&config.server.upstream_url, config.server.request_timeout_secs)?;

    // Accounts with no credential are shown in status but skipped during routing.
    // Mark them disabled immediately so the router ignores them.
    for a in config.accounts.iter().filter(|a| a.credential.is_none()) {
        state.set_auth_failed(&a.name);
    }

    let credentials: LiveCredentials = Arc::new(RwLock::new(
        config.accounts.iter()
            .filter_map(|a| a.credential.as_ref().map(|c| (a.name.clone(), c.clone())))
            .collect::<HashMap<_, _>>(),
    ));

    let app_state = AppState {
        config: Arc::new(config),
        forwarder: Arc::new(forwarder),
        state,
        credentials: Arc::clone(&credentials),
        refresh_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
        started_ms: now_ms(),
        anthropic_base_url,
    };

    // Always register both Anthropic and OpenAI routes so a single shunt
    // instance can serve clients of either protocol and route to accounts of
    // either provider, translating on the fly when needed.
    let proxy_routes = Router::new()
        .route("/v1/messages", post(proxy_handler))
        .route("/v1/messages/count_tokens", post(proxy_handler))
        .route("/v1/chat/completions", post(openai_compat_handler))
        .route("/v1/models", get(openai_models_handler))
        .fallback(proxy_handler);

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status_handler))
        .route("/use", post(use_handler))
        .merge(proxy_routes)
        .with_state(app_state);

    Ok((app, credentials))
}

async fn health() -> impl IntoResponse {
    axum::Json(json!({"status": "ok"}))
}

async fn status_handler(State(s): State<AppState>) -> impl IntoResponse {
    let account_states = s.state.account_states();
    let quotas = s.state.quota_snapshot();
    let rate_limits = s.state.rate_limit_snapshot();

    let accounts: Vec<_> = s.config.accounts.iter().map(|a| {
        let st = account_states.get(&a.name);
        let avail_status = if st.map(|s| s.auth_failed).unwrap_or(false) {
            "reauth_required"
        } else if st.map(|s| s.disabled).unwrap_or(false) {
            "disabled"
        } else if s.state.is_available(&a.name) {
            "available"
        } else {
            "cooling"
        };

        let quota = quotas.get(&a.name);
        let window_expires_ms = quota.and_then(|q| q.window_expires_ms());
        let window_expires_ms = window_expires_ms.filter(|&e| e > now_ms());
        let tokens_used = quota.map(|q| json!({
            "input": q.input_tokens,
            "output": q.output_tokens,
            "total": q.total_tokens(),
        }));

        let rl = rate_limits.get(&a.name);
        let rate_limit = rl.map(|r| json!({
            "utilization_5h": r.utilization_5h,
            "reset_5h": r.reset_5h,
            "status_5h": r.status_5h,
            "utilization_7d": r.utilization_7d,
            "reset_7d": r.reset_7d,
            "status_7d": r.status_7d,
            "representative_claim": r.representative_claim,
            "updated_ms": r.updated_ms,
        }));

        let acc_state = account_states.get(&a.name);
        let email = a.credential.as_ref().and_then(|c| c.email.as_deref()).map(|e| e.to_owned());
        let disabled = acc_state.map(|s| s.disabled).unwrap_or(false);
        let auth_failed = acc_state.map(|s| s.auth_failed).unwrap_or(false);
        let cooldown_until_ms = acc_state.map(|s| s.cooldown_until_ms).unwrap_or(0);
        let utilization_5h = rl.and_then(|r| r.utilization_5h).unwrap_or(0.0);
        let reset_5h = rl.and_then(|r| r.reset_5h);
        let utilization_7d = rl.and_then(|r| r.utilization_7d).unwrap_or(0.0);
        let reset_7d = rl.and_then(|r| r.reset_7d);
        let available = s.state.is_available(&a.name);

        json!({
            "name": a.name,
            "email": email,
            "plan_type": a.plan_type,
            "status": avail_status,
            "available": available,
            "disabled": disabled,
            "auth_failed": auth_failed,
            "cooldown_until_ms": cooldown_until_ms,
            "utilization_5h": utilization_5h,
            "reset_5h": reset_5h,
            "utilization_7d": utilization_7d,
            "reset_7d": reset_7d,
            "window_expires_ms": window_expires_ms,
            "tokens_used": tokens_used,
            "rate_limit": rate_limit,
        })
    }).collect();

    let recent_requests = s.state.recent_requests_snapshot();
    let savings = s.state.savings_snapshot();

    axum::Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "started_ms": s.started_ms,
        "accounts": accounts,
        "pinned_account": s.state.get_pinned(),
        "last_used_account": s.state.get_last_used(),
        "recent_requests": recent_requests,
        "savings": savings,
    }))
}

async fn use_handler(
    State(s): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let account = body["account"].as_str().map(|s| s.to_owned());
    // Validate the account name exists (unless clearing to auto)
    if let Some(ref name) = account {
        if name != "auto" && !s.config.accounts.iter().any(|a| &a.name == name) {
            return axum::Json(json!({
                "error": format!("unknown account '{name}'")
            }));
        }
        let pinned = if name == "auto" { None } else { Some(name.clone()) };
        s.state.set_pinned(pinned);
        axum::Json(json!({ "pinned": name }))
    } else {
        s.state.set_pinned(None);
        axum::Json(json!({ "pinned": null }))
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

async fn proxy_handler(
    State(s): State<AppState>,
    req: Request,
) -> Result<Response, ProxyError> {
    // Remote auth: if a remote_key is configured, the client must supply it as x-api-key.
    if let Some(ref expected) = s.config.server.remote_key {
        let provided = req.headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if provided != expected {
            return Err(ProxyError::Unauthorized);
        }
    }

    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    let headers = req.headers().clone();

    let body_bytes: Bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| ProxyError::BodyRead)?;

    let model = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|v| v["model"].as_str().map(|s| s.to_owned()))
        .unwrap_or_default();
    let req_start_ms = now_ms();

    let fp = router::fingerprint(&body_bytes);
    let fp_ref = fp.as_deref();

    let mut tried: HashSet<String> = HashSet::new();
    // Track accounts we've already attempted a token refresh for this request.
    let mut refreshed: HashSet<String> = HashSet::new();
    // Total wait budget: up to 5 hours (Claude's rate-limit reset window).
    let wait_deadline_ms = now_ms() + 5 * 60 * 60 * 1_000;

    loop {
        let account = match router::pick_account(
            &s.config.accounts, &s.state, fp_ref, &tried,
            s.config.server.sticky_ttl_ms, s.config.server.expiry_soon_secs,
        ) {
            Some(a) => a,
            None => {
                // Check whether any accounts are just temporarily cooling down
                // (429/529 backoff) rather than permanently disabled / auth_failed.
                // If so, wait for the soonest one to recover and retry.
                let account_states = s.state.account_states();
                let now = now_ms();
                let soonest_ms = s.config.accounts.iter()
                    .filter_map(|a| {
                        let st = account_states.get(&a.name)?;
                        if st.disabled { return None; } // auth_failed or permanently off
                        if st.cooldown_until_ms > now { Some(st.cooldown_until_ms) } else { None }
                    })
                    .min();

                match soonest_ms {
                    Some(wake_ms) if wake_ms <= wait_deadline_ms => {
                        let wait_ms = wake_ms.saturating_sub(now_ms()) + 50; // +50 ms buffer
                        warn!(wait_ms, "all accounts cooling — waiting for next available account");
                        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                        tried.clear(); // accounts may have recovered; try them again
                    }
                    _ => return Err(ProxyError::AllAccountsUnavailable),
                }
                continue;
            }
        };

        let account_name = account.name.clone();

        // Use the live (possibly refreshed) token rather than the one baked into config.
        // For OpenAI/chatgpt.com accounts, use the id_token (short-lived OIDC JWT) as
        // the bearer — chatgpt.com's API authenticates via id_token, not access_token.
        let token = {
            let creds = s.credentials.read().await;
            let cred = creds.get(&account_name)
                .cloned()
                .or_else(|| account.credential.clone());
            match cred {
                Some(c) => c.access_token,
                None => String::new(),
            }
        };

        // Detect request and account protocols.  When they differ, translate
        // the request body + path before forwarding and translate the response
        // back so the client always sees its native wire format.
        let req_is_anthropic = path.starts_with("/v1/messages");
        let acct_is_anthropic = matches!(account.provider, Provider::Anthropic);

        let (fwd_path, fwd_body, fwd_headers) = if req_is_anthropic == acct_is_anthropic {
            (path.clone(), body_bytes.clone(), headers.clone())
        } else if req_is_anthropic {
            // Anthropic client → OpenAI account: translate A→O, strip Anthropic headers.
            let val = serde_json::from_slice::<serde_json::Value>(&body_bytes).unwrap_or(json!({}));
            let translated = translate_anthropic_req_to_openai(val);
            let mut h = headers.clone();
            for name in &["anthropic-version", "anthropic-beta", "anthropic-dangerous-direct-browser-access"] {
                h.remove(*name);
            }
            (
                "/v1/chat/completions".to_owned(),
                bytes::Bytes::from(serde_json::to_vec(&translated).unwrap_or_default()),
                h,
            )
        } else {
            // OpenAI client → Anthropic account: translate O→A.
            let val = serde_json::from_slice::<serde_json::Value>(&body_bytes).unwrap_or(json!({}));
            let translated = translate_to_anthropic(val);
            (
                "/v1/messages".to_owned(),
                bytes::Bytes::from(serde_json::to_vec(&translated).unwrap_or_default()),
                headers.clone(),
            )
        };

        let upstream = account.provider.default_upstream_url();
        let response = s.forwarder
            .forward(upstream, &method, &fwd_path, fwd_body, &fwd_headers, account, &token)
            .await
            .map_err(|e| {
                error!("Forward error: {:#}", e);
                ProxyError::Upstream
            })?;

        match response.status().as_u16() {
            200..=299 => {
                s.state.set_last_used(&account_name);
                if let Some(info) = account.provider.parse_rate_limits(response.headers()) {
                    s.state.update_rate_limits(&account_name, info);
                }
                // Translate response back to the client's expected protocol.
                let response = if req_is_anthropic == acct_is_anthropic {
                    response
                } else if req_is_anthropic {
                    // Got OpenAI response; client expects Anthropic.
                    translate_response_openai_to_anthropic(response, &model).await
                } else {
                    // Got Anthropic response; client expects OpenAI.
                    translate_response_anthropic_to_openai(response).await
                };
                return Ok(tap_usage(response, &s.state, &account_name, &model, req_start_ms).await);
            }
            429 => {
                let info = account.provider.parse_rate_limits(response.headers());
                // Sleep until the actual reset time if the headers tell us when that is;
                // otherwise fall back to 60s so we don't hammer the API.
                let cooldown_ms = info.as_ref()
                    .and_then(|i| i.reset_5h.or(i.reset_7d))
                    .map(|reset_secs| {
                        let reset_ms = reset_secs.saturating_mul(1_000);
                        reset_ms.saturating_sub(now_ms()).saturating_add(500) // +500ms buffer
                    })
                    .unwrap_or(60_000);
                warn!(account = %account_name, cooldown_ms, "429 rate-limited — cooling until reset");
                if let Some(info) = info {
                    s.state.update_rate_limits(&account_name, info);
                }
                s.state.set_cooldown(&account_name, cooldown_ms);
                if cooldown_ms >= 5 * 60_000 {
                    let mins = cooldown_ms / 60_000;
                    notify(
                        "shunt: Rate Limited",
                        &format!("Account '{account_name}' hit quota limit — cooling {mins}m."),
                        "Ping",
                    );
                }
                tried.insert(account_name);
            }
            529 => {
                warn!(account = %account_name, "529 overloaded — cooling 30s");
                if let Some(info) = account.provider.parse_rate_limits(response.headers()) {
                    s.state.update_rate_limits(&account_name, info);
                }
                s.state.set_cooldown(&account_name, 30_000);
                tried.insert(account_name);
            }
            401 => {
                if !refreshed.contains(&account_name) {
                    // Access token invalidated (e.g. user logged out) — try refresh.
                    //
                    // Acquire the per-account refresh lock so concurrent requests
                    // for the same account serialise here. The first waiter to get
                    // the lock does the actual OAuth refresh; subsequent waiters
                    // re-check credentials and skip the refresh if the token was
                    // already rotated while they were queued.
                    let account_lock = {
                        let mut locks = s.refresh_locks.lock().unwrap();
                        locks.entry(account_name.clone())
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                            .clone()
                    };
                    let _guard = account_lock.lock().await;

                    // Re-read credentials after acquiring the lock — another task
                    // may have already refreshed while we were waiting.
                    let cred_before = {
                        let creds = s.credentials.read().await;
                        creds.get(&account_name).cloned()
                            .or_else(|| account.credential.clone())
                    };
                    let Some(cred) = cred_before else {
                        tried.insert(account_name);
                        continue;
                    };

                    // Check if the token already changed while we were waiting.
                    let token_before = cred.access_token.clone();
                    let already_refreshed = {
                        let creds = s.credentials.read().await;
                        creds.get(&account_name)
                            .map(|c| c.access_token != token_before)
                            .unwrap_or(false)
                    };

                    if already_refreshed {
                        // Another concurrent request already refreshed — just retry.
                        warn!(account = %account_name, "401 — token was refreshed by concurrent request, retrying");
                        refreshed.insert(account_name);
                    } else {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            account.provider.refresh_token(&cred),
                        ).await {
                            Ok(Ok(fresh)) => {
                                warn!(account = %account_name, "401 — token refreshed, retrying");
                                {
                                    let mut creds = s.credentials.write().await;
                                    creds.insert(account_name.clone(), fresh.clone());
                                }
                                // Persist to disk so the refreshed token survives a restart.
                                let name = account_name.clone();
                                let fresh = fresh.clone();
                                tokio::task::spawn_blocking(move || {
                                    let mut store = CredentialsStore::load();
                                    store.accounts.insert(name, fresh.clone());
                                    store.save().ok();
                                    if fresh.id_token.is_some() {
                                        crate::oauth::write_codex_auth_file(&fresh);
                                    }
                                });
                                // Mark as refreshed but don't add to tried — retry this account.
                                refreshed.insert(account_name);
                            }
                            _ => {
                                // Refresh failed/timed out — cool down, don't permanently disable.
                                error!(account = %account_name, "401 — token refresh failed, cooling 5min");
                                s.state.set_cooldown(&account_name, 5 * 60_000);
                                tried.insert(account_name);
                            }
                        }
                    }
                } else {
                    // Already refreshed once and still 401 — cool down this account.
                    error!(account = %account_name, "401 after refresh — cooling 5min");
                    s.state.set_cooldown(&account_name, 5 * 60_000);
                    tried.insert(account_name);
                }
            }
            403 => {
                // Forbidden — subscription lapsed or org restriction; refreshing won't help.
                error!(account = %account_name, "403 forbidden — cooling 30min");
                s.state.set_cooldown(&account_name, 30 * 60_000);
                notify(
                    "shunt: Account Forbidden",
                    &format!("Account '{account_name}' got 403 — subscription may have lapsed (cooling 30m)."),
                    "Basso",
                );
                tried.insert(account_name);
            }
            _ => {
                // 400, 404, 500, etc. — return as-is, no retry
                return Ok(response);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Usage extraction
// ---------------------------------------------------------------------------

/// Intercept a successful response to record token usage, then pass it through.
///
/// - Streaming: wraps the body stream with an SSE scanner (zero latency).
/// - Non-streaming: buffers the body, parses usage, rebuilds the response.
async fn tap_usage(
    resp: Response,
    state: &StateStore,
    account: &str,
    model: &str,
    req_start_ms: u64,
) -> Response {
    use axum::body::Body;
    use crate::state::RequestLog;

    if quota::is_streaming_response(&resp) {
        let state = state.clone();
        let account = account.to_owned();
        let model = model.to_owned();
        let on_complete = Arc::new(move |input: u64, output: u64| {
            state.record_usage(&account, input, output);
            state.record_global(&model, input, output);
            state.record_request(RequestLog {
                ts_ms: req_start_ms,
                account: account.clone(),
                model: model.clone(),
                status: 200,
                input_tokens: input,
                output_tokens: output,
                duration_ms: now_ms().saturating_sub(req_start_ms),
            });
        });
        let (parts, body) = resp.into_parts();
        let wrapped = quota::wrap_streaming_body(body, on_complete);
        return Response::from_parts(parts, wrapped);
    }

    // Non-streaming: buffer, extract, rebuild
    let (parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };
    let (input, output) = quota::extract_usage_from_json(&bytes);
    state.record_usage(account, input, output);
    state.record_global(model, input, output);
    state.record_request(RequestLog {
        ts_ms: req_start_ms,
        account: account.to_owned(),
        model: model.to_owned(),
        status: 200,
        input_tokens: input,
        output_tokens: output,
        duration_ms: now_ms().saturating_sub(req_start_ms),
    });
    Response::from_parts(parts, Body::from(bytes))
}


// ---------------------------------------------------------------------------
// Rate limit prefetch
// ---------------------------------------------------------------------------

/// For any account with no rate-limit data yet, make a cheap request directly
/// to the upstream API so we populate metrics without waiting for a real user
/// request. Runs as a background task after startup.
pub async fn prefetch_rate_limits(config: Arc<Config>, state: StateStore, live_creds: LiveCredentials) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();

    for account in &config.accounts {
        // Skip if we already have data for this account.
        let rl = state.rate_limit_snapshot();
        if let Some(r) = rl.get(&account.name) {
            if r.utilization_5h.is_some() || r.utilization_7d.is_some() {
                continue;
            }
        }

        // Skip accounts with no credentials or no prefetch support.
        let creds = match account.credential.clone() {
            Some(c) => c,
            None => continue,
        };

        let Some((path, body)) = account.provider.prefetch_request() else {
            // No POST prefetch for this provider — do a lightweight GET auth check instead.
            if let Some(probe_path) = account.provider.auth_probe_get_path() {
                auth_probe_get(&client, probe_path, account, &state).await;
            }
            continue;
        };
        let url = format!("{}{}", config.server.upstream_url, path);

        let resp = prefetch_send(&client, &url, &account.provider, &creds.access_token, &body).await;

        let r = match resp {
            Ok(r) => r,
            Err(e) => { tracing::warn!(account = %account.name, "prefetch failed: {e}"); continue; }
        };

        if r.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::info!(account = %account.name, "prefetch: token expired, refreshing");
            let fresh = match account.provider.refresh_token(&creds).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(account = %account.name, "token refresh failed: {e}");
                    state.set_auth_failed(&account.name);
                    continue;
                }
            };
            let mut store = crate::config::CredentialsStore::load();
            store.accounts.insert(account.name.clone(), fresh.clone());
            store.save().ok();
            if fresh.id_token.is_some() {
                crate::oauth::write_codex_auth_file(&fresh);
            }
            // Update live credentials so the proxy uses the fresh token immediately.
            live_creds.write().await.insert(account.name.clone(), fresh.clone());

            match prefetch_send(&client, &url, &account.provider, &fresh.access_token, &body).await {
                Ok(r2) if r2.status() == reqwest::StatusCode::UNAUTHORIZED => {
                    tracing::error!(account = %account.name, "401 after refresh — needs re-authorization");
                    state.set_auth_failed(&account.name);
                }
                Ok(r2) => {
                    if let Some(info) = account.provider.parse_rate_limits(r2.headers()) {
                        state.update_rate_limits(&account.name, info);
                    }
                }
                Err(e) => tracing::warn!(account = %account.name, "prefetch retry failed: {e}"),
            }
        } else {
            tracing::info!(account = %account.name, status = %r.status(), "prefetch response");
            if let Some(info) = account.provider.parse_rate_limits(r.headers()) {
                state.update_rate_limits(&account.name, info);
            }
        }
    }
}

/// Build and send a prefetch request for the given provider + token.
async fn prefetch_send(
    client: &reqwest::Client,
    url: &str,
    provider: &crate::provider::Provider,
    token: &str,
    body: &serde_json::Value,
) -> anyhow::Result<reqwest::Response> {
    let mut headers = reqwest::header::HeaderMap::new();
    provider.inject_auth_headers(&mut headers, token)?;
    for (name, value) in provider.prefetch_extra_headers() {
        headers.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
            reqwest::header::HeaderValue::from_static(value),
        );
    }
    Ok(client.post(url).headers(headers).json(body).send().await?)
}

/// GET a cheap endpoint to verify credentials are still valid for providers that
/// don't expose rate-limit headers (e.g. OpenAI). On 401, attempts a token refresh;
/// marks the account as `reauth_required` if the refresh also fails.
async fn auth_probe_get(
    client: &reqwest::Client,
    path: &str,
    account: &crate::config::AccountConfig,
    state: &StateStore,
) {
    let creds = match account.credential.clone() {
        Some(c) => c,
        None => return,
    };
    let upstream = match account.provider {
        crate::provider::Provider::OpenAI => "https://chatgpt.com",
        crate::provider::Provider::Anthropic => "https://api.anthropic.com",
    };
    let url = format!("{}{}", upstream, path);

    let do_get = |token: &str| -> reqwest::RequestBuilder {
        let mut headers = reqwest::header::HeaderMap::new();
        let _ = account.provider.inject_auth_headers(&mut headers, token);
        client.get(&url).headers(headers)
    };

    let probe_token = &creds.access_token;
    let resp = match do_get(probe_token).send().await {
        Ok(r) => r,
        Err(e) => { tracing::warn!(account = %account.name, "auth probe failed: {e}"); return; }
    };

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        tracing::info!(account = %account.name, "auth probe: access token rejected, refreshing");
        let fresh = match account.provider.refresh_token(&creds).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(account = %account.name, "token refresh failed: {e}");
                state.set_auth_failed(&account.name);
                return;
            }
        };
        let mut store = crate::config::CredentialsStore::load();
        store.accounts.insert(account.name.clone(), fresh.clone());
        store.save().ok();
        if fresh.id_token.is_some() {
            crate::oauth::write_codex_auth_file(&fresh);
        }

        let fresh_token = fresh.id_token.as_deref().unwrap_or(&fresh.access_token);
        match do_get(fresh_token).send().await {
            Ok(r2) if r2.status() == reqwest::StatusCode::UNAUTHORIZED => {
                tracing::error!(account = %account.name, "401 after refresh — needs re-authorization");
                state.set_auth_failed(&account.name);
            }
            Ok(_) => tracing::info!(account = %account.name, "auth probe ok after refresh"),
            Err(e) => tracing::warn!(account = %account.name, "auth probe retry failed: {e}"),
        }
    } else {
        tracing::info!(account = %account.name, status = %resp.status(), "auth probe ok");
        // Access token is valid. Do NOT refresh here — rotating the refresh_token races
        // with codex CLI, which also tries to refresh at startup using the same token.
        // Proactive refreshing is handled solely by openai_token_refresh_loop.
    }
}

// ---------------------------------------------------------------------------
// Proactive OpenAI token refresh loop
// ---------------------------------------------------------------------------

/// Returns true if the access_token inside `cred` has fewer than `threshold_mins`
/// minutes remaining. Falls back to the stored `expires_at` if the JWT cannot be decoded.
fn access_token_expires_soon(cred: &crate::oauth::OAuthCredential, threshold_mins: u64) -> bool {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let exp_ms = crate::oauth::jwt_exp_ms(&cred.access_token)
        .unwrap_or(cred.expires_at);
    exp_ms < now_ms + threshold_mins * 60 * 1_000
}

/// Sync live_creds from auth.json if auth.json has a newer token.
///
/// Codex CLI refreshes its own token and writes auth.json. Before we refresh,
/// we pull that in so we don't use a stale refresh_token that codex already rotated.
async fn sync_live_creds_from_auth_json(
    account_name: &str,
    live_creds: &LiveCredentials,
) {
    let Some(from_file) = crate::oauth::read_codex_credentials() else { return };
    let current_exp = live_creds.read().await
        .get(account_name)
        .map(|c| c.expires_at)
        .unwrap_or(0);
    if from_file.expires_at > current_exp {
        tracing::info!(account = %account_name, "synced fresher token from auth.json");
        live_creds.write().await.insert(account_name.to_owned(), from_file);
    }
}

/// Perform a single proactive refresh for one account and persist the result.
async fn do_proactive_refresh(
    account: &crate::config::AccountConfig,
    creds: &crate::oauth::OAuthCredential,
    live_creds: &LiveCredentials,
    state: &StateStore,
) {
    tracing::info!(account = %account.name, "proactive OpenAI token refresh");
    match account.provider.refresh_token(creds).await {
        Ok(fresh) => {
            tracing::info!(account = %account.name, "proactive refresh ok — auth.json updated");
            {
                let mut map = live_creds.write().await;
                map.insert(account.name.clone(), fresh.clone());
            }
            let mut store = crate::config::CredentialsStore::load();
            store.accounts.insert(account.name.clone(), fresh.clone());
            store.save().ok();
            if fresh.id_token.is_some() {
                crate::oauth::write_codex_auth_file(&fresh);
            }
            state.clear_auth_failed(&account.name);
        }
        Err(e) => {
            tracing::warn!(account = %account.name, "proactive refresh failed: {e}");
            state.set_auth_failed(&account.name);
        }
    }
}


/// Keeps shunt's live credentials in sync with Codex CLI's auth.json.
///
/// Strategy: never proactively rotate the refresh_token — that races with
/// Codex CLI's own refresh logic and causes "invalid_grant" errors. Instead,
/// just periodically sync from auth.json so shunt picks up whatever Codex wrote.
/// On-demand refresh (401 handler) covers the case where Codex isn't running
/// and the token has actually expired.
pub async fn openai_token_refresh_loop(
    config: Arc<Config>,
    state: StateStore,
    live_creds: LiveCredentials,
) {
    // Startup: sync from auth.json first (Codex may have refreshed since shunt last ran).
    for account in config.accounts.iter()
        .filter(|a| a.provider == crate::provider::Provider::OpenAI)
    {
        if state.account_states().get(&account.name).map(|s| s.auth_failed).unwrap_or(false) {
            continue;
        }
        sync_live_creds_from_auth_json(&account.name, &live_creds).await;

        let creds = {
            let map = live_creds.read().await;
            map.get(&account.name).cloned().or_else(|| account.credential.clone())
        };
        if let Some(creds) = creds {
            if access_token_expires_soon(&creds, 30) {
                // access_token is nearly expired — refresh now so shunt can serve requests immediately.
                do_proactive_refresh(account, &creds, &live_creds, &state).await;
            } else {
                tracing::info!(account = %account.name, "access_token fresh at startup");
            }
        }
    }

    // Periodic sync every 5 minutes — picks up any token Codex CLI has written.
    // No proactive refresh: Codex owns the refresh lifecycle; shunt uses what Codex produces.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5 * 60)).await;
        for account in config.accounts.iter()
            .filter(|a| a.provider == crate::provider::Provider::OpenAI)
        {
            sync_live_creds_from_auth_json(&account.name, &live_creds).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

enum ProxyError {
    BodyRead,
    Upstream,
    AllAccountsUnavailable,
    Unauthorized,
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ProxyError::BodyRead => (StatusCode::BAD_REQUEST, "failed to read request body"),
            ProxyError::Upstream => (StatusCode::BAD_GATEWAY, "upstream request failed"),
            ProxyError::AllAccountsUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "all accounts are on cooldown or disabled")
            }
            ProxyError::Unauthorized => (StatusCode::UNAUTHORIZED, "invalid or missing api key"),
        };

        (status, axum::Json(json!({
            "type": "error",
            "error": {"type": "api_error", "message": msg}
        }))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Recovery watcher — periodically retries token refresh for auth_failed accounts
// ---------------------------------------------------------------------------

/// Runs as a background task. Every 2 minutes, tries to refresh tokens for any
/// auth_failed account. If refresh succeeds the account is brought back online
/// without a process restart. If all accounts remain unrecoverable, fires a
/// macOS notification (at most once per hour).
pub async fn recovery_watcher(
    config: Arc<Config>,
    state: StateStore,
    credentials: LiveCredentials,
) {
    use std::time::{Duration, Instant};
    const CHECK_INTERVAL: Duration = Duration::from_secs(120);
    const NOTIFY_COOLDOWN: Duration = Duration::from_secs(3600);

    let account_names: Vec<String> = config.accounts.iter().map(|a| a.name.clone()).collect();
    let mut last_notified: Option<Instant> = None;

    loop {
        tokio::time::sleep(CHECK_INTERVAL).await;

        let name_refs: Vec<&str> = account_names.iter().map(String::as_str).collect();
        let failed = state.auth_failed_accounts(&name_refs);
        if failed.is_empty() {
            last_notified = None;
            continue;
        }

        tracing::warn!(
            accounts = ?failed,
            "recovery: {} account(s) auth_failed, attempting token refresh",
            failed.len()
        );

        let mut any_recovered = false;

        for name in &failed {
            let cred = {
                let map = credentials.read().await;
                map.get(*name).cloned()
            };
            let Some(cred) = cred else { continue };
            if cred.refresh_token.is_empty() { continue; }

            let provider = config.accounts.iter()
                .find(|a| a.name == *name)
                .map(|a| a.provider.clone())
                .unwrap_or_default();

            let result = tokio::time::timeout(
                Duration::from_secs(20),
                provider.refresh_token(&cred),
            ).await;

            match result {
                Ok(Ok(fresh)) => {
                    tracing::info!(account = %name, "recovery: token refreshed — account back online");
                    {
                        let mut map = credentials.write().await;
                        map.insert(name.to_string(), fresh.clone());
                    }
                    let name_owned = name.to_string();
                    let fresh_owned = fresh.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut store = crate::config::CredentialsStore::load();
                        store.accounts.insert(name_owned, fresh_owned.clone());
                        store.save().ok();
                        if fresh_owned.id_token.is_some() {
                            crate::oauth::write_codex_auth_file(&fresh_owned);
                        }
                    });
                    state.clear_auth_failed(name);
                    any_recovered = true;
                }
                Ok(Err(e)) => {
                    tracing::error!(account = %name, error = %e, "recovery: token refresh failed");
                    notify(
                        "shunt: Reauth Required",
                        &format!("Account '{name}' needs re-authorization. Run `shunt add-account`."),
                        "Basso",
                    );
                }
                Err(_) => {
                    tracing::error!(account = %name, "recovery: token refresh timed out");
                    notify(
                        "shunt: Reauth Required",
                        &format!("Account '{name}' token refresh timed out. Run `shunt add-account`."),
                        "Basso",
                    );
                }
            }
        }

        if any_recovered {
            tracing::info!("recovery: at least one account is back online");
            continue;
        }

        // All accounts still auth_failed after refresh attempts — notify.
        let still_failed = state.auth_failed_accounts(&name_refs);
        if still_failed.len() == account_names.len() {
            let should_notify = last_notified
                .map(|t| t.elapsed() >= NOTIFY_COOLDOWN)
                .unwrap_or(true);
            if should_notify {
                error!(
                    "ALL accounts are offline (auth failed). \
                     Run `shunt add-account` to re-authorize."
                );
                notify(
                    "shunt: All Accounts Offline",
                    "All accounts need re-authorization. Run `shunt add-account`.",
                    "Basso",
                );
                last_notified = Some(Instant::now());
            }
        }
    }
}

/// Sends a single lightweight prefetch request for `account` immediately after its
/// cooldown expires, so the router has fresh rate-limit headers before the next
/// real request arrives.
async fn post_cooldown_prefetch(
    client: &reqwest::Client,
    account: &crate::config::AccountConfig,
    token: &str,
    state: &StateStore,
    upstream_url: &str,
) {
    let Some((path, body)) = account.provider.prefetch_request() else {
        if let Some(probe_path) = account.provider.auth_probe_get_path() {
            auth_probe_get(client, probe_path, account, state).await;
        }
        return;
    };
    let url = format!("{upstream_url}{path}");
    match prefetch_send(client, &url, &account.provider, token, &body).await {
        Ok(r) => {
            if let Some(info) = account.provider.parse_rate_limits(r.headers()) {
                state.update_rate_limits(&account.name, info);
                tracing::info!(account = %account.name, "post-cooldown prefetch: quota refreshed");
            }
        }
        Err(e) => warn!(account = %account.name, "post-cooldown prefetch failed: {e}"),
    }
}

/// Watches for account cooldowns expiring and triggers a post-cooldown prefetch
/// so each account re-enters rotation with fresh rate-limit metrics.
///
/// Analogous to `recovery_watcher` (which handles `auth_failed` accounts), but
/// for timed cooldowns (429 / 529 / 401 / 403 backoffs). Sleeps precisely until
/// the next cooldown deadline rather than polling at a fixed interval.
///
/// Also handles stale rate-limit data: if an account's rate-limit snapshot is
/// older than STALE_RL_MS and the account is available, a lightweight prefetch
/// is triggered so the router always has fresh utilization metrics.
pub async fn cooldown_watcher(
    config: Arc<Config>,
    state: StateStore,
    credentials: LiveCredentials,
) {
    /// Re-fetch rate-limit headers if data is older than 1 hour.
    const STALE_RL_MS: u64 = 60 * 60_000;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();

    // In-memory: the cooldown_until_ms value we already ran a post-resume for.
    // Prevents re-triggering on every poll after expiry.
    let mut last_resumed: HashMap<String, u64> = HashMap::new();
    // Accounts whose cooldown was long enough (≥5 min) to deserve a "back online" notification.
    let mut notify_on_resume: HashSet<String> = HashSet::new();
    // Epoch-ms of the last successful stale-prefetch per account.
    let mut last_stale_prefetch: HashMap<String, u64> = HashMap::new();

    loop {
        let states = state.account_states();
        let rl_snapshot = state.rate_limit_snapshot();
        let now = now_ms();
        let mut next_wake_ms: Option<u64> = None;

        for account in &config.accounts {
            let Some(st) = states.get(&account.name) else { continue };
            if st.disabled { continue; } // auth_failed or permanently disabled
            let cdl = st.cooldown_until_ms;

            if cdl > 0 && cdl <= now {
                // Cooldown expired — skip if we already handled this exact deadline
                let handled = last_resumed.get(&account.name).map(|&t| t >= cdl).unwrap_or(false);
                if !handled {
                    tracing::info!(account = %account.name, "cooldown expired — strong resume prefetch");
                    let token = {
                        let creds = credentials.read().await;
                        creds.get(&account.name).map(|c| c.access_token.clone())
                    };
                    if let Some(token) = token {
                        post_cooldown_prefetch(
                            &client, account, &token, &state,
                            &config.server.upstream_url,
                        ).await;
                    }
                    if notify_on_resume.remove(&account.name) {
                        notify(
                            "shunt: Account Resumed",
                            &format!("Account '{}' is back online.", account.name),
                            "Glass",
                        );
                    }
                    last_resumed.insert(account.name.clone(), cdl);
                    last_stale_prefetch.insert(account.name.clone(), now);
                }
            } else if cdl > now {
                // Still cooling — schedule wake at expiry; flag for notification if long
                let remaining = cdl - now;
                if remaining >= 5 * 60_000 {
                    notify_on_resume.insert(account.name.clone());
                }
                next_wake_ms = Some(next_wake_ms.map(|m| m.min(cdl)).unwrap_or(cdl));
            } else {
                // Not in cooldown — check for stale rate-limit data
                let rl_age = rl_snapshot
                    .get(&account.name)
                    .map(|r| now.saturating_sub(r.updated_ms))
                    .unwrap_or(u64::MAX); // no data → treat as infinitely stale
                let last_fetched = last_stale_prefetch.get(&account.name).copied().unwrap_or(0);
                let fetched_ago = now.saturating_sub(last_fetched);

                if rl_age >= STALE_RL_MS && fetched_ago >= STALE_RL_MS {
                    tracing::debug!(
                        account = %account.name,
                        age_min = rl_age / 60_000,
                        "rate-limit data stale — refreshing"
                    );
                    let token = {
                        let creds = credentials.read().await;
                        creds.get(&account.name).map(|c| c.access_token.clone())
                    };
                    if let Some(token) = token {
                        post_cooldown_prefetch(
                            &client, account, &token, &state,
                            &config.server.upstream_url,
                        ).await;
                    }
                    last_stale_prefetch.insert(account.name.clone(), now);
                }
            }
        }

        // Sleep exactly until the next cooldown expires; fall back to 30s poll
        let sleep_ms = next_wake_ms
            .map(|wake| wake.saturating_sub(now_ms()).max(50))
            .unwrap_or(30_000);
        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
    }
}

use crate::notify::notify;

// ---------------------------------------------------------------------------
// OpenAI-compatible API (translates to Anthropic Claude)
// ---------------------------------------------------------------------------
//
// When the OpenAI proxy receives a request at /v1/chat/completions, if an
// anthropic_base_url is configured, it translates the request to Anthropic
// Messages format and forwards it to the Anthropic proxy (which handles
// account selection, token management, and rate limiting).
// The response is translated back to OpenAI Chat Completions format.

/// Map OpenAI model names → Claude model names.
/// Claude model names are passed through unchanged; only OpenAI aliases are remapped.
fn map_model(openai_model: &str) -> String {
    if openai_model.starts_with("claude-") {
        return openai_model.to_owned();
    }
    match openai_model {
        "gpt-4o" | "gpt-4.5" | "o1" | "o1-pro" | "o3" | "o3-pro" | "gpt-5" | "gpt-5.5" => {
            "claude-opus-4-6"
        }
        "gpt-4o-mini" | "gpt-4o-mini-2024-07-18" | "o1-mini" | "o3-mini" => {
            "claude-haiku-4-5-20251001"
        }
        _ => "claude-sonnet-4-6",
    }.to_owned()
}

/// Translate an OpenAI Chat Completions request body to an Anthropic Messages body.
fn translate_to_anthropic(body: serde_json::Value) -> serde_json::Value {
    let model = body["model"].as_str().unwrap_or("gpt-4o");
    let claude_model = map_model(model);

    // Extract system message from messages array.
    let mut system: Option<String> = None;
    let mut messages = Vec::new();
    if let Some(arr) = body["messages"].as_array() {
        for msg in arr {
            let role = msg["role"].as_str().unwrap_or("");
            if role == "system" {
                // system can be a string or array of content parts
                let content = msg["content"].as_str()
                    .map(|s| s.to_owned())
                    .unwrap_or_else(|| serde_json::to_string(&msg["content"]).unwrap_or_default());
                system = Some(content);
            } else if role == "tool" {
                // OpenAI tool result → Anthropic tool_result content block
                let tool_use_id = msg["tool_call_id"].as_str().unwrap_or("").to_owned();
                let content = msg["content"].as_str().unwrap_or("").to_owned();
                messages.push(json!({
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content}]
                }));
            } else {
                // Check for tool_calls in assistant messages
                if let Some(tool_calls) = msg["tool_calls"].as_array() {
                    let mut content_blocks: Vec<serde_json::Value> = Vec::new();
                    if let Some(text) = msg["content"].as_str().filter(|s| !s.is_empty()) {
                        content_blocks.push(json!({"type": "text", "text": text}));
                    }
                    for tc in tool_calls {
                        content_blocks.push(json!({
                            "type": "tool_use",
                            "id": tc["id"].as_str().unwrap_or(""),
                            "name": tc["function"]["name"].as_str().unwrap_or(""),
                            "input": serde_json::from_str::<serde_json::Value>(
                                tc["function"]["arguments"].as_str().unwrap_or("{}")
                            ).unwrap_or(json!({})),
                        }));
                    }
                    messages.push(json!({"role": "assistant", "content": content_blocks}));
                } else {
                    let content = msg["content"].as_str().unwrap_or("").to_owned();
                    messages.push(json!({ "role": role, "content": content }));
                }
            }
        }
    }

    let max_tokens = body["max_tokens"].as_u64().unwrap_or(8096);
    let stream = body["stream"].as_bool().unwrap_or(false);

    let mut req = json!({
        "model": claude_model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": stream,
    });

    if let Some(sys) = system {
        req["system"] = json!(sys);
    }
    if let Some(temp) = body.get("temperature") {
        req["temperature"] = temp.clone();
    }
    if let Some(sp) = body.get("stop") {
        req["stop_sequences"] = sp.clone();
    }

    // Translate OpenAI tools → Anthropic tools format
    if let Some(tools) = body["tools"].as_array() {
        let claude_tools: Vec<serde_json::Value> = tools.iter().filter_map(|t| {
            let func = &t["function"];
            Some(json!({
                "name": func["name"].as_str()?,
                "description": func["description"].as_str().unwrap_or(""),
                "input_schema": func.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
            }))
        }).collect();
        if !claude_tools.is_empty() {
            req["tools"] = json!(claude_tools);
        }
    }

    req
}

/// Translate a complete (non-streaming) Anthropic Messages response to OpenAI format.
fn translate_from_anthropic(body: serde_json::Value) -> serde_json::Value {
    let id = format!("chatcmpl-{}", &uuid_v4()[..8]);
    let model = body["model"].as_str().unwrap_or("claude-sonnet-4-6").to_owned();

    // Extract text content and tool_use blocks.
    let mut text_content = String::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
    if let Some(blocks) = body["content"].as_array() {
        for (idx, block) in blocks.iter().enumerate() {
            match block["type"].as_str() {
                Some("text") => {
                    text_content.push_str(block["text"].as_str().unwrap_or(""));
                }
                Some("tool_use") => {
                    let args = match &block["input"] {
                        serde_json::Value::String(s) => s.clone(),
                        v => serde_json::to_string(v).unwrap_or_default(),
                    };
                    tool_calls.push(json!({
                        "id": block["id"].as_str().unwrap_or(""),
                        "type": "function",
                        "index": idx,
                        "function": {
                            "name": block["name"].as_str().unwrap_or(""),
                            "arguments": args,
                        }
                    }));
                }
                _ => {}
            }
        }
    }

    let stop_reason = body["stop_reason"].as_str().unwrap_or("end_turn");
    let finish_reason = match stop_reason {
        "end_turn"   => "stop",
        "tool_use"   => "tool_calls",
        "max_tokens" => "length",
        other        => other,
    };

    let input_tokens = body["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = body["usage"]["output_tokens"].as_u64().unwrap_or(0);

    let mut message = json!({"role": "assistant", "content": text_content});
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    json!({
        "id": id,
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    })
}

fn uuid_v4() -> String {
    use crate::oauth::rand_bytes;
    let b: [u8; 16] = rand_bytes();
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(b[0..4].try_into().unwrap()),
        u16::from_be_bytes(b[4..6].try_into().unwrap()),
        u16::from_be_bytes(b[6..8].try_into().unwrap()),
        u16::from_be_bytes(b[8..10].try_into().unwrap()),
        {
            let mut v = 0u64;
            for &x in &b[10..16] { v = (v << 8) | x as u64; }
            v
        }
    )
}

/// GET /v1/models — return Claude models in OpenAI format.
async fn openai_models_handler() -> impl IntoResponse {
    axum::Json(json!({
        "object": "list",
        "data": [
            { "id": "claude-opus-4-6",           "object": "model", "owned_by": "anthropic" },
            { "id": "claude-sonnet-4-6",          "object": "model", "owned_by": "anthropic" },
            { "id": "claude-haiku-4-5-20251001",  "object": "model", "owned_by": "anthropic" },
        ]
    }))
}

/// POST /v1/chat/completions — translate OpenAI request to Anthropic, proxy through Claude pool.
async fn openai_compat_handler(
    State(s): State<AppState>,
    req: Request,
) -> Result<Response, ProxyError> {
    let Some(ref anthropic_url) = s.anthropic_base_url else {
        // No Anthropic proxy configured — fall back to normal forwarding
        return proxy_handler(State(s), req).await;
    };

    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| ProxyError::BodyRead)?;

    let openai_body: serde_json::Value = serde_json::from_slice(&body_bytes)
        .unwrap_or(json!({}));

    let stream = openai_body["stream"].as_bool().unwrap_or(false);
    let anthropic_body = translate_to_anthropic(openai_body);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|_| ProxyError::Upstream)?;

    let resp = client
        .post(format!("{anthropic_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
        .header("x-shunt-compat", "openai")
        .json(&anthropic_body)
        .send()
        .await
        .map_err(|_| ProxyError::Upstream)?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let code = status.as_u16();
        return Ok(axum::response::Response::builder()
            .status(code)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap());
    }

    if stream {
        // Translate Anthropic SSE stream → OpenAI SSE stream
        let chat_id = format!("chatcmpl-{}", &uuid_v4()[..8]);
        let stream = translate_anthropic_stream(resp, chat_id);
        Ok(axum::response::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(axum::body::Body::from_stream(stream))
            .unwrap())
    } else {
        let anthropic_resp: serde_json::Value = resp.json().await.map_err(|_| ProxyError::Upstream)?;
        let openai_resp = translate_from_anthropic(anthropic_resp);
        Ok(axum::Json(openai_resp).into_response())
    }
}

/// Translate Anthropic SSE events to OpenAI SSE format, yielding raw bytes.
/// Handles text content, tool_use blocks, and finish reasons.
fn translate_anthropic_stream(
    resp: reqwest::Response,
    chat_id: String,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures_util::StreamExt;

    let id = chat_id;
    let byte_stream = resp.bytes_stream();

    async_stream::stream! {
        let mut buf = String::new();
        // Per-block state: block_index -> (tool_call_oai_index, tool_id, tool_name)
        let mut tool_blocks: std::collections::HashMap<u64, (usize, String, String)> = std::collections::HashMap::new();
        let mut tool_call_count: usize = 0;
        futures_util::pin_mut!(byte_stream);

        // Send initial role chunk
        let init = format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id": id,
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
            })).unwrap()
        );
        yield Ok(bytes::Bytes::from(init));

        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => break,
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE lines
            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_owned();
                buf = buf[nl + 1..].to_owned();

                if !line.starts_with("data: ") { continue; }
                let data = &line["data: ".len()..];
                if data == "[DONE]" { continue; }

                let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else { continue };
                let event_type = event["type"].as_str().unwrap_or("");

                let maybe_chunk = match event_type {
                    "content_block_start" => {
                        let block_idx = event["index"].as_u64().unwrap_or(0);
                        let cb = &event["content_block"];
                        if cb["type"].as_str() == Some("tool_use") {
                            let tool_id = cb["id"].as_str().unwrap_or("").to_owned();
                            let tool_name = cb["name"].as_str().unwrap_or("").to_owned();
                            let oai_idx = tool_call_count;
                            tool_call_count += 1;
                            tool_blocks.insert(block_idx, (oai_idx, tool_id.clone(), tool_name.clone()));
                            Some(json!({
                                "id": id,
                                "object": "chat.completion.chunk",
                                "choices": [{"index": 0, "delta": {
                                    "tool_calls": [{
                                        "index": oai_idx,
                                        "id": tool_id,
                                        "type": "function",
                                        "function": {"name": tool_name, "arguments": ""}
                                    }]
                                }, "finish_reason": null}]
                            }))
                        } else {
                            None
                        }
                    }
                    "content_block_delta" => {
                        let block_idx = event["index"].as_u64().unwrap_or(0);
                        let delta = &event["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => {
                                let text = delta["text"].as_str().unwrap_or("");
                                if text.is_empty() { continue; }
                                Some(json!({
                                    "id": id,
                                    "object": "chat.completion.chunk",
                                    "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                                }))
                            }
                            Some("input_json_delta") => {
                                let args = delta["partial_json"].as_str().unwrap_or("");
                                if let Some((oai_idx, _, _)) = tool_blocks.get(&block_idx) {
                                    Some(json!({
                                        "id": id,
                                        "object": "chat.completion.chunk",
                                        "choices": [{"index": 0, "delta": {
                                            "tool_calls": [{"index": oai_idx, "function": {"arguments": args}}]
                                        }, "finish_reason": null}]
                                    }))
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                    }
                    "message_delta" => {
                        let stop_reason = event["delta"]["stop_reason"].as_str().unwrap_or("stop");
                        let finish = match stop_reason {
                            "end_turn"  => "stop",
                            "tool_use"  => "tool_calls",
                            "max_tokens" => "length",
                            other       => other,
                        };
                        Some(json!({
                            "id": id,
                            "object": "chat.completion.chunk",
                            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
                        }))
                    }
                    _ => None,
                };

                if let Some(c) = maybe_chunk {
                    let out = format!("data: {}\n\n", serde_json::to_string(&c).unwrap());
                    yield Ok(bytes::Bytes::from(out));
                }
            }
        }

        yield Ok(bytes::Bytes::from("data: [DONE]\n\n"));
    }
}

// ---------------------------------------------------------------------------
// Cross-protocol translation: Anthropic ↔ OpenAI
// ---------------------------------------------------------------------------

/// Translate an Anthropic `/v1/messages` request body to OpenAI `/v1/chat/completions` format.
/// Used when routing an Anthropic-protocol request to an OpenAI/Codex account.
fn translate_anthropic_req_to_openai(body: serde_json::Value) -> serde_json::Value {
    let model = body["model"].as_str().unwrap_or("claude-sonnet-4-6");
    let stream = body["stream"].as_bool().unwrap_or(false);
    let max_tokens = body["max_tokens"].as_u64().unwrap_or(8096);

    let mut messages: Vec<serde_json::Value> = Vec::new();

    // Prepend system prompt if present.
    if let Some(sys) = body["system"].as_str().filter(|s| !s.is_empty()) {
        messages.push(json!({"role": "system", "content": sys}));
    }

    if let Some(arr) = body["messages"].as_array() {
        for msg in arr {
            let role = msg["role"].as_str().unwrap_or("user");

            if let Some(blocks) = msg["content"].as_array() {
                // Check for tool_result blocks (user turn carrying tool results).
                let has_tool_result = blocks.iter().any(|b| b["type"] == "tool_result");
                if has_tool_result {
                    for b in blocks {
                        if b["type"] == "tool_result" {
                            let content = b["content"].as_str()
                                .map(|s| s.to_owned())
                                .unwrap_or_else(|| serde_json::to_string(&b["content"]).unwrap_or_default());
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": b["tool_use_id"].as_str().unwrap_or(""),
                                "content": content,
                            }));
                        }
                    }
                    continue;
                }

                // Regular content blocks — may include text and tool_use.
                let mut text = String::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
                        Some("tool_use") => {
                            let args = match &b["input"] {
                                serde_json::Value::String(s) => s.clone(),
                                v => serde_json::to_string(v).unwrap_or_default(),
                            };
                            tool_calls.push(json!({
                                "id": b["id"].as_str().unwrap_or(""),
                                "type": "function",
                                "function": {"name": b["name"].as_str().unwrap_or(""), "arguments": args},
                            }));
                        }
                        _ => {}
                    }
                }
                let mut m = json!({"role": role, "content": text});
                if !tool_calls.is_empty() {
                    m["tool_calls"] = json!(tool_calls);
                }
                messages.push(m);
            } else if let Some(s) = msg["content"].as_str() {
                messages.push(json!({"role": role, "content": s}));
            }
        }
    }

    let mut req = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": stream,
    });

    // Request usage data in stream final chunk.
    if stream {
        req["stream_options"] = json!({"include_usage": true});
    }
    if let Some(t) = body.get("temperature") { req["temperature"] = t.clone(); }
    if let Some(sp) = body.get("stop_sequences") { req["stop"] = sp.clone(); }

    // Anthropic tools → OpenAI tools.
    if let Some(tools) = body["tools"].as_array() {
        let oai: Vec<serde_json::Value> = tools.iter().map(|t| json!({
            "type": "function",
            "function": {
                "name": t["name"].as_str().unwrap_or(""),
                "description": t["description"].as_str().unwrap_or(""),
                "parameters": t.get("input_schema").cloned()
                    .unwrap_or(json!({"type": "object", "properties": {}})),
            }
        })).collect();
        if !oai.is_empty() { req["tools"] = json!(oai); }
    }

    if let Some(tc) = body.get("tool_choice") {
        req["tool_choice"] = match tc["type"].as_str() {
            Some("any")  => json!({"type": "required"}),
            Some("tool") => json!({"type": "function", "function": {"name": tc["name"]}}),
            _            => json!("auto"),
        };
    }

    req
}

/// Translate an OpenAI `/v1/chat/completions` non-streaming response to Anthropic format.
fn translate_openai_resp_to_anthropic(body: serde_json::Value, model: &str) -> serde_json::Value {
    let id = format!("msg_{}", &uuid_v4()[..8]);
    let choice = &body["choices"][0];
    let msg = &choice["message"];

    let mut content: Vec<serde_json::Value> = Vec::new();
    if let Some(text) = msg["content"].as_str().filter(|s| !s.is_empty()) {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for tc in tcs {
            content.push(json!({
                "type": "tool_use",
                "id": tc["id"].as_str().unwrap_or(""),
                "name": tc["function"]["name"].as_str().unwrap_or(""),
                "input": serde_json::from_str::<serde_json::Value>(
                    tc["function"]["arguments"].as_str().unwrap_or("{}")
                ).unwrap_or(json!({})),
            }));
        }
    }

    let stop_reason = match choice["finish_reason"].as_str().unwrap_or("stop") {
        "stop"       => "end_turn",
        "tool_calls" => "tool_use",
        "length"     => "max_tokens",
        other        => other,
    };

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens":  body["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            "output_tokens": body["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        }
    })
}

/// Translate the response back from OpenAI format to Anthropic format.
/// Handles both streaming and non-streaming responses.
async fn translate_response_openai_to_anthropic(resp: Response, model: &str) -> Response {
    use axum::body::Body;
    let msg_id = format!("msg_{}", &uuid_v4()[..8]);
    let model = model.to_owned();

    if quota::is_streaming_response(&resp) {
        let (mut parts, body) = resp.into_parts();
        parts.headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        let stream = translate_openai_stream_to_anthropic(body, model, msg_id);
        Response::from_parts(parts, Body::from_stream(stream))
    } else {
        let (mut parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap_or_default();
        let openai_val: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
        let anthropic_val = translate_openai_resp_to_anthropic(openai_val, &model);
        let out = serde_json::to_vec(&anthropic_val).unwrap_or_default();
        parts.headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        Response::from_parts(parts, Body::from(out))
    }
}

/// Translate the response back from Anthropic format to OpenAI format.
async fn translate_response_anthropic_to_openai(resp: Response) -> Response {
    use axum::body::Body;
    let chat_id = format!("chatcmpl-{}", &uuid_v4()[..8]);

    if quota::is_streaming_response(&resp) {
        let (parts, body) = resp.into_parts();
        let stream = translate_body_anthropic_to_openai(body, chat_id);
        Response::from_parts(parts, Body::from_stream(stream))
    } else {
        let (mut parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap_or_default();
        let anthropic_val: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
        let openai_val = translate_from_anthropic(anthropic_val);
        let out = serde_json::to_vec(&openai_val).unwrap_or_default();
        parts.headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        Response::from_parts(parts, Body::from(out))
    }
}

/// Stream-translate an OpenAI SSE response body into Anthropic SSE events.
///
/// Emits: `message_start` → `content_block_start` → N×`content_block_delta`
///       → `content_block_stop` → `message_delta` → `message_stop`
fn translate_openai_stream_to_anthropic(
    body: axum::body::Body,
    model: String,
    msg_id: String,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures_util::StreamExt;

    async_stream::stream! {
        // Send message_start immediately (input_tokens unknown yet, use 0).
        let start_evt = format!(
            "event: message_start\ndata: {}\n\nevent: ping\ndata: {{\"type\":\"ping\"}}\n\n",
            serde_json::to_string(&json!({
                "type": "message_start",
                "message": {
                    "id": msg_id, "type": "message", "role": "assistant",
                    "content": [], "model": model, "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            })).unwrap()
        );
        yield Ok(bytes::Bytes::from(start_evt));

        let mut buf = String::new();
        let mut content_block_open = false;
        let mut tool_blocks: std::collections::HashMap<u64, (usize, String, String)> = std::collections::HashMap::new();
        let mut tool_call_count: usize = 0;
        let mut output_tokens: u64 = 0;
        let mut input_tokens: u64 = 0;
        let byte_stream = body.into_data_stream();
        futures_util::pin_mut!(byte_stream);

        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk { Ok(c) => c, Err(_) => break };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_owned();
                buf = buf[nl + 1..].to_owned();
                if !line.starts_with("data: ") { continue; }
                let data = &line["data: ".len()..];
                if data == "[DONE]" { continue; }
                let Ok(ev) = serde_json::from_str::<serde_json::Value>(data) else { continue };

                // Collect usage from final chunk (stream_options.include_usage).
                if let Some(u) = ev.get("usage") {
                    input_tokens  = u["prompt_tokens"].as_u64().unwrap_or(input_tokens);
                    output_tokens = u["completion_tokens"].as_u64().unwrap_or(output_tokens);
                }

                let choice = &ev["choices"][0];
                let delta = &choice["delta"];
                let finish = choice["finish_reason"].as_str();

                // Text delta.
                if let Some(text) = delta["content"].as_str().filter(|s| !s.is_empty()) {
                    if !content_block_open {
                        content_block_open = true;
                        let cb = format!(
                            "event: content_block_start\ndata: {}\n\n",
                            serde_json::to_string(&json!({
                                "type": "content_block_start", "index": 0,
                                "content_block": {"type": "text", "text": ""}
                            })).unwrap()
                        );
                        yield Ok(bytes::Bytes::from(cb));
                    }
                    let d = format!(
                        "event: content_block_delta\ndata: {}\n\n",
                        serde_json::to_string(&json!({
                            "type": "content_block_delta", "index": 0,
                            "delta": {"type": "text_delta", "text": text}
                        })).unwrap()
                    );
                    yield Ok(bytes::Bytes::from(d));
                }

                // Tool call deltas.
                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let oai_idx = tc["index"].as_u64().unwrap_or(0);
                        // New tool call: emit content_block_start for tool_use.
                        if let Some(id) = tc["id"].as_str() {
                            let name = tc["function"]["name"].as_str().unwrap_or("").to_owned();
                            let my_idx = tool_call_count;
                            tool_call_count += 1;
                            tool_blocks.insert(oai_idx, (my_idx, id.to_owned(), name.clone()));
                            let cb = format!(
                                "event: content_block_start\ndata: {}\n\n",
                                serde_json::to_string(&json!({
                                    "type": "content_block_start",
                                    "index": my_idx + 1, // +1: text block at 0
                                    "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                                })).unwrap()
                            );
                            yield Ok(bytes::Bytes::from(cb));
                        }
                        // Streaming arguments.
                        if let Some(args_chunk) = tc["function"]["arguments"].as_str() {
                            if let Some(&(my_idx, _, _)) = tool_blocks.get(&oai_idx) {
                                let d = format!(
                                    "event: content_block_delta\ndata: {}\n\n",
                                    serde_json::to_string(&json!({
                                        "type": "content_block_delta",
                                        "index": my_idx + 1,
                                        "delta": {"type": "input_json_delta", "partial_json": args_chunk}
                                    })).unwrap()
                                );
                                yield Ok(bytes::Bytes::from(d));
                            }
                        }
                    }
                }

                // Finish reason → close blocks + message_delta + message_stop.
                if let Some(fr) = finish {
                    let stop_reason = match fr {
                        "stop"       => "end_turn",
                        "tool_calls" => "tool_use",
                        "length"     => "max_tokens",
                        other        => other,
                    };

                    // Close open content/tool blocks.
                    if content_block_open {
                        yield Ok(bytes::Bytes::from(format!(
                            "event: content_block_stop\ndata: {}\n\n",
                            serde_json::to_string(&json!({"type":"content_block_stop","index":0})).unwrap()
                        )));
                    }
                    for (_, (my_idx, _, _)) in &tool_blocks {
                        yield Ok(bytes::Bytes::from(format!(
                            "event: content_block_stop\ndata: {}\n\n",
                            serde_json::to_string(&json!({"type":"content_block_stop","index": my_idx + 1})).unwrap()
                        )));
                    }

                    yield Ok(bytes::Bytes::from(format!(
                        "event: message_delta\ndata: {}\n\n",
                        serde_json::to_string(&json!({
                            "type": "message_delta",
                            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                            "usage": {"output_tokens": output_tokens}
                        })).unwrap()
                    )));
                    yield Ok(bytes::Bytes::from(
                        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                    ));
                }
            }
        }
    }
}

/// Stream-translate an Anthropic SSE response body (from axum `Body`) into OpenAI SSE format.
/// Equivalent to `translate_anthropic_stream` but consumes an axum `Body` instead of a
/// `reqwest::Response`, so it can be used after the forwarder returns.
fn translate_body_anthropic_to_openai(
    body: axum::body::Body,
    chat_id: String,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures_util::StreamExt;

    async_stream::stream! {
        let id = chat_id;

        // Initial role chunk.
        let init = format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id": id, "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
            })).unwrap()
        );
        yield Ok(bytes::Bytes::from(init));

        let mut buf = String::new();
        let mut tool_blocks: std::collections::HashMap<u64, (usize, String, String)> = std::collections::HashMap::new();
        let mut tool_call_count: usize = 0;
        let byte_stream = body.into_data_stream();
        futures_util::pin_mut!(byte_stream);

        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk { Ok(c) => c, Err(_) => break };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_owned();
                buf = buf[nl + 1..].to_owned();
                if !line.starts_with("data: ") { continue; }
                let data = &line["data: ".len()..];
                if data == "[DONE]" { continue; }
                let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else { continue };
                let event_type = event["type"].as_str().unwrap_or("");

                let maybe_chunk = match event_type {
                    "content_block_start" => {
                        let block_idx = event["index"].as_u64().unwrap_or(0);
                        let cb = &event["content_block"];
                        if cb["type"].as_str() == Some("tool_use") {
                            let tool_id = cb["id"].as_str().unwrap_or("").to_owned();
                            let tool_name = cb["name"].as_str().unwrap_or("").to_owned();
                            let oai_idx = tool_call_count;
                            tool_call_count += 1;
                            tool_blocks.insert(block_idx, (oai_idx, tool_id.clone(), tool_name.clone()));
                            Some(json!({
                                "id": id, "object": "chat.completion.chunk",
                                "choices": [{"index": 0, "delta": {
                                    "tool_calls": [{"index": oai_idx, "id": tool_id, "type": "function",
                                        "function": {"name": tool_name, "arguments": ""}}]
                                }, "finish_reason": null}]
                            }))
                        } else { None }
                    }
                    "content_block_delta" => {
                        let block_idx = event["index"].as_u64().unwrap_or(0);
                        let delta = &event["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => {
                                let text = delta["text"].as_str().unwrap_or("");
                                if text.is_empty() { continue; }
                                Some(json!({
                                    "id": id, "object": "chat.completion.chunk",
                                    "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                                }))
                            }
                            Some("input_json_delta") => {
                                let args = delta["partial_json"].as_str().unwrap_or("");
                                tool_blocks.get(&block_idx).map(|(oai_idx, _, _)| json!({
                                    "id": id, "object": "chat.completion.chunk",
                                    "choices": [{"index": 0, "delta": {
                                        "tool_calls": [{"index": oai_idx, "function": {"arguments": args}}]
                                    }, "finish_reason": null}]
                                }))
                            }
                            _ => None,
                        }
                    }
                    "message_delta" => {
                        let stop_reason = event["delta"]["stop_reason"].as_str().unwrap_or("stop");
                        let finish = match stop_reason {
                            "end_turn"   => "stop",
                            "tool_use"   => "tool_calls",
                            "max_tokens" => "length",
                            other        => other,
                        };
                        Some(json!({
                            "id": id, "object": "chat.completion.chunk",
                            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
                        }))
                    }
                    _ => None,
                };

                if let Some(c) = maybe_chunk {
                    let out = format!("data: {}\n\n", serde_json::to_string(&c).unwrap());
                    yield Ok(bytes::Bytes::from(out));
                }
            }
        }
        yield Ok(bytes::Bytes::from("data: [DONE]\n\n"));
    }
}
