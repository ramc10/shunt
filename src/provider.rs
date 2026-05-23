//! Provider abstraction — encapsulates all per-provider protocol differences.
//!
//! Adding a new provider means adding a variant and implementing each method.
//! Everything else (routing, quota, state, monitor) is provider-agnostic.

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::credential::Credential;
use crate::oauth::OAuthCredential;
use crate::state::RateLimitInfo;

// ---------------------------------------------------------------------------
// AuthKind — how this provider authenticates
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthKind {
    /// OAuth with access + refresh tokens (Anthropic, OpenAI chatgpt.com).
    OAuth,
    /// Static API key in `Authorization: Bearer <key>`.
    ApiKey,
    /// No authentication (local servers).
    None,
}

// ---------------------------------------------------------------------------
// WireProtocol — request/response format
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireProtocol {
    /// Anthropic native Messages API format.
    Anthropic,
    /// OpenAI-compatible Chat Completions format.
    OpenAICompat,
}

// ---------------------------------------------------------------------------
// Provider enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Anthropic claude.ai — OAuth, Anthropic wire format.
    #[default]
    Anthropic,
    /// OpenAI chatgpt.com — OAuth, OpenAI-compat wire format.
    OpenAI,
    /// OpenAI API (api.openai.com) — API key, OpenAI-compat wire format.
    #[serde(rename = "openai-api")]
    OpenAIApi,
    /// Ollama Cloud (api.ollama.com) — API key, OpenAI-compat wire format.
    #[serde(rename = "ollama")]
    OllamaCloud,
    /// Groq (api.groq.com) — API key, OpenAI-compat wire format.
    Groq,
    /// Mistral AI (api.mistral.ai) — API key, OpenAI-compat wire format.
    Mistral,
    /// Together AI (api.together.xyz) — API key, OpenAI-compat wire format.
    Together,
    /// OpenRouter (openrouter.ai) — API key, OpenAI-compat wire format.
    OpenRouter,
    /// DeepSeek (api.deepseek.com) — API key, OpenAI-compat wire format.
    DeepSeek,
    /// Fireworks AI (api.fireworks.ai) — API key, OpenAI-compat wire format.
    Fireworks,
    /// Google Gemini (generativelanguage.googleapis.com) — API key, OpenAI-compat wire format.
    Gemini,
    /// Generic local OpenAI-compatible server (Ollama local, LM Studio, llama.cpp).
    Local,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provider::Anthropic   => write!(f, "anthropic"),
            Provider::OpenAI      => write!(f, "openai"),
            Provider::OpenAIApi   => write!(f, "openai-api"),
            Provider::OllamaCloud => write!(f, "ollama"),
            Provider::Groq        => write!(f, "groq"),
            Provider::Mistral     => write!(f, "mistral"),
            Provider::Together    => write!(f, "together"),
            Provider::OpenRouter  => write!(f, "openrouter"),
            Provider::DeepSeek    => write!(f, "deepseek"),
            Provider::Fireworks   => write!(f, "fireworks"),
            Provider::Gemini      => write!(f, "gemini"),
            Provider::Local       => write!(f, "local"),
        }
    }
}

