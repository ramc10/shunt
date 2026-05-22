//! Encryption helpers used by the `remote` command for device-to-device notification relay.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use sha2::{Digest, Sha256};
use serde_json;

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

/// Generate a random remote-watch code like `RM-a3f2b1c4d5e6f7a8b9`.
pub fn generate_remote_code() -> String {
    let bytes = crate::oauth::rand_bytes::<9>();
    format!("RM-{}", hex::encode(bytes))
}

/// Validate that a remote-watch code looks like what we generated.
pub fn validate_remote_code(code: &str) -> Result<()> {
    if !code.starts_with("RM-") || code.len() != 21 {
        anyhow::bail!("Invalid remote code format. Expected RM-<18 hex chars>.");
    }
    if !code[3..].chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("Invalid remote code — must be hex characters after 'RM-'.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Encryption / decryption
// ---------------------------------------------------------------------------

fn derive_key(code: &str) -> [u8; 32] {
    let hash = Sha256::digest(code.as_bytes());
    hash.into()
}

/// Encrypt arbitrary bytes with the given code; returns a base64 payload string.
pub fn encrypt_bytes(data: &[u8], code: &str) -> Result<String> {
    let key_bytes = derive_key(code);
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce_bytes = crate::oauth::rand_bytes::<12>();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, data)
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;
    let mut wire = Vec::with_capacity(12 + ciphertext.len());
    wire.extend_from_slice(&nonce_bytes);
    wire.extend_from_slice(&ciphertext);
    Ok(B64.encode(wire))
}

// ---------------------------------------------------------------------------
// Share code helpers (SC- prefix — one-time relay handshake for shunt connect)
// ---------------------------------------------------------------------------

/// Generate a random share code like `SC-a3f2b1c4d5e6f7a8b9`.
pub fn generate_share_code() -> String {
    let bytes = crate::oauth::rand_bytes::<9>();
    format!("SC-{}", hex::encode(bytes))
}

/// Validate that a share code has the expected format.
pub fn validate_share_code(code: &str) -> Result<()> {
    if !code.starts_with("SC-") || code.len() != 21 {
        anyhow::bail!("Invalid share code format. Expected SC-<18 hex chars>.");
    }
    if !code[3..].chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("Invalid share code — must be hex characters after 'SC-'.");
    }
    Ok(())
}

/// Push {base_url, api_key} to the relay under `code`. TTL 10 minutes, one-time read.
pub async fn push_share(code: &str, base_url: &str, api_key: &str, relay_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{relay_url}/share/{code}");
    let res = client
        .put(&url)
        .json(&serde_json::json!({ "base_url": base_url, "api_key": api_key }))
        .send()
        .await
        .context("Failed to reach relay")?;
    if !res.status().is_success() {
        let body = res.text().await.unwrap_or_default();
        anyhow::bail!("Relay rejected share push ({}): {}", url, body);
    }
    Ok(())
}

/// Pull {base_url, api_key} from the relay for `code`. Deletes the entry on success.
pub async fn pull_share(code: &str, relay_url: &str) -> Result<(String, String)> {
    let client = reqwest::Client::new();
    let url = format!("{relay_url}/share/{code}");
    let res = client
        .get(&url)
        .send()
        .await
        .context("Failed to reach relay")?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("Share code not found, expired, or already used. Ask the host to run `shunt share` again.");
    }
    if !res.status().is_success() {
        let body = res.text().await.unwrap_or_default();
        anyhow::bail!("Relay error: {body}");
    }
    let json: serde_json::Value = res.json().await.context("Invalid JSON from relay")?;
    let base_url = json["base_url"].as_str().context("Missing base_url")?.to_owned();
    let api_key = json["api_key"].as_str().context("Missing api_key")?.to_owned();
    Ok((base_url, api_key))
}

/// Decrypt a base64 payload into bytes using the given code.
pub fn decrypt_bytes(payload_b64: &str, code: &str) -> Result<Vec<u8>> {
    let wire = B64.decode(payload_b64).context("invalid base64 in payload")?;
    if wire.len() < 12 {
        anyhow::bail!("payload too short");
    }
    let (nonce_bytes, ciphertext) = wire.split_at(12);
    let key_bytes = derive_key(code);
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong code or corrupted payload"))
}
