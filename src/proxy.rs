use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex as ParkingMutex;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::config::{state_path, Config, CredentialsStore};
use crate::credential::Credential;
use crate::forwarder::Forwarder;
use crate::provider::Provider;
use crate::quota;
use crate::router;
use crate::state::StateStore;
use crate::telemetry::TelemetryClient;

/// 100 MB limit — sufficient for any LLM request including large context windows.
const MAX_REQUEST_BODY: usize = 100 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    forwarder: Arc<Forwarder>,
    state: StateStore,
    /// Live credentials — can be refreshed at runtime without restarting.
    credentials: Arc<RwLock<HashMap<String, Credential>>>,
    /// Per-account mutex that serialises concurrent token-refresh attempts.
    ///
    /// When multiple in-flight requests hit a 401 for the same account at the
    /// same time, only one should call the upstream OAuth endpoint; the others
    /// should wait and then re-use the fresh token instead of each making their
    /// own refresh call (which would rotate the refresh_token out from under the
    /// others and cause cascading auth failures).
    refresh_locks: Arc<ParkingMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Epoch-ms when this proxy instance started.
    started_ms: u64,
    /// If set, /v1/chat/completions requests are translated and forwarded here
    /// (the Anthropic proxy base URL, e.g. "http://127.0.0.1:8082").
    anthropic_base_url: Option<String>,
    /// Optional relay-server telemetry client.
    telemetry: Option<TelemetryClient>,
    /// Per-IP token-bucket rate limiter (#16). None when rate_limit_rpm == 0.
    rate_limiter: Option<Arc<ParkingMutex<HashMap<IpAddr, TokenBucket>>>>,
}

/// Simple token-bucket for per-IP rate limiting.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: f64) -> Self {
        Self { tokens: capacity, last_refill: Instant::now() }
    }

    /// Refill tokens proportional to elapsed time, then consume one.
    /// Returns true if the request is allowed.
    fn check_and_consume(&mut self, rpm: f64) -> bool {
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        self.last_refill = Instant::now();
        // Refill at rpm/60 tokens per second; cap at burst (10 tokens).
        let burst = (rpm / 6.0).max(10.0);
        self.tokens = (self.tokens + elapsed * rpm / 60.0).min(burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub fn create_app(config: Config) -> anyhow::Result<Router> {
    let (app, _, _) = create_app_with_state(config, StateStore::load(&state_path()), None)?;
    Ok(app)
}

/// Shared live credentials map — can be written to without restarting the proxy.
pub type LiveCredentials = Arc<RwLock<HashMap<String, Credential>>>;

/// Create a pure proxy app (no management routes).
/// Registers /v1/messages, /v1/chat/completions, /v1/models, and a fallback.
/// Build a shared `AppState` and the `LiveCredentials` handle it references.
fn build_app_state(
    config: Config,
    state: StateStore,
    anthropic_base_url: Option<String>,
) -> anyhow::Result<(AppState, LiveCredentials)> {
    let forwarder = Forwarder::new(config.server.request_timeout_secs)?;

    for a in &config.accounts {
        if a.provider.auth_kind() == crate::provider::AuthKind::None {
            // Local providers never need credentials — clear any stale auth_failed from disk.
            state.clear_auth_failed(&a.name);
        } else if a.credential.is_none() {
            state.set_auth_failed(&a.name);
        }
    }

    let credentials: LiveCredentials = Arc::new(RwLock::new(
        config.accounts.iter()
            .filter_map(|a| a.credential.as_ref().map(|c| (a.name.clone(), c.clone())))
            .collect::<HashMap<_, _>>(),
    ));

    let telemetry = config.server.telemetry_url.as_deref().map(|url| {
        TelemetryClient::new(url, config.server.telemetry_token.clone(), config.server.instance_name.clone())
    });

    let rate_limiter = if config.server.rate_limit_rpm > 0 {
        Some(Arc::new(ParkingMutex::new(HashMap::<IpAddr, TokenBucket>::new())))
    } else {
        None
    };

    let app_state = AppState {
        config: Arc::new(config),
        forwarder: Arc::new(forwarder),
        state,
        credentials: Arc::clone(&credentials),
        refresh_locks: Arc::new(ParkingMutex::new(HashMap::new())),
        started_ms: now_ms(),
        anthropic_base_url,
        telemetry,
        rate_limiter,
    };

    Ok((app_state, credentials))
}

pub fn create_proxy_app(
    config: Config,
    state: StateStore,
    anthropic_base_url: Option<String>,
) -> anyhow::Result<(Router, LiveCredentials)> {
    let (app_state, credentials) = build_app_state(config, state, anthropic_base_url)?;

    let app = Router::new()
        .route("/v1/messages", post(proxy_handler))
        .route("/v1/messages/count_tokens", post(proxy_handler))
        .route("/v1/chat/completions", post(openai_compat_handler))
        .route("/v1/models", get(openai_models_handler))
        .fallback(proxy_handler)
        .with_state(app_state);

    Ok((app, credentials))
}

/// Create a control plane app (management routes only — sees ALL accounts).
/// Registers /health, /status, /use.
pub fn create_control_app(
    config: Config,
    state: StateStore,
) -> anyhow::Result<Router> {
    let (app_state, _) = build_app_state(config, state, None)?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status_handler))
        .route("/use", post(use_handler))
        .route("/model", get(model_get_handler).post(model_set_handler).delete(model_clear_handler))
        .route("/strategy", get(strategy_get_handler).post(strategy_set_handler).delete(strategy_clear_handler))
        .route("/alerts", get(alerts_get_handler).post(alerts_set_handler))
        .with_state(app_state);

    Ok(app)
}

