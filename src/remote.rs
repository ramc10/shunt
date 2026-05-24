/// `shunt remote` — relay-based remote account event watcher.
///
/// **Host mode** (`shunt remote`):
///   - Generates a one-time watch code (`RM-…`)
///   - Polls the local shunt `/status` every 10 s
///   - Encrypts the snapshot and pushes it to the relay
///   - Prints the code so the user can enter it on another device
///
/// **Client mode** (`shunt remote RM-…`):
///   - Polls the relay for the latest encrypted snapshot
///   - Decrypts and diffs against the previous poll
///   - Fires local system notifications on account events
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::term::{bold, cyan, dim, fmt_duration_ms, green, red, yellow};

// ---------------------------------------------------------------------------
// Relay URL default
// ---------------------------------------------------------------------------

const DEFAULT_RELAY: &str = "https://relay.ramcharan.shop";

// ---------------------------------------------------------------------------
// Snapshot types (serialized + encrypted over the relay)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AccountStatus {
    pub name: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub auth_failed: bool,
    #[serde(default)]
    pub cooldown_until_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct RemoteSnapshot {
    accounts: Vec<AccountStatus>,
    ts_ms: u64,
}

// ---------------------------------------------------------------------------
// State snapshot for diffing (client side)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Snap {
    auth_failed: bool,
    /// true if `cooldown_until_ms > now_ms` at snapshot time
    cooling: bool,
    disabled: bool,
}

impl Snap {
    fn from_status(acc: &AccountStatus, now_ms: u64) -> Self {
        Self {
            auth_failed: acc.auth_failed,
            cooling: acc.cooldown_until_ms > now_ms,
            disabled: acc.disabled,
        }
    }
}

// ---------------------------------------------------------------------------
// Thresholds
// ---------------------------------------------------------------------------

const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// How often the host pushes to the relay even if nothing changed (keeps session alive).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Cooldowns shorter than this are transient — skip notification.
const LONG_COOLDOWN_MS: u64 = 5 * 60_000;
/// Minimum gap between "all accounts offline" notifications.
const ALL_OFFLINE_NOTIFY_COOLDOWN: Duration = Duration::from_secs(3_600);

// ---------------------------------------------------------------------------
// Entry point — dispatches host vs client
// ---------------------------------------------------------------------------

pub async fn run_remote(code: Option<String>, relay_url: Option<String>, local_url: String) -> Result<()> {
    let relay = relay_url.unwrap_or_else(|| DEFAULT_RELAY.to_string());
    // Require HTTPS for relay to protect encrypted payloads in transit
    if !relay.starts_with("https://") {
        bail!("Relay URL must use HTTPS (got: {}). Use the default relay or provide an https:// URL.", relay);
    }
    match code {
        None        => run_host(relay, local_url).await,
        Some(code)  => run_client(code, relay).await,
    }
}

// ---------------------------------------------------------------------------
// Host mode
// ---------------------------------------------------------------------------

