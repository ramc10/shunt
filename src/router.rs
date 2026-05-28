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
fn most_urgent_window(
    util_5h: f64, reset_5h: Option<u64>,
    util_7d: f64, reset_7d: Option<u64>,
) -> (f64, Option<u64>) {
    let effective = util_5h.max(util_7d);
    let binding_reset = if util_5h >= util_7d { reset_5h } else { reset_7d };
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
fn maximus_score(state: &StateStore, name: &str, now_secs: u64) -> f64 {
    const WINDOW_5H_SECS: f64 = 5.0 * 3600.0;
    const WINDOW_7D_SECS: f64 = 7.0 * 24.0 * 3600.0;

    let util_5h = state.utilization_5h(name);
    let util_7d = state.utilization_7d(name);

    // time_fraction = 0.0 means resetting imminently → no penalty even if heavily used.
    // Unknown reset (fresh account, util already 0.0) → time_frac = 0.0 → health = 1.0.
    let time_frac_5h = state.reset_5h_secs(name)
        .map(|r| (r.saturating_sub(now_secs) as f64 / WINDOW_5H_SECS).min(1.0))
        .unwrap_or(0.0);

    let time_frac_7d = state.reset_7d_secs(name)
        .map(|r| (r.saturating_sub(now_secs) as f64 / WINDOW_7D_SECS).min(1.0))
        .unwrap_or(0.0);

    let health_5h = 1.0 - time_frac_5h * util_5h;
    let health_7d = 1.0 - time_frac_7d * util_7d;

    health_5h * health_7d
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
        // ── Carousel: rotate through accounts in index order ────────────────
        RoutingStrategy::Carousel => {
            let idx = state.next_rr_index() % candidates.len();
            candidates[idx]
        }

        // ── Cushion: most remaining quota (binding window primary, secondary tiebreak) ──
        RoutingStrategy::Cushion => {
            candidates.iter().copied().min_by(|a, b| {
                let a5 = state.utilization_5h(&a.name);
                let a7 = state.utilization_7d(&a.name);
                let b5 = state.utilization_5h(&b.name);
                let b7 = state.utilization_7d(&b.name);

                let a_binding   = a5.max(a7);
                let b_binding   = b5.max(b7);
                let a_secondary = a5.min(a7);
                let b_secondary = b5.min(b7);

                a_binding.partial_cmp(&b_binding)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| a_secondary.partial_cmp(&b_secondary).unwrap_or(Ordering::Equal))
            })?
        }

        // ── Maximus: time-weighted dual-window scorer ────────────────────────
        RoutingStrategy::Maximus => {
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            candidates.iter().copied().max_by(|a, b| {
                let sa = maximus_score(state, &a.name, now_secs);
                let sb = maximus_score(state, &b.name, now_secs);
                sa.partial_cmp(&sb).unwrap_or(Ordering::Equal)
            })?
        }

        // ── Reaper: use-it-or-lose-it; drain expiring windows first ─────────
        RoutingStrategy::Reaper => {
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
                let a_expiring = ra.map(|r| r.saturating_sub(now_secs) <= expiry_soon_secs).unwrap_or(false);
                let b_expiring = rb.map(|r| r.saturating_sub(now_secs) <= expiry_soon_secs).unwrap_or(false);

                match (a_expiring, b_expiring) {
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (true, true)  => ra.cmp(&rb), // most urgent first
                    (false, false) => match (ra, rb) {
                        (None, None)         => Ordering::Equal,
                        (Some(_), None)      => Ordering::Less,   // known-expiry before fresh
                        (None, Some(_))      => Ordering::Greater, // fresh last
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
            overage_status: None,
            overage_disabled_reason: None,
            representative_claim: None,
            updated_ms: now * 1000,
        });
    }

    fn pick(accounts: &[AccountConfig], state: &StateStore, strategy: RoutingStrategy) -> Option<String> {
        pick_account(accounts, state, None, &HashSet::new(), 600_000, 1800, strategy)
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
        // Both 5h at 50%; "better" has more room in 7d.
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
        // "draining": 5h at 60%, resets in 3h; 7d at 30%, resets in 4 days
        set_both_windows(&state, "draining",     0.6, 3 * 3600, 0.3, 4 * 24 * 3600);
        // "almost_reset": 5h at 90%, resets in 10 min; 7d at 20%, resets in 3 days
        // Despite 90% 5h utilization, it resets very soon — should score well
        set_both_windows(&state, "almost_reset", 0.9, 10 * 60,  0.2, 3 * 24 * 3600);

        assert_eq!(pick(&accounts, &state, RoutingStrategy::Maximus).as_deref(), Some("almost_reset"),
            "maximus: nearly-reset window should outscore higher raw remaining");
    }

    #[test]
    fn maximus_picks_fresh_over_depleted() {
        let accounts = vec![make_account("depleted"), make_account("fresh")];
        let state = StateStore::new_empty();
        set_both_windows(&state, "depleted", 0.8, 3 * 3600, 0.7, 5 * 24 * 3600);
        // "fresh" has no rate-limit data → scores 1.0

        assert_eq!(pick(&accounts, &state, RoutingStrategy::Maximus).as_deref(), Some("fresh"),
            "maximus: fresh account should beat heavily utilised one");
    }
}
