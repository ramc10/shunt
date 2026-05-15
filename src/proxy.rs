use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
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
    /// Epoch-ms when this proxy instance started.
    started_ms: u64,
}

pub fn create_app(config: Config) -> anyhow::Result<Router> {
    let (app, _) = create_app_with_state(config, StateStore::load(&state_path()))?;
    Ok(app)
}

/// Shared live credentials map — can be written to without restarting the proxy.
pub type LiveCredentials = Arc<RwLock<HashMap<String, OAuthCredential>>>;

pub fn create_app_with_state(
    config: Config,
    state: StateStore,
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
        started_ms: now_ms(),
    };

    // Register proxy routes appropriate for the provider.
    // Anthropic: explicit paths only (maintains existing behaviour).
    // OpenAI/others: wildcard catches all paths with any HTTP method.
    let provider = app_state.config.accounts.first()
        .map(|a| &a.provider)
        .cloned()
        .unwrap_or_default();

    let proxy_routes = match provider {
        Provider::Anthropic => Router::new()
            .route("/v1/messages", post(proxy_handler))
            .route("/v1/messages/count_tokens", post(proxy_handler)),
        Provider::OpenAI => Router::new()
            .route("/*path", any(proxy_handler)),
    };

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
        let total_tokens = quota.map(|q| q.total_tokens()).unwrap_or(0);
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
            "total_tokens": total_tokens,
            "window_expires_ms": window_expires_ms,
            "tokens_used": tokens_used,
            "rate_limit": rate_limit,
        })
    }).collect();

    let recent_requests = s.state.recent_requests_snapshot();

    axum::Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "started_ms": s.started_ms,
        "accounts": accounts,
        "pinned_account": s.state.get_pinned(),
        "last_used_account": s.state.get_last_used(),
        "recent_requests": recent_requests,
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

    loop {
        let account = match router::pick_account(
            &s.config.accounts, &s.state, fp_ref, &tried,
            s.config.server.sticky_ttl_ms, s.config.server.expiry_soon_secs,
        ) {
            Some(a) => a,
            None => return Err(ProxyError::AllAccountsUnavailable),
        };

        let account_name = account.name.clone();

        // Use the live (possibly refreshed) token rather than the one baked into config.
        let token = {
            let creds = s.credentials.read().await;
            creds.get(&account_name)
                .map(|c| c.access_token.clone())
                .or_else(|| account.credential.as_ref().map(|c| c.access_token.clone()))
                .unwrap_or_default()
        };

        let response = s.forwarder
            .forward(&method, &path, body_bytes.clone(), &headers, account, &token)
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
                return Ok(tap_usage(response, &s.state, &account_name, &model, req_start_ms).await);
            }
            429 => {
                warn!(account = %account_name, "429 rate-limited — cooling 60s");
                if let Some(info) = account.provider.parse_rate_limits(response.headers()) {
                    s.state.update_rate_limits(&account_name, info);
                }
                s.state.set_cooldown(&account_name, 60_000);
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
                    let cred = {
                        let creds = s.credentials.read().await;
                        creds.get(&account_name).cloned()
                            .or_else(|| account.credential.clone())
                    };
                    let Some(cred) = cred else {
                        tried.insert(account_name);
                        continue;
                    };
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
                                store.accounts.insert(name, fresh);
                                store.save().ok();
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
pub async fn prefetch_rate_limits(config: Arc<Config>, state: StateStore) {
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
        let Some((path, body)) = account.provider.prefetch_request() else { continue };
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
                        store.accounts.insert(name_owned, fresh_owned);
                        store.save().ok();
                    });
                    state.clear_auth_failed(name);
                    any_recovered = true;
                }
                Ok(Err(e)) => {
                    tracing::error!(account = %name, error = %e, "recovery: token refresh failed");
                }
                Err(_) => {
                    tracing::error!(account = %name, "recovery: token refresh timed out");
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
                notify_all_accounts_offline();
                last_notified = Some(Instant::now());
            }
        }
    }
}

fn notify_all_accounts_offline() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .args(["-e", concat!(
                r#"display notification "#,
                r#""All accounts have lost authentication. Run `shunt add-account` to re-authorize." "#,
                r#"with title "shunt: All Accounts Offline" sound name "Basso""#
            )])
            .status();
    }
}
