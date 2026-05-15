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

// ---------------------------------------------------------------------------
// Anthropic OAuth constants
// ---------------------------------------------------------------------------

pub const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const OAUTH_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

// ---------------------------------------------------------------------------
// OpenAI / Codex OAuth constants
// ---------------------------------------------------------------------------

pub const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/authorize";
pub const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const OPENAI_OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

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

// ---------------------------------------------------------------------------
// Auto-import from Codex CLI's credential file (~/.codex/auth.json)
// ---------------------------------------------------------------------------

/// Raw format used by ~/.codex/auth.json
/// The tokens are nested under a "tokens" key; there is no top-level expires_at.
/// Expiry is read from the JWT `exp` claim inside the access_token.
#[derive(Deserialize)]
struct CodexAuth {
    tokens: CodexTokens,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

pub fn codex_credentials_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("auth.json")
}

/// Read the OAuth credential from the Codex CLI's stored auth file.
pub fn read_codex_credentials() -> Option<OAuthCredential> {
    let text = std::fs::read_to_string(codex_credentials_path()).ok()?;
    let raw: CodexAuth = serde_json::from_str(&text).ok()?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Extract exp from the JWT payload without verifying signature.
    let expires_at = jwt_exp_ms(&raw.tokens.access_token)
        .unwrap_or(now_ms + 3600 * 1000); // default: 1 hour from now

    Some(OAuthCredential {
        access_token: raw.tokens.access_token,
        refresh_token: raw.tokens.refresh_token.unwrap_or_default(),
        expires_at,
        email: None,
    })
}

/// Decode the `exp` claim from a JWT payload (no signature verification).
/// Returns expiry as Unix milliseconds.
fn jwt_exp_ms(token: &str) -> Option<u64> {
    let payload_b64 = token.splitn(3, '.').nth(1)?;
    // base64url decode (no padding)
    let decoded = base64_url_decode(payload_b64)?;
    let v: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let exp_secs = v.get("exp")?.as_u64()?;
    Some(exp_secs * 1000)
}

/// Minimal base64url decoder (no padding, URL-safe alphabet).
fn base64_url_decode(s: &str) -> Option<Vec<u8>> {
    // Convert base64url to standard base64 with padding
    let mut standard = s.replace('-', "+").replace('_', "/");
    match standard.len() % 4 {
        2 => standard.push_str("=="),
        3 => standard.push('='),
        _ => {}
    }
    use std::io::Read;
    // Use the standard library's base64 via a simple approach
    // Rust std doesn't have base64, implement a small decoder
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [0u8; 256];
    for (i, &c) in alphabet.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let bytes = standard.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut i = 0;
    while i + 3 < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes[i + 1];
        let b2 = bytes[i + 2];
        let b3 = bytes[i + 3];
        if b0 == b'=' { break; }
        let n0 = table[b0 as usize] as u32;
        let n1 = table[b1 as usize] as u32;
        let n2 = if b2 == b'=' { 0 } else { table[b2 as usize] as u32 };
        let n3 = if b3 == b'=' { 0 } else { table[b3 as usize] as u32 };
        let val = (n0 << 18) | (n1 << 12) | (n2 << 6) | n3;
        out.push(((val >> 16) & 0xFF) as u8);
        if b2 != b'=' { out.push(((val >> 8) & 0xFF) as u8); }
        if b3 != b'=' { out.push((val & 0xFF) as u8); }
        i += 4;
    }
    let _ = Read::read(&mut out.as_slice(), &mut []); // suppress unused import warning
    Some(out)
}


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

/// Generate N cryptographically random bytes using the OS entropy source.
/// Panics if the system RNG is unavailable (unrecoverable error in a security context).
pub fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::getrandom(&mut bytes)
        .expect("OS random number generator unavailable — cannot generate secure random bytes");
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

// ---------------------------------------------------------------------------
// OpenAI token refresh
// ---------------------------------------------------------------------------

/// Refresh an expired OpenAI / Codex access token using the stored refresh_token.
pub async fn refresh_openai_token(cred: &OAuthCredential) -> Result<OAuthCredential> {
    let client = reqwest::Client::new();

    let resp = client
        .post(OPENAI_OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            urlencoding::encode(&cred.refresh_token),
            OPENAI_OAUTH_CLIENT_ID,
        ))
        .send()
        .await
        .context("OpenAI token refresh request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("OpenAI token refresh failed ({status}): {body}");
    }

    let body: serde_json::Value = resp.json().await.context("OpenAI token refresh: invalid JSON")?;

    let access_token = body["access_token"]
        .as_str()
        .context("OpenAI token refresh: missing access_token")?
        .to_owned();

    let refresh_token = body["refresh_token"]
        .as_str()
        .unwrap_or(&cred.refresh_token)
        .to_owned();

    let expires_in_secs = body["expires_in"].as_u64().unwrap_or(3600);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Ok(OAuthCredential {
        access_token,
        refresh_token,
        expires_at: now_ms + expires_in_secs * 1000,
        email: cred.email.clone(),
    })
}

// ---------------------------------------------------------------------------
// OpenAI PKCE browser OAuth flow (for adding additional Codex accounts)
// ---------------------------------------------------------------------------

