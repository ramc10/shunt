/// Account selection: stickiness + earliest-expiry-first scoring + failover.
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::config::AccountConfig;
use crate::state::StateStore;

const STICKY_TTL_MS: u64 = 10 * 60 * 1000; // 10 minutes

// ---------------------------------------------------------------------------
// Fingerprinting
// ---------------------------------------------------------------------------

/// Compute a stable conversation fingerprint from the raw request body.
///
/// Fingerprint = SHA-256( system_text \0 first_user_text \0 tools_json )
///
/// Returns None if the body is not JSON or carries no identifying content.
pub fn fingerprint(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;

    let system = extract_text(&v["system"]);
    let first_user = v["messages"]
        .as_array()?
        .iter()
        .find(|m| m["role"].as_str() == Some("user"))
        .map(|m| extract_text(&m["content"]))
        .unwrap_or_default();

    if system.is_empty() && first_user.is_empty() {
        return None;
    }

    // Canonical tool list: sorted by name so insertion order doesn't matter.
    let tools_json = canonical_tools(&v["tools"]);

    let combined = format!("{system}\x00{first_user}\x00{tools_json}");
    Some(hex::encode(Sha256::digest(combined.as_bytes())))
}

fn extract_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                (b["type"].as_str() == Some("text")).then(|| b["text"].as_str().unwrap_or("").to_owned())
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn canonical_tools(v: &serde_json::Value) -> String {
    match v.as_array() {
        None => "null".into(),
        Some(arr) => {
            let mut names: Vec<_> = arr
                .iter()
                .filter_map(|t| t["name"].as_str())
                .collect();
            names.sort_unstable();
            names.join(",")
        }
    }
}

// ---------------------------------------------------------------------------
// Account selection
// ---------------------------------------------------------------------------

/// Pick the best account for this request.
///
/// 1. If the conversation fingerprint maps to a sticky account that is still
///    available (and not in `tried`), use it.
/// 2. Otherwise, pick the first available account not in `tried`, and record
///    it as sticky for this fingerprint.
///
/// Returns `None` when every account is on cooldown, disabled, or in `tried`.
pub fn pick_account<'a>(
    accounts: &'a [AccountConfig],
    state: &StateStore,
    fp: Option<&str>,
    tried: &HashSet<String>,
) -> Option<&'a AccountConfig> {
    // Try sticky account first
    if let Some(fp) = fp {
        if let Some(sticky_name) = state.get_sticky(fp) {
            if !tried.contains(&sticky_name) {
                if let Some(acc) = accounts.iter().find(|a| a.name == sticky_name) {
                    if state.is_available(&acc.name) {
                        return Some(acc);
                    }
                }
            }
        }
    }

    // Least-utilized-first: pick the account with the most headroom in its 5h window.
    // Fresh accounts (never used) have utilization 0.0 and are picked first, ensuring
    // all accounts get load. Falls back to window_start_ms ordering when utilization
    // data is identical (e.g. before any requests have been proxied).
    let chosen = accounts
        .iter()
        .filter(|a| !tried.contains(&a.name) && state.is_available(&a.name))
        .min_by(|a, b| {
            let ua = state.utilization_5h(&a.name);
            let ub = state.utilization_5h(&b.name);
            ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| state.window_start_ms(&a.name).cmp(&state.window_start_ms(&b.name)))
        })?;

    // Record stickiness for future requests in this conversation
    if let Some(fp) = fp {
        state.set_sticky(fp, &chosen.name, STICKY_TTL_MS);
    }

    Some(chosen)
}
