/// Runtime state: per-account cooldowns/disabling + conversation stickiness.
///
/// Thread-safe via Arc<Mutex<>>. Cooldowns and disables are persisted to disk;
/// stickiness is ephemeral (lost on restart is acceptable).
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct StateStore {
    path: PathBuf,
    inner: Arc<Mutex<StateData>>,
}

impl StateStore {
    /// Create a fresh in-memory store with no backing file (useful for tests).
    pub fn new_empty() -> Self {
        Self {
            path: PathBuf::from("/dev/null"),
            inner: Arc::new(Mutex::new(StateData::default())),
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

        Self { path: path.to_owned(), inner: Arc::new(Mutex::new(data)) }
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
    // Persistence
    // -----------------------------------------------------------------------

    fn persist(&self) {
        let data = self.inner.lock().unwrap().clone();
        let path = self.path.clone();
        std::thread::spawn(move || {
            if let Err(e) = write_to_disk(&data, &path) {
                warn!("Failed to persist state: {e}");
            }
        });
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
