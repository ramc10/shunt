//! Credential bundle encryption and relay upload/download for `shunt push` / `shunt login`.
//!
//! Security model:
//! - Transfer code = 9 random bytes encoded as 18 hex chars, prefixed with "SH-"
//! - Encryption key = SHA-256(code) — 32 bytes, never sent to the relay
//! - Cipher: AES-256-GCM with a random 12-byte nonce
//! - Wire payload = base64(nonce_12B ‖ ciphertext_with_tag)
//! - Relay stores only ciphertext; bundle is deleted after first download

use std::collections::HashMap;

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::oauth::OAuthCredential;

// ---------------------------------------------------------------------------
// Bundle
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncBundle {
    pub config_toml: String,
    pub accounts: HashMap<String, OAuthCredential>,
}

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

/// Generate a random transfer code like `SH-a3f2b1c4d5e6f7a8b9`.
pub fn generate_code() -> String {
    let bytes = crate::oauth::rand_bytes::<9>();
    format!("SH-{}", hex::encode(bytes))
}

/// Validate that a code looks like what we generated.
pub fn validate_code(code: &str) -> Result<()> {
    if !code.starts_with("SH-") || code.len() != 21 {
        bail!("Invalid transfer code format. Expected SH-<18 hex chars> (e.g. SH-a3f2b1c4d5e6f7a8b9).");
    }
    if !code[3..].chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("Invalid transfer code — must be hex characters after 'SH-'.");
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

/// Encrypt a `SyncBundle` and return a base64-encoded payload string.
pub fn encrypt_bundle(bundle: &SyncBundle, code: &str) -> Result<String> {
    let json = serde_json::to_vec(bundle).context("failed to serialize bundle")?;

    let key_bytes = derive_key(code);
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);

    let nonce_bytes = crate::oauth::rand_bytes::<12>();
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, json.as_slice())
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

    // wire: nonce(12) ‖ ciphertext
    let mut wire = Vec::with_capacity(12 + ciphertext.len());
    wire.extend_from_slice(&nonce_bytes);
    wire.extend_from_slice(&ciphertext);

    Ok(B64.encode(wire))
}

/// Decrypt a base64-encoded payload into a `SyncBundle`.
pub fn decrypt_bundle(payload_b64: &str, code: &str) -> Result<SyncBundle> {
    let wire = B64
        .decode(payload_b64)
        .context("invalid base64 in payload")?;

    if wire.len() < 12 {
        bail!("payload too short");
    }

    let (nonce_bytes, ciphertext) = wire.split_at(12);

    let key_bytes = derive_key(code);
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong code or corrupted payload"))?;

    serde_json::from_slice::<SyncBundle>(&plaintext).context("failed to deserialize bundle")
}

// ---------------------------------------------------------------------------
// Relay HTTP
// ---------------------------------------------------------------------------

/// Upload an encrypted payload to the relay under the given code.
pub async fn push_to_relay(code: &str, payload: &str, relay_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let body = serde_json::json!({ "code": code, "payload": payload });

    let resp = client
        .post(format!("{relay_url}/bundle"))
        .json(&body)
        .send()
        .await
        .context("failed to reach relay")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("relay returned {status}: {text}");
    }

    Ok(())
}

/// Download and delete the encrypted payload for the given code from the relay.
/// Returns the base64 payload string.
pub async fn pull_from_relay(code: &str, relay_url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let resp = client
        .get(format!("{relay_url}/bundle/{code}"))
        .send()
        .await
        .context("failed to reach relay")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("Code not found or already used. Codes are one-time use — run `shunt push` again to get a new one.");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("relay returned {status}: {text}");
    }

    let json: serde_json::Value = resp.json().await.context("invalid response from relay")?;
    json["payload"]
        .as_str()
        .map(|s| s.to_owned())
        .context("relay response missing 'payload' field")
}
