/// Account selection: stickiness + earliest-expiry-first scoring + failover.
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::config::AccountConfig;
use crate::state::StateStore;


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
// Account selection helpers
// ---------------------------------------------------------------------------

/// Return (effective_utilization, soonest_reset) for an account's rate-limit windows.
///
/// `effective_utilization` = max(util_5h, util_7d) — the binding constraint is whichever
/// window is more exhausted, not just whichever expires sooner.
///
/// `soonest_reset` = the earliest reset timestamp across both windows, used for the
/// "use-it-or-lose-it" expiry check (if any window is expiring soon, prefer this account).
fn most_urgent_window(
    util_5h: f64, reset_5h: Option<u64>,
    util_7d: f64, reset_7d: Option<u64>,
) -> (f64, Option<u64>) {
    let effective = util_5h.max(util_7d);
    let soonest = match (reset_5h, reset_7d) {
        (Some(r5), Some(r7)) => Some(r5.min(r7)),
        (Some(r5), None)     => Some(r5),
        (None, Some(r7))     => Some(r7),
        (None, None)         => None,
    };
    (effective, soonest)
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
    sticky_ttl_ms: u64,
    expiry_soon_secs: u64,
) -> Option<&'a AccountConfig> {
    // Pinned account overrides everything — user explicitly chose this one
    if let Some(pinned) = state.get_pinned() {
        if !tried.contains(&pinned) {
            if let Some(acc) = accounts.iter().find(|a| a.name == pinned) {
                if state.is_available(&acc.name) {
                    return Some(acc);
                }
            }
        }
        // Pinned account is unavailable or already tried — fall through to normal routing
    }

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

    // Pick the best account:
    // - "Expiring soon" (reset within 30 min, not exhausted) → use it or lose it;
    //   among those, prefer the most urgent (soonest reset).
    // - Otherwise → drain most-utilized-first within the same reset window so tokens
    //   aren't wasted; across different windows, prefer the soonest-expiring window.
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let chosen = accounts
        .iter()
        .filter(|a| !tried.contains(&a.name) && state.is_available(&a.name))
        .min_by(|a, b| {
            // Use the most-urgent (soonest-resetting) window for each account.
            // If both windows are active, the one expiring sooner is the binding constraint.
            let (ua, ra) = most_urgent_window(
                state.utilization_5h(&a.name), state.reset_5h_secs(&a.name),
                state.utilization_7d(&a.name), state.reset_7d_secs(&a.name),
            );
            let (ub, rb) = most_urgent_window(
                state.utilization_5h(&b.name), state.reset_5h_secs(&b.name),
                state.utilization_7d(&b.name), state.reset_7d_secs(&b.name),
            );

            let a_expiring = ra.map(|r| r.saturating_sub(now_secs) <= expiry_soon_secs).unwrap_or(false) && ua < 1.0;
            let b_expiring = rb.map(|r| r.saturating_sub(now_secs) <= expiry_soon_secs).unwrap_or(false) && ub < 1.0;

            match (a_expiring, b_expiring) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (true, true) => ra.cmp(&rb), // most urgent first
                (false, false) => {
                    // Drain most-utilized first (reversed: higher utilization = preferred).
                    ub.partial_cmp(&ua).unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| {
                            // Break ties by Anthropic's actual reset time: soonest expiry first.
                            ra.unwrap_or(u64::MAX).cmp(&rb.unwrap_or(u64::MAX))
                        })
                }
            }
        })?;

    tracing::debug!(account = %chosen.name, "routing request to account");

    // Record stickiness for future requests in this conversation
    if let Some(fp) = fp {
        state.set_sticky(fp, &chosen.name, sticky_ttl_ms);
    }

    Some(chosen)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{RateLimitInfo, StateStore};

    fn make_account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_owned(),
            plan_type: "pro".to_owned(),
            provider: crate::provider::Provider::Anthropic,
            credential: None,
            upstream_url: None,
        }
    }

    fn set_rate_limits(state: &StateStore, name: &str, util_5h: f64, reset_5h_offset_secs: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        state.update_rate_limits(name, RateLimitInfo {
            utilization_5h: Some(util_5h),
            reset_5h: Some(now + reset_5h_offset_secs),
            status_5h: Some("allowed".to_owned()),
            utilization_7d: None,
            reset_7d: None,
            status_7d: None,
            overage_status: None,
            overage_disabled_reason: None,
            representative_claim: None,
            updated_ms: now * 1000,
        });
    }

    #[test]
    fn test_routing_drains_high_utilization_first() {
        let accounts = vec![make_account("low"), make_account("high")];
        let state = StateStore::new_empty();

        // Account "low" at 20%, "high" at 80%, both resetting in 3 hours (not expiring soon)
        set_rate_limits(&state, "low", 0.2, 3 * 3600);
        set_rate_limits(&state, "high", 0.8, 3 * 3600);

        let chosen = pick_account(&accounts, &state, None, &HashSet::new(), 600_000, 1800);
        assert_eq!(chosen.map(|a| a.name.as_str()), Some("high"),
            "should drain the high-utilization account first");
    }

    #[test]
    fn test_routing_prefers_expiring_soon() {
        let accounts = vec![make_account("fresh"), make_account("expiring")];
        let state = StateStore::new_empty();

        // "expiring" has 30% util and resets in 15 min (within 30-min window) — use-it-or-lose-it
        // "fresh" has 5% util and resets in 4 hours
        set_rate_limits(&state, "fresh", 0.05, 4 * 3600);
        set_rate_limits(&state, "expiring", 0.3, 15 * 60);

        let chosen = pick_account(&accounts, &state, None, &HashSet::new(), 600_000, 1800);
        assert_eq!(chosen.map(|a| a.name.as_str()), Some("expiring"),
            "should prefer the account expiring soon (use-it-or-lose-it)");
    }

    #[test]
    fn test_routing_equal_utilization_prefers_earlier_reset() {
        let accounts = vec![make_account("later"), make_account("sooner")];
        let state = StateStore::new_empty();

        // Both at 50% but different reset times — prefer the one that resets sooner
        set_rate_limits(&state, "later", 0.5, 5 * 3600);
        set_rate_limits(&state, "sooner", 0.5, 2 * 3600);

        let chosen = pick_account(&accounts, &state, None, &HashSet::new(), 600_000, 1800);
        assert_eq!(chosen.map(|a| a.name.as_str()), Some("sooner"),
            "equal utilization: should prefer the account whose window resets sooner");
    }

    #[test]
    fn test_routing_skips_unavailable() {
        let accounts = vec![make_account("cooling"), make_account("ready")];
        let state = StateStore::new_empty();
        state.set_cooldown("cooling", 60_000);

        let chosen = pick_account(&accounts, &state, None, &HashSet::new(), 600_000, 1800);
        assert_eq!(chosen.map(|a| a.name.as_str()), Some("ready"),
            "should skip accounts on cooldown");
    }

    #[test]
    fn test_routing_pinned_account_wins() {
        let accounts = vec![make_account("a"), make_account("b")];
        let state = StateStore::new_empty();
        set_rate_limits(&state, "a", 0.9, 3600);
        set_rate_limits(&state, "b", 0.1, 3600);
        state.set_pinned(Some("b".to_owned()));

        let chosen = pick_account(&accounts, &state, None, &HashSet::new(), 600_000, 1800);
        assert_eq!(chosen.map(|a| a.name.as_str()), Some("b"),
            "pinned account should override utilization-based routing");
    }
}