/// Combined app used by tests and the single-port fallback mode.
/// Includes both proxy routes and management routes (/health, /status, /use)
/// sharing a single AppState so state changes are visible across all routes.
pub fn create_app_with_state(
    config: Config,
    state: StateStore,
    anthropic_base_url: Option<String>,
) -> anyhow::Result<(Router, LiveCredentials, Option<TelemetryClient>)> {
    let (app_state, credentials) = build_app_state(config, state, anthropic_base_url)?;
    let telemetry = app_state.telemetry.clone();

    let app = Router::new()
        // Management routes
        .route("/health", get(health))
        .route("/status", get(status_handler))
        .route("/use", post(use_handler))
        .route("/model", get(model_get_handler).post(model_set_handler).delete(model_clear_handler))
        .route("/strategy", get(strategy_get_handler).post(strategy_set_handler).delete(strategy_clear_handler))
        .route("/alerts", get(alerts_get_handler).post(alerts_set_handler))
        // Proxy routes
        .route("/v1/messages", post(proxy_handler))
        .route("/v1/messages/count_tokens", post(proxy_handler))
        .route("/v1/chat/completions", post(openai_compat_handler))
        .route("/v1/models", get(openai_models_handler))
        .fallback(proxy_handler)
        .with_state(app_state);

    Ok((app, credentials, telemetry))
}

/// Build a status JSON snapshot from config + state — used by the heartbeat loop.
pub fn build_status_snapshot(config: &Config, state: &StateStore, started_ms: u64) -> serde_json::Value {
    let account_states = state.account_states();
    let rate_limits    = state.rate_limit_snapshot();

    let accounts: Vec<_> = config.accounts.iter().map(|a| {
        let st            = account_states.get(&a.name);
        let rl            = rate_limits.get(&a.name);
        let utilization_5h = rl.and_then(|r| r.utilization_5h).unwrap_or(0.0);
        let utilization_7d = rl.and_then(|r| r.utilization_7d).unwrap_or(0.0);
        let reset_5h       = rl.and_then(|r| r.reset_5h);
        let reset_7d       = rl.and_then(|r| r.reset_7d);
        let disabled       = st.map(|s| s.disabled).unwrap_or(false);
        let auth_failed    = st.map(|s| s.auth_failed).unwrap_or(false);
        let health_check_failed = st.map(|s| s.health_check_failed).unwrap_or(false);
        let cooldown_until_ms = st.map(|s| s.cooldown_until_ms).unwrap_or(0);
        let available      = state.is_available(&a.name);
        let email          = a.credential.as_ref().and_then(|c| c.email()).map(|e| e.to_owned());

        json!({
            "name": a.name,
            "email": email,
            "provider": a.provider.to_string(),
            "available": available,
            "disabled": disabled,
            "auth_failed": auth_failed,
            "health_check_failed": health_check_failed,
            "cooldown_until_ms": cooldown_until_ms,
            "utilization_5h": utilization_5h,
            "reset_5h": reset_5h,
            "utilization_7d": utilization_7d,
            "reset_7d": reset_7d,
        })
    }).collect();

    json!({
        "started_ms": started_ms,
        "accounts": accounts,
        "pinned_account": state.get_pinned(),
        "last_used_account": state.get_last_used(),
    })
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
        } else if st.map(|s| s.health_check_failed).unwrap_or(false) {
            "unhealthy"
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
        let email = a.credential.as_ref().and_then(|c| c.email()).map(|e| e.to_owned());
        let disabled = acc_state.map(|s| s.disabled).unwrap_or(false);
        let auth_failed = acc_state.map(|s| s.auth_failed).unwrap_or(false);
        let health_check_failed = acc_state.map(|s| s.health_check_failed).unwrap_or(false);
        let cooldown_until_ms = acc_state.map(|s| s.cooldown_until_ms).unwrap_or(0);
        let utilization_5h = rl.and_then(|r| r.utilization_5h).unwrap_or(0.0);
        let reset_5h = rl.and_then(|r| r.reset_5h);
        let status_5h = rl.and_then(|r| r.status_5h.clone());
        let utilization_7d = rl.and_then(|r| r.utilization_7d).unwrap_or(0.0);
        let reset_7d = rl.and_then(|r| r.reset_7d);
        let status_7d = rl.and_then(|r| r.status_7d.clone());
        let available = s.state.is_available(&a.name);

        json!({
            "name": a.name,
            "email": email,
            "plan_type": a.plan_type,
            "provider": a.provider.to_string(),
            "status": avail_status,
            "available": available,
            "disabled": disabled,
            "auth_failed": auth_failed,
            "health_check_failed": health_check_failed,
            "cooldown_until_ms": cooldown_until_ms,
            "utilization_5h": utilization_5h,
            "reset_5h": reset_5h,
            "status_5h": status_5h,
            "utilization_7d": utilization_7d,
            "reset_7d": reset_7d,
            "status_7d": status_7d,
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
) -> Response {
    let account = body["account"].as_str().map(|s| s.to_owned());
    // Validate the account name exists (unless clearing to auto)
    if let Some(ref name) = account {
        if name != "auto" && !s.config.accounts.iter().any(|a| &a.name == name) {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({
                "error": format!("unknown account '{name}'")
            }))).into_response();
        }
        let pinned = if name == "auto" { None } else { Some(name.clone()) };
        s.state.set_pinned(pinned);
        axum::Json(json!({ "pinned": name })).into_response()
    } else {
        s.state.set_pinned(None);
        axum::Json(json!({ "pinned": null })).into_response()
    }
}

async fn model_get_handler(State(s): State<AppState>) -> impl IntoResponse {
    let model = s.state.get_model_override();
    axum::Json(json!({ "model": model }))
}

async fn model_set_handler(
    State(s): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(model) = body["model"].as_str() else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "missing model field" }))).into_response();
    };
    s.state.set_model_override(model.to_owned());
    info!(model, "model override set");
    axum::Json(json!({ "model": model })).into_response()
}

async fn model_clear_handler(State(s): State<AppState>) -> impl IntoResponse {
    s.state.clear_model_override();
    info!("model override cleared");
    axum::Json(json!({ "model": null }))
}

