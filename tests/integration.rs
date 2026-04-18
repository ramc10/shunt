/// End-to-end integration tests.
///
/// Architecture:
///   test → reqwest::Client → shunt (axum, real TcpListener) → mock_upstream (axum, real TcpListener)
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use bytes::Bytes;
use reqwest::Client;
use serde_json::json;
use tokio::net::TcpListener;

use shunt::config::{AccountConfig, Config, ServerConfig};
use shunt::oauth::OAuthCredential;
use shunt::proxy::create_app_with_state;
use shunt::state::StateStore;

// ---------------------------------------------------------------------------
// Test server helper
// ---------------------------------------------------------------------------

struct TestServer {
    pub addr: SocketAddr,
    _shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl TestServer {
    async fn start(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { rx.await.ok(); })
                .await
                .ok();
        });

        Self { addr, _shutdown_tx: tx }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

// ---------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Captures {
    inner: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl Captures {
    fn push(&self, r: CapturedRequest) { self.inner.lock().unwrap().push(r); }
    fn get(&self, i: usize) -> CapturedRequest { self.inner.lock().unwrap()[i].clone() }
    fn len(&self) -> usize { self.inner.lock().unwrap().len() }
}

#[derive(Clone)]
struct CapturedRequest {
    pub headers: reqwest::header::HeaderMap,
    pub body: Bytes,
}

fn make_mock_upstream(captures: Captures, streaming: bool, status: u16) -> Router {
    Router::new()
        .route("/v1/messages", post({
            let caps = captures.clone();
            move |req: Request| handle_request(req, caps.clone(), streaming, status)
        }))
        .route("/v1/messages/count_tokens", post({
            let caps = captures.clone();
            move |req: Request| handle_count_tokens(req, caps.clone())
        }))
}

async fn handle_request(req: Request, caps: Captures, streaming: bool, status: u16) -> Response {
    let (parts, body) = req.into_parts();
    let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    caps.push(CapturedRequest {
        headers: to_reqwest_headers(&parts.headers),
        body: body_bytes,
    });

    if status != 200 {
        return (
            axum::http::StatusCode::from_u16(status).unwrap(),
            axum::Json(json!({"type":"error","error":{"type":"rate_limit_error","message":"slow down"}})),
        ).into_response();
    }

    if streaming {
        let sse = b"data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hello\"}}\n\n\
                    data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n\
                    data: [DONE]\n\n";
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::from(Bytes::from_static(sse)))
            .unwrap();
    }

    axum::Json(json!({"id":"msg_test","type":"message","content":[{"type":"text","text":"Hi"}]}))
        .into_response()
}

async fn handle_count_tokens(req: Request, caps: Captures) -> impl IntoResponse {
    let (parts, body) = req.into_parts();
    let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    caps.push(CapturedRequest { headers: to_reqwest_headers(&parts.headers), body: body_bytes });
    axum::Json(json!({"input_tokens": 99}))
}

fn to_reqwest_headers(h: &axum::http::HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (k, v) in h.iter() {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out.insert(n, v);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

const TEST_TOKEN: &str = "test-oauth-token-abc123";

fn test_credential() -> OAuthCredential {
    OAuthCredential {
        email: None,
        access_token: TEST_TOKEN.into(),
        refresh_token: "test-refresh-token".into(),
        // Far future — never needs refresh
        expires_at: u64::MAX / 2,
    }
}

fn test_account() -> AccountConfig {
    AccountConfig {
        name: "test".into(),
        plan_type: "pro".into(),
        credential: test_credential(),
    }
}

async fn setup(streaming: bool, upstream_status: u16) -> (TestServer, TestServer, Captures, Client) {
    let caps = Captures::default();
    let upstream = TestServer::start(make_mock_upstream(caps.clone(), streaming, upstream_status)).await;

    let cfg = Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            log_level: "error".into(),
            upstream_url: upstream.url(),
        },
        accounts: vec![test_account()],
        config_file: std::path::PathBuf::from("/dev/null"),
    };
    let proxy = TestServer::start(create_app_with_state(cfg, StateStore::new_empty()).unwrap()).await;
    (proxy, upstream, caps, Client::new())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health() {
    let (proxy, _up, _caps, client) = setup(false, 200).await;
    let resp = client.get(format!("{}/health", proxy.url())).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<serde_json::Value>().await.unwrap()["status"], "ok");
}

#[tokio::test]
async fn test_status() {
    let (proxy, _up, _caps, client) = setup(false, 200).await;
    let resp = client.get(format!("{}/status", proxy.url())).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<serde_json::Value>().await.unwrap()["accounts"][0]["name"], "test");
}

