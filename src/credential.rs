//! Credential abstraction — supports OAuth (with refresh) and static API keys.
//!
//! All provider-specific auth is gated behind this enum so the rest of the
//! codebase stays credential-type-agnostic.

use serde::{Deserialize, Serialize};

use crate::oauth::OAuthCredential;

// ---------------------------------------------------------------------------
// Credential enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Credential {
    /// OAuth credential with access + refresh tokens and an expiry.
    /// Used by Anthropic (claude.ai) and OpenAI (chatgpt.com) accounts.
    Oauth(OAuthCredential),
    /// Static API key — no expiry, no refresh.
    /// Used by Groq, Mistral, OpenRouter, Gemini, Ollama Cloud, etc.
    Apikey { key: String },
}

impl Credential {
    /// The bearer token to send in `Authorization: Bearer <token>`.
    ///
    /// For OAuth accounts: prefers `id_token` over `access_token` when
    /// present (required by chatgpt.com / Codex). Falls back to
    /// `access_token` for standard Anthropic OAuth.
    ///
    /// For API-key accounts: returns the raw key directly.
    pub fn bearer_token(&self) -> &str {
        match self {
            Credential::Oauth(c) => c.id_token.as_deref().unwrap_or(&c.access_token),
            Credential::Apikey { key } => key,
        }
    }

    /// The raw `access_token` string.
    ///
    /// Used when you need the access_token specifically (e.g. token-rotation
    /// comparison in the 401 handler, Anthropic auth headers).
    ///
    /// For API-key accounts returns the key (same as `bearer_token`).
    pub fn access_token(&self) -> &str {
        match self {
            Credential::Oauth(c) => &c.access_token,
            Credential::Apikey { key } => key,
        }
    }

    /// True if the credential should be refreshed before use.
    /// Always false for API-key credentials.
    pub fn needs_refresh(&self) -> bool {
        match self {
            Credential::Oauth(c) => c.needs_refresh(),
            Credential::Apikey { .. } => false,
        }
    }

    /// Account email, if known. None for API-key credentials.
    pub fn email(&self) -> Option<&str> {
        match self {
            Credential::Oauth(c) => c.email.as_deref(),
            Credential::Apikey { .. } => None,
        }
    }

    /// True when a refresh_token is available to attempt recovery.
    /// Always false for API-key credentials.
    pub fn has_refresh_token(&self) -> bool {
        match self {
            Credential::Oauth(c) => !c.refresh_token.is_empty(),
            Credential::Apikey { .. } => false,
        }
    }

    /// Borrow the inner OAuthCredential, if this is an OAuth credential.
    pub fn as_oauth(&self) -> Option<&OAuthCredential> {
        match self {
            Credential::Oauth(c) => Some(c),
            Credential::Apikey { .. } => None,
        }
    }

    /// Mutably borrow the inner OAuthCredential.
    pub fn as_oauth_mut(&mut self) -> Option<&mut OAuthCredential> {
        match self {
            Credential::Oauth(c) => Some(c),
            Credential::Apikey { .. } => None,
        }
    }

    /// Display string for status/monitor output.
    /// Shows email for OAuth accounts, masked key for API-key accounts.
    pub fn masked_display(&self) -> String {
        match self {
            Credential::Oauth(c) => c.email.clone().unwrap_or_else(|| "oauth".to_owned()),
            Credential::Apikey { key } => {
                let suffix = &key[key.len().saturating_sub(4)..];
                format!("···{suffix}")
            }
        }
    }
}

impl From<OAuthCredential> for Credential {
    fn from(c: OAuthCredential) -> Self {
        Credential::Oauth(c)
    }
}

// ---------------------------------------------------------------------------
// Backwards-compatible deserialization for CredentialsStore
// ---------------------------------------------------------------------------

/// Deserialize a `HashMap<String, Credential>` that may contain old-format
/// entries (written before the `"type"` tag was introduced).
///
/// Old format: `{ "access_token": "...", "refresh_token": "...", ... }`
/// New format: `{ "type": "oauth", "access_token": "...", ... }`
///             `{ "type": "apikey", "key": "..." }`
pub fn deserialize_credential_map<'de, D>(
    deserializer: D,
) -> Result<std::collections::HashMap<String, Credential>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use std::collections::HashMap;
    let raw: HashMap<String, serde_json::Value> = HashMap::deserialize(deserializer)?;
    let mut out = HashMap::with_capacity(raw.len());
    for (k, v) in raw {
        let cred = if v.get("type").is_some() {
            // New tagged format — deserialize directly.
            serde_json::from_value::<Credential>(v).map_err(serde::de::Error::custom)?
        } else {
            // Legacy format — treat as OAuth.
            serde_json::from_value::<OAuthCredential>(v)
                .map(Credential::Oauth)
                .map_err(serde::de::Error::custom)?
        };
        out.insert(k, cred);
    }
    Ok(out)
}
