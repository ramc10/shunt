/// Runtime state: per-account cooldowns/disabling + conversation stickiness.
///
/// Thread-safe via Arc<Mutex<>>. Cooldowns and disables are persisted to disk;
/// stickiness is ephemeral (lost on restart is acceptable).
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
    /// Recent request log (ephemeral — not persisted to disk).
    #[serde(skip)]
    recent_requests: VecDeque<RequestLog>,
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
}

impl StateStore {
    /// Create a fresh in-memory store with no backing file (useful for tests).
    pub fn new_empty() -> Self {
        // No background writer thread for the null store — writes are no-ops.
        Self {
            path: PathBuf::from("/dev/null"),
            inner: Arc::new(Mutex::new(StateData::default())),
            pending: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn load(path: &Path) -> Self {
        let data: StateData = if path.exists() {
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

        let store = Self {
            path: path.to_owned(),
            inner: Arc::new(Mutex::new(data)),
            pending: Arc::new(AtomicBool::new(false)),
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
                    let data = inner.lock().unwrap().clone();
                    if let Err(e) = write_to_disk(&data, &path) {
                        warn!("Failed to persist state: {e}");
                    }
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // Availability
    // -----------------------------------------------------------------------

    pub fn is_available(&self, name: &str) -> bool {
        let data = self.inner.lock().unwrap();
        match data.accounts.get(name) {
            None => true,
            Some(s) => !s.disabled && now_ms() >= s.cooldown_until_ms,
        }
    }

    /// Returns a snapshot of all account states for the status endpoint.
    pub fn account_states(&self) -> HashMap<String, AccountState> {
        self.inner.lock().unwrap().accounts.clone()
    }

    // -----------------------------------------------------------------------
    // Cooldown / disable
    // -----------------------------------------------------------------------

    pub fn set_cooldown(&self, name: &str, duration_ms: u64) {
        {
            let mut data = self.inner.lock().unwrap();
            let acc = data.accounts.entry(name.to_owned()).or_default();
            acc.cooldown_until_ms = now_ms() + duration_ms;
        }
        self.persist();
    }

    pub fn disable_account(&self, name: &str) {
        {
            let mut data = self.inner.lock().unwrap();
            data.accounts.entry(name.to_owned()).or_default().disabled = true;
        }
        self.persist();
    }

    pub fn set_auth_failed(&self, name: &str) {
        {
            let mut data = self.inner.lock().unwrap();
            let acc = data.accounts.entry(name.to_owned()).or_default();
            acc.auth_failed = true;
            acc.disabled = true; // also disable so it's skipped in routing
        }
        self.persist();
    }

    // -----------------------------------------------------------------------
    // Stickiness (ephemeral — not persisted)
    // -----------------------------------------------------------------------

    pub fn get_sticky(&self, fingerprint: &str) -> Option<String> {
        let data = self.inner.lock().unwrap();
        let entry = data.sticky.get(fingerprint)?;
        if now_ms() < entry.expires_at_ms {
            Some(entry.account_name.clone())
        } else {
            None
        }
    }

    pub fn set_sticky(&self, fingerprint: &str, account_name: &str, ttl_ms: u64) {
        let mut data = self.inner.lock().unwrap();
        data.sticky.insert(
            fingerprint.to_owned(),
            StickyEntry { account_name: account_name.to_owned(), expires_at_ms: now_ms() + ttl_ms },
        );
    }

    // -----------------------------------------------------------------------
    // Quota tracking
    // -----------------------------------------------------------------------

    /// Epoch-ms when the account's current window started.
    /// Returns u64::MAX for accounts with no window (sorts last in earliest-expiry).
    pub fn window_start_ms(&self, name: &str) -> u64 {
        let data = self.inner.lock().unwrap();
        data.quota.get(name).map(|q| q.window_start_ms).unwrap_or(u64::MAX)
    }

    /// Unix epoch seconds when this account's 5h window resets.
    /// Returns None if unknown or already past.
    pub fn reset_5h_secs(&self, name: &str) -> Option<u64> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let data = self.inner.lock().unwrap();
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
        let data = self.inner.lock().unwrap();
        let Some(rl) = data.rate_limits.get(name) else { return 0.0 };
        // If the reset time is in the past, the window has rolled over — treat as fresh
        if rl.reset_5h.map(|t| t <= now_secs).unwrap_or(false) {
            return 0.0;
        }
        rl.utilization_5h.unwrap_or(0.0)
    }

    /// Record token usage from a completed request.
    /// Lazily resets the window if the 5-hour period has elapsed.
    pub fn record_usage(&self, name: &str, input_tokens: u64, output_tokens: u64) {
        if input_tokens == 0 && output_tokens == 0 {
            return;
        }
        {
            let mut data = self.inner.lock().unwrap();
            let quota = data.quota.entry(name.to_owned()).or_default();
            let now = now_ms();
            if quota.window_start_ms == 0 || now >= quota.window_start_ms + WINDOW_MS {
                quota.window_start_ms = now;
                quota.input_tokens = 0;
                quota.output_tokens = 0;
            }
            quota.input_tokens += input_tokens;
            quota.output_tokens += output_tokens;
        }
        self.persist();
    }

    /// Snapshot of all quota windows for the status endpoint.
    pub fn quota_snapshot(&self) -> HashMap<String, QuotaWindow> {
        self.inner.lock().unwrap().quota.clone()
    }

    // -----------------------------------------------------------------------
    // Rate limit header tracking
    // -----------------------------------------------------------------------

    pub fn update_rate_limits(&self, name: &str, info: RateLimitInfo) {
        {
            let mut data = self.inner.lock().unwrap();
            data.rate_limits.insert(name.to_owned(), info);
        }
        self.persist();
    }

    pub fn rate_limit_snapshot(&self) -> HashMap<String, RateLimitInfo> {
        self.inner.lock().unwrap().rate_limits.clone()
    }

    // -----------------------------------------------------------------------
    // Account pinning
    // -----------------------------------------------------------------------

    pub fn get_pinned(&self) -> Option<String> {
        self.inner.lock().unwrap().pinned_account.clone()
    }

    pub fn set_pinned(&self, name: Option<String>) {
        {
            let mut data = self.inner.lock().unwrap();
            data.pinned_account = name;
        }
        self.persist();
    }

    // -----------------------------------------------------------------------
    // Last-used tracking
    // -----------------------------------------------------------------------

    pub fn get_last_used(&self) -> Option<String> {
        self.inner.lock().unwrap().last_used_account.clone()
    }

    pub fn set_last_used(&self, name: &str) {
        {
            let mut data = self.inner.lock().unwrap();
            data.last_used_account = Some(name.to_owned());
        }
        self.persist();
    }

    // -----------------------------------------------------------------------
    // Request log
    // -----------------------------------------------------------------------

    pub fn record_request(&self, log: RequestLog) {
        let mut data = self.inner.lock().unwrap();
        if data.recent_requests.len() >= MAX_RECENT {
            data.recent_requests.pop_front();
        }
        data.recent_requests.push_back(log);
    }

    /// Most-recent first snapshot for the monitor / status endpoint.
    pub fn recent_requests_snapshot(&self) -> Vec<RequestLog> {
        let data = self.inner.lock().unwrap();
        data.recent_requests.iter().rev().cloned().collect()
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
        store.set_sticky(fp, "account1", 1); // 1 ms TTL
        assert_eq!(store.get_sticky(fp).as_deref(), Some("account1"),
            "sticky should be available immediately");
        std::thread::sleep(std::time::Duration::from_millis(10));
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
}

fn write_to_disk(data: &StateData, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(data)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