#[tokio::test]
async fn test_bearer_token_injected() {
    // Proxy strips client's Authorization and injects the account's Bearer token.
    let (proxy, _up, caps, client) = setup(false, 200).await;

    let body = r#"{"model":"claude-opus-4-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#;

    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        // Client sends its own token — must be replaced
        .header("authorization", "Bearer sk-ant-client-wrong-token")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let received = caps.get(0);
    // Proxy injected account token
    assert_eq!(
        received.headers.get("authorization").unwrap().to_str().unwrap(),
        format!("Bearer {TEST_TOKEN}")
    );
    // anthropic-version preserved
    assert_eq!(
        received.headers.get("anthropic-version").unwrap().to_str().unwrap(),
        "2023-06-01"
    );
    // x-api-key NOT injected (Claude Code mode uses Bearer)
    assert!(received.headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn test_request_body_byte_exact() {
    // Proxy must NOT re-serialize JSON — unusual whitespace/ordering must survive.
    let (proxy, _up, caps, client) = setup(false, 200).await;
    let raw = b"{\"model\":  \"claude-opus-4-5\"  ,  \"max_tokens\":1,\"messages\":[]}";

    client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body(raw.as_ref())
        .send()
        .await
        .unwrap();

    assert_eq!(caps.get(0).body.as_ref(), raw.as_ref());
}

#[tokio::test]
async fn test_streaming_forward() {
    let (proxy, _up, _caps, client) = setup(true, 200).await;

    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-opus-4-5","max_tokens":10,"stream":true,"messages":[]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"].to_str().unwrap().contains("text/event-stream"));
    let content = resp.bytes().await.unwrap();
    assert!(content.windows(b"content_block_delta".len()).any(|w| w == b"content_block_delta"));
    assert!(content.windows(b"[DONE]".len()).any(|w| w == b"[DONE]"));
}

#[tokio::test]
async fn test_upstream_error_returned_to_client() {
    // Single account returning 429 → all accounts exhausted → proxy returns 503
    let (proxy, _up, _caps, client) = setup(false, 429).await;
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.json::<serde_json::Value>().await.unwrap()["type"], "error");
}

#[tokio::test]
async fn test_count_tokens_forwarded() {
    let (proxy, _up, caps, client) = setup(false, 200).await;
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", proxy.url()))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-opus-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<serde_json::Value>().await.unwrap()["input_tokens"], 99);
    assert_eq!(caps.len(), 1);
}

#[tokio::test]
async fn test_hop_by_hop_headers_stripped() {
    let (proxy, _up, caps, client) = setup(false, 200).await;
    client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();

    let received = caps.get(0);
    assert!(received.headers.get("connection").is_none());
    assert!(received.headers.get("transfer-encoding").is_none());
    // Bearer token is injected
    assert!(received.headers.get("authorization").unwrap()
        .to_str().unwrap().starts_with("Bearer "));
}

#[tokio::test]
async fn test_concurrent_requests() {
    let (proxy, _up, caps, client) = setup(false, 200).await;
    let client = Arc::new(client);
    let url = proxy.url();

    let handles: Vec<_> = (0..10u32)
        .map(|i| {
            let c = client.clone();
            let u = format!("{url}/v1/messages");
            tokio::spawn(async move {
                c.post(u)
                    .header("content-type", "application/json")
                    .body(format!("{{\"i\":{i}}}"))
                    .send()
                    .await
                    .unwrap()
                    .status()
            })
        })
        .collect();

    let statuses: Vec<_> = futures_util::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    assert!(statuses.iter().all(|s| s.as_u16() == 200));
    assert_eq!(caps.len(), 10);
}

// ---------------------------------------------------------------------------
// Phase 2: multi-account failover + stickiness
// ---------------------------------------------------------------------------

const TEST_TOKEN_2: &str = "test-oauth-token-second-account";

fn test_account2() -> AccountConfig {
    AccountConfig {
        name: "second".into(),
        plan_type: "pro".into(),
        credential: OAuthCredential {
        email: None,
            access_token: TEST_TOKEN_2.into(),
            refresh_token: "test-refresh-2".into(),
            expires_at: u64::MAX / 2,
        },
    }
}

/// Mock upstream that returns 429 when it sees account1's token, 200 otherwise.
fn make_token_aware_upstream(captures: Captures) -> Router {
    Router::new().route("/v1/messages", post({
        let caps = captures.clone();
        move |req: Request| async move {
            let auth = req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let (parts, body) = req.into_parts();
            let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            caps.push(CapturedRequest { headers: to_reqwest_headers(&parts.headers), body: body_bytes });

            if auth == format!("Bearer {TEST_TOKEN}") {
                // First account → rate limited
                return (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({"type":"error","error":{"type":"rate_limit_error","message":"slow down"}})),
                ).into_response();
            }
            axum::Json(json!({"id":"msg_ok","type":"message","content":[{"type":"text","text":"ok"}]}))
                .into_response()
        }
    }))
}

async fn setup_multi() -> (TestServer, TestServer, Captures, Client) {
    let caps = Captures::default();
    let upstream = TestServer::start(make_token_aware_upstream(caps.clone())).await;

    let cfg = Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            log_level: "error".into(),
            upstream_url: upstream.url(),
        },
        accounts: vec![test_account(), test_account2()],
        config_file: std::path::PathBuf::from("/dev/null"),
    };
    let proxy = TestServer::start(create_app_with_state(cfg, StateStore::new_empty()).unwrap()).await;
    (proxy, upstream, caps, Client::new())
}

