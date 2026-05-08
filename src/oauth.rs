/// OAuth 2.0 PKCE flow + token refresh for claude.ai accounts.
///
/// Claude Code authenticates via OAuth, not API keys. Credentials are stored
/// in ~/.claude/.credentials.json and sent as `Authorization: Bearer <token>`.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const OAUTH_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

// ---------------------------------------------------------------------------
// Credential type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access_token: String,
    pub refresh_token: String,
    /// Milliseconds since Unix epoch
    pub expires_at: u64,
    /// Account email, fetched from roles endpoint after auth
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl OAuthCredential {
    /// True if the token expires within the next 5 minutes.
    pub fn needs_refresh(&self) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        now_ms >= self.expires_at.saturating_sub(5 * 60 * 1000)
    }
}

// ---------------------------------------------------------------------------
// Auto-import from Claude Code's own credential file
// ---------------------------------------------------------------------------

/// Raw format used by ~/.claude/.credentials.json
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeCredentials {
    claude_ai_oauth: Option<ClaudeOAuthRaw>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeOAuthRaw {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

// ---------------------------------------------------------------------------
// Session info (plan + identity) from stored credentials
// ---------------------------------------------------------------------------

pub struct SessionInfo {
    pub email_or_id: String,
    pub plan: String,
}

/// Read plan and identity from Claude Code's stored credentials JSON.
/// Works for both keychain and file-based storage.
pub fn read_claude_session_info() -> Option<SessionInfo> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Outer {
        claude_ai_oauth: Option<Inner>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Inner {
        subscription_type: Option<String>,
        #[serde(rename = "rateLimitTier")]
        rate_limit_tier: Option<String>,
    }

    let text = read_raw_credentials_json()?;
    let outer: Outer = serde_json::from_str(&text).ok()?;
    let inner = outer.claude_ai_oauth?;

    let plan = inner.subscription_type.unwrap_or_else(|| "pro".into());
    let email_or_id = inner.rate_limit_tier.unwrap_or_else(|| "unknown".into());

    Some(SessionInfo { email_or_id, plan })
}

/// Returns the raw credentials JSON string from keychain (macOS) or file.
fn read_raw_credentials_json() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .output()
            .ok()?;
        if out.status.success() {
            let s = String::from_utf8(out.stdout).ok()?;
            return Some(s.trim().to_owned());
        }
    }
    std::fs::read_to_string(claude_credentials_path()).ok()
}

pub fn claude_credentials_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join(".credentials.json")
}

/// Read the OAuth credential from Claude Code's own credential file.
/// On macOS, tries the Keychain first (service "Claude Code-credentials"),
/// then falls back to ~/.claude/.credentials.json.
pub fn read_claude_credentials() -> Option<OAuthCredential> {
    // macOS: try Keychain first
    #[cfg(target_os = "macos")]
    if let Some(cred) = read_claude_credentials_keychain() {
        return Some(cred);
    }

    // Fallback: JSON file (older Claude Code versions / non-macOS)
    let path = claude_credentials_path();
    let text = std::fs::read_to_string(&path).ok()?;
    parse_claude_credentials_json(&text)
}

#[cfg(target_os = "macos")]
fn read_claude_credentials_keychain() -> Option<OAuthCredential> {
    let text = read_raw_credentials_json()?;
    parse_claude_credentials_json(&text)
}

fn parse_claude_credentials_json(text: &str) -> Option<OAuthCredential> {
    let raw: ClaudeCredentials = serde_json::from_str(text).ok()?;
    let inner = raw.claude_ai_oauth?;
    Some(OAuthCredential {
        access_token: inner.access_token,
        refresh_token: inner.refresh_token,
        expires_at: inner.expires_at,
        email: None,
    })
}

// ---------------------------------------------------------------------------
// Token refresh
// ---------------------------------------------------------------------------

/// Refresh an expired access token. Returns the updated credential.
pub async fn refresh_token(cred: &OAuthCredential) -> Result<OAuthCredential> {
    let client = reqwest::Client::new();

    let resp = client
        .post(OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            urlencoding::encode(&cred.refresh_token),
            OAUTH_CLIENT_ID,
        ))
        .send()
        .await
        .context("token refresh request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("token refresh failed ({status}): {body}");
    }

    let body: serde_json::Value = resp.json().await.context("token refresh: invalid JSON")?;

    let access_token = body["access_token"]
        .as_str()
        .context("token refresh: missing access_token")?
        .to_owned();

    let refresh_token = body["refresh_token"]
        .as_str()
        .unwrap_or(&cred.refresh_token)
        .to_owned();

    // expires_in is seconds from now
    let expires_in_secs = body["expires_in"].as_u64().unwrap_or(3600);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let expires_at = now_ms + expires_in_secs * 1000;

    Ok(OAuthCredential { access_token, refresh_token, expires_at, email: cred.email.clone() })
}

// ---------------------------------------------------------------------------
// Account identity (email) from roles endpoint
// ---------------------------------------------------------------------------