async fn strategy_get_handler(State(s): State<AppState>) -> impl IntoResponse {
    let (strategy_str, source) = match s.state.get_routing_strategy() {
        Some(st) => (st.as_str(), "override"),
        None => (s.config.server.routing_strategy.as_str(), "config"),
    };
    axum::Json(json!({ "strategy": strategy_str, "source": source }))
}

async fn strategy_set_handler(
    State(s): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(name) = body["strategy"].as_str() else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "missing strategy field" }))).into_response();
    };
    let Some(strategy) = crate::config::RoutingStrategy::from_str(name) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": format!("unknown strategy '{name}'") }))).into_response();
    };
    s.state.set_routing_strategy(strategy);
    info!(strategy = name, "routing strategy override set");
    axum::Json(json!({ "strategy": strategy.as_str(), "source": "override" })).into_response()
}

async fn strategy_clear_handler(State(s): State<AppState>) -> impl IntoResponse {
    s.state.clear_routing_strategy();
    info!("routing strategy override cleared");
    let strategy_str = s.config.server.routing_strategy.as_str();
    axum::Json(json!({ "strategy": strategy_str, "source": "config" }))
}

async fn alerts_get_handler(State(s): State<AppState>) -> impl IntoResponse {
    let muted = s.state.get_alerts_muted();
    axum::Json(json!({ "muted": muted }))
}

async fn alerts_set_handler(
    State(s): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(muted) = body["muted"].as_bool() else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "missing muted bool field" }))).into_response();
    };
    s.state.set_alerts_muted(muted);
    info!(muted, "alerts mute state changed");
    axum::Json(json!({ "muted": muted })).into_response()
}

use crate::state::now_ms_pub as now_ms;