#[tokio::test]
async fn test_failover_to_second_account() {
    // First account gets 429 → proxy retries with second account → 200
    let (proxy, _up, caps, client) = setup_multi().await;

    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-haiku-4-5-20251001","max_tokens":8,"messages":[{"role":"user","content":"hello failover"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200, "expected success after failover");

    // Two upstream requests were made: first with token1 (429), then token2 (200)
    assert_eq!(caps.len(), 2);
    assert_eq!(
        caps.get(0).headers.get("authorization").unwrap().to_str().unwrap(),
        format!("Bearer {TEST_TOKEN}")
    );
    assert_eq!(
        caps.get(1).headers.get("authorization").unwrap().to_str().unwrap(),
        format!("Bearer {TEST_TOKEN_2}")
    );
}

#[tokio::test]
async fn test_stickiness_same_conversation() {
    // Two requests with the same fingerprint body → same account used both times.
    // Both accounts are healthy — stickiness pins to the first chosen account.
    let caps = Captures::default();
    let upstream = TestServer::start(make_mock_upstream(caps.clone(), false, 200)).await;

    let cfg = Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            log_level: "error".into(),
            upstream_url: upstream.url(),
        },
        accounts: vec![test_account(), test_account2()],
        config_file: std::path::PathBuf::from("/dev/null"),
    };
    let proxy = TestServer::start(create_app_with_state(cfg, StateStore::new_empty()).unwrap()).await;
    let client = Client::new();

    // Same system + first user message = same fingerprint
    let body = r#"{"model":"claude-haiku-4-5-20251001","max_tokens":8,"system":"You are helpful","messages":[{"role":"user","content":"sticky question"},{"role":"assistant","content":"answer"},{"role":"user","content":"follow-up"}]}"#;

    for _ in 0..3 {
        client
            .post(format!("{}/v1/messages", proxy.url()))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
    }

    // All 3 requests should use the same account (same Bearer token)
    let token0 = caps.get(0).headers.get("authorization").unwrap().to_str().unwrap().to_owned();
    for i in 1..3 {
        assert_eq!(
            caps.get(i).headers.get("authorization").unwrap().to_str().unwrap(),
            token0,
            "request {i} used a different account — stickiness broken"
        );
    }
}

#[tokio::test]
async fn test_all_accounts_exhausted_returns_503() {
    // All accounts return 429 → proxy returns 503
    let caps = Captures::default();
    let upstream = TestServer::start(make_mock_upstream(caps.clone(), false, 429)).await;

    let cfg = Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            log_level: "error".into(),
            upstream_url: upstream.url(),
        },
        accounts: vec![test_account(), test_account2()],
        config_file: std::path::PathBuf::from("/dev/null"),
    };
    let proxy = TestServer::start(create_app_with_state(cfg, StateStore::new_empty()).unwrap()).await;
    let client = Client::new();

    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 503);
    // Both accounts were tried
    assert_eq!(caps.len(), 2);
}

#[tokio::test]
async fn test_status_shows_account_status() {
    // After a 429 the account shows "cooling" in /status
    let (proxy, _up, _caps, client) = setup_multi().await;

    // This request hits account1 (429) then succeeds on account2,
    // leaving account1 in cooling state.
    client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body(r#"{"model":"m","max_tokens":1,"messages":[{"role":"user","content":"x"}]}"#)
        .send()
        .await
        .unwrap();

    let status: serde_json::Value = client
        .get(format!("{}/status", proxy.url()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let accounts = status["accounts"].as_array().unwrap();
    let a1 = accounts.iter().find(|a| a["name"] == "test").unwrap();
    let a2 = accounts.iter().find(|a| a["name"] == "second").unwrap();

    assert_eq!(a1["status"], "cooling", "account1 should be cooling after 429");
    assert_eq!(a2["status"], "available", "account2 should still be available");
}

// ---------------------------------------------------------------------------
// Live test — skipped unless ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_live_api() {
    // Accepts either an OAuth token (Claude Code) or API key
    let (token, is_bearer) =
        if let Ok(t) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
            (t, true)
        } else if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
            (k, false)
        } else {
            eprintln!("CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY not set — skipping");
            return;
        };

    let credential = OAuthCredential {
        email: None,
        access_token: if is_bearer { token.clone() } else { token.clone() },
        refresh_token: String::new(),
        expires_at: u64::MAX / 2,
    };

    let cfg = Config {
        server: ServerConfig {
            upstream_url: "https://api.anthropic.com".into(),
            host: "127.0.0.1".into(),
            port: 0,
            log_level: "error".into(),
        },
        accounts: vec![AccountConfig { name: "live".into(), plan_type: "pro".into(), credential }],
        config_file: std::path::PathBuf::from("/dev/null"),
    };

    let proxy = TestServer::start(create_app_with_state(cfg, StateStore::new_empty()).unwrap()).await;
    let client = Client::new();

    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
        .json(&json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "Reply with exactly: OK"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200, "body: {}", resp.text().await.unwrap());
}