async fn run_host(relay_url: String, local_url: String) -> Result<()> {
    let code = crate::sync::generate_remote_code();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(0)
        .build()?;

    println!();
    println!("  {}  {}  {}", bold("◆"), bold("shunt"), dim("remote  host"));
    println!();
    println!("  {}  {}", dim("code"), cyan(&code));
    println!("  {}  on another device run:", dim("·"));
    println!("  {}  {}", dim("·"), bold(&format!("shunt remote {code}")));
    println!();
    println!("  {}  watching local accounts — Ctrl-C to stop", dim("·"));
    println!();

    let mut last_push: Option<Instant> = None;
    let mut last_accounts_json: Option<String> = None;

    loop {
        match fetch_local_status(&client, &local_url).await {
            Ok(accounts) => {
                let accounts_json = serde_json::to_string(&accounts).unwrap_or_default();
                let changed = last_accounts_json.as_deref() != Some(accounts_json.as_str());
                let heartbeat_due = last_push
                    .map(|t| t.elapsed() >= HEARTBEAT_INTERVAL)
                    .unwrap_or(true);

                if changed || heartbeat_due {
                    let snapshot = RemoteSnapshot { accounts, ts_ms: now_ms() };
                    let data = serde_json::to_vec(&snapshot)?;
                    let payload = crate::sync::encrypt_bytes(&data, &code)?;

                    let push_url = format!("{relay_url}/watch/{code}");
                    match client
                        .put(&push_url)
                        .json(&serde_json::json!({ "payload": payload }))
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            last_push = Some(Instant::now());
                            last_accounts_json = Some(accounts_json);
                        }
                        Ok(r) => {
                            let status = r.status();
                            eprintln!("  {}  relay push failed: {status}", red("✗"));
                        }
                        Err(e) => eprintln!("  {}  relay unreachable: {e}", red("✗")),
                    }
                }
            }
            Err(e) => eprintln!("  {}  local shunt unreachable: {e}", red("✗")),
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Client mode
// ---------------------------------------------------------------------------

async fn run_client(code: String, relay_url: String) -> Result<()> {
    crate::sync::validate_remote_code(&code)
        .context("invalid remote code")?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(0)
        .build()?;

    println!();
    println!("  {}  {}  {}", bold("◆"), bold("shunt"), dim("remote  client"));
    println!("  {}  {}", dim("·"), cyan(&relay_url));
    println!("  {}  connecting…", dim("·"));
    println!();

    let mut prev: HashMap<String, Snap> = HashMap::new();
    let mut first_poll = true;
    let mut was_session_missing = false;

    // Throttle state
    let mut notified_cooldown: HashMap<String, u64> = HashMap::new();
    let mut notified_auth_failed: HashSet<String> = HashSet::new();
    let mut last_all_offline: Option<Instant> = None;
    let mut was_all_offline = false;

    loop {
        let poll_url = format!("{relay_url}/watch/{code}");
        match client.get(&poll_url).send().await {
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                if !was_session_missing {
                    println!("  {}  session not found — waiting for host…", yellow("⏸"));
                    was_session_missing = true;
                }
            }
            Ok(resp) if resp.status().is_success() => {
                if was_session_missing {
                    println!("  {}  host connected", green("✓"));
                    was_session_missing = false;
                }

                let body: serde_json::Value = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => { eprintln!("  {}  bad relay response: {e}", red("✗")); continue; }
                };

                let payload = match body["payload"].as_str() {
                    Some(p) => p.to_owned(),
                    None => { eprintln!("  {}  relay response missing payload", red("✗")); continue; }
                };

                let data = match crate::sync::decrypt_bytes(&payload, &code) {
                    Ok(d) => d,
                    Err(e) => { eprintln!("  {}  decryption failed: {e}", red("✗")); continue; }
                };

                let snapshot: RemoteSnapshot = match serde_json::from_slice(&data) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("  {}  snapshot parse error: {e}", red("✗")); continue; }
                };

                let now = now_ms();

                if first_poll {
                    print_initial_state(&snapshot.accounts, now);
                    for acc in &snapshot.accounts {
                        prev.insert(acc.name.clone(), Snap::from_status(acc, now));
                    }
                    first_poll = false;
                } else {
                    diff_and_notify(
                        &snapshot.accounts,
                        &prev,
                        now,
                        &mut notified_cooldown,
                        &mut notified_auth_failed,
                        &mut last_all_offline,
                        &mut was_all_offline,
                    );
                    prev.clear();
                    for acc in &snapshot.accounts {
                        prev.insert(acc.name.clone(), Snap::from_status(acc, now));
                    }
                }
            }
            Ok(resp) => {
                eprintln!("  {}  relay error: {}", red("✗"), resp.status());
            }
            Err(e) => {
                eprintln!("  {}  cannot reach relay: {e}", red("✗"));
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// State diffing + notification dispatch
// ---------------------------------------------------------------------------

fn diff_and_notify(
    accounts: &[AccountStatus],
    prev: &HashMap<String, Snap>,
    now_ms: u64,
    notified_cooldown: &mut HashMap<String, u64>,
    notified_auth_failed: &mut HashSet<String>,
    last_all_offline: &mut Option<Instant>,
    was_all_offline: &mut bool,
) {
    let all_unavailable = accounts.iter().all(|a| !a.available);

    for acc in accounts {
        let Some(p) = prev.get(&acc.name) else { continue };

        // ── Reauth required (newly auth_failed) ─────────────────────────────
        if acc.auth_failed && !p.auth_failed && !notified_auth_failed.contains(&acc.name) {
            let msg = format!(
                "Account '{}' needs re-authorization. Run `shunt add-account`.",
                acc.name
            );
            println!("  {}  [{}]  reauth required", red("✗"), yellow(&acc.name));
            crate::notify::notify("shunt: Reauth Required", &msg, "Basso");
            notified_auth_failed.insert(acc.name.clone());
        }
        if !acc.auth_failed {
            notified_auth_failed.remove(&acc.name);
        }

        // ── Entered cooldown (newly, long enough to matter) ──────────────────
        let curr_cooling = acc.cooldown_until_ms > now_ms;
        if curr_cooling && !p.cooling {
            let remaining_ms = acc.cooldown_until_ms - now_ms;
            let last_cdl = notified_cooldown.get(&acc.name).copied().unwrap_or(0);
            if remaining_ms >= LONG_COOLDOWN_MS && acc.cooldown_until_ms != last_cdl {
                let mins = remaining_ms / 60_000;
                let msg = format!("Account '{}' hit quota limit — cooling {}m.", acc.name, mins);
                println!(
                    "  {}  [{}]  rate limited — cooling {}",
                    yellow("⏸"), yellow(&acc.name), yellow(&fmt_duration_ms(remaining_ms)),
                );
                crate::notify::notify("shunt: Rate Limited", &msg, "Ping");
                notified_cooldown.insert(acc.name.clone(), acc.cooldown_until_ms);
            }
        }

        // ── Resumed from cooldown ────────────────────────────────────────────
        if p.cooling && acc.available && !acc.auth_failed {
            println!("  {}  [{}]  back online", green("✓"), green(&acc.name));
            crate::notify::notify(
                "shunt: Account Resumed",
                &format!("Account '{}' is back online.", acc.name),
                "Glass",
            );
            notified_cooldown.remove(&acc.name);
        }

        // ── Recovered from auth_failed / disabled ────────────────────────────
        if (p.auth_failed || p.disabled) && acc.available {
            println!("  {}  [{}]  recovered", green("✓"), green(&acc.name));
            crate::notify::notify(
                "shunt: Account Recovered",
                &format!("Account '{}' is back online.", acc.name),
                "Glass",
            );
        }
    }

    // ── All accounts offline ─────────────────────────────────────────────────
    if all_unavailable && !*was_all_offline {
        let should_notify = last_all_offline
            .map(|t| t.elapsed() >= ALL_OFFLINE_NOTIFY_COOLDOWN)
            .unwrap_or(true);
        if should_notify {
            println!("  {}  all accounts are offline", red("✗"));
            crate::notify::notify(
                "shunt: All Accounts Offline",
                "All accounts are offline or on cooldown.",
                "Basso",
            );
            *last_all_offline = Some(Instant::now());
        }
    }
    if *was_all_offline && !all_unavailable {
        println!("  {}  accounts back online", green("✓"));
    }
    *was_all_offline = all_unavailable;
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

fn print_initial_state(accounts: &[AccountStatus], now_ms: u64) {
    println!("  {}  {} account(s)", green("✓"), accounts.len());
    for acc in accounts {
        let (sym, label) = if acc.auth_failed || acc.disabled {
            (red("✗"), red(&acc.name))
        } else if acc.cooldown_until_ms > now_ms {
            let rem = fmt_duration_ms(acc.cooldown_until_ms - now_ms);
            (yellow("⏸"), yellow(&format!("{}  cooling {}", acc.name, rem)))
        } else {
            (green("✓"), green(&acc.name))
        };
        println!("    {}  {}", sym, label);
    }
    println!();
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async fn fetch_local_status(
    client: &reqwest::Client,
    local_url: &str,
) -> Result<Vec<AccountStatus>> {
    let url = format!("{}/status", local_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await?;
    let body: serde_json::Value = resp.json().await?;
    let accounts = serde_json::from_value(body["accounts"].clone())
        .context("failed to parse accounts from /status")?;
    Ok(accounts)
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
