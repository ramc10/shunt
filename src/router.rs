/// Account selection: stickiness + configurable routing strategy + failover.
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::cmp::Ordering;

use crate::config::{AccountConfig, RoutingStrategy};
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

/// Return (effective_utilization, binding_reset) for an account's rate-limit windows.
///
/// `effective_utilization` = max(util_5h, util_7d) — the binding constraint is whichever
/// window is more exhausted.
///
/// `binding_reset` = the reset timestamp of the most-utilized (binding) window.
fn most_urgent_window(
    util_5h: f64, reset_5h: Option<u64>,
    util_7d: f64, reset_7d: Option<u64>,
) -> (f64, Option<u64>) {
    let effective = util_5h.max(util_7d);
    let binding_reset = if util_5h >= util_7d { reset_5h } else { reset_7d };
    (effective, binding_reset)
}

// ---------------------------------------------------------------------------
// Account selection
// ---------------------------------------------------------------------------

/// Pick the best account for this request.
///
/// 1. If a pinned account is set and available, use it.
/// 2. If the conversation fingerprint maps to a sticky account that is still
///    available and not exhausted (and not in `tried`), use it.
/// 3. Otherwise, apply `strategy` to pick from all available, non-exhausted accounts
///    not already in `tried`, and record the result as sticky.
///
/// Returns `None` when every account is on cooldown, disabled, exhausted, or in `tried`.
pub fn pick_account<'a>(
    accounts: &'a [AccountConfig],
    state: &StateStore,
    fp: Option<&str>,
    tried: &HashSet<String>,
    sticky_ttl_ms: u64,
    expiry_soon_secs: u64,
    strategy: RoutingStrategy,
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
                    if state.is_available(&acc.name) && !state.is_exhausted(&acc.name) {
                        return Some(acc);
                    }
                }
            }
        }
    }

    // Gather candidates: available (not on cooldown/disabled), not exhausted, not already tried.
    let candidates: Vec<&AccountConfig> = accounts
        .iter()
        .filter(|a| !tried.contains(&a.name) && state.is_available(&a.name) && !state.is_exhausted(&a.name))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    let chosen = match strategy {
        RoutingStrategy::RoundRobin => {
            let idx = state.next_rr_index() % candidates.len();
            candidates[idx]
        }

        RoutingStrategy::LeastUtilized => {
            candidates.iter().copied().min_by(|a, b| {
                // Prefer the account with the most remaining quota (lowest effective util).
                let ua = state.utilization_5h(&a.name).max(state.utilization_7d(&a.name));
                let ub = state.utilization_5h(&b.name).max(state.utilization_7d(&b.name));
                ua.partial_cmp(&ub).unwrap_or(Ordering::Equal)
            })?
        }

        RoutingStrategy::EarliestExpiry => {
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            candidates.iter().copied().min_by(|a, b| {
                let (ua, ra) = most_urgent_window(
                    state.utilization_5h(&a.name), state.reset_5h_secs(&a.name),
                    state.utilization_7d(&a.name), state.reset_7d_secs(&a.name),
                );
                let (ub, rb) = most_urgent_window(
                    state.utilization_5h(&b.name), state.reset_5h_secs(&b.name),
                    state.utilization_7d(&b.name), state.reset_7d_secs(&b.name),
                );

                // "Expiring soon": binding window resets within expiry_soon_secs.
                // Route to these first — use-it-or-lose-it.
                let a_expiring = ra.map(|r| r.saturating_sub(now_secs) <= expiry_soon_secs).unwrap_or(false);
                let b_expiring = rb.map(|r| r.saturating_sub(now_secs) <= expiry_soon_secs).unwrap_or(false);

                match (a_expiring, b_expiring) {
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    // Both expiring soon: prefer the most urgent (soonest reset) first.
                    (true, true) => ra.cmp(&rb),
                    // Neither expiring soon:
                    // - prefer accounts with a known reset time over fresh accounts
                    //   (fresh accounts have full quota; save them as backup)
                    // - among those, prefer soonest reset (tokens most at risk of being wasted)
                    // - tiebreak: prefer lowest utilization (most tokens remaining)
                    (false, false) => match (ra, rb) {
                        (None, None) => Ordering::Equal,
                        (Some(_), None) => Ordering::Less,   // known-expiry before fresh
                        (None, Some(_)) => Ordering::Greater, // fresh last
                        (Some(ra_t), Some(rb_t)) => {
                            ra_t.cmp(&rb_t)
                                .then_with(|| ua.partial_cmp(&ub).unwrap_or(Ordering::Equal))
                        }
                    },
                }
            })?
        }
    };

    tracing::debug!(account = %chosen.name, strategy = ?strategy, "routing request to account");

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
            model: None,
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

    fn pick_ee(accounts: &[AccountConfig], state: &StateStore) -> Option<String> {
        pick_account(accounts, state, None, &HashSet::new(), 600_000, 1800,
            RoutingStrategy::EarliestExpiry)
            .map(|a| a.name.clone())
    }

    #[test]
    fn test_routing_prefers_expiring_soon() {
        let accounts = vec![make_account("fresh"), make_account("expiring")];
        let state = StateStore::new_empty();

        // "expiring" has 30% util and resets in 15 min (within 30-min window) — use-it-or-lose-it
        // "fresh" has 5% util and resets in 4 hours
        set_rate_limits(&state, "fresh", 0.05, 4 * 3600);
        set_rate_limits(&state, "expiring", 0.3, 15 * 60);

        assert_eq!(pick_ee(&accounts, &state).as_deref(), Some("expiring"),
            "should prefer the account expiring soon (use-it-or-lose-it)");
    }

    #[test]
    fn test_routing_equal_utilization_prefers_earlier_reset() {
        let accounts = vec![make_account("later"), make_account("sooner")];
        let state = StateStore::new_empty();

        // Both at 50% but different reset times — prefer the one that resets sooner
        set_rate_limits(&state, "later", 0.5, 5 * 3600);
        set_rate_limits(&state, "sooner", 0.5, 2 * 3600);

        assert_eq!(pick_ee(&accounts, &state).as_deref(), Some("sooner"),
            "equal utilization: should prefer the account whose window resets sooner");
    }

    #[test]
    fn test_routing_same_reset_prefers_more_remaining() {
        let accounts = vec![make_account("high"), make_account("low")];
        let state = StateStore::new_empty();

        // Same reset time; "low" has more remaining (20% used vs 80% used).
        set_rate_limits(&state, "high", 0.8, 3 * 3600);
        set_rate_limits(&state, "low",  0.2, 3 * 3600);

        assert_eq!(pick_ee(&accounts, &state).as_deref(), Some("low"),
            "same reset time: should prefer the account with most remaining quota");
    }

    #[test]
    fn test_routing_skips_exhausted() {
        let accounts = vec![make_account("exhausted"), make_account("fresh")];
        let state = StateStore::new_empty();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        state.update_rate_limits("exhausted", RateLimitInfo {
            utilization_5h: Some(1.0),
            reset_5h: Some(now + 3600),
            status_5h: Some("exhausted".to_owned()),
            utilization_7d: None,
            reset_7d: None,
            status_7d: None,
            overage_status: None,
            overage_disabled_reason: None,
            representative_claim: None,
            updated_ms: now * 1000,
        });

        assert_eq!(pick_ee(&accounts, &state).as_deref(), Some("fresh"),
            "should skip accounts with exhausted quota");
    }

    #[test]
    fn test_routing_skips_unavailable() {
        let accounts = vec![make_account("cooling"), make_account("ready")];
        let state = StateStore::new_empty();
        state.set_cooldown("cooling", 60_000);

        assert_eq!(pick_ee(&accounts, &state).as_deref(), Some("ready"),
            "should skip accounts on cooldown");
    }

    #[test]
    fn test_routing_pinned_account_wins() {
        let accounts = vec![make_account("a"), make_account("b")];
        let state = StateStore::new_empty();
        set_rate_limits(&state, "a", 0.9, 3600);
        set_rate_limits(&state, "b", 0.1, 3600);
        state.set_pinned(Some("b".to_owned()));

        assert_eq!(
            pick_account(&accounts, &state, None, &HashSet::new(), 600_000, 1800,
                RoutingStrategy::EarliestExpiry)
                .map(|a| a.name.as_str()),
            Some("b"),
            "pinned account should override routing strategy");
    }

    #[test]
    fn test_round_robin_cycles() {
        let accounts = vec![make_account("a"), make_account("b"), make_account("c")];
        let state = StateStore::new_empty();

        let picks: Vec<_> = (0..6)
            .map(|_| pick_account(&accounts, &state, None, &HashSet::new(), 600_000, 1800,
                RoutingStrategy::RoundRobin)
                .map(|a| a.name.clone()))
            .collect();

        // Should cycle a → b → c → a → b → c
        assert_eq!(picks[0].as_deref(), Some("a"));
        assert_eq!(picks[1].as_deref(), Some("b"));
        assert_eq!(picks[2].as_deref(), Some("c"));
        assert_eq!(picks[3].as_deref(), Some("a"));
    }

    #[test]
    fn test_least_utilized_picks_freshest() {
        let accounts = vec![make_account("heavy"), make_account("light")];
        let state = StateStore::new_empty();

        set_rate_limits(&state, "heavy", 0.8, 3600);
        set_rate_limits(&state, "light", 0.1, 3600);

        assert_eq!(
            pick_account(&accounts, &state, None, &HashSet::new(), 600_000, 1800,
                RoutingStrategy::LeastUtilized)
                .map(|a| a.name.as_str()),
            Some("light"),
            "least-utilized should pick the account with the most remaining quota");
    }
}
