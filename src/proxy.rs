use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use serde_json::json;
use tracing::{error, warn};

use crate::config::{state_path, Config};
use crate::forwarder::Forwarder;
use crate::quota;
use crate::router;
use crate::state::{RateLimitInfo, StateStore};

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    forwarder: Arc<Forwarder>,
    state: StateStore,
}

pub fn create_app(config: Config) -> anyhow::Result<Router> {
    create_app_with_state(config, StateStore::load(&state_path()))
}

pub fn create_app_with_state(config: Config, state: StateStore) -> anyhow::Result<Router> {
    let forwarder = Forwarder::new(&config.server.upstream_url)?;

    let app_state = AppState {
        config: Arc::new(config),
        forwarder: Arc::new(forwarder),
        state,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status_handler))
        .route("/v1/messages", post(proxy_handler))
        .route("/v1/messages/count_tokens", post(proxy_handler))
        .with_state(app_state);

    Ok(app)
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
        let avail_status = if st.map(|s| s.disabled).unwrap_or(false) {
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
            "overage_status": r.overage_status,
            "overage_disabled_reason": r.overage_disabled_reason,
            "representative_claim": r.representative_claim,
            "updated_ms": r.updated_ms,
        }));

        json!({
            "name": a.name,
            "plan_type": a.plan_type,
            "status": avail_status,
            "window_expires_ms": window_expires_ms,
            "tokens_used": tokens_used,
            "rate_limit": rate_limit,
        })
    }).collect();

    axum::Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "accounts": accounts,
    }))
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

async fn proxy_handler(
    State(s): State<AppState>,
    req: Request,
) -> Result<Response, ProxyError> {
    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    let headers = req.headers().clone();

    let body_bytes: Bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| ProxyError::BodyRead)?;

    let fp = router::fingerprint(&body_bytes);
    let fp_ref = fp.as_deref();

    let mut tried: HashSet<String> = HashSet::new();

    loop {
        let account = match router::pick_account(&s.config.accounts, &s.state, fp_ref, &tried) {
            Some(a) => a,
            None => return Err(ProxyError::AllAccountsUnavailable),
        };

        let account_name = account.name.clone();

        let response = s.forwarder
            .forward(&method, &path, body_bytes.clone(), &headers, account)
            .await
            .map_err(|e| {
                error!("Forward error: {:#}", e);
                ProxyError::Upstream
            })?;

        match response.status().as_u16() {
            200..=299 => return Ok(tap_usage(response, &s.state, &account_name).await),
            429 => {
                warn!(account = %account_name, "429 rate-limited — cooling 60s");
                capture_rate_limit_headers(response.headers(), &s.state, &account_name);
                s.state.set_cooldown(&account_name, 60_000);
                tried.insert(account_name);
            }
            529 => {
                warn!(account = %account_name, "529 overloaded — cooling 30s");
                capture_rate_limit_headers(response.headers(), &s.state, &account_name);
                s.state.set_cooldown(&account_name, 30_000);
                tried.insert(account_name);
            }
            401 | 403 => {
                error!(account = %account_name, "auth error — disabling account permanently");
                s.state.disable_account(&account_name);
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
async fn tap_usage(resp: Response, state: &StateStore, account: &str) -> Response {
    use axum::body::Body;

    // Capture rate-limit headers before the response is consumed
    capture_rate_limit_headers(resp.headers(), state, account);

    if quota::is_streaming_response(&resp) {
        let state = state.clone();
        let account = account.to_owned();
        let on_complete = Arc::new(move |input: u64, output: u64| {
            state.record_usage(&account, input, output);
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
    Response::from_parts(parts, Body::from(bytes))
}

fn capture_rate_limit_headers(headers: &axum::http::HeaderMap, state: &StateStore, account: &str) {
    fn hdr_u64(headers: &axum::http::HeaderMap, name: &str) -> Option<u64> {
        headers.get(name)?.to_str().ok()?.parse().ok()
    }
    fn hdr_f64(headers: &axum::http::HeaderMap, name: &str) -> Option<f64> {
        headers.get(name)?.to_str().ok()?.parse().ok()
    }
    fn hdr_str(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
        Some(headers.get(name)?.to_str().ok()?.to_owned())
    }

    // Claude Code OAuth uses anthropic-ratelimit-unified-* headers
    let utilization_5h  = hdr_f64(headers, "anthropic-ratelimit-unified-5h-utilization");
    let reset_5h        = hdr_u64(headers, "anthropic-ratelimit-unified-5h-reset");
    let status_5h       = hdr_str(headers, "anthropic-ratelimit-unified-5h-status");
    let utilization_7d  = hdr_f64(headers, "anthropic-ratelimit-unified-7d-utilization");
    let reset_7d        = hdr_u64(headers, "anthropic-ratelimit-unified-7d-reset");
    let status_7d       = hdr_str(headers, "anthropic-ratelimit-unified-7d-status");
    let overage_status          = hdr_str(headers, "anthropic-ratelimit-unified-overage-status");
    let overage_disabled_reason = hdr_str(headers, "anthropic-ratelimit-unified-overage-disabled-reason");
    let representative_claim    = hdr_str(headers, "anthropic-ratelimit-unified-representative-claim");

    if utilization_5h.is_some() || utilization_7d.is_some() {
        state.update_rate_limits(account, RateLimitInfo {
            utilization_5h,
            reset_5h,
            status_5h,
            utilization_7d,
            reset_7d,
            status_7d,
            overage_status,
            overage_disabled_reason,
            representative_claim,
            updated_ms: now_ms(),
        });
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

enum ProxyError {
    BodyRead,
    Upstream,
    AllAccountsUnavailable,
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ProxyError::BodyRead => (StatusCode::BAD_REQUEST, "failed to read request body"),
            ProxyError::Upstream => (StatusCode::BAD_GATEWAY, "upstream request failed"),
            ProxyError::AllAccountsUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "all accounts are on cooldown or disabled")
            }
        };

        (status, axum::Json(json!({
            "type": "error",
            "error": {"type": "api_error", "message": msg}
        }))).into_response()
    }
}
