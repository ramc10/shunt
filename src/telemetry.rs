/// Fire-and-forget telemetry client.
///
/// Pushes request events and periodic heartbeats to a relay-server instance.
/// All network failures are silently ignored — the relay is optional and must
/// never block or degrade the proxy.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex as PLMutex;
use serde_json::json;
use tracing::debug;

use crate::config::install_id_path;
use crate::state::RequestLog;

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TelemetryClient {
    inner: Arc<Inner>,
}

struct Inner {
    event_url:     String,
    heartbeat_url: String,
    token:         Option<String>,
    instance:      String,
    client:        reqwest::Client,
}

impl TelemetryClient {
    pub fn new(base_url: &str, token: Option<String>, instance: String) -> Self {
        let base = base_url.trim_end_matches('/');
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");

        Self {
            inner: Arc::new(Inner {
                event_url:     format!("{base}/event"),
                heartbeat_url: format!("{base}/heartbeat"),
                token,
                instance,
                client,
            }),
        }
    }

    /// Push a completed request event — spawns a background task, never blocks.
    pub fn push_event(&self, log: &RequestLog) {
        let inner = Arc::clone(&self.inner);
        let body = json!({
            "instance":    inner.instance,
            "ts_ms":       log.ts_ms,
            "account":     log.account,
            "model":       log.model,
            "status":      log.status,
            "duration_ms": log.duration_ms,
        });

        tokio::spawn(async move {
            let mut req = inner.client.post(&inner.event_url).json(&body);
            if let Some(ref t) = inner.token {
                req = req.bearer_auth(t);
            }
            match req.send().await {
                Ok(r) if !r.status().is_success() => {
                    debug!(status = %r.status(), "relay rejected event");
                }
                Err(e) => debug!(err = %e, "relay event send failed"),
                _ => {}
            }
        });
    }

    /// Push a full status snapshot as a heartbeat. Called periodically from a
    /// background task — awaited by the caller so they can control the cadence.
    pub async fn push_heartbeat(&self, status: serde_json::Value) {
        let body = json!({
            "instance": self.inner.instance,
            "status":   status,
        });
        let mut req = self.inner.client.post(&self.inner.heartbeat_url).json(&body);
        if let Some(ref t) = self.inner.token {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if !r.status().is_success() => {
                debug!(status = %r.status(), "relay rejected heartbeat");
            }
            Err(e) => debug!(err = %e, "relay heartbeat send failed"),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Supabase telemetry
// ---------------------------------------------------------------------------

/// Supabase project URL — fill in after creating a project.
const SUPABASE_URL: &str = "https://fldpbuojpdiabnaravqy.supabase.co";
/// Supabase anon key (INSERT-only RLS) — fill in after creating a project.
const SUPABASE_ANON_KEY: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6ImZsZHBidW9qcGRpYWJuYXJhdnF5Iiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODExMDk0MzEsImV4cCI6MjA5NjY4NTQzMX0.KohqBWgbGGnOn-7blBDXDUaJG3S6SQmHW3DA1h4Z4yk";

const FLUSH_INTERVAL_SECS: u64 = 60;
const MAX_QUEUE_LEN: usize = 50;

struct SbEntry {
    event_type: &'static str,
    payload: serde_json::Value,
}

struct SbInner {
    install_id: String,
    queue: PLMutex<Vec<SbEntry>>,
    client: reqwest::Client,
    total_requests: AtomicU64,
    started_ms: u64,
}

/// Anonymous Supabase telemetry — batches events and flushes every 60 s or on
/// high-priority events. Never blocks the proxy; all errors are dropped silently.
#[derive(Clone)]
pub struct SupabaseTelemetry {
    inner: Arc<SbInner>,
}

/// Read the persisted install_id or create a new random one.
pub fn get_or_create_install_id() -> String {
    let path = install_id_path();
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_owned();
        if id.len() == 36 { return id; } // UUID length
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &id);
    id
}

impl SupabaseTelemetry {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            inner: Arc::new(SbInner {
                install_id: get_or_create_install_id(),
                queue: PLMutex::new(Vec::new()),
                client,
                total_requests: AtomicU64::new(0),
                started_ms,
            }),
        }
    }

    pub fn install_id(&self) -> &str {
        &self.inner.install_id
    }

    pub fn total_requests(&self) -> u64 {
        self.inner.total_requests.load(Ordering::Relaxed)
    }

    /// Emit a daemon_start event. Upserts the installs row and enqueues the event.
    pub fn emit_daemon_start(
        &self,
        version: &str,
        account_count: usize,
        routing_strategy: &str,
        providers: &[String],
        has_custom_domain: bool,
    ) {
        let payload = json!({
            "version": version,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "account_count": account_count,
            "routing_strategy": routing_strategy,
            "providers": providers,
            "has_custom_domain": has_custom_domain,
            "multi_account": account_count > 1,
        });
        self.enqueue("daemon_start", payload, false);
        // Upsert install row immediately in background.
        let inner = Arc::clone(&self.inner);
        let version = version.to_owned();
        tokio::spawn(async move {
            let row = json!({
                "id": inner.install_id,
                "last_seen": chrono_now_iso(),
                "version": version,
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            });
            let _ = inner.client
                .post(format!("{SUPABASE_URL}/rest/v1/installs"))
                .header("apikey", SUPABASE_ANON_KEY)
                .header("Authorization", format!("Bearer {SUPABASE_ANON_KEY}"))
                .header("Content-Type", "application/json")
                .header("Prefer", "resolution=merge-duplicates")
                .query(&[("on_conflict", "id")])
                .json(&row)
                .send()
                .await;
        });
    }

