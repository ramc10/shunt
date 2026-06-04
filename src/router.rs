/// Account selection: stickiness + configurable routing strategy + failover.
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::cmp::Ordering;

use crate::config::{AccountConfig, RoutingStrategy};
use crate::state::{AccountRoutingData, RoutingSnapshot, StateStore};


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
fn most_urgent_window(d: &AccountRoutingData) -> (f64, Option<u64>) {
    let effective = d.util_5h.max(d.util_7d);
    let binding_reset = if d.util_5h >= d.util_7d { d.reset_5h_secs } else { d.reset_7d_secs };
    (effective, binding_reset)
}

/// Compute the Maximus composite score for an account.
///
/// For each window:
///   time_fraction = secs_to_reset / window_duration  (0.0 = resetting now, 1.0 = just started)
///   health        = 1.0 - time_fraction × utilization
///
/// score = health_5h × health_7d
///
/// This rewards accounts where:
///   - quota is mostly unused (low utilization)
///   - any depleted window is about to refresh (low time_fraction)
///
/// Fresh accounts with no rate-limit data score 1.0 (best possible).
fn maximus_score(d: &AccountRoutingData, now_secs: u64) -> f64 {
    const WINDOW_5H_SECS: f64 = 5.0 * 3600.0;
    const WINDOW_7D_SECS: f64 = 7.0 * 24.0 * 3600.0;

    let time_frac_5h = d.reset_5h_secs
        .map(|r| (r.saturating_sub(now_secs) as f64 / WINDOW_5H_SECS).min(1.0))
        .unwrap_or(0.0);

    let time_frac_7d = d.reset_7d_secs
        .map(|r| (r.saturating_sub(now_secs) as f64 / WINDOW_7D_SECS).min(1.0))
        .unwrap_or(0.0);

    let health_5h = 1.0 - time_frac_5h * d.util_5h;
    let health_7d = 1.0 - time_frac_7d * d.util_7d;

    health_5h * health_7d
}

/// Helper: check if an account is a viable candidate from the snapshot.
fn is_candidate(name: &str, snap: &RoutingSnapshot, tried: &HashSet<String>) -> bool {
    if tried.contains(name) { return false; }
    match snap.accounts.get(name) {
        Some(d) => d.available && !d.exhausted && !d.health_check_failed,
        None => true, // no state data = fresh account, available
    }
}

/// Get routing data for an account, falling back to defaults.
fn get_data(name: &str, snap: &RoutingSnapshot) -> AccountRoutingData {
    snap.accounts.get(name).cloned().unwrap_or(AccountRoutingData {
        available: true,
        health_check_failed: false,
        exhausted: false,
        cooldown_until_ms: 0,
        util_5h: 0.0,
        util_7d: 0.0,
        reset_5h_secs: None,
        reset_7d_secs: None,
    })
}

// ---------------------------------------------------------------------------
// Account selection
// ---------------------------------------------------------------------------