impl Provider {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "openai" | "codex"              => Provider::OpenAI,
            "openai-api" | "openai_api"     => Provider::OpenAIApi,
            "ollama" | "ollama-cloud" | "ollamacloud" => Provider::OllamaCloud,
            "groq"                          => Provider::Groq,
            "mistral"                       => Provider::Mistral,
            "together" | "together-ai"      => Provider::Together,
            "openrouter" | "open-router"    => Provider::OpenRouter,
            "deepseek" | "deep-seek"        => Provider::DeepSeek,
            "fireworks" | "fireworks-ai"    => Provider::Fireworks,
            "gemini" | "google"             => Provider::Gemini,
            "local"                         => Provider::Local,
            _                               => Provider::Anthropic,
        }
    }

    /// How this provider authenticates.
    pub fn auth_kind(&self) -> AuthKind {
        match self {
            Provider::Anthropic | Provider::OpenAI => AuthKind::OAuth,
            Provider::Local                        => AuthKind::None,
            _                                      => AuthKind::ApiKey,
        }
    }

    /// Wire protocol used for requests/responses.
    pub fn wire_protocol(&self) -> WireProtocol {
        match self {
            Provider::Anthropic => WireProtocol::Anthropic,
            _                   => WireProtocol::OpenAICompat,
        }
    }

    /// Well-known environment variable that holds an API key for this provider.
    /// `None` for OAuth and Local providers.
    pub fn api_key_env_var(&self) -> Option<&'static str> {
        match self {
            Provider::OpenAIApi   => Some("OPENAI_API_KEY"),
            Provider::OllamaCloud => Some("OLLAMA_API_KEY"),
            Provider::Groq        => Some("GROQ_API_KEY"),
            Provider::Mistral     => Some("MISTRAL_API_KEY"),
            Provider::Together    => Some("TOGETHER_API_KEY"),
            Provider::OpenRouter  => Some("OPENROUTER_API_KEY"),
            Provider::DeepSeek    => Some("DEEPSEEK_API_KEY"),
            Provider::Fireworks   => Some("FIREWORKS_API_KEY"),
            Provider::Gemini      => Some("GEMINI_API_KEY"),
            _                     => None,
        }
    }

    /// Default upstream API base URL.
    pub fn default_upstream_url(&self) -> &'static str {
        match self {
            Provider::Anthropic   => "https://api.anthropic.com",
            Provider::OpenAI      => "https://chatgpt.com",
            Provider::OpenAIApi   => "https://api.openai.com",
            Provider::OllamaCloud => "https://api.ollama.com",
            Provider::Groq        => "https://api.groq.com",
            Provider::Mistral     => "https://api.mistral.ai",
            Provider::Together    => "https://api.together.xyz",
            Provider::OpenRouter  => "https://openrouter.ai",
            Provider::DeepSeek    => "https://api.deepseek.com",
            Provider::Fireworks   => "https://api.fireworks.ai",
            Provider::Gemini      => "https://generativelanguage.googleapis.com",
            Provider::Local       => "http://localhost:11434",
        }
    }

    /// Default local proxy port (used when multiple providers are active).
    pub fn default_port(&self) -> u16 {
        match self {
            Provider::Anthropic   => 8082,
            Provider::OpenAI      => 8083,
            Provider::OpenAIApi   => 8084,
            Provider::OllamaCloud => 8085,
            Provider::Groq        => 8086,
            Provider::Mistral     => 8087,
            Provider::Together    => 8088,
            Provider::OpenRouter  => 8089,
            Provider::DeepSeek    => 8090,
            Provider::Fireworks   => 8091,
            Provider::Gemini      => 8092,
            Provider::Local       => 8093,
        }
    }

    /// Inject provider-specific auth and protocol headers into an upstream request.
    ///
    /// Called by the forwarder before each proxied request. The live token
    /// has already been retrieved by the caller.
    pub fn inject_auth_headers(
        &self,
        headers: &mut reqwest::header::HeaderMap,
        token: &str,
    ) -> anyhow::Result<()> {
        use reqwest::header::{HeaderName, HeaderValue};

        // Local provider needs no auth.
        if self.auth_kind() == AuthKind::None {
            return Ok(());
        }

        // All authenticated providers use Bearer.
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
            Provider::OpenRouter => {
                // OpenRouter recommends sending an HTTP-Referer for tracking.
                headers.insert(
                    HeaderName::from_static("http-referer"),
                    HeaderValue::from_static("https://github.com/shunt-proxy/shunt"),
                );
            }
            // All other providers: Bearer token is sufficient.
            _ => {}
        }

        Ok(())
    }

    /// Additional non-auth headers required for prefetch requests (not normal proxy requests).
    ///
    /// Returns `(header-name, header-value)` pairs as static strings.
    pub fn prefetch_extra_headers(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            Provider::Anthropic => &[("anthropic-version", "2023-06-01")],
            _ => &[],
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
            // API-key providers: auth_probe_get_path() is used instead to avoid spending tokens.
            _ => None,
        }
    }

    /// GET path for a lightweight auth-validity check (no rate-limit data expected).
    /// Used for providers where `prefetch_request` is unavailable.
    pub fn auth_probe_get_path(&self) -> Option<&'static str> {
        match self {
            Provider::Anthropic   => None, // prefetch_request() already verifies auth
            Provider::OpenAI      => Some("/backend-api/me"),
            Provider::OpenAIApi   => Some("/v1/models"),
            Provider::OllamaCloud => Some("/v1/models"),
            Provider::Groq        => Some("/openai/v1/models"),
            Provider::Mistral     => Some("/v1/models"),
            Provider::Together    => Some("/v1/models"),
            Provider::OpenRouter  => Some("/api/v1/models"),
            Provider::DeepSeek    => Some("/v1/models"),
            Provider::Fireworks   => Some("/v1/models"),
            Provider::Gemini      => Some("/v1beta/models"),
            Provider::Local       => None, // trust the local server is up
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
            // OpenAI-compat providers that return x-ratelimit-* headers.
            Provider::OpenAI
            | Provider::OpenAIApi
            | Provider::OllamaCloud
            | Provider::Groq
            | Provider::Mistral
            | Provider::Together
            | Provider::OpenRouter
            | Provider::DeepSeek
            | Provider::Fireworks => parse_openai_rate_limits(headers, now_ms),
            // Gemini and Local don't return standard rate-limit headers.
            Provider::Gemini | Provider::Local => None,
        }
    }

    /// Read credentials from the provider's local CLI tool or well-known environment variable.
    ///
    /// - OAuth providers: import from the provider's local CLI auth store.
    /// - API-key providers: read from the well-known environment variable.
    /// - Local provider: always returns `None` (no auth needed).
    pub fn read_local_credentials(&self) -> Option<Credential> {
        match self.auth_kind() {
            AuthKind::OAuth => match self {
                Provider::Anthropic => {
                    crate::oauth::read_claude_credentials().map(Credential::Oauth)
                }
                Provider::OpenAI => {
                    crate::oauth::read_codex_credentials().map(Credential::Oauth)
                }
                _ => None,
            },
            AuthKind::ApiKey => {
                // Try the well-known environment variable for this provider.
                self.api_key_env_var()
                    .and_then(|var| std::env::var(var).ok())
                    .map(|key| Credential::Apikey { key })
            }
            AuthKind::None => None,
        }
    }

    /// Refresh an expired access token using the provider's token endpoint.
    ///
    /// Only applicable to OAuth providers. Returns an error for API-key and Local providers.
    pub async fn refresh_token(&self, cred: &OAuthCredential) -> anyhow::Result<OAuthCredential> {
        match self {
            Provider::Anthropic => crate::oauth::refresh_token(cred).await,
            Provider::OpenAI    => crate::oauth::refresh_openai_token(cred).await,
            _ => anyhow::bail!("provider {} does not support token refresh", self),
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
        assert_eq!(Provider::from_str("openai-api"), Provider::OpenAIApi);
        assert_eq!(Provider::from_str("ollama"), Provider::OllamaCloud);
        assert_eq!(Provider::from_str("ollama-cloud"), Provider::OllamaCloud);
        assert_eq!(Provider::from_str("groq"), Provider::Groq);
        assert_eq!(Provider::from_str("mistral"), Provider::Mistral);
        assert_eq!(Provider::from_str("together"), Provider::Together);
        assert_eq!(Provider::from_str("openrouter"), Provider::OpenRouter);
        assert_eq!(Provider::from_str("deepseek"), Provider::DeepSeek);
        assert_eq!(Provider::from_str("fireworks"), Provider::Fireworks);
        assert_eq!(Provider::from_str("gemini"), Provider::Gemini);
        assert_eq!(Provider::from_str("local"), Provider::Local);
        assert_eq!(Provider::from_str("unknown"), Provider::Anthropic);
    }

    #[test]
    fn test_provider_display() {
        assert_eq!(Provider::Anthropic.to_string(), "anthropic");
        assert_eq!(Provider::OpenAI.to_string(), "openai");
        assert_eq!(Provider::OpenAIApi.to_string(), "openai-api");
        assert_eq!(Provider::OllamaCloud.to_string(), "ollama");
        assert_eq!(Provider::Groq.to_string(), "groq");
        assert_eq!(Provider::Mistral.to_string(), "mistral");
        assert_eq!(Provider::Together.to_string(), "together");
        assert_eq!(Provider::OpenRouter.to_string(), "openrouter");
        assert_eq!(Provider::DeepSeek.to_string(), "deepseek");
        assert_eq!(Provider::Fireworks.to_string(), "fireworks");
        assert_eq!(Provider::Gemini.to_string(), "gemini");
        assert_eq!(Provider::Local.to_string(), "local");
    }

    #[test]
    fn test_auth_kind() {
        assert_eq!(Provider::Anthropic.auth_kind(), AuthKind::OAuth);
        assert_eq!(Provider::OpenAI.auth_kind(), AuthKind::OAuth);
        assert_eq!(Provider::Local.auth_kind(), AuthKind::None);
        assert_eq!(Provider::Groq.auth_kind(), AuthKind::ApiKey);
        assert_eq!(Provider::OpenAIApi.auth_kind(), AuthKind::ApiKey);
        assert_eq!(Provider::OllamaCloud.auth_kind(), AuthKind::ApiKey);
    }

    #[test]
    fn test_wire_protocol() {
        assert_eq!(Provider::Anthropic.wire_protocol(), WireProtocol::Anthropic);
        assert_eq!(Provider::OpenAI.wire_protocol(), WireProtocol::OpenAICompat);
        assert_eq!(Provider::Groq.wire_protocol(), WireProtocol::OpenAICompat);
        assert_eq!(Provider::Local.wire_protocol(), WireProtocol::OpenAICompat);
    }

    #[test]
    fn test_api_key_env_var() {
        assert_eq!(Provider::Groq.api_key_env_var(), Some("GROQ_API_KEY"));
        assert_eq!(Provider::OpenAIApi.api_key_env_var(), Some("OPENAI_API_KEY"));
        assert_eq!(Provider::Gemini.api_key_env_var(), Some("GEMINI_API_KEY"));
        assert_eq!(Provider::Anthropic.api_key_env_var(), None);
        assert_eq!(Provider::Local.api_key_env_var(), None);
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