    /// Emit a request_complete event.
    pub fn emit_request_complete(
        &self, model: &str, provider: &str,
        latency_ms: u64, input_tokens: u64, output_tokens: u64,
    ) {
        self.inner.total_requests.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "model": model,
            "provider": provider,
            "status": "success",
            "latency_ms": latency_ms,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        });
        self.enqueue("request_complete", payload, false);
    }

    /// Emit a rate_limit_hit event (flushes immediately).
    pub fn emit_rate_limit_hit(&self, provider: &str, cooldown_ms: u64, accounts_available: usize) {
        let payload = json!({
            "provider": provider,
            "cooldown_ms": cooldown_ms,
            "accounts_available": accounts_available,
        });
        self.enqueue("rate_limit_hit", payload, true);
    }

    /// Emit an auth_failure event (flushes immediately).
    pub fn emit_auth_failure(&self, provider: &str) {
        let payload = json!({ "provider": provider });
        self.enqueue("auth_failure", payload, true);
    }

    /// Emit a daemon_stop event and flush synchronously.
    pub async fn emit_daemon_stop(&self, total_savings_usd: f64) {
        let uptime_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(self.inner.started_ms / 1000);
        let payload = json!({
            "uptime_secs": uptime_secs,
            "total_requests": self.total_requests(),
            "total_savings_usd": total_savings_usd,
        });
        self.enqueue("daemon_stop", payload, false);
        self.flush().await;
    }

    fn enqueue(&self, event_type: &'static str, payload: serde_json::Value, high_priority: bool) {
        let should_flush = {
            let mut q = self.inner.queue.lock();
            q.push(SbEntry { event_type, payload });
            high_priority || q.len() >= MAX_QUEUE_LEN
        };
        if should_flush {
            let clone = self.clone();
            tokio::spawn(async move { clone.flush().await });
        }
    }

    pub async fn flush(&self) {
        let entries: Vec<SbEntry> = {
            let mut q = self.inner.queue.lock();
            std::mem::take(&mut *q)
        };
        if entries.is_empty() { return; }

        let rows: Vec<serde_json::Value> = entries.into_iter().map(|e| json!({
            "install_id": self.inner.install_id,
            "event_type": e.event_type,
            "payload": e.payload,
        })).collect();

        let _ = self.inner.client
            .post(format!("{SUPABASE_URL}/rest/v1/events"))
            .header("apikey", SUPABASE_ANON_KEY)
            .header("Authorization", format!("Bearer {SUPABASE_ANON_KEY}"))
            .header("Content-Type", "application/json")
            .json(&rows)
            .send()
            .await;
    }

    /// Spawn a background task that flushes the queue every 60 seconds.
    pub fn start_flush_loop(&self) {
        let clone = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(FLUSH_INTERVAL_SECS));
            loop {
                interval.tick().await;
                clone.flush().await;
            }
        });
    }
}

/// Emit a single feature_used event from a CLI command.
/// Fire-and-forget: spawns a background task. Errors drop silently.
/// Call with `tokio::spawn(track_cli_feature("monitor"))` — no blocking.
pub async fn track_cli_feature(feature: &'static str) {
    let install_id = get_or_create_install_id();
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(5)).build() {
        Ok(c) => c,
        Err(_) => return,
    };
    let rows = json!([{
        "install_id": install_id,
        "event_type": "feature_used",
        "payload": {
            "feature": feature,
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        }
    }]);
    let _ = client
        .post(format!("{SUPABASE_URL}/rest/v1/events"))
        .header("apikey", SUPABASE_ANON_KEY)
        .header("Authorization", format!("Bearer {SUPABASE_ANON_KEY}"))
        .header("Content-Type", "application/json")
        .json(&rows)
        .send()
        .await;
}

fn chrono_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as ISO 8601 without chrono dep: "YYYY-MM-DDTHH:MM:SSZ"
    let s = secs;
    let (y, mo, d, h, mi, se) = epoch_to_ymd_hms(s);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z")
}

fn epoch_to_ymd_hms(mut s: u64) -> (u64, u64, u64, u64, u64, u64) {
    let se = s % 60; s /= 60;
    let mi = s % 60; s /= 60;
    let h  = s % 24; s /= 24;
    // Days since 1970-01-01
    let (y, mo, d) = days_to_ymd(s);
    (y, mo, d, h, mi, se)
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut y = 1970u64;
    loop {
        let dy = if is_leap(y) { 366 } else { 365 };
        if days < dy { break; }
        days -= dy;
        y += 1;
    }
    let months = if is_leap(y) {
        [31,29,31,30,31,30,31,31,30,31,30,31]
    } else {
        [31,28,31,30,31,30,31,31,30,31,30,31]
    };
    let mut mo = 1u64;
    for &dm in &months {
        if days < dm { break; }
        days -= dm;
        mo += 1;
    }
    (y, mo, days + 1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