/// Pick the best account for this request.
///
/// 1. If a pinned account is set and available, use it.
/// 2. If the conversation fingerprint maps to a sticky account that is still
///    available and not exhausted (and not in `tried`), use it.
/// 3. Otherwise apply `strategy` to pick from all available, non-exhausted candidates
///    not already in `tried`, and record the result as sticky.
///
/// Returns `None` when every account is on cooldown, disabled, exhausted, or in `tried`.
///
/// Takes a `RoutingSnapshot` (single mutex lock) instead of calling back into the
/// StateStore for each account — reduces lock contention on the hot path.
pub fn pick_account<'a>(
    accounts: &'a [AccountConfig],
    state: &StateStore,
    snap: &RoutingSnapshot,
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
                let d = get_data(&acc.name, snap);
                if d.available {
                    return Some(acc);
                }
            }
        }
    }

    // Try sticky account first
    if let Some(fp) = fp {
        if let Some(sticky_name) = state.get_sticky(fp) {
            if let Some(acc) = accounts.iter().find(|a| a.name == sticky_name) {
                if is_candidate(&acc.name, snap, tried) {
                    return Some(acc);
                }
            }
        }
    }

    // Gather candidates: available, not exhausted, not health-check-failed, not tried.
    let candidates: Vec<&AccountConfig> = accounts
        .iter()
        .filter(|a| is_candidate(&a.name, snap, tried))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    let chosen = match strategy {
        // ── Carousel: rotate through accounts in index order ────────────────
        RoutingStrategy::Carousel => {
            let idx = state.next_rr_index() % candidates.len();
            candidates[idx]
        }

        // ── Cushion: most remaining quota (binding window primary, secondary tiebreak) ──
        RoutingStrategy::Cushion => {
            candidates.iter().copied().min_by(|a, b| {
                let da = get_data(&a.name, snap);
                let db = get_data(&b.name, snap);

                let a_binding   = da.util_5h.max(da.util_7d);
                let b_binding   = db.util_5h.max(db.util_7d);
                let a_secondary = da.util_5h.min(da.util_7d);
                let b_secondary = db.util_5h.min(db.util_7d);

                a_binding.partial_cmp(&b_binding)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| a_secondary.partial_cmp(&b_secondary).unwrap_or(Ordering::Equal))
            })?
        }

        // ── Maximus: time-weighted dual-window scorer ────────────────────────
        RoutingStrategy::Maximus => {
            candidates.iter().copied().max_by(|a, b| {
                let da = get_data(&a.name, snap);
                let db = get_data(&b.name, snap);
                let sa = maximus_score(&da, snap.now_secs);
                let sb = maximus_score(&db, snap.now_secs);
                sa.partial_cmp(&sb).unwrap_or(Ordering::Equal)
            })?
        }

        // ── Reaper: use-it-or-lose-it; drain expiring windows first ─────────
        RoutingStrategy::Reaper => {
            candidates.iter().copied().min_by(|a, b| {
                let da = get_data(&a.name, snap);
                let db = get_data(&b.name, snap);
                let (ua, ra) = most_urgent_window(&da);
                let (ub, rb) = most_urgent_window(&db);

                let a_expiring = ra.map(|r| r.saturating_sub(snap.now_secs) <= expiry_soon_secs).unwrap_or(false);
                let b_expiring = rb.map(|r| r.saturating_sub(snap.now_secs) <= expiry_soon_secs).unwrap_or(false);

                match (a_expiring, b_expiring) {
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (true, true)  => ra.cmp(&rb),
                    (false, false) => match (ra, rb) {
                        (None, None)         => Ordering::Equal,
                        (Some(_), None)      => Ordering::Less,
                        (None, Some(_))      => Ordering::Greater,
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

    #[test]
    fn fingerprint_none_for_empty_body() {
        assert!(fingerprint(b"{}").is_none());
        assert!(fingerprint(b"not json").is_none());
    }

    #[test]
    fn fingerprint_stable_for_same_content() {
        let body = br#"{"system":"You are helpful","messages":[{"role":"user","content":"hello"}]}"#;
        let fp1 = fingerprint(body).unwrap();
        let fp2 = fingerprint(body).unwrap();
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn fingerprint_different_for_different_content() {
        let body1 = br#"{"system":"a","messages":[{"role":"user","content":"x"}]}"#;
        let body2 = br#"{"system":"b","messages":[{"role":"user","content":"x"}]}"#;
        assert_ne!(fingerprint(body1), fingerprint(body2));
    }

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
            utilization_7d: None, reset_7d: None, status_7d: None,
            overage_status: None, overage_disabled_reason: None,
            representative_claim: None, updated_ms: now * 1000,
        });
    }

    fn set_both_windows(state: &StateStore, name: &str,
        util_5h: f64, reset_5h_offset: u64,
        util_7d: f64, reset_7d_offset: u64)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        state.update_rate_limits(name, RateLimitInfo {
            utilization_5h: Some(util_5h),
            reset_5h: Some(now + reset_5h_offset),
            status_5h: Some("allowed".to_owned()),
            utilization_7d: Some(util_7d),
            reset_7d: Some(now + reset_7d_offset),
            status_7d: Some("allowed".to_owned()),
            overage_status: None, overage_disabled_reason: None,
            representative_claim: None, updated_ms: now * 1000,
        });
    }

    fn pick(accounts: &[AccountConfig], state: &StateStore, strategy: RoutingStrategy) -> Option<String> {
        let snap = state.routing_snapshot();
        pick_account(accounts, state, &snap, None, &HashSet::new(), 600_000, 1800, strategy)
            .map(|a| a.name.clone())
    }

    // ── Reaper tests ─────────────────────────────────────────────────────────

    #[test]
    fn reaper_prefers_expiring_soon() {
        let accounts = vec![make_account("fresh"), make_account("expiring")];
        let state = StateStore::new_empty();
        set_rate_limits(&state, "fresh", 0.05, 4 * 3600);
        set_rate_limits(&state, "expiring", 0.3, 15 * 60);
        assert_eq!(pick(&accounts, &state, RoutingStrategy::Reaper).as_deref(), Some("expiring"),
            "reaper: should prefer the account expiring soon");
    }

    #[test]
    fn reaper_prefers_earlier_reset_on_equal_util() {
        let accounts = vec![make_account("later"), make_account("sooner")];
        let state = StateStore::new_empty();
        set_rate_limits(&state, "later", 0.5, 5 * 3600);
        set_rate_limits(&state, "sooner", 0.5, 2 * 3600);
        assert_eq!(pick(&accounts, &state, RoutingStrategy::Reaper).as_deref(), Some("sooner"),
            "reaper: equal util → prefer sooner reset");
    }

    #[test]
    fn reaper_prefers_more_remaining_on_equal_reset() {
        let accounts = vec![make_account("high"), make_account("low")];
        let state = StateStore::new_empty();
        set_rate_limits(&state, "high", 0.8, 3 * 3600);
        set_rate_limits(&state, "low",  0.2, 3 * 3600);
        assert_eq!(pick(&accounts, &state, RoutingStrategy::Reaper).as_deref(), Some("low"),
            "reaper: same reset → prefer lowest utilization (most remaining)");
    }

    // ── Shared availability tests ─────────────────────────────────────────────

    #[test]
    fn all_strategies_skip_exhausted() {
        for strategy in [RoutingStrategy::Reaper, RoutingStrategy::Cushion, RoutingStrategy::Maximus] {
            let accounts = vec![make_account("exhausted"), make_account("fresh")];
            let state = StateStore::new_empty();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
            state.update_rate_limits("exhausted", RateLimitInfo {
                utilization_5h: Some(1.0),
                reset_5h: Some(now + 3600),
                status_5h: Some("exhausted".to_owned()),
                utilization_7d: None, reset_7d: None, status_7d: None,
                overage_status: None, overage_disabled_reason: None,
                representative_claim: None, updated_ms: now * 1000,
            });
            assert_eq!(pick(&accounts, &state, strategy).as_deref(), Some("fresh"),
                "{strategy:?}: should skip exhausted account");
        }
    }

    #[test]
    fn all_strategies_skip_cooldown() {
        for strategy in [RoutingStrategy::Reaper, RoutingStrategy::Carousel,
                         RoutingStrategy::Cushion, RoutingStrategy::Maximus] {
            let accounts = vec![make_account("cooling"), make_account("ready")];
            let state = StateStore::new_empty();
            state.set_cooldown("cooling", 60_000);
            assert_eq!(pick(&accounts, &state, strategy).as_deref(), Some("ready"),
                "{strategy:?}: should skip accounts on cooldown");
        }
    }

    #[test]
    fn pinned_account_beats_all_strategies() {
        for strategy in [RoutingStrategy::Reaper, RoutingStrategy::Carousel,
                         RoutingStrategy::Cushion, RoutingStrategy::Maximus] {
            let accounts = vec![make_account("a"), make_account("b")];
            let state = StateStore::new_empty();
            set_rate_limits(&state, "a", 0.9, 3600);
            set_rate_limits(&state, "b", 0.1, 3600);
            state.set_pinned(Some("b".to_owned()));
            assert_eq!(pick(&accounts, &state, strategy).as_deref(), Some("b"),
                "{strategy:?}: pinned account should always win");
            state.set_pinned(None);
        }
    }

    // ── Carousel tests ────────────────────────────────────────────────────────

    #[test]
    fn carousel_cycles_in_order() {
        let accounts = vec![make_account("a"), make_account("b"), make_account("c")];
        let state = StateStore::new_empty();
        let picks: Vec<_> = (0..6)
            .map(|_| pick(&accounts, &state, RoutingStrategy::Carousel))
            .collect();
        assert_eq!(picks[0].as_deref(), Some("a"));
        assert_eq!(picks[1].as_deref(), Some("b"));
        assert_eq!(picks[2].as_deref(), Some("c"));
        assert_eq!(picks[3].as_deref(), Some("a"));
    }

    // ── Cushion tests ─────────────────────────────────────────────────────────

    #[test]
    fn cushion_picks_lowest_binding() {
        let accounts = vec![make_account("heavy"), make_account("light")];
        let state = StateStore::new_empty();
        set_rate_limits(&state, "heavy", 0.8, 3600);
        set_rate_limits(&state, "light", 0.1, 3600);
        assert_eq!(pick(&accounts, &state, RoutingStrategy::Cushion).as_deref(), Some("light"),
            "cushion: should pick the lowest binding-window utilization");
    }

    #[test]
    fn cushion_tiebreaks_on_secondary_window() {
        let accounts = vec![make_account("worse"), make_account("better")];
        let state = StateStore::new_empty();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        state.update_rate_limits("worse", RateLimitInfo {
            utilization_5h: Some(0.5), reset_5h: Some(now + 3600),
            status_5h: Some("allowed".to_owned()),
            utilization_7d: Some(0.6), reset_7d: Some(now + 86400),
            status_7d: Some("allowed".to_owned()),
            overage_status: None, overage_disabled_reason: None,
            representative_claim: None, updated_ms: now * 1000,
        });
        state.update_rate_limits("better", RateLimitInfo {
            utilization_5h: Some(0.5), reset_5h: Some(now + 3600),
            status_5h: Some("allowed".to_owned()),
            utilization_7d: Some(0.2), reset_7d: Some(now + 86400),
            status_7d: Some("allowed".to_owned()),
            overage_status: None, overage_disabled_reason: None,
            representative_claim: None, updated_ms: now * 1000,
        });
        assert_eq!(pick(&accounts, &state, RoutingStrategy::Cushion).as_deref(), Some("better"),
            "cushion: tied binding → prefer lower secondary window");
    }

    // ── Maximus tests ─────────────────────────────────────────────────────────

    #[test]
    fn maximus_prefers_imminent_reset_over_raw_utilization() {
        let accounts = vec![make_account("draining"), make_account("almost_reset")];
        let state = StateStore::new_empty();
        set_both_windows(&state, "draining",     0.6, 3 * 3600, 0.3, 4 * 24 * 3600);
        set_both_windows(&state, "almost_reset", 0.9, 10 * 60,  0.2, 3 * 24 * 3600);
        assert_eq!(pick(&accounts, &state, RoutingStrategy::Maximus).as_deref(), Some("almost_reset"),
            "maximus: nearly-reset window should outscore higher raw remaining");
    }

    #[test]
    fn maximus_picks_fresh_over_depleted() {
        let accounts = vec![make_account("depleted"), make_account("fresh")];
        let state = StateStore::new_empty();
        set_both_windows(&state, "depleted", 0.8, 3 * 3600, 0.7, 5 * 24 * 3600);
        assert_eq!(pick(&accounts, &state, RoutingStrategy::Maximus).as_deref(), Some("fresh"),
            "maximus: fresh account should beat heavily utilised one");
    }

    // ── Health-check-failed tests ───────────────────────────────────────────

    #[test]
    fn all_strategies_skip_health_check_failed() {
        for strategy in [RoutingStrategy::Reaper, RoutingStrategy::Carousel,
                         RoutingStrategy::Cushion, RoutingStrategy::Maximus] {
            let accounts = vec![make_account("unhealthy"), make_account("healthy")];
            let state = StateStore::new_empty();
            state.set_health_check_failed("unhealthy");
            assert_eq!(pick(&accounts, &state, strategy).as_deref(), Some("healthy"),
                "{strategy:?}: should skip health-check-failed account");
        }
    }

    #[test]
    fn health_check_failed_cleared_allows_routing() {
        let accounts = vec![make_account("acc")];
        let state = StateStore::new_empty();
        state.set_health_check_failed("acc");
        assert!(pick(&accounts, &state, RoutingStrategy::Maximus).is_none(),
            "should not route to unhealthy account");
        state.clear_health_check_failed("acc");
        assert_eq!(pick(&accounts, &state, RoutingStrategy::Maximus).as_deref(), Some("acc"),
            "should route after health check clears");
    }

    #[test]
    fn sticky_skips_health_check_failed() {
        let accounts = vec![make_account("sticky_acc"), make_account("fallback")];
        let state = StateStore::new_empty();
        state.set_sticky("fp1", "sticky_acc", 600_000);
        state.set_health_check_failed("sticky_acc");
        let snap = state.routing_snapshot();
        let result = pick_account(
            &accounts, &state, &snap, Some("fp1"), &HashSet::new(),
            600_000, 1800, RoutingStrategy::Maximus,
        );
        assert_eq!(result.map(|a| a.name.as_str()), Some("fallback"),
            "sticky account should be skipped when health-check-failed, fallback used instead");
    }

    #[test]
    fn all_unhealthy_returns_none() {
        let accounts = vec![make_account("a"), make_account("b")];
        let state = StateStore::new_empty();
        state.set_health_check_failed("a");
        state.set_health_check_failed("b");
        assert!(pick(&accounts, &state, RoutingStrategy::Maximus).is_none(),
            "should return None when all accounts are health-check-failed");
    }
}
