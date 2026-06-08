/// Runtime state: per-account cooldowns/disabling + conversation stickiness.
///
/// Thread-safe via Arc<Mutex<>>. Cooldowns and disables are persisted to disk;
/// stickiness is ephemeral (lost on restart is acceptable).
use crate::config::RoutingStrategy;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Public version of `now_ms()` for use from other modules.
pub fn now_ms_pub() -> u64 {
    now_ms()
}

// ---------------------------------------------------------------------------
// Routing snapshot (single-lock data for pick_account)
// ---------------------------------------------------------------------------

/// Pre-computed per-account data for the router, taken from a single mutex lock.
#[derive(Debug, Clone)]
pub struct AccountRoutingData {
    pub available: bool,
    pub health_check_failed: bool,
    pub exhausted: bool,
    pub cooldown_until_ms: u64,
    pub util_5h: f64,
    pub util_7d: f64,
    pub reset_5h_secs: Option<u64>,
    pub reset_7d_secs: Option<u64>,
    pub burst_request_count: usize,
}

/// Snapshot of all routing-relevant state, taken with a single lock.
#[derive(Debug, Clone)]
pub struct RoutingSnapshot {
    pub accounts: HashMap<String, AccountRoutingData>,
    pub now_secs: u64,
}

// ---------------------------------------------------------------------------
// On-disk data
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct AccountState {
    /// Epoch-ms timestamp after which this account is usable again (0 = not cooling).
    #[serde(default)]
    pub cooldown_until_ms: u64,
    /// Permanently disabled (auth failure).
    #[serde(default)]
    pub disabled: bool,
    /// OAuth credentials are expired and need re-authorization via `shunt add-account`.
    #[serde(default)]
    pub auth_failed: bool,
    /// Account failed health-check probes — skip in routing until it recovers.
    #[serde(default)]
    pub health_check_failed: bool,
    /// Consecutive health-check failure count (for exponential backoff). Ephemeral.
    #[serde(skip)]
    pub health_check_failures: u32,
    /// Epoch-ms of the last health-check probe attempt. Ephemeral.
    #[serde(skip)]
    pub last_health_check_ms: u64,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct StickyEntry {
    account_name: String,
    expires_at_ms: u64,
}

/// Rolling 5-hour quota window per account.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct QuotaWindow {
    /// Epoch-ms when this window started (0 = never used).
    #[serde(default)]
    pub window_start_ms: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

impl QuotaWindow {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
    pub fn window_expires_ms(&self) -> Option<u64> {
        if self.window_start_ms == 0 { None } else { Some(self.window_start_ms + WINDOW_MS) }
    }
}

pub const WINDOW_MS: u64 = 5 * 60 * 60 * 1000; // 5 hours

// ---------------------------------------------------------------------------
// Request log
// ---------------------------------------------------------------------------

/// A single proxied request recorded for the live monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLog {
    pub ts_ms: u64,
    pub account: String,
    pub model: String,
    pub status: u16,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
}

const MAX_RECENT: usize = 200;

/// Rate-limit info extracted from `anthropic-ratelimit-unified-*` response headers.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct RateLimitInfo {
    /// 5-hour window utilization 0.0–1.0
    pub utilization_5h: Option<f64>,
    /// Unix epoch seconds when 5h window resets
    pub reset_5h: Option<u64>,
    /// "allowed" | "exhausted"
    pub status_5h: Option<String>,
    /// 7-day window utilization 0.0–1.0
    pub utilization_7d: Option<f64>,
    /// Unix epoch seconds when 7d window resets
    pub reset_7d: Option<u64>,
    pub status_7d: Option<String>,
    /// Extra usage (overage) status: "allowed" | "rejected"
    pub overage_status: Option<String>,
    pub overage_disabled_reason: Option<String>,
    /// Which claim is currently representative ("five_hour" | "seven_day")
    pub representative_claim: Option<String>,
    pub updated_ms: u64,
}

/// Per-day token and API-cost accumulator (all accounts combined).
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct DailyBucket {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// What those tokens would have cost on the public API (USD).
    pub api_cost_usd: f64,
}

