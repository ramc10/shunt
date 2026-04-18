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

/// Auth headers that the proxy manages — always stripped from client requests
/// and replaced with the selected account's credential.
const CLIENT_AUTH_HEADERS: &[&str] = &["authorization", "x-api-key"];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.contains(&name.to_ascii_lowercase().as_str())
}

fn is_client_auth(name: &str) -> bool {
    CLIENT_AUTH_HEADERS.contains(&name.to_ascii_lowercase().as_str())
}

pub struct Forwarder {
    client: Client,
    base_url: String,
}

impl Forwarder {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self { client, base_url: base_url.into() })
    }

    /// Forward a request to the upstream using the given account's OAuth credential.
    ///
    /// - Strips `Authorization` and `x-api-key` from the client request.
    /// - Injects `Authorization: Bearer <token>` (live token, may differ from account.credential).
    /// - Keeps the upstream TCP connection alive for streaming responses.
    pub async fn forward(
        &self,
        method: &str,
        path: &str,
        body: Bytes,
        client_headers: &HeaderMap,
        account: &AccountConfig,
        token: &str,
    ) -> Result<Response<Body>> {
        let request_id = &Uuid::new_v4().to_string()[..8];
        let url = format!("{}{}", self.base_url, path);

        let mut upstream_headers = reqwest::header::HeaderMap::new();

        for (name, value) in client_headers.iter() {
            let lower = name.as_str().to_ascii_lowercase();
            if is_hop_by_hop(&lower) || is_client_auth(&lower) {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                reqwest::header::HeaderName::from_str(name.as_str()),
                reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
            ) {
                upstream_headers.insert(n, v);
            }
        }

        // Inject the live OAuth Bearer token
        upstream_headers.insert(
            reqwest::header::HeaderName::from_static("authorization"),
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .context("invalid access token value")?,
        );

        // Required by Anthropic when authenticating with OAuth tokens
        upstream_headers.insert(
            reqwest::header::HeaderName::from_static("anthropic-dangerous-direct-browser-access"),
            reqwest::header::HeaderValue::from_static("true"),
        );

        // Ensure oauth-2025-04-20 is present in anthropic-beta (required for OAuth tokens).
        // Merge with any beta flags the client already sent.
        let beta_key = reqwest::header::HeaderName::from_static("anthropic-beta");
        let existing_beta = upstream_headers
            .get(&beta_key)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let merged_beta = if existing_beta.split(',').any(|s| s.trim() == "oauth-2025-04-20") {
            existing_beta
        } else if existing_beta.is_empty() {
            "oauth-2025-04-20".to_owned()
        } else {
            format!("{existing_beta},oauth-2025-04-20")
        };
        upstream_headers.insert(
            beta_key,
            reqwest::header::HeaderValue::from_str(&merged_beta).unwrap(),
        );

        let t0 = Instant::now();
        let upstream_resp = self
            .client
            .request(
                reqwest::Method::from_str(method).context("invalid method")?,
                &url,
            )
            .headers(upstream_headers)
            .body(body)
            .send()
            .await
            .context("upstream request failed")?;

        let latency_ms = t0.elapsed().as_millis();
        let status = upstream_resp.status();

        info!(
            request_id = %request_id,
            account = %account.name,
            status = status.as_u16(),
            latency_ms = %latency_ms,
            path = %path,
            "request forwarded"
        );

        let mut builder = Response::builder().status(status.as_u16());

        for (name, value) in upstream_resp.headers().iter() {
            if !is_hop_by_hop(name.as_str()) {
                if let (Ok(n), Ok(v)) = (
                    HeaderName::from_str(name.as_str()),
                    HeaderValue::from_bytes(value.as_bytes()),
                ) {
                    builder = builder.header(n, v);
                }
            }
        }

        let body = Body::from_stream(upstream_resp.bytes_stream());
        Ok(builder.body(body).expect("response builder invariant"))
    }
}
