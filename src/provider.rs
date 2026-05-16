//! Provider abstraction — encapsulates all per-provider protocol differences.
//!
//! Adding a new provider means adding a variant and implementing each method.
//! Everything else (routing, quota, state, monitor) is provider-agnostic.

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::oauth::OAuthCredential;
use crate::state::RateLimitInfo;

// ---------------------------------------------------------------------------
// Provider enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Anthropic,
    OpenAI,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provider::Anthropic => write!(f, "anthropic"),
            Provider::OpenAI => write!(f, "openai"),
        }
    }
}

impl Provider {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "openai" | "codex" => Provider::OpenAI,
            _ => Provider::Anthropic,
        }
    }

    /// Default upstream API base URL.
    pub fn default_upstream_url(&self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::OpenAI => "https://chatgpt.com",
        }
    }

    /// Default local proxy port.
    pub fn default_port(&self) -> u16 {
        match self {
            Provider::Anthropic => 8082,
            Provider::OpenAI => 8083,
        }
    }

    /// Inject provider-specific auth and protocol headers into an upstream request.
    ///
    /// Called by the forwarder before each proxied request. The live OAuth token
    /// has already been retrieved by the caller.
    pub fn inject_auth_headers(
        &self,
        headers: &mut reqwest::header::HeaderMap,
        token: &str,
    ) -> anyhow::Result<()> {
        use reqwest::header::{HeaderName, HeaderValue};

        // Every provider uses Bearer auth.
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| anyhow::anyhow!("invalid access token"))?,
        );

        match self {
            Provider::Anthropic => {
                // Required when authenticating with OAuth tokens instead of API keys.
                headers.insert(
                    HeaderName::from_static("anthropic-dangerous-direct-browser-access"),
                    HeaderValue::from_static("true"),
                );

                // Ensure oauth-2025-04-20 is present in anthropic-beta, merged with
                // any beta flags the client already sent.
                let beta_key = HeaderName::from_static("anthropic-beta");
                let existing = headers
                    .get(&beta_key)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                let merged = if existing.split(',').any(|s| s.trim() == "oauth-2025-04-20") {
                    existing
                } else if existing.is_empty() {
                    "oauth-2025-04-20".to_owned()
                } else {
                    format!("{existing},oauth-2025-04-20")
                };
                headers.insert(beta_key, HeaderValue::from_str(&merged).unwrap());
            }
            Provider::OpenAI => {
                // OpenAI OAuth session: only the Bearer token is needed.
            }
        }

        Ok(())
    }

    /// Additional non-auth headers required for prefetch requests (not normal proxy requests).
    ///
    /// Returns `(header-name, header-value)` pairs as static strings.
    pub fn prefetch_extra_headers(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            Provider::Anthropic => &[("anthropic-version", "2023-06-01")],
            Provider::OpenAI => &[],
        }
    }

    /// Path and minimal JSON body for a prefetch request that returns rate-limit headers.
    ///
    /// Returns `None` if this provider doesn't support prefetching.
    pub fn prefetch_request(&self) -> Option<(&'static str, serde_json::Value)> {
        match self {
            Provider::Anthropic => Some((
                "/v1/messages",
                serde_json::json!({
                    "model": "claude-haiku-4-5-20251001",
                    "max_tokens": 1,
                    "messages": [{"role": "user", "content": "hi"}]
                }),
            )),
            // chatgpt.com does not return x-ratelimit-* headers on any endpoint — no probe possible.
            Provider::OpenAI => None,
        }
    }

    /// Extract rate-limit utilization from an upstream response's headers.
    ///
    /// Returns `None` when the response carries no recognisable rate-limit data.
    pub fn parse_rate_limits(&self, headers: &HeaderMap) -> Option<RateLimitInfo> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        match self {
            Provider::Anthropic => parse_anthropic_rate_limits(headers, now_ms),
            Provider::OpenAI => parse_openai_rate_limits(headers, now_ms),
        }
    }

    /// Read locally stored credentials from this provider's CLI tool.
    pub fn read_local_credentials(&self) -> Option<OAuthCredential> {
        match self {
            Provider::Anthropic => crate::oauth::read_claude_credentials(),
            Provider::OpenAI => crate::oauth::read_codex_credentials(),
        }
    }

    /// Refresh an expired access token using the provider's token endpoint.
    pub async fn refresh_token(&self, cred: &OAuthCredential) -> anyhow::Result<OAuthCredential> {
        match self {
            Provider::Anthropic => crate::oauth::refresh_token(cred).await,
            Provider::OpenAI => crate::oauth::refresh_openai_token(cred).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Anthropic rate-limit header parsing
// ---------------------------------------------------------------------------

fn parse_anthropic_rate_limits(headers: &HeaderMap, now_ms: u64) -> Option<RateLimitInfo> {
    fn hdr_u64(h: &HeaderMap, name: &str) -> Option<u64> {
        h.get(name)?.to_str().ok()?.parse().ok()
    }
    fn hdr_f64(h: &HeaderMap, name: &str) -> Option<f64> {
        h.get(name)?.to_str().ok()?.parse().ok()
    }
    fn hdr_str(h: &HeaderMap, name: &str) -> Option<String> {
        Some(h.get(name)?.to_str().ok()?.to_owned())
    }

    let utilization_5h = hdr_f64(headers, "anthropic-ratelimit-unified-5h-utilization");
    let utilization_7d = hdr_f64(headers, "anthropic-ratelimit-unified-7d-utilization");

    if utilization_5h.is_none() && utilization_7d.is_none() {
        return None;
    }

    Some(RateLimitInfo {
        utilization_5h,
        reset_5h:       hdr_u64(headers, "anthropic-ratelimit-unified-5h-reset"),
        status_5h:      hdr_str(headers, "anthropic-ratelimit-unified-5h-status"),
        utilization_7d,
        reset_7d:       hdr_u64(headers, "anthropic-ratelimit-unified-7d-reset"),
        status_7d:      hdr_str(headers, "anthropic-ratelimit-unified-7d-status"),
        overage_status:          hdr_str(headers, "anthropic-ratelimit-unified-overage-status"),
        overage_disabled_reason: hdr_str(headers, "anthropic-ratelimit-unified-overage-disabled-reason"),
        representative_claim:    hdr_str(headers, "anthropic-ratelimit-unified-representative-claim"),
        updated_ms: now_ms,
    })
}

// ---------------------------------------------------------------------------
// OpenAI rate-limit header parsing
// ---------------------------------------------------------------------------

fn parse_openai_rate_limits(headers: &HeaderMap, now_ms: u64) -> Option<RateLimitInfo> {
    fn hdr_u64(h: &HeaderMap, name: &str) -> Option<u64> {
        h.get(name)?.to_str().ok()?.parse().ok()
    }
    fn hdr_str(h: &HeaderMap, name: &str) -> Option<String> {
        Some(h.get(name)?.to_str().ok()?.to_owned())
    }

    // Token-based limits are the primary signal (maps to Anthropic's 5h utilization).
    let limit_tok     = hdr_u64(headers, "x-ratelimit-limit-tokens");
    let remaining_tok = hdr_u64(headers, "x-ratelimit-remaining-tokens");
    let reset_tok_str = hdr_str(headers, "x-ratelimit-reset-tokens");

    let utilization = match (limit_tok, remaining_tok) {
        (Some(limit), Some(remaining)) if limit > 0 => {
            Some(1.0_f64 - (remaining as f64 / limit as f64))
        }
        _ => None,
    };

    // OpenAI reset is a relative duration like "1m30s"; convert to epoch seconds.
    let reset_secs = reset_tok_str.as_deref().and_then(parse_openai_reset_duration);

    if utilization.is_none() && reset_secs.is_none() {
        return None;
    }

    Some(RateLimitInfo {
        utilization_5h: utilization,
        reset_5h: reset_secs,
        status_5h: utilization.map(|u| if u >= 1.0 { "exhausted".into() } else { "allowed".into() }),
        // OpenAI has no 7-day window concept.
        utilization_7d: None,
        reset_7d:       None,
        status_7d:      None,
        overage_status:          None,
        overage_disabled_reason: None,
        representative_claim:    None,
        updated_ms: now_ms,
    })
}

/// Parse an OpenAI reset duration string ("1m30s", "45s", "2m") into an
/// absolute Unix epoch second timestamp.
fn parse_openai_reset_duration(s: &str) -> Option<u64> {
    if s.is_empty() { return None; }

    let mut total_secs: u64 = 0;
    let mut parsed = false;
    let mut rest = s;

    if let Some(idx) = rest.find('m') {
        let mins: u64 = rest[..idx].parse().ok()?;
        total_secs += mins * 60;
        rest = &rest[idx + 1..];
        parsed = true;
    }

    if let Some(stripped) = rest.strip_suffix('s') {
        if !stripped.is_empty() {
            let secs: u64 = stripped.parse().ok()?;
            total_secs += secs;
        }
        parsed = true;
    } else if !rest.is_empty() {
        return None; // unexpected trailing chars
    }

    if !parsed { return None; }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Some(now_secs + total_secs)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_from_str() {
        assert_eq!(Provider::from_str("anthropic"), Provider::Anthropic);
        assert_eq!(Provider::from_str("ANTHROPIC"), Provider::Anthropic);
        assert_eq!(Provider::from_str("openai"), Provider::OpenAI);
        assert_eq!(Provider::from_str("codex"), Provider::OpenAI);
        assert_eq!(Provider::from_str("unknown"), Provider::Anthropic);
    }

    #[test]
    fn test_provider_display() {
        assert_eq!(Provider::Anthropic.to_string(), "anthropic");
        assert_eq!(Provider::OpenAI.to_string(), "openai");
    }

    #[test]
    fn test_parse_openai_reset_duration_formats() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let r = parse_openai_reset_duration("1m30s").unwrap();
        assert!(r >= now + 89 && r <= now + 91, "1m30s should be ~90s from now");

        let r = parse_openai_reset_duration("45s").unwrap();
        assert!(r >= now + 44 && r <= now + 46, "45s should be ~45s from now");

        let r = parse_openai_reset_duration("2m").unwrap();
        assert!(r >= now + 119 && r <= now + 121, "2m should be ~120s from now");

        let r = parse_openai_reset_duration("0s").unwrap();
        assert!(r >= now && r <= now + 1, "0s should be now");
    }

    #[test]
    fn test_parse_openai_reset_duration_invalid() {
        assert!(parse_openai_reset_duration("bad").is_none());
        assert!(parse_openai_reset_duration("").is_none());
    }

    #[test]
    fn test_openai_utilization_computation() {
        use axum::http::HeaderMap;
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-limit-tokens", "100000".parse().unwrap());
        headers.insert("x-ratelimit-remaining-tokens", "75000".parse().unwrap());
        headers.insert("x-ratelimit-reset-tokens", "45s".parse().unwrap());

        let info = Provider::OpenAI.parse_rate_limits(&headers).unwrap();
        let util = info.utilization_5h.unwrap();
        assert!((util - 0.25).abs() < 0.001, "utilization should be 0.25 (75k/100k remaining)");
        assert_eq!(info.status_5h.as_deref(), Some("allowed"));
        assert!(info.reset_5h.is_some());
    }

    #[test]
    fn test_anthropic_rate_limits_absent() {
        let headers = axum::http::HeaderMap::new();
        assert!(Provider::Anthropic.parse_rate_limits(&headers).is_none());
    }

    #[test]
    fn test_openai_rate_limits_absent() {
        let headers = axum::http::HeaderMap::new();
        assert!(Provider::OpenAI.parse_rate_limits(&headers).is_none());
    }
}