/// Fetch the account email from the Anthropic roles endpoint.
/// Returns `None` on any error (non-fatal).
pub async fn fetch_account_email(access_token: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;
    let resp = client
        .get("https://api.anthropic.com/api/oauth/claude_cli/roles")
        .header("authorization", format!("Bearer {access_token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-dangerous-direct-browser-access", "true")
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    // organization_name is "email's Organization" — extract email prefix
    let org = body["organization_name"].as_str()?;
    if let Some(email) = org.strip_suffix("'s Organization") {
        Some(email.to_owned())
    } else {
        Some(org.to_owned())
    }
}

// ---------------------------------------------------------------------------
// PKCE browser OAuth flow (for adding additional accounts)
// ---------------------------------------------------------------------------

struct Pkce {
    verifier: String,
    challenge: String,
}

fn generate_pkce() -> Pkce {
    let verifier_bytes: [u8; 32] = rand_bytes();
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hash);

    Pkce { verifier, challenge }
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // Simple random bytes — not crypto-grade but fine for PKCE verifier.
    // The verifier doesn't need to be secret from a client-side tool perspective.
    let mut bytes = [0u8; N];
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let pid = std::process::id();
    for (i, b) in bytes.iter_mut().enumerate() {
        let mut h = DefaultHasher::new();
        (seed, pid, i).hash(&mut h);
        *b = (h.finish() & 0xff) as u8;
    }
    bytes
}

fn random_state() -> String {
    let bytes: [u8; 16] = rand_bytes();
    hex::encode(bytes)
}

pub const OAUTH_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

/// Run the PKCE OAuth flow using the registered redirect URI.
///
/// Opens the browser to claude.ai. After the user authorizes, the callback page
/// displays a code (format: CODE#STATE). The user pastes it here; we split out
/// the state and exchange the code at the token endpoint.
pub async fn run_oauth_flow() -> Result<OAuthCredential> {
    use std::io::{self, Write};

    let pkce = generate_pkce();
    let state = random_state();
    let redirect_uri = OAUTH_REDIRECT_URI;

    let scope = urlencoding::encode(
        "user:inference user:profile user:file_upload user:mcp_servers user:sessions:claude_code",
    );
    let auth_url = format!(
        "{base}?response_type=code\
         &client_id={client_id}\
         &redirect_uri={redirect}\
         &scope={scope}\
         &state={state}\
         &code_challenge={challenge}\
         &code_challenge_method=S256",
        base = OAUTH_AUTHORIZE_URL,
        client_id = OAUTH_CLIENT_ID,
        redirect = urlencoding::encode(redirect_uri),
        scope = scope,
        state = state,
        challenge = pkce.challenge,
    );

    println!("\nOpening browser for claude.ai login...");
    println!("If it does not open automatically, visit:\n  {auth_url}\n");
    open_browser(&auth_url);

    println!("After you authorize, the page will show an authorization code.");
    println!("Copy it and paste it here.");
    println!();
    print!("Paste code: ");
    io::stdout().flush()?;

    let mut pasted = String::new();
    io::stdin().read_line(&mut pasted)?;
    // Page shows "code#state"
    let pasted = pasted.trim();
    let (code, pasted_state) = if let Some((c, s)) = pasted.split_once('#') {
        (c.trim(), s.trim())
    } else {
        (pasted, state.as_str())
    };

    if code.is_empty() {
        bail!("No code entered.");
    }

    let cred = exchange_code(code, pasted_state, redirect_uri, &pkce.verifier).await?;
    Ok(cred)
}

async fn exchange_code(code: &str, state: &str, redirect_uri: &str, verifier: &str) -> Result<OAuthCredential> {
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "state": state,
        "redirect_uri": redirect_uri,
        "client_id": OAUTH_CLIENT_ID,
        "code_verifier": verifier,
    });

    let resp = client
        .post(OAUTH_TOKEN_URL)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("token exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("token exchange failed ({status}): {body}");
    }

    let body: serde_json::Value = resp.json().await.context("token exchange: invalid JSON")?;

    let access_token = body["access_token"]
        .as_str()
        .context("token exchange: missing access_token")?
        .to_owned();
    let refresh_token = body["refresh_token"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Ok(OAuthCredential {
        access_token,
        refresh_token,
        expires_at: now_ms + expires_in * 1000,
        email: None,
    })
}

// ---------------------------------------------------------------------------
// Token revocation
// ---------------------------------------------------------------------------

pub const OAUTH_REVOKE_URL: &str = "https://platform.claude.com/v1/oauth/revoke";

/// Revoke an OAuth token on the server. Best-effort — errors are non-fatal.
pub async fn revoke_token(access_token: &str) -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_default();
    client
        .post(OAUTH_REVOKE_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("anthropic-version", "2023-06-01")
        .body(format!("token={}", urlencoding::encode(access_token)))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    { std::process::Command::new("open").arg(url).spawn().ok(); }

    #[cfg(target_os = "linux")]
    { std::process::Command::new("xdg-open").arg(url).spawn().ok(); }

    #[cfg(target_os = "windows")]
    { std::process::Command::new("cmd").args(["/c", "start", url]).spawn().ok(); }
}
