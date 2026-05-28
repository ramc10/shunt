use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Response};
use bytes::Bytes;
use reqwest::Client;
use std::str::FromStr;
use std::time::Instant;
use tracing::info;
use uuid::Uuid;

use crate::config::AccountConfig;

/// Headers that must never be forwarded in either direction.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// Headers the proxy explicitly passes through to upstream.
/// All other client-supplied headers are dropped (allowlist approach, #15).
const ALLOWED_REQUEST_HEADERS: &[&str] = &[
    "content-type",
    "accept",
    "anthropic-version",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    "x-request-id",
    "user-agent",
    // chatgpt.com sentinel token — injected by proxy, pass through
    "openai-sentinel-chat-requirements-token",
];

/// Sensitive response headers that upstream must never inject into client responses (#21).
const BLOCKED_RESPONSE_HEADERS: &[&str] = &[
    "set-cookie",
    "set-cookie2",
    "access-control-allow-origin",
    "access-control-allow-credentials",
    "access-control-allow-methods",
    "access-control-allow-headers",
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.contains(&name.to_ascii_lowercase().as_str())
}

pub struct Forwarder {
    client: Client,
}

impl Forwarder {
    pub fn new(_base_url: impl Into<String>, timeout_secs: u64) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self { client })
    }

    /// Forward a request to the upstream using the given account's OAuth credential.
    ///
    /// - `upstream` overrides the base URL for this account (per-provider routing).
    /// - Strips `Authorization` and `x-api-key` from the client request.
    /// - Injects `Authorization: Bearer <token>` (live token, may differ from account.credential).
    /// - Keeps the upstream TCP connection alive for streaming responses.
    pub async fn forward(
        &self,
        upstream: &str,
        method: &str,
        path: &str,
        body: Bytes,
        client_headers: &HeaderMap,
        account: &AccountConfig,
        token: &str,
    ) -> Result<Response<Body>> {
        let request_id = &Uuid::new_v4().to_string()[..8];
        let url = format!("{}{}", upstream, path);

        let mut upstream_headers = reqwest::header::HeaderMap::new();

        // #15: allowlist — only forward explicitly permitted client headers.
        for &name in ALLOWED_REQUEST_HEADERS {
            if let Some(value) = client_headers.get(name) {
                if let Ok(n) = reqwest::header::HeaderName::from_str(name) {
                    if let Ok(v) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
                        upstream_headers.insert(n, v);
                    }
                }
            }
        }

        // Inject provider-specific auth headers (Bearer token + any required protocol headers).
        account.provider.inject_auth_headers(&mut upstream_headers, token)
            .context("failed to inject auth headers")?;

        let t0 = Instant::now();
        let upstream_resp = self
            .client
            .request(
                reqwest::Method::from_str(method).context("invalid method")?,
                &url,
            )
            .headers(upstream_headers)
            .body(body.clone())
            .send()
            .await
            .context("upstream request failed")?;

        let status = upstream_resp.status();

        let mut builder = Response::builder().status(status.as_u16());

        for (name, value) in upstream_resp.headers().iter() {
            let lower = name.as_str().to_ascii_lowercase();
            // #21: drop hop-by-hop and sensitive response headers.
            if is_hop_by_hop(&lower) || BLOCKED_RESPONSE_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_str(name.as_str()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                builder = builder.header(n, v);
            }
        }

        let body = Body::from_stream(upstream_resp.bytes_stream());
        Ok(builder.body(body).expect("response builder invariant"))
    }
}
