/// Fire-and-forget telemetry client.
///
/// Pushes request events and periodic heartbeats to a relay-server instance.
/// All network failures are silently ignored — the relay is optional and must
/// never block or degrade the proxy.
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tracing::debug;

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