/// Snapshot returned by `savings_snapshot()` for the status endpoint + CLI.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct SavingsSnapshot {
    pub today_input: u64,
    pub today_output: u64,
    pub today_cost_usd: f64,
    pub week_input: u64,
    pub week_output: u64,
    pub week_cost_usd: f64,
    pub all_time_input: u64,
    pub all_time_output: u64,
    pub all_time_cost_usd: f64,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct StateData {
    #[serde(default)]
    accounts: HashMap<String, AccountState>,
    #[serde(default)]
    sticky: HashMap<String, StickyEntry>,
    #[serde(default)]
    quota: HashMap<String, QuotaWindow>,
    #[serde(default)]
    rate_limits: HashMap<String, RateLimitInfo>,
    /// If set, all requests are forced to this account (overrides routing).
    #[serde(default)]
    pinned_account: Option<String>,
    /// The most recent account that successfully handled a proxied request.
    #[serde(default)]
    last_used_account: Option<String>,
    /// Recent request log — capped at MAX_RECENT entries, persisted to survive restarts.
    #[serde(default)]
    recent_requests: VecDeque<RequestLog>,
    /// Runtime model override — all requests use this model if set (ephemeral).
    #[serde(skip)]
    model_override: Option<String>,
    /// Runtime routing strategy override (ephemeral — not persisted).
    #[serde(skip)]
    routing_strategy_override: Option<RoutingStrategy>,
    /// Per-account burst window: timestamps of recent requests (ephemeral).
    #[serde(skip)]
    burst_windows: HashMap<String, VecDeque<u64>>,
    /// Runtime burst RPM limit override (ephemeral).
    #[serde(skip)]
    burst_rpm_limit_override: Option<u32>,
    /// Runtime fallback model override (ephemeral).
    /// `Some(Some("model"))` = explicit override, `Some(None)` = explicitly disabled, `None` = use config/auto.
    #[serde(skip)]
    fallback_model_override: Option<Option<String>>,
    /// Runtime effort override (ephemeral). None = passthrough, Some("max") = override.
    #[serde(skip)]
    effort_override: Option<String>,
    /// Runtime thinking mode override (ephemeral). None = passthrough, Some("adaptive"/"disabled") = override.
    #[serde(skip)]
    thinking_override: Option<String>,
    /// Daily token + cost buckets keyed by "YYYY-MM-DD" (all accounts combined).
    #[serde(default)]
    global_daily: HashMap<String, DailyBucket>,
    /// All-time totals.
    #[serde(default)]
    all_time_input: u64,
    #[serde(default)]
    all_time_output: u64,
    #[serde(default)]
    all_time_cost_usd: f64,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct StateStore {
    path: PathBuf,
    inner: Arc<Mutex<StateData>>,
    /// Set to true when a write is needed; the background writer thread clears it.
    pending: Arc<AtomicBool>,
    /// Monotonically-increasing counter for round-robin account selection.
    round_robin: Arc<AtomicUsize>,
    /// When true, all daemon alert notifications are suppressed (ephemeral).
    alerts_muted: Arc<AtomicBool>,
}

impl StateStore {
    /// Create a fresh in-memory store with no backing file (useful for tests).
    pub fn new_empty() -> Self {
        // No background writer thread for the null store — writes are no-ops.
        Self {
            path: PathBuf::from("/dev/null"),
            inner: Arc::new(Mutex::new(StateData::default())),
            pending: Arc::new(AtomicBool::new(false)),
            round_robin: Arc::new(AtomicUsize::new(0)),
            alerts_muted: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn load(path: &Path) -> Self {
        let mut data: StateData = if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                    warn!("State file unreadable ({e}), starting fresh");
                    StateData::default()
                }),
                Err(e) => {
                    warn!("Cannot read state file ({e}), starting fresh");
                    StateData::default()
                }
            }
        } else {
            StateData::default()
        };
        // Prune expired sticky entries so the file doesn't grow unbounded.
        let now = now_ms();
        data.sticky.retain(|_, v| v.expires_at_ms > now);
        // Trim request log to cap in case the file was written with a higher limit.
        while data.recent_requests.len() > MAX_RECENT {
            data.recent_requests.pop_front();
        }

        let store = Self {
            path: path.to_owned(),
            inner: Arc::new(Mutex::new(data)),
            pending: Arc::new(AtomicBool::new(false)),
            round_robin: Arc::new(AtomicUsize::new(0)),
            alerts_muted: Arc::new(AtomicBool::new(false)),
        };
        store.start_writer_thread();
        store
    }

    /// Spawn a single background thread that flushes state to disk at most every 100 ms.
    /// This prevents unbounded thread spawning when many requests fire in rapid succession.
    fn start_writer_thread(&self) {
        let pending = Arc::clone(&self.pending);
        let inner   = Arc::clone(&self.inner);
        let path    = self.path.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if pending.compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                    let data = inner.lock().clone();
                    if let Err(e) = write_to_disk(&data, &path) {
                        warn!("Failed to persist state: {e}");
                    }
                }
            }
        });
    }

    /// Force a synchronous write to disk. Called at graceful shutdown to ensure
    /// the final state (including the last few requests and token counts) is
    /// persisted before the process exits.
    pub fn flush_sync(&self) {
        let data = self.inner.lock().clone();
        if let Err(e) = write_to_disk(&data, &self.path) {
            warn!("Final state flush failed: {e}");
        }
    }

    // -----------------------------------------------------------------------
    // Availability
    // -----------------------------------------------------------------------

    pub fn is_available(&self, name: &str) -> bool {
        let data = self.inner.lock();
        match data.accounts.get(name) {
            None => true,
            Some(s) => !s.disabled && now_ms() >= s.cooldown_until_ms,
        }
    }

    /// Returns true if the account's Anthropic quota is currently exhausted in any
    /// active window (5h or 7d) — i.e. sending another request will get a 429.
    pub fn is_exhausted(&self, name: &str) -> bool {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let data = self.inner.lock();
        let Some(rl) = data.rate_limits.get(name) else { return false };
        // Only consider a window exhausted if its reset is still in the future
        // (i.e. the window hasn't rolled over yet).
        let exhausted_5h = rl.status_5h.as_deref() == Some("exhausted")
            && rl.reset_5h.map(|t| t > now_secs).unwrap_or(false);
        let exhausted_7d = rl.status_7d.as_deref() == Some("exhausted")
            && rl.reset_7d.map(|t| t > now_secs).unwrap_or(false);
        exhausted_5h || exhausted_7d
    }

    /// Fetch-and-increment monotonic counter for round-robin account cycling.
    pub fn next_rr_index(&self) -> usize {
        self.round_robin.fetch_add(1, Ordering::Relaxed)
    }

    /// Returns a snapshot of all account states for the status endpoint.
    pub fn account_states(&self) -> HashMap<String, AccountState> {
        self.inner.lock().accounts.clone()
    }

    /// Single-lock snapshot of everything the router needs for account selection.
    /// Avoids per-account mutex acquisitions (O(N) → O(1) locks per pick_account call).
    pub fn routing_snapshot(&self) -> RoutingSnapshot {
        let now_ms  = now_ms();
        let now_secs = now_ms / 1_000;
        let mut data = self.inner.lock();

        // Collect all account names from both accounts and rate_limits maps.
        let all_names: Vec<String> = {
            let mut names: HashSet<&String> = data.accounts.keys().collect();
            names.extend(data.rate_limits.keys());
            names.into_iter().cloned().collect()
        };

        // Pre-compute burst counts (needs mutable access for pruning).
        let burst_counts: HashMap<String, usize> = all_names.iter()
            .map(|name| {
                let count = data.burst_windows.get_mut(name)
                    .map(|deque| Self::burst_count_inner(deque, 60_000))
                    .unwrap_or(0);
                (name.clone(), count)
            })
            .collect();

        let accounts: HashMap<String, AccountRoutingData> = all_names.iter().map(|name| {
            let acc = data.accounts.get(name);
            let available = acc.map(|a| !a.disabled && !a.auth_failed && now_ms >= a.cooldown_until_ms).unwrap_or(true);
            let health_check_failed = acc.map(|a| a.health_check_failed).unwrap_or(false);
            let cooldown_until_ms = acc.map(|a| a.cooldown_until_ms).unwrap_or(0);

            let (util_5h, reset_5h, util_7d, reset_7d, exhausted) =
                if let Some(rl) = data.rate_limits.get(name) {
                    let r5 = rl.reset_5h.filter(|&t| t > now_secs);
                    let r7 = rl.reset_7d.filter(|&t| t > now_secs);
                    let u5 = if r5.is_some() { rl.utilization_5h.unwrap_or(0.0) } else { 0.0 };
                    let u7 = if r7.is_some() { rl.utilization_7d.unwrap_or(0.0) } else { 0.0 };
                    let ex = (rl.status_5h.as_deref() == Some("exhausted") && r5.is_some())
                          || (rl.status_7d.as_deref() == Some("exhausted") && r7.is_some());
                    (u5, r5, u7, r7, ex)
                } else {
                    (0.0, None, 0.0, None, false)
                };

            let burst_request_count = burst_counts.get(name).copied().unwrap_or(0);

            (name.clone(), AccountRoutingData {
                available,
                health_check_failed,
                exhausted,
                cooldown_until_ms,
                util_5h,
                util_7d,
                reset_5h_secs: reset_5h,
                reset_7d_secs: reset_7d,
                burst_request_count,
            })
        }).collect();

        RoutingSnapshot { accounts, now_secs }
    }

    // -----------------------------------------------------------------------
    // Burst window tracking
    // -----------------------------------------------------------------------

    /// Record a request timestamp for burst-rate tracking.
    pub fn record_request_burst(&self, name: &str) {
        let mut data = self.inner.lock();
        data.burst_windows.entry(name.to_owned()).or_default().push_back(now_ms());
    }

    /// Count requests in the last `window_ms` for an account.
    fn burst_count_inner(deque: &mut VecDeque<u64>, window_ms: u64) -> usize {
        let cutoff = now_ms().saturating_sub(window_ms);
        // Prune old entries from front
        while deque.front().map(|&t| t < cutoff).unwrap_or(false) {
            deque.pop_front();
        }
        deque.len()
    }

    // -----------------------------------------------------------------------
    // Cooldown / disable
    // -----------------------------------------------------------------------

    pub fn set_cooldown(&self, name: &str, duration_ms: u64) {
        {
            let mut data = self.inner.lock();
            let acc = data.accounts.entry(name.to_owned()).or_default();
            acc.cooldown_until_ms = now_ms() + duration_ms;
        }
        self.persist();
    }

    /// Like `set_cooldown`, but staggers the deadline so it doesn't collide with
    /// other accounts already cooling. Prevents the cascade where both accounts
    /// expire simultaneously, both get 429'd again, and loop forever.
    /// Adds 5s offset per account already cooling within ±5s of our target deadline.
    pub fn set_cooldown_staggered(&self, name: &str, duration_ms: u64) {
        const STAGGER_MS: u64 = 5_000;
        {
            let mut data = self.inner.lock();
            let now = now_ms();
            let target = now + duration_ms;

            // Count other accounts with cooldowns expiring within STAGGER_MS of our target
            let nearby_count = data.accounts.iter()
                .filter(|(n, a)| {
                    *n != name
                        && a.cooldown_until_ms > now
                        && (a.cooldown_until_ms as i64 - target as i64).unsigned_abs() < STAGGER_MS
                })
                .count() as u64;

            let offset = nearby_count.saturating_mul(STAGGER_MS);
            let acc = data.accounts.entry(name.to_owned()).or_default();
            acc.cooldown_until_ms = target + offset;
        }
        self.persist();
    }

    pub fn disable_account(&self, name: &str) {
        {
            let mut data = self.inner.lock();
            data.accounts.entry(name.to_owned()).or_default().disabled = true;
        }
        self.persist();
    }

    pub fn set_auth_failed(&self, name: &str) {
        {
            let mut data = self.inner.lock();
            let acc = data.accounts.entry(name.to_owned()).or_default();
            acc.auth_failed = true;
            acc.disabled = true; // also disable so it's skipped in routing
        }
        self.persist();
    }

    /// Clear auth_failed + disabled for an account after a successful token refresh.
    pub fn clear_auth_failed(&self, name: &str) {
        {
            let mut data = self.inner.lock();
            if let Some(acc) = data.accounts.get_mut(name) {
                acc.auth_failed = false;
                acc.disabled = false;
            }
        }
        self.persist();
    }

    /// Returns names of accounts (from the given list) that have auth_failed set.
    pub fn auth_failed_accounts<'a>(&self, names: &[&'a str]) -> Vec<&'a str> {
        let data = self.inner.lock();
        names.iter()
            .filter(|&&n| data.accounts.get(n).map(|s| s.auth_failed).unwrap_or(false))
            .copied()
            .collect()
    }

    // -----------------------------------------------------------------------
    // Health check state
    // -----------------------------------------------------------------------

    pub fn is_health_check_failed(&self, name: &str) -> bool {
        let data = self.inner.lock();
        data.accounts.get(name).map(|s| s.health_check_failed).unwrap_or(false)
    }

    pub fn set_health_check_failed(&self, name: &str) {
        {
            let mut data = self.inner.lock();
            let acc = data.accounts.entry(name.to_owned()).or_default();
            acc.health_check_failed = true;
        }
        self.persist();
    }

    pub fn clear_health_check_failed(&self, name: &str) {
        {
            let mut data = self.inner.lock();
            if let Some(acc) = data.accounts.get_mut(name) {
                acc.health_check_failed = false;
                acc.health_check_failures = 0;
            }
        }
        self.persist();
    }

    /// Increment consecutive failure count and return the new value.
    /// Sets `health_check_failed = true` once failures >= `threshold`.
    pub fn record_health_check_failure(&self, name: &str, threshold: u32) -> u32 {
        let count;
        {
            let mut data = self.inner.lock();
            let acc = data.accounts.entry(name.to_owned()).or_default();
            acc.health_check_failures = acc.health_check_failures.saturating_add(1);
            count = acc.health_check_failures;
            if count >= threshold {
                acc.health_check_failed = true;
            }
        }
        if count >= threshold {
            self.persist();
        }
        count
    }

    /// Update last_health_check_ms to now. Returns the previous value.
    pub fn update_last_health_check(&self, name: &str) -> u64 {
        let mut data = self.inner.lock();
        let acc = data.accounts.entry(name.to_owned()).or_default();
        let prev = acc.last_health_check_ms;
        acc.last_health_check_ms = now_ms();
        prev
    }

    /// Get the last health check timestamp and consecutive failure count.
    pub fn health_check_info(&self, name: &str) -> (u64, u32) {
        let data = self.inner.lock();
        match data.accounts.get(name) {
            Some(acc) => (acc.last_health_check_ms, acc.health_check_failures),
            None => (0, 0),
        }
    }

    // -----------------------------------------------------------------------
    // Stickiness (ephemeral — not persisted)
    // -----------------------------------------------------------------------

    pub fn get_sticky(&self, fingerprint: &str) -> Option<String> {
        let data = self.inner.lock();
        let entry = data.sticky.get(fingerprint)?;
        if now_ms() < entry.expires_at_ms {
            Some(entry.account_name.clone())
        } else {
            None
        }
    }

    pub fn set_sticky(&self, fingerprint: &str, account_name: &str, ttl_ms: u64) {
        const MAX_STICKY_ENTRIES: usize = 10_000;
        {
            let mut data = self.inner.lock();
            // Prune expired entries if approaching limit
            if data.sticky.len() >= MAX_STICKY_ENTRIES {
                let now = now_ms();
                data.sticky.retain(|_, v| v.expires_at_ms > now);
                // If still at limit after pruning, clear oldest half to prevent DoS
                if data.sticky.len() >= MAX_STICKY_ENTRIES {
                    data.sticky.clear();
                }
            }
            data.sticky.insert(
                fingerprint.to_owned(),
                StickyEntry { account_name: account_name.to_owned(), expires_at_ms: now_ms() + ttl_ms },
            );
        }
        self.persist();
    }

    // -----------------------------------------------------------------------
    // Quota tracking
    // -----------------------------------------------------------------------

    /// Unix epoch seconds when this account's 5h window resets.
    /// Returns None if unknown or already past.
    pub fn reset_5h_secs(&self, name: &str) -> Option<u64> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let data = self.inner.lock();
        let reset = data.rate_limits.get(name)?.reset_5h?;
        if reset > now_secs { Some(reset) } else { None }
    }

    /// 5-hour utilization 0.0–1.0 from the last upstream response headers.
    /// Returns 0.0 for fresh accounts or when the reset window has already passed.
    pub fn utilization_5h(&self, name: &str) -> f64 {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let data = self.inner.lock();
        let Some(rl) = data.rate_limits.get(name) else { return 0.0 };
        // If the reset time is in the past, the window has rolled over — treat as fresh
        if rl.reset_5h.map(|t| t <= now_secs).unwrap_or(false) {
            return 0.0;
        }
        rl.utilization_5h.unwrap_or(0.0)
    }

    /// 7-day utilization 0.0–1.0 from the last upstream response headers.
    /// Returns 0.0 for fresh accounts or when the reset window has already passed.
    pub fn utilization_7d(&self, name: &str) -> f64 {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let data = self.inner.lock();
        let Some(rl) = data.rate_limits.get(name) else { return 0.0 };
        if rl.reset_7d.map(|t| t <= now_secs).unwrap_or(false) {
            return 0.0;
        }
        rl.utilization_7d.unwrap_or(0.0)
    }

    /// Unix epoch seconds when this account's 7d window resets.
    /// Returns None if unknown or already past.
    pub fn reset_7d_secs(&self, name: &str) -> Option<u64> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let data = self.inner.lock();
        let reset = data.rate_limits.get(name)?.reset_7d?;
        if reset > now_secs { Some(reset) } else { None }
    }

    /// Record token usage from a completed request.
    /// Lazily resets the window if the 5-hour period has elapsed.
    pub fn record_usage(&self, name: &str, input_tokens: u64, output_tokens: u64) {
        if input_tokens == 0 && output_tokens == 0 {
            return;
        }
        {
            let mut data = self.inner.lock();
            let quota = data.quota.entry(name.to_owned()).or_default();
            let now = now_ms();
            if quota.window_start_ms == 0 || now >= quota.window_start_ms + WINDOW_MS {
                quota.window_start_ms = now;
                quota.input_tokens = 0;
                quota.output_tokens = 0;
            }
            quota.input_tokens = quota.input_tokens.saturating_add(input_tokens);
            quota.output_tokens = quota.output_tokens.saturating_add(output_tokens);
        }
        self.persist();
    }

    /// Snapshot of all quota windows for the status endpoint.
    pub fn quota_snapshot(&self) -> HashMap<String, QuotaWindow> {
        self.inner.lock().quota.clone()
    }

    // -----------------------------------------------------------------------
    // Rate limit header tracking
    // -----------------------------------------------------------------------

    pub fn update_rate_limits(&self, name: &str, info: RateLimitInfo) {
        let prev = self.inner.lock().rate_limits.get(name).cloned();

        // Warn the first time utilization crosses 90% for each window.
        let prev_5h = prev.as_ref().and_then(|p| p.utilization_5h).unwrap_or(0.0);
        let prev_7d = prev.as_ref().and_then(|p| p.utilization_7d).unwrap_or(0.0);
        if let Some(u) = info.utilization_5h {
            if u >= 0.9 && prev_5h < 0.9 {
                warn!(account = %name, utilization = %format!("{:.0}%", u * 100.0),
                    "5h rate limit above 90% — approaching quota");
            }
        }
        if let Some(u) = info.utilization_7d {
            if u >= 0.9 && prev_7d < 0.9 {
                warn!(account = %name, utilization = %format!("{:.0}%", u * 100.0),
                    "7d rate limit above 90% — approaching quota");
            }
        }

        {
            let mut data = self.inner.lock();
            data.rate_limits.insert(name.to_owned(), info);
        }
        self.persist();
    }

    pub fn rate_limit_snapshot(&self) -> HashMap<String, RateLimitInfo> {
        self.inner.lock().rate_limits.clone()
    }

    // -----------------------------------------------------------------------
    // Account pinning
    // -----------------------------------------------------------------------

    pub fn get_pinned(&self) -> Option<String> {
        self.inner.lock().pinned_account.clone()
    }

    pub fn set_pinned(&self, name: Option<String>) {
        {
            let mut data = self.inner.lock();
            data.pinned_account = name;
        }
        self.persist();
    }

    // -----------------------------------------------------------------------
    // Last-used tracking
    // -----------------------------------------------------------------------

    pub fn get_last_used(&self) -> Option<String> {
        self.inner.lock().last_used_account.clone()
    }

    pub fn set_last_used(&self, name: &str) {
        {
            let mut data = self.inner.lock();
            data.last_used_account = Some(name.to_owned());
        }
        self.persist();
    }

    // -----------------------------------------------------------------------
    // Model override
    // -----------------------------------------------------------------------

    pub fn get_model_override(&self) -> Option<String> {
        self.inner.lock().model_override.clone()
    }

    pub fn set_model_override(&self, model: String) {
        self.inner.lock().model_override = Some(model);
    }

    pub fn clear_model_override(&self) {
        self.inner.lock().model_override = None;
    }

    // -----------------------------------------------------------------------
    // Routing strategy override
    // -----------------------------------------------------------------------

    pub fn get_routing_strategy(&self) -> Option<RoutingStrategy> {
        self.inner.lock().routing_strategy_override
    }

    pub fn set_routing_strategy(&self, strategy: RoutingStrategy) {
        self.inner.lock().routing_strategy_override = Some(strategy);
    }

    pub fn clear_routing_strategy(&self) {
        self.inner.lock().routing_strategy_override = None;
    }

    // -----------------------------------------------------------------------
    // Burst RPM limit override
    // -----------------------------------------------------------------------

    pub fn get_burst_rpm_limit_override(&self) -> Option<u32> {
        self.inner.lock().burst_rpm_limit_override
    }

    pub fn set_burst_rpm_limit_override(&self, limit: u32) {
        self.inner.lock().burst_rpm_limit_override = Some(limit);
    }

    pub fn clear_burst_rpm_limit_override(&self) {
        self.inner.lock().burst_rpm_limit_override = None;
    }

    // -----------------------------------------------------------------------
    // Fallback model override
    // -----------------------------------------------------------------------

    /// Returns `Some(Some("model"))` for explicit override, `Some(None)` for explicitly disabled,
    /// `None` for "use config/auto".
    pub fn get_fallback_model_override(&self) -> Option<Option<String>> {
        self.inner.lock().fallback_model_override.clone()
    }

    pub fn set_fallback_model_override(&self, model: Option<String>) {
        self.inner.lock().fallback_model_override = Some(model);
    }

    pub fn clear_fallback_model_override(&self) {
        self.inner.lock().fallback_model_override = None;
    }

    // -----------------------------------------------------------------------
    // Effort override
    // -----------------------------------------------------------------------

    pub fn get_effort_override(&self) -> Option<String> {
        self.inner.lock().effort_override.clone()
    }

    pub fn set_effort_override(&self, effort: String) {
        self.inner.lock().effort_override = Some(effort);
    }

    pub fn clear_effort_override(&self) {
        self.inner.lock().effort_override = None;
    }

    // -----------------------------------------------------------------------
    // Thinking mode override
    // -----------------------------------------------------------------------

    pub fn get_thinking_override(&self) -> Option<String> {
        self.inner.lock().thinking_override.clone()
    }

    pub fn set_thinking_override(&self, mode: String) {
        self.inner.lock().thinking_override = Some(mode);
    }

    pub fn clear_thinking_override(&self) {
        self.inner.lock().thinking_override = None;
    }

    // -----------------------------------------------------------------------
    // Alerts mute
    // -----------------------------------------------------------------------

    pub fn get_alerts_muted(&self) -> bool {
        self.alerts_muted.load(Ordering::Relaxed)
    }

    pub fn set_alerts_muted(&self, muted: bool) {
        self.alerts_muted.store(muted, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Request log
    // -----------------------------------------------------------------------

    pub fn record_request(&self, log: RequestLog) {
        let mut data = self.inner.lock();
        if data.recent_requests.len() >= MAX_RECENT {
            data.recent_requests.pop_front();
        }
        data.recent_requests.push_back(log);
    }

    /// Most-recent first snapshot for the monitor / status endpoint.
    pub fn recent_requests_snapshot(&self) -> Vec<RequestLog> {
        let data = self.inner.lock();
        data.recent_requests.iter().rev().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Global savings tracking
    // -----------------------------------------------------------------------

    /// Record tokens + API cost globally (across all accounts) for the savings display.
    pub fn record_global(&self, model: &str, input_tokens: u64, output_tokens: u64) {
        if input_tokens == 0 && output_tokens == 0 {
            return;
        }
        let cost = crate::pricing::api_cost_usd(model, input_tokens, output_tokens);
        let key = today_key();
        {
            let mut data = self.inner.lock();
            let bucket = data.global_daily.entry(key).or_default();
            bucket.input_tokens  = bucket.input_tokens.saturating_add(input_tokens);
            bucket.output_tokens = bucket.output_tokens.saturating_add(output_tokens);
            bucket.api_cost_usd  += cost;
            data.all_time_input  = data.all_time_input.saturating_add(input_tokens);
            data.all_time_output = data.all_time_output.saturating_add(output_tokens);
            data.all_time_cost_usd += cost;

            // Prune buckets older than 90 days to prevent unbounded growth.
            if data.global_daily.len() > 100 {
                let cutoff = epoch_to_ymd(
                    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
                        .saturating_sub(90 * 86400)
                );
                data.global_daily.retain(|k, _| k.as_str() >= cutoff.as_str());
            }
        }
        self.persist();
    }

    /// Snapshot of daily and all-time savings for the status endpoint and CLI.
    pub fn savings_snapshot(&self) -> SavingsSnapshot {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let today   = today_key();
        let week_ago = epoch_to_ymd(now_secs.saturating_sub(7 * 86400));

        let data = self.inner.lock();

        let today_bucket = data.global_daily.get(&today).cloned().unwrap_or_default();

        let (week_input, week_output, week_cost) = data.global_daily.iter()
            .filter(|(k, _)| k.as_str() >= week_ago.as_str())
            .fold((0u64, 0u64, 0f64), |(i, o, c), (_, b)| {
                (i + b.input_tokens, o + b.output_tokens, c + b.api_cost_usd)
            });

        SavingsSnapshot {
            today_input:      today_bucket.input_tokens,
            today_output:     today_bucket.output_tokens,
            today_cost_usd:   today_bucket.api_cost_usd,
            week_input,
            week_output,
            week_cost_usd:    week_cost,
            all_time_input:   data.all_time_input,
            all_time_output:  data.all_time_output,
            all_time_cost_usd: data.all_time_cost_usd,
        }
    }

    // -----------------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------------

    fn persist(&self) {
        // Signal the background writer thread; it will flush within ~100 ms.
        self.pending.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sticky_ttl_expiry() {
        let store = StateStore::new_empty();
        let fp = "conv-fp-ttl";
        store.set_sticky(fp, "account1", 500); // 500 ms TTL
        assert_eq!(store.get_sticky(fp).as_deref(), Some("account1"),
            "sticky should be available immediately");
        std::thread::sleep(std::time::Duration::from_millis(600));
        assert!(store.get_sticky(fp).is_none(),
            "sticky must expire after TTL elapses");
    }

    #[test]
    fn test_cooldown_blocks_availability() {
        let store = StateStore::new_empty();
        store.set_cooldown("acc", 5_000); // 5s cooldown
        assert!(!store.is_available("acc"), "account should be unavailable during cooldown");
    }

    #[test]
    fn test_disable_blocks_availability() {
        let store = StateStore::new_empty();
        store.disable_account("acc");
        assert!(!store.is_available("acc"), "disabled account must be unavailable");
    }

    #[test]
    fn test_quota_accumulates() {
        let store = StateStore::new_empty();
        store.record_usage("acc", 100, 50);
        store.record_usage("acc", 200, 75);
        let snap = store.quota_snapshot();
        let q = &snap["acc"];
        assert_eq!(q.input_tokens, 300);
        assert_eq!(q.output_tokens, 125);
        assert_eq!(q.total_tokens(), 425);
    }

    #[test]
    fn test_pinned_account_round_trip() {
        let store = StateStore::new_empty();
        assert!(store.get_pinned().is_none());
        store.set_pinned(Some("myaccount".into()));
        assert_eq!(store.get_pinned().as_deref(), Some("myaccount"));
        store.set_pinned(None);
        assert!(store.get_pinned().is_none());
    }

    #[test]
    fn test_last_used_round_trip() {
        let store = StateStore::new_empty();
        assert!(store.get_last_used().is_none());
        store.set_last_used("acc1");
        assert_eq!(store.get_last_used().as_deref(), Some("acc1"));
    }

    #[test]
    fn test_recent_requests_ring_buffer() {
        let store = StateStore::new_empty();
        // Fill past MAX_RECENT
        for i in 0..=(MAX_RECENT + 5) {
            store.record_request(RequestLog {
                ts_ms: i as u64,
                account: "acc".into(),
                model: "m".into(),
                status: 200,
                input_tokens: 1,
                output_tokens: 1,
                duration_ms: 1,
            });
        }
        let snap = store.recent_requests_snapshot();
        assert_eq!(snap.len(), MAX_RECENT, "buffer must not grow beyond MAX_RECENT");
        // Most recent first
        assert!(snap[0].ts_ms > snap[snap.len() - 1].ts_ms, "snapshot must be newest-first");
    }

    #[test]
    fn test_health_check_failed_round_trip() {
        let store = StateStore::new_empty();
        assert!(!store.is_health_check_failed("acc"), "fresh account should not be health-check-failed");

        store.set_health_check_failed("acc");
        assert!(store.is_health_check_failed("acc"), "should be marked unhealthy after set");

        store.clear_health_check_failed("acc");
        assert!(!store.is_health_check_failed("acc"), "should be cleared after clear");
    }

    #[test]
    fn test_health_check_failure_threshold() {
        let store = StateStore::new_empty();

        // First failure: count=1, threshold=2 → not yet marked
        let count = store.record_health_check_failure("acc", 2);
        assert_eq!(count, 1);
        assert!(!store.is_health_check_failed("acc"),
            "should not be marked after 1 failure (threshold=2)");

        // Second failure: count=2, threshold=2 → now marked
        let count = store.record_health_check_failure("acc", 2);
        assert_eq!(count, 2);
        assert!(store.is_health_check_failed("acc"),
            "should be marked after 2 failures (threshold=2)");
    }

    #[test]
    fn test_clear_health_check_resets_failure_count() {
        let store = StateStore::new_empty();
        store.record_health_check_failure("acc", 2);
        store.record_health_check_failure("acc", 2);
        assert!(store.is_health_check_failed("acc"));

        store.clear_health_check_failed("acc");
        assert!(!store.is_health_check_failed("acc"));

        let (_, failures) = store.health_check_info("acc");
        assert_eq!(failures, 0, "failure count must reset to 0 after clear");
    }

    #[test]
    fn test_health_check_info_and_last_check() {
        let store = StateStore::new_empty();
        let (last, failures) = store.health_check_info("acc");
        assert_eq!(last, 0, "fresh account last_health_check_ms should be 0");
        assert_eq!(failures, 0);

        let prev = store.update_last_health_check("acc");
        assert_eq!(prev, 0, "first update should return previous value 0");

        let (last2, _) = store.health_check_info("acc");
        assert!(last2 > 0, "last_health_check_ms should be updated to now");
    }

    #[test]
    fn test_health_check_failed_persists() {
        let path = std::env::temp_dir().join(format!(
            "shunt_test_hc_{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        {
            let store = StateStore::load(&path);
            store.set_health_check_failed("acc");
            std::thread::sleep(std::time::Duration::from_millis(300));
        }

        let store2 = StateStore::load(&path);
        assert!(store2.is_health_check_failed("acc"),
            "health_check_failed must survive restart");

        // Ephemeral fields should NOT persist
        let (last, failures) = store2.health_check_info("acc");
        assert_eq!(last, 0, "last_health_check_ms is ephemeral, should be 0 after reload");
        assert_eq!(failures, 0, "health_check_failures is ephemeral, should be 0 after reload");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_state_persistence_roundtrip() {
        // Use a unique temp path so parallel tests don't collide
        let path = std::env::temp_dir().join(format!(
            "shunt_test_state_{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        {
            let store = StateStore::load(&path);
            store.set_cooldown("acc", 999_999_000); // far-future cooldown
            store.record_usage("acc", 111, 222);
            store.set_last_used("acc");
            // Wait for the background writer (polls every 100 ms) to flush
            std::thread::sleep(std::time::Duration::from_millis(300));
        }

        // Load a fresh store from the persisted file
        let store2 = StateStore::load(&path);
        assert!(!store2.is_available("acc"), "cooldown must survive restart");
        let snap = store2.quota_snapshot();
        assert_eq!(snap["acc"].input_tokens, 111, "quota must survive restart");
        assert_eq!(snap["acc"].output_tokens, 222);
        assert_eq!(store2.get_last_used().as_deref(), Some("acc"),
            "last_used_account must survive restart");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_burst_window_tracking() {
        let store = StateStore::new_empty();
        // Record 5 requests
        for _ in 0..5 {
            store.record_request_burst("acc");
        }
        // Snapshot should show 5 in burst_request_count
        let snap = store.routing_snapshot();
        let data = snap.accounts.get("acc");
        assert!(data.is_none() || data.unwrap().burst_request_count == 0,
            "no account state yet, burst tracked separately");
        // Now create account state so it appears in snapshot
        store.set_cooldown("acc", 0); // creates the AccountState entry
        for _ in 0..3 {
            store.record_request_burst("acc");
        }
        let snap = store.routing_snapshot();
        let data = snap.accounts.get("acc").expect("acc should exist in snapshot");
        // Should have all 8 requests (5 + 3) since they're within the 60s window
        assert_eq!(data.burst_request_count, 8, "should count all recent requests");
    }
}

/// "YYYY-MM-DD" string for today in UTC.
fn today_key() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    epoch_to_ymd(secs)
}

/// Convert Unix epoch seconds to "YYYY-MM-DD" (UTC) using Hinnant's civil_from_days.
fn epoch_to_ymd(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let z    = days + 719_468;
    let era  = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe  = z - era * 146_097;
    let yoe  = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y    = yoe + era * 400;
    let doy  = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp   = (5 * doy + 2) / 153;
    let d    = doy - (153 * mp + 2) / 5 + 1;
    let m    = if mp < 10 { mp + 3 } else { mp - 9 };
    let y    = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn write_to_disk(data: &StateData, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(data)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