/// Extract client IP for rate limiting.
///
/// `X-Real-IP` is only trusted when `trust_proxy_headers` is explicitly enabled
/// in config — otherwise any client could spoof the header to rotate its bucket.
/// When not trusted (the default), all requests share a single loopback bucket,
/// giving a global RPM cap rather than a per-IP one.
fn extract_client_ip(req: &Request, trust_proxy_headers: bool) -> IpAddr {
    if trust_proxy_headers {
        if let Some(ip) = req.headers()
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
        {
            return ip;
        }
    }
    IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
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

    // #16: per-IP rate limiting (token bucket, configurable via rate_limit_rpm).
    if let Some(ref rl) = s.rate_limiter {
        let ip = extract_client_ip(&req, s.config.server.trust_proxy_headers);
        let rpm = s.config.server.rate_limit_rpm as f64;
        let allowed = rl.lock().entry(ip).or_insert_with(|| TokenBucket::new(rpm)).check_and_consume(rpm);
        if !allowed {
            return Err(ProxyError::RateLimited);
        }
    }

    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    let headers = req.headers().clone();

    let body_bytes: Bytes = axum::body::to_bytes(req.into_body(), MAX_REQUEST_BODY)
        .await
        .map_err(|_| ProxyError::BodyRead)?;

    // Apply model override: if set, patch the `model` field in the JSON body before forwarding.
    // Also strip unsupported params for models that don't support them (e.g. Haiku).
    // Parse once and reuse the value to extract the model name (avoids double-parse).
    let (body_bytes, model) = if let Ok(mut val) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
        let mut changed = false;
        if let Some(override_model) = s.state.get_model_override() {
            if val.get("model").is_some() {
                val["model"] = serde_json::Value::String(override_model);
                changed = true;
            }
        }
        let resolved_model = val["model"].as_str().unwrap_or("").to_owned();
        if is_simple_model(&resolved_model) {
            if let Some(obj) = val.as_object_mut() {
                // Strip features unsupported by simpler models (Haiku).
                for key in &["thinking", "effort", "reasoning_effort"] {
                    if obj.remove(*key).is_some() { changed = true; }
                }
                // Strip effort from output_config if present
                if let Some(serde_json::Value::Object(oc)) = obj.get_mut("output_config") {
                    if oc.remove("effort").is_some() { changed = true; }
                    // Remove output_config entirely if empty
                    if oc.is_empty() { obj.remove("output_config"); }
                }
                // Strip context_management (thinking-related edit rules)
                if obj.remove("context_management").is_some() { changed = true; }
                // Remove extended-thinking beta flag
                if let Some(serde_json::Value::Array(betas)) = obj.get_mut("betas") {
                    let before = betas.len();
                    betas.retain(|b| b.as_str() != Some("interleaved-thinking-2025-05-14"));
                    if betas.len() != before { changed = true; }
                }
            }
        }
        let model = val["model"].as_str().unwrap_or("").to_owned();
        let bytes = if changed {
            Bytes::from(serde_json::to_vec(&val).unwrap_or_else(|_| body_bytes.to_vec()))
        } else {
            body_bytes
        };
        (bytes, model)
    } else {
        (body_bytes, String::new())
    };

    // Strip thinking/effort-related beta flags from the anthropic-beta header for simple models.
    let mut headers = headers;
    if is_simple_model(&model) {
        if let Some(beta_val) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok().map(|s| s.to_owned())) {
            let filtered: Vec<&str> = beta_val.split(',')
                .map(|s| s.trim())
                .filter(|b| !b.contains("thinking") && !b.contains("effort"))
                .collect();
            let new_beta = filtered.join(",");
            if filtered.is_empty() {
                headers.remove("anthropic-beta");
            } else if let Ok(v) = axum::http::HeaderValue::from_str(&new_beta) {
                headers.insert("anthropic-beta", v);
            }
        }
    }

    let req_start_ms = now_ms();
    let request_id = uuid::Uuid::new_v4().to_string()[..8].to_owned();

    let fp = router::fingerprint(&body_bytes);
    let fp_ref = fp.as_deref();

    let mut tried: HashSet<String> = HashSet::new();
    // Track accounts we've already attempted a token refresh for this request.
    let mut refreshed: HashSet<String> = HashSet::new();
    // Cap wait to the configured request timeout — the client's TCP connection
    // won't survive 5 hours anyway; return 503 so the client can retry.
    let wait_deadline_ms = now_ms() + s.config.server.request_timeout_secs.saturating_mul(1_000);

    loop {
        let effective_strategy = s.state.get_routing_strategy()
            .unwrap_or(s.config.server.routing_strategy);
        let snap = s.state.routing_snapshot();
        let account = match router::pick_account(
            &s.config.accounts, &s.state, &snap, fp_ref, &tried,
            s.config.server.sticky_ttl_ms, s.config.server.expiry_soon_secs,
            effective_strategy,
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
        // For OpenAI/chatgpt.com accounts, Credential::bearer_token() returns id_token
        // (short-lived OIDC JWT) which chatgpt.com requires. For all other providers it
        // returns access_token. API-key accounts return the key directly.
        let token = {
            let creds = s.credentials.read().await;
            let cred = creds.get(&account_name)
                .cloned()
                .or_else(|| account.credential.clone());
            match cred {
                Some(c) => c.bearer_token().to_owned(),
                None => String::new(),
            }
        };

        // Detect request and account protocols.  When they differ, translate
        // the request body + path before forwarding and translate the response
        // back so the client always sees its native wire format.
        let req_is_anthropic = path.starts_with("/v1/messages");
        let acct_is_anthropic = account.provider.wire_protocol()
            == crate::provider::WireProtocol::Anthropic;
        // chatgpt.com (Provider::OpenAI) uses a proprietary backend-api path + sentinel token.
        // All other OpenAI-compat providers (OpenAIApi, Groq, Mistral, …) use /v1/chat/completions.
        let acct_is_chatgpt = matches!(account.provider, Provider::OpenAI);

        // log_model: what we actually send to the upstream (after resolve_model).
        // Defaults to the incoming model; overridden in the OpenAI-compat branch.
        let mut log_model = model.clone();

        let (fwd_path, fwd_body, mut fwd_headers) = if req_is_anthropic == acct_is_anthropic {
            // Same wire protocol — pass through unchanged.
            (path.clone(), body_bytes.clone(), headers.clone())
        } else if req_is_anthropic && acct_is_chatgpt {
            // Anthropic client → chatgpt.com account: translate to backend-api format.
            let val = serde_json::from_slice::<serde_json::Value>(&body_bytes).unwrap_or(json!({}));
            let translated = translate_anthropic_req_to_chatgpt(&val);
            let mut h = headers.clone();
            for name in &["anthropic-version", "anthropic-beta", "anthropic-dangerous-direct-browser-access"] {
                h.remove(*name);
            }
            (
                "/backend-api/conversation".to_owned(),
                bytes::Bytes::from(serde_json::to_vec(&translated).unwrap_or_default()),
                h,
            )
        } else if req_is_anthropic {
            // Anthropic client → standard OpenAI-compat account (OpenAIApi, Groq, Mistral, …).
            let val = serde_json::from_slice::<serde_json::Value>(&body_bytes).unwrap_or(json!({}));
            // Resolve the target model: account pin → global mapping → provider default.
            let target_model = resolve_model(&model, account, &s.config.model_mapping);
            log_model = target_model.clone();
            let translated = translate_anthropic_req_to_openai(val, &target_model);
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

        // Resolve upstream URL: per-account override (set at load time for non-primary
        // providers, or explicitly in tests) → config server URL.
        let upstream = account.upstream_url.as_deref()
            .unwrap_or(&s.config.server.upstream_url);

        // Inject chatgpt.com sentinel token — only for the chatgpt.com proprietary path.
        // Wrap in tokio::time::timeout (3s) to guarantee we don't block on Cloudflare challenges.
        if req_is_anthropic && acct_is_chatgpt {
            tracing::info!(account = %account_name, upstream = %upstream, "routing to chatgpt.com — fetching sentinel");
            let sentinel_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .unwrap_or_default();
            let sentinel_opt = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                fetch_sentinel_token(&sentinel_client, upstream, &token),
            ).await.ok().flatten();
            if let Some(sentinel) = sentinel_opt {
                if let Ok(name) = axum::http::header::HeaderName::from_bytes(
                    b"openai-sentinel-chat-requirements-token",
                ) {
                    if let Ok(val) = axum::http::HeaderValue::from_str(&sentinel) {
                        fwd_headers.insert(name, val);
                    }
                }
            }
        }

        // Apply a hard 15s cap only for chatgpt.com: Cloudflare may hold the TCP connection
        // open indefinitely for certain TLS fingerprints.  Standard API providers don't need this.
        let response = if acct_is_chatgpt {
            tracing::info!(account = %account_name, path = %fwd_path, "forwarding to chatgpt.com (15s cap)");
            match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                s.forwarder.forward(upstream, &method, &fwd_path, fwd_body, &fwd_headers, account, &token),
            ).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    error!(account = %account_name, "chatgpt.com forward error: {:#}", e);
                    s.state.set_cooldown(&account_name, 5 * 60_000);
                    tried.insert(account_name);
                    continue;
                }
                Err(_) => {
                    warn!(account = %account_name, "chatgpt.com request timed out (Cloudflare) — cooling 5min");
                    s.state.set_cooldown(&account_name, 5 * 60_000);
                    tried.insert(account_name);
                    continue;
                }
            }
        } else {
            s.forwarder
                .forward(upstream, &method, &fwd_path, fwd_body, &fwd_headers, account, &token)
                .await
                .map_err(|e| {
                    error!("Forward error: {:#}", e);
                    ProxyError::Upstream
                })?
        };

        match response.status().as_u16() {
            200..=299 => {
                s.state.set_last_used(&account_name);
                if let Some(info) = account.provider.parse_rate_limits(response.headers()) {
                    s.state.update_rate_limits(&account_name, info);
                }
                // Translate response back to the client's expected protocol.
                let response = if req_is_anthropic == acct_is_anthropic {
                    response
                } else if req_is_anthropic && acct_is_chatgpt {
                    // Got chatgpt.com response; client expects Anthropic.
                    translate_response_chatgpt_to_anthropic(response, &model).await
                } else if req_is_anthropic {
                    // Got standard OpenAI-compat response; client expects Anthropic.
                    translate_response_openai_to_anthropic(response, &model).await
                } else {
                    // Got Anthropic response; client expects OpenAI.
                    translate_response_anthropic_to_openai(response).await
                };
                return Ok(tap_usage(response, &s.state, s.telemetry.as_ref(), &account_name, &log_model, req_start_ms, &request_id, &path, tried.len()).await);
            }
            429 => {
                let info = account.provider.parse_rate_limits(response.headers());
                // Sleep until the actual reset time if the headers tell us when that is.
                // Fall back to Retry-After (per-minute rate limits), then 60s.
                let retry_after_ms = response.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|secs| secs.saturating_mul(1_000).max(500));
                let cooldown_ms = info.as_ref()
                    .and_then(|i| i.reset_5h.or(i.reset_7d))
                    .map(|reset_secs| {
                        let reset_ms = reset_secs.saturating_mul(1_000);
                        reset_ms.saturating_sub(now_ms()).saturating_add(500) // +500ms buffer
                    })
                    .or(retry_after_ms)
                    .unwrap_or(60_000);
                warn!(account = %account_name, cooldown_ms, "429 rate-limited — cooling until reset");
                if let Some(info) = info {
                    s.state.update_rate_limits(&account_name, info);
                }
                s.state.set_cooldown(&account_name, cooldown_ms);
                if cooldown_ms >= 5 * 60_000 && !s.state.get_alerts_muted() {
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
                        let mut locks = s.refresh_locks.lock();
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
                    let token_before = cred.access_token().to_owned();
                    let already_refreshed = {
                        let creds = s.credentials.read().await;
                        creds.get(&account_name)
                            .map(|c| c.access_token() != token_before)
                            .unwrap_or(false)
                    };

                    if already_refreshed {
                        // Another concurrent request already refreshed — just retry.
                        warn!(account = %account_name, "401 — token was refreshed by concurrent request, retrying");
                        refreshed.insert(account_name);
                    } else if let Some(oauth_cred) = cred.as_oauth() {
                        // OAuth account — attempt token refresh.
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            account.provider.refresh_token(oauth_cred),
                        ).await {
                            Ok(Ok(fresh)) => {
                                warn!(account = %account_name, "401 — token refreshed, retrying");
                                {
                                    let mut creds = s.credentials.write().await;
                                    creds.insert(account_name.clone(), Credential::Oauth(fresh.clone()));
                                }
                                // Persist to disk so the refreshed token survives a restart.
                                let name = account_name.clone();
                                let fresh = fresh.clone();
                                tokio::task::spawn_blocking(move || {
                                    let mut store = CredentialsStore::load();
                                    store.accounts.insert(name, Credential::Oauth(fresh.clone()));
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
                    } else {
                        // API-key account — 401 means the key is invalid; no refresh possible.
                        error!(account = %account_name, "401 — API key rejected, cooling 5min");
                        s.state.set_cooldown(&account_name, 5 * 60_000);
                        tried.insert(account_name);
                    }
                } else {
                    // Already refreshed once and still 401 — cool down this account.
                    error!(account = %account_name, "401 after refresh — cooling 5min");
                    s.state.set_cooldown(&account_name, 5 * 60_000);
                    tried.insert(account_name);
                }
            }
            403 => {
                // Forbidden — could be a Cloudflare challenge (non-Anthropic providers)
                // or a genuine subscription/org block (Anthropic). Use a short cooldown
                // for non-Anthropic accounts so a CF block doesn't lock them out for 30m.
                if acct_is_anthropic {
                    error!(account = %account_name, "403 forbidden — cooling 30min");
                    s.state.set_cooldown(&account_name, 30 * 60_000);
                    if !s.state.get_alerts_muted() {
                        notify(
                            "shunt: Account Forbidden",
                            &format!("Account '{account_name}' got 403 — subscription may have lapsed (cooling 30m)."),
                            "Basso",
                        );
                    }
                } else {
                    warn!(account = %account_name, "403 from chatgpt.com (Cloudflare) — cooling 5min");
                    s.state.set_cooldown(&account_name, 5 * 60_000);
                }
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
    telemetry: Option<&TelemetryClient>,
    account: &str,
    model: &str,
    req_start_ms: u64,
    request_id: &str,
    path: &str,
    retries: usize,
) -> Response {
    use axum::body::Body;
    use crate::state::RequestLog;

    let streaming = quota::is_streaming_response(&resp);

    if streaming {
        let state      = state.clone();
        let telem      = telemetry.cloned();
        let account    = account.to_owned();
        let model      = model.to_owned();
        let request_id = request_id.to_owned();
        let path       = path.to_owned();
        let on_complete = Arc::new(move |input: u64, output: u64| {
            let duration_ms = now_ms().saturating_sub(req_start_ms);
            info!(
                request_id = %request_id,
                account    = %account,
                model      = %model,
                status     = 200,
                latency_ms = duration_ms,
                path       = %path,
                stream     = true,
                input_tokens  = input,
                output_tokens = output,
                retries    = retries,
                "request complete"
            );
            let log = RequestLog {
                ts_ms: req_start_ms,
                account: account.clone(),
                model: model.clone(),
                status: 200,
                input_tokens: input,
                output_tokens: output,
                duration_ms,
            };
            state.record_usage(&account, input, output);
            state.record_global(&model, input, output);
            if let Some(ref t) = telem { t.push_event(&log); }
            state.record_request(log);
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
    let duration_ms = now_ms().saturating_sub(req_start_ms);
    info!(
        request_id    = %request_id,
        account       = %account,
        model         = %model,
        status        = 200,
        latency_ms    = duration_ms,
        path          = %path,
        stream        = false,
        input_tokens  = input,
        output_tokens = output,
        retries       = retries,
        "request complete"
    );
    let log = RequestLog {
        ts_ms: req_start_ms,
        account: account.to_owned(),
        model: model.to_owned(),
        status: 200,
        input_tokens: input,
        output_tokens: output,
        duration_ms,
    };
    state.record_usage(account, input, output);
    state.record_global(model, input, output);
    if let Some(t) = telemetry { t.push_event(&log); }
    state.record_request(log);
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

    let existing_rl = state.rate_limit_snapshot();
    for account in &config.accounts {
        // Skip if we already have data for this account.
        if let Some(r) = existing_rl.get(&account.name) {
            if r.utilization_5h.is_some() || r.utilization_7d.is_some() {
                continue;
            }
        }

        // Skip accounts with no credentials or no prefetch support.
        let cred = match account.credential.clone() {
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

        let resp = prefetch_send(&client, &url, &account.provider, cred.bearer_token(), &body).await;

        let r = match resp {
            Ok(r) => r,
            Err(e) => { tracing::warn!(account = %account.name, "prefetch failed: {e}"); continue; }
        };

        if r.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::info!(account = %account.name, "prefetch: token expired, refreshing");
            let Some(oauth_cred) = cred.as_oauth() else {
                // API-key account — 401 during prefetch means the key is invalid.
                tracing::error!(account = %account.name, "prefetch 401 — API key rejected");
                state.set_auth_failed(&account.name);
                continue;
            };
            let fresh = match account.provider.refresh_token(oauth_cred).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(account = %account.name, "token refresh failed: {e}");
                    state.set_auth_failed(&account.name);
                    continue;
                }
            };
            let mut store = crate::config::CredentialsStore::load();
            store.accounts.insert(account.name.clone(), Credential::Oauth(fresh.clone()));
            store.save().ok();
            if fresh.id_token.is_some() {
                crate::oauth::write_codex_auth_file(&fresh);
            }
            // Update live credentials so the proxy uses the fresh token immediately.
            live_creds.write().await.insert(account.name.clone(), Credential::Oauth(fresh.clone()));

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
    let cred = match account.credential.clone() {
        Some(c) => c,
        None => return,
    };
    let upstream = account.upstream_url.as_deref()
        .unwrap_or_else(|| account.provider.default_upstream_url());
    let url = format!("{}{}", upstream, path);

    let do_get = |token: &str| -> reqwest::RequestBuilder {
        let mut headers = reqwest::header::HeaderMap::new();
        let _ = account.provider.inject_auth_headers(&mut headers, token);
        client.get(&url).headers(headers)
    };

    let resp = match do_get(cred.bearer_token()).send().await {
        Ok(r) => r,
        Err(e) => { tracing::warn!(account = %account.name, "auth probe failed: {e}"); return; }
    };

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        tracing::info!(account = %account.name, "auth probe: token rejected, refreshing");
        let Some(oauth_cred) = cred.as_oauth() else {
            // API-key account — key is invalid; no refresh possible.
            tracing::error!(account = %account.name, "auth probe 401 — API key rejected");
            state.set_auth_failed(&account.name);
            return;
        };
        let fresh = match account.provider.refresh_token(oauth_cred).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(account = %account.name, "token refresh failed: {e}");
                state.set_auth_failed(&account.name);
                return;
            }
        };
        let mut store = crate::config::CredentialsStore::load();
        store.accounts.insert(account.name.clone(), Credential::Oauth(fresh.clone()));
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
        .and_then(|c| c.as_oauth())
        .map(|c| c.expires_at)
        .unwrap_or(0);
    if from_file.expires_at > current_exp {
        tracing::info!(account = %account_name, "synced fresher token from auth.json");
        live_creds.write().await.insert(account_name.to_owned(), Credential::Oauth(from_file));
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
                map.insert(account.name.clone(), Credential::Oauth(fresh.clone()));
            }
            let mut store = crate::config::CredentialsStore::load();
            store.accounts.insert(account.name.clone(), Credential::Oauth(fresh.clone()));
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
            if let Some(oauth) = creds.as_oauth() {
                if access_token_expires_soon(oauth, 30) {
                    // access_token is nearly expired — refresh now so shunt can serve requests immediately.
                    do_proactive_refresh(account, oauth, &live_creds, &state).await;
                } else {
                    tracing::info!(account = %account.name, "access_token fresh at startup");
                }
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
    RateLimited,
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        match self {
            ProxyError::RateLimited => {
                let mut resp = (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({
                        "type": "error",
                        "error": {"type": "rate_limit_error", "message": "too many requests — slow down"}
                    })),
                ).into_response();
                resp.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from_static("60"),
                );
                resp
            }
            other => {
                let (status, msg) = match other {
                    ProxyError::BodyRead => (StatusCode::BAD_REQUEST, "failed to read request body"),
                    ProxyError::Upstream => (StatusCode::BAD_GATEWAY, "upstream request failed"),
                    ProxyError::AllAccountsUnavailable => {
                        (StatusCode::SERVICE_UNAVAILABLE, "all accounts are on cooldown or disabled")
                    }
                    ProxyError::Unauthorized => (StatusCode::UNAUTHORIZED, "invalid or missing api key"),
                    ProxyError::RateLimited => unreachable!(),
                };
                (status, axum::Json(json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": msg}
                }))).into_response()
            }
        }
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
            if !cred.has_refresh_token() { continue; }
            let Some(oauth_cred) = cred.as_oauth().cloned() else { continue };

            let provider = config.accounts.iter()
                .find(|a| a.name == *name)
                .map(|a| a.provider.clone())
                .unwrap_or_default();

            let result = tokio::time::timeout(
                Duration::from_secs(20),
                provider.refresh_token(&oauth_cred),
            ).await;

            match result {
                Ok(Ok(fresh)) => {
                    tracing::info!(account = %name, "recovery: token refreshed — account back online");
                    {
                        let mut map = credentials.write().await;
                        map.insert(name.to_string(), Credential::Oauth(fresh.clone()));
                    }
                    let name_owned = name.to_string();
                    let fresh_owned = fresh.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut store = crate::config::CredentialsStore::load();
                        store.accounts.insert(name_owned, Credential::Oauth(fresh_owned.clone()));
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
                    if !state.get_alerts_muted() {
                        notify(
                            "shunt: Reauth Required",
                            &format!("Account '{name}' needs re-authorization. Run `shunt add-account`."),
                            "Basso",
                        );
                    }
                }
                Err(_) => {
                    tracing::error!(account = %name, "recovery: token refresh timed out");
                    if !state.get_alerts_muted() {
                        notify(
                            "shunt: Reauth Required",
                            &format!("Account '{name}' token refresh timed out. Run `shunt add-account`."),
                            "Basso",
                        );
                    }
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
                if !state.get_alerts_muted() {
                    notify(
                        "shunt: All Accounts Offline",
                        "All accounts need re-authorization. Run `shunt add-account`.",
                        "Basso",
                    );
                }
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

/// Periodic health-check loop: probes every account at a configurable interval
/// to detect dead/invalid accounts before real traffic hits them.
///
/// Uses exponential backoff (base_interval * 2^min(failures, 3)) per account,
/// capped at ~40 min. Marks accounts as `health_check_failed` after 2 consecutive
/// failures (tolerates one transient blip). On 401, delegates to `set_auth_failed`.
pub async fn health_check_loop(
    config: Arc<Config>,
    state: StateStore,
    live_creds: LiveCredentials,
) {
    if !config.server.health_check_enabled {
        return;
    }

    // Let prefetch_rate_limits finish first.
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;

    let base_interval_ms = config.server.health_check_interval_secs * 1000;
    let timeout = std::time::Duration::from_secs(config.server.health_check_timeout_secs);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_default();

    const FAILURE_THRESHOLD: u32 = 2;
    const MAX_BACKOFF_EXP: u32 = 3; // 2^3 = 8x → 40 min at 5-min base

    loop {
        for account in &config.accounts {
            // Skip accounts already handled by recovery_watcher.
            {
                let states = state.account_states();
                if let Some(acc_state) = states.get(&account.name) {
                    if acc_state.disabled || acc_state.auth_failed {
                        continue;
                    }
                }
            }

            // Exponential backoff based on consecutive failure count.
            let (last_check_ms, failures) = state.health_check_info(&account.name);
            let backoff_factor = 1u64 << failures.min(MAX_BACKOFF_EXP);
            let effective_interval_ms = base_interval_ms.saturating_mul(backoff_factor);
            let now = crate::state::now_ms_pub();
            if last_check_ms > 0 && now.saturating_sub(last_check_ms) < effective_interval_ms {
                continue;
            }

            state.update_last_health_check(&account.name);

            // Resolve current credential from live_creds (may have been refreshed).
            let cred = {
                let creds = live_creds.read().await;
                creds.get(&account.name).cloned()
            }.or_else(|| account.credential.clone());

            let cred = match cred {
                Some(c) => c,
                None => {
                    // Local providers have no cred — probe reachability via GET /v1/models.
                    if let Some(probe_path) = account.provider.auth_probe_get_path() {
                        let upstream = account.upstream_url.as_deref()
                            .unwrap_or_else(|| account.provider.default_upstream_url());
                        let url = format!("{upstream}{probe_path}");
                        match client.get(&url).send().await {
                            Ok(r) if r.status().is_success() => {
                                if state.is_health_check_failed(&account.name) {
                                    tracing::info!(account = %account.name, "health check recovered");
                                }
                                state.clear_health_check_failed(&account.name);
                            }
                            Ok(r) => {
                                let count = state.record_health_check_failure(&account.name, FAILURE_THRESHOLD);
                                tracing::warn!(account = %account.name, status = %r.status(),
                                    failures = count, "health check failed");
                            }
                            Err(e) => {
                                let count = state.record_health_check_failure(&account.name, FAILURE_THRESHOLD);
                                tracing::warn!(account = %account.name, failures = count,
                                    "health check unreachable: {e}");
                            }
                        }
                    }
                    continue;
                }
            };

            let token = cred.bearer_token();
            let upstream = account.upstream_url.as_deref()
                .unwrap_or(&config.server.upstream_url);

            // Try POST prefetch (Anthropic) or GET auth probe (other providers).
            if let Some((path, body)) = account.provider.prefetch_request() {
                let url = format!("{upstream}{path}");
                match prefetch_send(&client, &url, &account.provider, token, &body).await {
                    Ok(r) => {
                        let status = r.status();
                        if status == reqwest::StatusCode::UNAUTHORIZED {
                            // Attempt refresh for OAuth accounts.
                            if let Some(oauth_cred) = cred.as_oauth() {
                                match account.provider.refresh_token(oauth_cred).await {
                                    Ok(fresh) => {
                                        let mut store = crate::config::CredentialsStore::load();
                                        store.accounts.insert(account.name.clone(), Credential::Oauth(fresh.clone()));
                                        store.save().ok();
                                        live_creds.write().await.insert(account.name.clone(), Credential::Oauth(fresh));
                                        state.clear_auth_failed(&account.name);
                                        if state.is_health_check_failed(&account.name) {
                                            state.clear_health_check_failed(&account.name);
                                        }
                                        tracing::info!(account = %account.name, "health check: token refreshed");
                                    }
                                    Err(e) => {
                                        tracing::error!(account = %account.name, "health check: refresh failed: {e}");
                                        state.set_auth_failed(&account.name);
                                    }
                                }
                            } else {
                                tracing::error!(account = %account.name, "health check: 401 — API key rejected");
                                state.set_auth_failed(&account.name);
                            }
                        } else if status.is_server_error() {
                            let count = state.record_health_check_failure(&account.name, FAILURE_THRESHOLD);
                            tracing::warn!(account = %account.name, status = %status,
                                failures = count, "health check: server error");
                        } else {
                            // Success — update rate limits if available.
                            if let Some(info) = account.provider.parse_rate_limits(r.headers()) {
                                state.update_rate_limits(&account.name, info);
                            }
                            if state.is_health_check_failed(&account.name) {
                                tracing::info!(account = %account.name, "health check recovered");
                            }
                            state.clear_health_check_failed(&account.name);
                        }
                    }
                    Err(e) => {
                        let count = state.record_health_check_failure(&account.name, FAILURE_THRESHOLD);
                        tracing::warn!(account = %account.name, failures = count,
                            "health check probe failed: {e}");
                    }
                }
            } else if let Some(probe_path) = account.provider.auth_probe_get_path() {
                let probe_upstream = account.upstream_url.as_deref()
                    .unwrap_or_else(|| account.provider.default_upstream_url());
                let url = format!("{probe_upstream}{probe_path}");
                let mut headers = reqwest::header::HeaderMap::new();
                let _ = account.provider.inject_auth_headers(&mut headers, token);
                match client.get(&url).headers(headers).send().await {
                    Ok(r) => {
                        let status = r.status();
                        if status == reqwest::StatusCode::UNAUTHORIZED {
                            if let Some(oauth_cred) = cred.as_oauth() {
                                match account.provider.refresh_token(oauth_cred).await {
                                    Ok(fresh) => {
                                        let mut store = crate::config::CredentialsStore::load();
                                        store.accounts.insert(account.name.clone(), Credential::Oauth(fresh.clone()));
                                        store.save().ok();
                                        live_creds.write().await.insert(account.name.clone(), Credential::Oauth(fresh));
                                        state.clear_auth_failed(&account.name);
                                        state.clear_health_check_failed(&account.name);
                                        tracing::info!(account = %account.name, "health check: token refreshed (GET probe)");
                                    }
                                    Err(e) => {
                                        tracing::error!(account = %account.name, "health check: refresh failed: {e}");
                                        state.set_auth_failed(&account.name);
                                    }
                                }
                            } else {
                                tracing::error!(account = %account.name, "health check: 401 — API key rejected");
                                state.set_auth_failed(&account.name);
                            }
                        } else if status.is_server_error() {
                            let count = state.record_health_check_failure(&account.name, FAILURE_THRESHOLD);
                            tracing::warn!(account = %account.name, status = %status,
                                failures = count, "health check: server error (GET probe)");
                        } else {
                            if state.is_health_check_failed(&account.name) {
                                tracing::info!(account = %account.name, "health check recovered");
                            }
                            state.clear_health_check_failed(&account.name);
                        }
                    }
                    Err(e) => {
                        let count = state.record_health_check_failure(&account.name, FAILURE_THRESHOLD);
                        tracing::warn!(account = %account.name, failures = count,
                            "health check probe failed: {e}");
                    }
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(config.server.health_check_interval_secs)).await;
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
                        creds.get(&account.name).map(|c| c.bearer_token().to_owned())
                    };
                    if let Some(token) = token {
                        post_cooldown_prefetch(
                            &client, account, &token, &state,
                            &config.server.upstream_url,
                        ).await;
                    }
                    if notify_on_resume.remove(&account.name) && !state.get_alerts_muted() {
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
                        creds.get(&account.name).map(|c| c.bearer_token().to_owned())
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
use crate::translate::{
    translate_to_anthropic,
    translate_from_anthropic,
    uuid_v4,
    translate_anthropic_stream,
    translate_anthropic_req_to_chatgpt,
    translate_response_chatgpt_to_anthropic,
    translate_anthropic_req_to_openai,
    translate_response_openai_to_anthropic,
    translate_response_anthropic_to_openai,
};

// ---------------------------------------------------------------------------
// OpenAI-compatible API (translates to Anthropic Claude)
// ---------------------------------------------------------------------------
//
// When the OpenAI proxy receives a request at /v1/chat/completions, if an
// anthropic_base_url is configured, it translates the request to Anthropic
// Messages format and forwards it to the Anthropic proxy (which handles
// account selection, token management, and rate limiting).
// The response is translated back to OpenAI Chat Completions format.




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

    let body_bytes = axum::body::to_bytes(req.into_body(), MAX_REQUEST_BODY)
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

    let mut req_builder = client
        .post(format!("{anthropic_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
        .header("x-shunt-compat", "openai");
    if let Some(ref key) = s.config.server.remote_key {
        req_builder = req_builder.header("x-api-key", key.as_str());
    }
    let resp = req_builder
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

// ---------------------------------------------------------------------------
// ChatGPT backend API translation (chatgpt.com /backend-api/conversation)
// ---------------------------------------------------------------------------

/// Fetch the sentinel token required by chatgpt.com's backend API.
/// Returns None if the request fails or proof-of-work is required.
async fn fetch_sentinel_token(client: &reqwest::Client, upstream: &str, token: &str) -> Option<String> {
    let url = format!("{}/backend-api/sentinel/chat-requirements", upstream);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    if json["proofofwork"]["required"].as_bool() == Some(true) {
        return None;
    }
    json["token"].as_str().map(ToOwned::to_owned)
}


/// Returns true if the model lacks support for extended thinking / effort.
/// These params must be stripped before forwarding.
fn is_simple_model(model: &str) -> bool {
    model.contains("haiku")
}

/// Resolve the target model name for a non-Anthropic account.
///
/// Priority: per-account `model` pin → global `model_mapping` → provider `default_model()`.
/// If the provider is `Local` (default_model = ""), the incoming model name is passed through.
fn resolve_model(
    incoming: &str,
    account: &crate::config::AccountConfig,
    mapping: &std::collections::HashMap<String, String>,
) -> String {
    // 1. Per-account pin (highest priority).
    if let Some(m) = &account.model {
        return m.clone();
    }
    // 2. Global mapping for this specific incoming model name.
    if let Some(m) = mapping.get(incoming) {
        return m.clone();
    }
    // 3. Provider default.
    let default = account.provider.default_model();
    if !default.is_empty() {
        return default.to_owned();
    }
    // 4. Pass through (Local provider — model name is server-defined).
    incoming.to_owned()
}