/// Run the PKCE OAuth flow for OpenAI / Codex.
///
/// Opens the browser to OpenAI's auth page. A local HTTP server on port 1455
/// captures the redirect callback automatically — no manual code paste needed.
pub async fn run_openai_oauth_flow() -> Result<OAuthCredential> {
    let pkce = generate_pkce();
    let state = random_state();

    let scope = urlencoding::encode("openid email profile offline_access");
    let auth_url = format!(
        "{base}?response_type=code\
         &client_id={client_id}\
         &redirect_uri={redirect}\
         &scope={scope}\
         &state={state}\
         &code_challenge={challenge}\
         &code_challenge_method=S256\
         &id_token_add_organizations=true\
         &codex_cli_simplified_flow=true",
        base = OPENAI_OAUTH_AUTHORIZE_URL,
        client_id = OPENAI_OAUTH_CLIENT_ID,
        redirect = urlencoding::encode(OPENAI_OAUTH_REDIRECT_URI),
        scope = scope,
        state = state,
        challenge = pkce.challenge,
    );

    println!("\nOpening browser for OpenAI / Codex login...");
    println!("If it does not open automatically, visit:\n  {auth_url}\n");
    open_browser(&auth_url);

    println!("Waiting for authorization (listening on port 1455)...");
    let code = listen_for_oauth_callback(1455).await
        .context("failed to receive OAuth callback on localhost:1455")?;

    let cred = exchange_openai_code(&code, &state, &pkce.verifier).await?;
    Ok(cred)
}

/// Spin up a minimal HTTP server on `port`, wait for the first GET request to
/// `/auth/callback?code=...`, extract the code, and send a success page.
async fn listen_for_oauth_callback(port: u16) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .with_context(|| format!("cannot bind to 127.0.0.1:{port} for OAuth callback"))?;

    // Accept exactly one connection — the browser redirect.
    let (mut stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        listener.accept(),
    )
    .await
    .context("timed out waiting for OAuth callback (120s)")?
    .context("accept failed")?;

    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.context("callback read failed")?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse: "GET /auth/callback?code=XXXX&state=YYYY HTTP/1.1"
    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

    let code = query
        .split('&')
        .find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == "code" { Some(urlencoding::decode(v).ok()?.into_owned()) } else { None }
        })
        .context("OAuth callback did not contain a 'code' parameter")?;

    let html = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body style='font-family:sans-serif;padding:2em'>\
        <h2>Authorization successful</h2>\
        <p>You can close this tab and return to the terminal.</p>\
        </body></html>";
    stream.write_all(html.as_bytes()).await.ok();

    Ok(code)
}

async fn exchange_openai_code(code: &str, _state: &str, verifier: &str) -> Result<OAuthCredential> {
    let client = reqwest::Client::new();

    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(OPENAI_OAUTH_REDIRECT_URI),
        OPENAI_OAUTH_CLIENT_ID,
        urlencoding::encode(verifier),
    );

    let resp = client
        .post(OPENAI_OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("OpenAI code exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("OpenAI code exchange failed ({status}): {body}");
    }

    let body: serde_json::Value = resp.json().await.context("OpenAI code exchange: invalid JSON")?;

    let access_token = body["access_token"]
        .as_str()
        .context("OpenAI code exchange: missing access_token")?
        .to_owned();
    let refresh_token = body["refresh_token"].as_str().unwrap_or("").to_owned();
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
// OpenAI account identity
// ---------------------------------------------------------------------------

/// Fetch the account email from OpenAI's userinfo endpoint.
pub async fn fetch_openai_account_email(access_token: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;
    let resp = client
        .get("https://auth.openai.com/userinfo")
        .header("authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() { return None; }
    let body: serde_json::Value = resp.json().await.ok()?;
    body["email"].as_str().map(|s| s.to_owned())
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    { std::process::Command::new("open").arg(url).spawn().ok(); }

    #[cfg(target_os = "linux")]
    { std::process::Command::new("xdg-open").arg(url).spawn().ok(); }

    // Use explorer.exe directly — avoids cmd.exe shell expansion of OAuth URL
    // special characters (& % etc.) that would misparse with `cmd /c start`.
    #[cfg(target_os = "windows")]
    { std::process::Command::new("explorer").arg(url).spawn().ok(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rand_bytes_correct_length() {
        let a: [u8; 16] = rand_bytes();
        assert_eq!(a.len(), 16);
        let b: [u8; 32] = rand_bytes();
        assert_eq!(b.len(), 32);
    }

    #[test]
    fn test_rand_bytes_not_all_zeros() {
        // The probability of 32 random bytes all being zero is 1/2^256 — effectively impossible.
        let bytes: [u8; 32] = rand_bytes();
        assert!(bytes.iter().any(|&b| b != 0), "rand_bytes must not return all-zero output");
    }

    #[test]
    fn test_rand_bytes_unique() {
        // Two calls must not return the same value (probability 1/2^256 they'd collide).
        let a: [u8; 32] = rand_bytes();
        let b: [u8; 32] = rand_bytes();
        assert_ne!(a, b, "rand_bytes must return unique values each call");
    }

    #[test]
    fn test_pkce_pair_properties() {
        let pkce = generate_pkce();
        // Verifier must be base64url-safe (no padding, only URL-safe chars)
        assert!(pkce.verifier.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
            "PKCE verifier must be base64url-safe");
        // Challenge must differ from verifier (it's the SHA-256 hash)
        assert_ne!(pkce.verifier, pkce.challenge,
            "PKCE challenge must not equal verifier");
        assert!(!pkce.challenge.is_empty());
        assert!(!pkce.verifier.is_empty());
    }
}
