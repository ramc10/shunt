use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{config_path, config_template, credentials_path, log_path, pid_path, CredentialsStore};
use crate::oauth::{claude_credentials_path, read_claude_credentials, refresh_token, run_oauth_flow};
use crate::term::{self, bold, bold_white, cyan, dim, green, red, yellow, CHECK, CROSS, DASH, DOT, EMPTY};

#[derive(Parser)]
#[command(name = "shunt", about = "Local Claude Code account-pooling proxy", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive setup — auto-imports your existing Claude Code session
    Setup {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Start the proxy (runs setup first if not configured)
    Start {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Keep the process in the foreground instead of daemonizing
        #[arg(long)]
        foreground: bool,
        /// Internal: running as background daemon (do not use directly)
        #[arg(long, hide = true)]
        daemon: bool,
    },
    /// Print current config and proxy status
    Status {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Import the current Claude Code session as an additional account
    AddAccount {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Name for this account (e.g. "secondary", "work"). Omit to auto-detect.
        name: Option<String>,
    },
    /// Remove an account from the pool
    RemoveAccount {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Name of the account to remove (omit to pick interactively)
        name: Option<String>,
    },
    /// Pin routing to a specific account, or restore automatic routing
    ///
    /// Examples:
    ///   shunt use            — interactive picker
    ///   shunt use work       — force all requests through 'work'
    ///   shunt use auto       — restore automatic least-utilization routing
    Use {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Account name to pin to, or "auto". Omit to pick interactively.
        account: Option<String>,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Setup { config } => cmd_setup(config).await,
        Command::Start { config, host, port, .. } => cmd_start(config, host, port).await,
        Command::Status { config } => cmd_status(config).await,
        Command::AddAccount { config, name } => cmd_add_account(config, name).await,
        Command::RemoveAccount { config, name } => cmd_remove_account(config, name).await,
        Command::Use { config, account } => cmd_use(config, account).await,
    }
}

// ---------------------------------------------------------------------------
// setup
// ---------------------------------------------------------------------------

pub async fn cmd_setup(config_override: Option<PathBuf>) -> Result<()> {
    let config_p = config_override.clone().unwrap_or_else(config_path);

    print_splash(&[
        format!("{}  {}", bold_white("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        dim("Setup"),
        String::new(),
    ]);

    if config_p.exists() {
        println!("  {} Already configured.", green(CHECK));
        println!("  {} Use {} to add more accounts.", dim("·"), cyan("shunt add-account"));
        println!();
        return Ok(());
    }

    // Auto-detect existing Claude Code session — no user action needed
    let cred = match read_claude_credentials() {
        Some(mut c) => {
            if c.needs_refresh() {
                print!("  {} Token expired, refreshing… ", yellow("↻"));
                use std::io::Write;
                std::io::stdout().flush().ok();
                match refresh_token(&c).await {
                    Ok(fresh) => { println!("{}", green("done")); c = fresh; }
                    Err(e) => println!("{} ({})", yellow("failed"), dim(&e.to_string())),
                }
            } else {
                println!("  {} Claude Code session found", green(CHECK));
            }
            c
        }
        None => {
            println!("  {} No Claude Code session at {}", red(CROSS), dim(&claude_credentials_path().display().to_string()));
            println!("  {} Run {} first, then re-run setup.", dim("·"), cyan("claude"));
            println!();
            bail!("No Claude Code credentials found.");
        }
    };

    let plan = crate::oauth::read_claude_session_info()
        .map(|s| s.plan)
        .unwrap_or_else(|| "pro".to_string());
    println!("  {} Plan: {}", green(CHECK), bold(&plan));

    // Fetch account email (non-fatal)
    let email = crate::oauth::fetch_account_email(&cred.access_token).await;
    if let Some(ref e) = email {
        println!("  {} Account: {}", green(CHECK), bold(e));
    }
    let mut cred = cred;
    cred.email = email;

    // Write config
    if let Some(parent) = config_p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&config_p, config_template(&[("main", &plan)]))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config_p, std::fs::Permissions::from_mode(0o600))?;
    }

    // Store credential
    let mut store = CredentialsStore::default();
    store.accounts.insert("main".into(), cred);
    store.save()?;

    println!();
    println!("  {} Config      {}", green("→"), dim(&config_p.display().to_string()));
    println!("  {} Credentials {}", green("→"), dim(&credentials_path().display().to_string()));

    offer_shell_export()?;

    println!();
    println!("  {} Run {} to start.", green(CHECK), cyan("shunt start"));

    Ok(())
}

// ---------------------------------------------------------------------------
// add-account
// ---------------------------------------------------------------------------

async fn cmd_add_account(config_override: Option<PathBuf>, name: Option<String>) -> Result<()> {
    let config_p = config_override.clone().unwrap_or_else(config_path);
    if !config_p.exists() {
        bail!("No config found. Run `shunt setup` first.");
    }

    let existing_config = std::fs::read_to_string(&config_p)?;
    let store = CredentialsStore::load();

    // Resolve name: if not given, find accounts missing credentials or let user pick
    let (name, already_in_config) = if let Some(n) = name {
        let in_config = existing_config.contains(&format!("name = \"{n}\""));
        let has_cred  = store.accounts.contains_key(&n);
        if in_config && has_cred {
            bail!("Account '{}' already exists with a valid credential.\nTo add a new account use: shunt add-account <name>", n);
        }
        (n, in_config)
    } else {
        // Find accounts in config that have no credential yet
        let config = crate::config::load_config(config_override.as_deref())?;
        let missing: Vec<_> = config.accounts.iter()
            .filter(|a| a.credential.is_none())
            .collect();
        match missing.len() {
            0 => {
                // All accounts are authorised — user wants to add a brand new one
                println!("  {} All accounts have credentials.", green(CHECK));
                println!("  {} To add a new account, run: {}", dim("·"),
                    cyan("shunt add-account <name>"));
                println!();
                return Ok(());
            }
            1 => {
                println!("  {} Account '{}' has no credential — authorizing now",
                    yellow("↻"), missing[0].name);
                (missing[0].name.clone(), true)
            }
            _ => {
                let items: Vec<term::SelectItem> = missing.iter().map(|a| term::SelectItem {
                    label: bold(&a.name).to_string(),
                    value: a.name.clone(),
                }).collect();
                match term::select("Authorize account:", &items, 0) {
                    Some(v) => (v, true),
                    None => return Ok(()),
                }
            }
        }
    };

    print_splash(&[
        format!("{}  {}", bold_white("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        format!("Adding account {}", bold(&format!("'{name}'"))),
        String::new(),
    ]);

    let mut cred = run_oauth_flow().await?;

    // Fetch email (non-fatal)
    let email = crate::oauth::fetch_account_email(&cred.access_token).await;
    if let Some(ref e) = email {
        println!("  {} Account: {}", green(CHECK), bold(e));
    }
    cred.email = email;

    // Only append to config if not already there
    if !already_in_config {
        let mut config_text = existing_config;
        config_text.push_str(&format!("\n[[accounts]]\nname = \"{name}\"\nplan_type = \"pro\"\n"));
        std::fs::write(&config_p, &config_text)?;
    }

    let mut store = CredentialsStore::load();
    store.accounts.insert(name.clone(), cred);
    store.save()?;

    println!();
    println!("  {} Account {} authorized.", green(CHECK), bold(&format!("'{name}'")));
    println!("  {} Restart to apply: {}", dim("·"), cyan("shunt start"));
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// remove-account
// ---------------------------------------------------------------------------

async fn cmd_remove_account(config_override: Option<PathBuf>, name: Option<String>) -> Result<()> {
    let config_p = config_override.clone().unwrap_or_else(config_path);
    if !config_p.exists() {
        bail!("No config found. Run `shunt setup` first.");
    }

    // Resolve name — pick interactively if not given
    let name = if let Some(n) = name {
        n
    } else {
        let config = crate::config::load_config(config_override.as_deref())?;
        let removable: Vec<_> = config.accounts.iter()
            .filter(|a| a.name != "main")
            .collect();
        if removable.is_empty() {
            bail!("No removable accounts (cannot remove 'main').");
        }
        let items: Vec<term::SelectItem> = removable.iter().map(|a| {
            let email = a.credential.as_ref().and_then(|c| c.email.as_deref()).unwrap_or("");
            term::SelectItem {
                label: format!("{}  {}", bold(&pad(&a.name, 12)), dim(&pad(email, 32))),
                value: a.name.clone(),
            }
        }).collect();
        match term::select("Remove account:", &items, 0) {
            Some(v) => v,
            None => return Ok(()),
        }
    };

    if name == "main" {
        bail!("Cannot remove the 'main' account. Use `shunt setup` to reconfigure.");
    }

    let config_text = std::fs::read_to_string(&config_p)?;
    if !config_text.contains(&format!("name = \"{name}\"")) {
        bail!("Account '{name}' not found.");
    }

    print_splash(&[
        format!("{}  {}", bold_white("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        format!("Removing account {}", bold(&format!("'{name}'"))),
        String::new(),
    ]);

    // Strip the [[accounts]] block for this name from config
    let new_config = remove_account_block(&config_text, &name);
    std::fs::write(&config_p, &new_config)?;
    println!("  {} Removed from config", green(CHECK));

    // Remove credential from store
    let mut store = CredentialsStore::load();
    if store.accounts.remove(&name).is_some() {
        store.save()?;
        println!("  {} Credential removed", green(CHECK));
    }

    println!();
    println!("  {} Account {} removed.", green(CHECK), bold(&format!("'{name}'")));
    println!("  {} Restart to apply: {}", dim("·"), cyan("shunt start"));
    println!();
    Ok(())
}

/// Remove a `[[accounts]]` TOML block with the given name from config text.
fn remove_account_block(config: &str, name: &str) -> String {
    let marker = format!("name = \"{name}\"");

    // Split config into sections: preamble + one section per [[accounts]] block.
    // Each section starts at the [[accounts]] line (except the first which is the preamble).
    let mut sections: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in config.lines() {
        if line.trim() == "[[accounts]]" {
            sections.push(std::mem::take(&mut current));
            current = format!("[[accounts]]\n");
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    sections.push(current);

    // Drop the section that contains the marker, keep the rest.
    let mut result: String = sections.into_iter()
        .filter(|s| !s.contains(&marker))
        .collect();

    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

async fn cmd_start(
    config_override: Option<PathBuf>,
    host_override: Option<String>,
    port_override: Option<u16>,
) -> Result<()> {
    use std::io::Write as _;

    let config_p = config_override.clone().unwrap_or_else(config_path);

    // Auto-run setup if not yet configured
    if !config_p.exists() {
        cmd_setup(config_override.clone()).await?;
    }

    let mut config = crate::config::load_config(config_override.as_deref())?;

    let host = host_override.unwrap_or_else(|| config.server.host.clone());
    let port = port_override.unwrap_or(config.server.port);
    let n = config.accounts.len();
    let acct_label = if n == 1 { "account".to_string() } else { format!("{n} accounts") };

    // Print splash immediately so the user sees output right away
    print_splash(&[
        format!("{}  {}  {}  {}",
            bold_white("shunt"),
            dim(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            dim("·"),
            dim(&acct_label),
        ),
    ]);

    // Refresh any expired tokens (with visible progress + 10-second timeout each)
    for account in &mut config.accounts {
        if let Some(cred) = &account.credential {
            if cred.needs_refresh() {
                print!("  {} Refreshing token for '{}'… ", yellow("↻"), account.name);
                std::io::stdout().flush().ok();
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    refresh_token(cred),
                ).await {
                    Ok(Ok(fresh)) => {
                        println!("{}", green("done"));
                        let mut store = CredentialsStore::load();
                        store.accounts.insert(account.name.clone(), fresh.clone());
                        store.save().ok();
                        account.credential = Some(fresh);
                    }
                    Ok(Err(e)) => println!("{} ({})", yellow("failed"), dim(&e.to_string())),
                    Err(_)    => println!("{}", yellow("timed out — using existing token")),
                }
            }
        }
    }

    let lp = log_path();
    let _log_guard = crate::logging::setup(&lp, &config.server.log_level)?;

    let col = 13usize;
    println!("  {}  {}", dim(&pad("listening", col)), cyan(&format!("http://{host}:{port}")));
    println!("  {}  {}", dim(&pad("config", col)), dim(&config.config_file.display().to_string()));
    println!("  {}  {}", dim(&pad("logs", col)), dim(&lp.display().to_string()));
    println!();
    println!("  {}{}",
        dim("export ANTHROPIC_BASE_URL="),
        cyan(&format!("http://{host}:{port}")),
    );
    println!();

    // Share one StateStore between the proxy and the prefetch task
    use std::sync::Arc;
    let state = crate::state::StateStore::load(&crate::config::state_path());
    let app = crate::proxy::create_app_with_state(config.clone(), state.clone())?;
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port)).await?;
    write_pid();

    // Prefetch rate-limit data for accounts with no metrics yet (background)
    tokio::spawn(crate::proxy::prefetch_rate_limits(Arc::new(config), state));

    axum::serve(listener, app).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn cmd_status(config_override: Option<PathBuf>) -> Result<()> {
    let mut config = crate::config::load_config(config_override.as_deref())?;
    let proxy_url = format!("http://{}:{}", config.server.host, config.server.port);
    let status_url = format!("{proxy_url}/status");

    // Try to fetch live data from running proxy
    let live: Option<serde_json::Value> = reqwest::get(&status_url).await.ok()
        .and_then(|r| futures_executor_hack(r));

    // Back-fill missing emails (existing accounts set up before email support).
    // Fetch in parallel, persist any that are new.
    let mut store_dirty = false;
    let mut store = CredentialsStore::load();
    for acc in &mut config.accounts {
        if acc.credential.as_ref().map(|c| c.email.is_none()).unwrap_or(false) {
            let token = acc.credential.as_ref().map(|c| c.access_token.clone()).unwrap_or_default();
            if let Some(email) = crate::oauth::fetch_account_email(&token).await {
                if let Some(c) = acc.credential.as_mut() { c.email = Some(email.clone()); }
                if let Some(stored) = store.accounts.get_mut(&acc.name) {
                    stored.email = Some(email);
                    store_dirty = true;
                }
            }
        }
    }
    if store_dirty {
        store.save().ok();
    }

    let (proxy_dot, proxy_label) = if live.is_some() {
        (green(DOT), format!("{} running  {}", green(DOT), cyan(&proxy_url)))
    } else {
        (dim(EMPTY), format!("{} stopped  {}", dim(EMPTY), dim("shunt start to run")))
    };
    let _ = proxy_dot;

    let n = config.accounts.len();
    let acct_label = if n == 1 { "1 account".to_string() } else { format!("{n} accounts") };

    println!();
    println!("  {}  {}  {}  {}",
        bold_white("shunt"),
        dim(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        dim("·"),
        dim(&acct_label),
    );
    println!("  {proxy_label}");
    println!();

    // Pinned notice
    if let Some(pinned) = live.as_ref().and_then(|v| v["pinned"].as_str()) {
        println!("  {} Pinned to {}  {}",
            yellow("◆"), bold(pinned),
            dim("· shunt use auto to restore"),
        );
        println!();
    }

    for acc in &config.accounts {
        let live_acc = live.as_ref()
            .and_then(|v| v["accounts"].as_array())
            .and_then(|arr| arr.iter().find(|a| a["name"] == acc.name));

        let status = live_acc.and_then(|a| a["status"].as_str()).unwrap_or("offline");

        let (status_icon, status_text): (String, String) = match status {
            "available"       => (green(CHECK),  green("available")),
            "cooling"         => (yellow("↻"),   yellow("cooling")),
            "disabled"        => (red(CROSS),    red("disabled")),
            "reauth_required" => (red(CROSS),    red("session expired")),
            "needs restart"   => (yellow("↻"),   yellow("needs restart")),
            _ => match &acc.credential {
                None        => (red(CROSS),  red("no credential")),
                Some(c) if c.needs_refresh() => (yellow(CROSS), dim("offline")),
                _           => (dim(EMPTY),  dim("offline")),
            },
        };

        let tokens_str = live_acc
            .and_then(|a| a["tokens_used"]["total"].as_u64())
            .map(|t| dim(&format!("{} tokens used", term::fmt_tokens(t))))
            .unwrap_or_default();

        let email_str = acc.credential.as_ref().and_then(|c| c.email.as_deref()).unwrap_or("");
        let plan_label = match acc.plan_type.to_lowercase().as_str() {
            "max" | "claude_max" => "Max",
            "team"               => "Team",
            _                    => "Pro",
        };

        // Account header row
        println!("  {}  {}  {}  {}  {}",
            status_icon,
            bold(&acc.name),
            dim(plan_label),
            dim(email_str),
            status_text,
        );
        if !tokens_str.is_empty() {
            println!("     {tokens_str}");
        }

        // Rate limit bars
        if let Some(rl) = live_acc.and_then(|a| a["rate_limit"].as_object()) {
            let util_5h  = rl.get("utilization_5h").and_then(|v| v.as_f64());
            let reset_5h = rl.get("reset_5h").and_then(|v| v.as_u64());
            let status_5h = rl.get("status_5h").and_then(|v| v.as_str()).unwrap_or("allowed");
            let util_7d  = rl.get("utilization_7d").and_then(|v| v.as_f64());
            let reset_7d = rl.get("reset_7d").and_then(|v| v.as_u64());
            let status_7d = rl.get("status_7d").and_then(|v| v.as_str()).unwrap_or("allowed");
            let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()).unwrap_or(0);

            let print_window = |label: &str, util: Option<f64>, reset: Option<u64>, wstatus: &str| {
                if reset.map(|t| t <= now_secs).unwrap_or(false) {
                    let ago = reset.map(|t| {
                        let s = now_secs.saturating_sub(t);
                        format!("  reset {} ago", term::fmt_duration_ms(s * 1000))
                    }).unwrap_or_default();
                    println!("     {}  {}  {}", dim(label), green("fresh"), dim(&ago));
                } else if let Some(u) = util {
                    let remaining = 100u64.saturating_sub((u * 100.0) as u64);
                    let bar = util_bar(u, 20);
                    let reset_str = reset.and_then(|t| secs_until(t))
                        .map(|s| format!("  resets in {}", term::fmt_duration_ms(s * 1000)))
                        .unwrap_or_default();
                    let exhausted = wstatus == "exhausted";
                    let pct_str = if exhausted { red("exhausted") } else { format!("{}%", cyan(&remaining.to_string())) };
                    println!("     {}  {}  {} remaining{}", dim(label), bar, pct_str, dim(&reset_str));
                }
            };

            if util_5h.is_some() || reset_5h.is_some() {
                print_window("5h", util_5h, reset_5h, status_5h);
            }
            if util_7d.is_some() || reset_7d.is_some() {
                print_window("7d", util_7d, reset_7d, status_7d);
            }
        } else if acc.credential.is_none() {
            println!("     {} run {} to authorize",
                dim("·"), cyan(&format!("shunt add-account {}", acc.name)));
        } else if status == "reauth_required" {
            println!("     {} run {} to re-authorize",
                dim("·"), cyan(&format!("shunt add-account {}", acc.name)));
        } else if live.is_some() && live_acc.is_some() {
            println!("     {}", dim("no rate-limit data yet — make a request first"));
        }

        println!();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// use (pin account)
// ---------------------------------------------------------------------------

async fn cmd_use(config_override: Option<PathBuf>, account: Option<String>) -> Result<()> {
    let config = crate::config::load_config(config_override.as_deref())?;
    let use_url = format!("http://{}:{}/use", config.server.host, config.server.port);

    // Fetch live state for utilization info
    let live: Option<serde_json::Value> = reqwest::get(
        &format!("http://{}:{}/status", config.server.host, config.server.port)
    ).await.ok().and_then(|r| futures_executor_hack(r));

    let current_pinned = live.as_ref()
        .and_then(|v| v["pinned"].as_str())
        .map(|s| s.to_owned());

    // Build menu items
    let mut items: Vec<term::SelectItem> = config.accounts.iter().map(|a| {
        let live_acc = live.as_ref()
            .and_then(|v| v["accounts"].as_array())
            .and_then(|arr| arr.iter().find(|x| x["name"] == a.name));

        let status = live_acc.and_then(|x| x["status"].as_str()).unwrap_or("offline");
        let util = live_acc.and_then(|x| x["rate_limit"]["utilization_5h"].as_f64());
        let is_pinned = current_pinned.as_deref() == Some(&a.name);

        let status_str = match status {
            "reauth_required" => red("session expired"),
            "disabled"        => red("disabled"),
            "cooling"         => yellow("cooling"),
            "available"       => {
                match util {
                    Some(u) => {
                        let rem = 100u64.saturating_sub((u * 100.0) as u64);
                        green(&format!("{}% remaining", rem))
                    }
                    None => dim("fresh").to_string(),
                }
            }
            _ => dim("offline").to_string(),
        };

        let email = a.credential.as_ref().and_then(|c| c.email.as_deref()).unwrap_or("");
        let pin = if is_pinned { format!("  {}", yellow("▶ active")) } else { String::new() };

        term::SelectItem {
            label: format!("{}  {}  {}{}", bold(&pad(&a.name, 12)), dim(&pad(email, 32)), status_str, pin),
            value: a.name.clone(),
        }
    }).collect();

    let auto_marker = if current_pinned.is_none() { format!("  {}", yellow("▶ active")) } else { String::new() };
    items.push(term::SelectItem {
        label: format!("{}  {}{}", bold(&pad("auto", 12)), dim("least-utilization routing"), auto_marker),
        value: "auto".to_owned(),
    });

    // Determine initial cursor position (current pinned account or auto)
    let initial = current_pinned.as_ref()
        .and_then(|p| items.iter().position(|it| &it.value == p))
        .unwrap_or(items.len() - 1);

    // If account name was given directly, skip the picker
    let chosen = if let Some(name) = account {
        name
    } else {
        match term::select("Route traffic to:", &items, initial) {
            Some(v) => v,
            None => return Ok(()), // cancelled
        }
    };

    // Validate
    let is_auto = chosen == "auto";
    if !is_auto && !config.accounts.iter().any(|a| a.name == chosen) {
        let names: Vec<_> = config.accounts.iter().map(|a| a.name.as_str()).collect();
        anyhow::bail!("Unknown account '{}'. Available: {}", chosen, names.join(", "));
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(&use_url)
        .json(&serde_json::json!({ "account": chosen }))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            if is_auto {
                println!("  {} Automatic routing restored", green(CHECK));
            } else {
                println!("  {} Pinned to {}  ·  {}", green(CHECK), bold(&chosen), dim("shunt use auto to restore"));
            }
            println!();
        }
        Ok(r) => {
            let body = r.text().await.unwrap_or_default();
            anyhow::bail!("Proxy returned error: {body}");
        }
        Err(_) => {
            anyhow::bail!("Proxy is not running — start it with: shunt start");
        }
    }
    Ok(())
}

/// Synchronously awaits a reqwest response to get its JSON.
fn futures_executor_hack(resp: reqwest::Response) -> Option<serde_json::Value> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            resp.json::<serde_json::Value>().await.ok()
        })
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn print_splash(info: &[String]) {
    println!();
    for line in info {
        if !line.is_empty() {
            println!("  {line}");
        }
    }
    println!();
}

/// Capacity bar — `util` is 0.0–1.0; filled blocks show REMAINING capacity.
/// Green = plenty left, yellow = getting low, red = nearly exhausted.
fn util_bar(util: f64, width: usize) -> String {
    let used = (util.clamp(0.0, 1.0) * width as f64).round() as usize;
    let free = width.saturating_sub(used);
    // filled = remaining, empty = used — so a full bar means lots of quota left
    let bar = format!("{}{}", "█".repeat(free), "░".repeat(used));
    let pct = (util * 100.0) as u64;
    if pct < 50 { green(&bar) } else if pct < 80 { yellow(&bar) } else { red(&bar) }
}

/// Seconds until a Unix-epoch reset timestamp. Returns None if past or zero.
fn secs_until(epoch_secs: u64) -> Option<u64> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    epoch_secs.checked_sub(now).filter(|&s| s > 0)
}

fn write_pid() {
    let p = pid_path();
    if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
    let _ = std::fs::write(&p, std::process::id().to_string());
}

/// PIDs of processes listening on the given port.
fn port_pids(port: u16) -> Vec<u32> {
    let out = std::process::Command::new("lsof")
        .args(["-ti", &format!(":{port}")])
        .output();
    let Ok(out) = out else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

#[allow(dead_code)]
fn kill_port(port: u16) -> bool {
    let pids = port_pids(port);
    let mut any = false;
    for pid in pids {
        if std::process::Command::new("kill").arg(pid.to_string()).status().map(|s| s.success()).unwrap_or(false) {
            any = true;
        }
    }
    any
}

/// Pad a string to width using spaces (ignores ANSI codes — use before coloring).
fn pad(s: &str, width: usize) -> String {
    let visible_len = strip_ansi(s).len();
    if visible_len >= width {
        s.to_owned()
    } else {
        format!("{s}{}", " ".repeat(width - visible_len))
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() { break; }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn offer_shell_export() -> Result<()> {
    use std::io::{self, Write};

    let line = "export ANTHROPIC_BASE_URL=http://127.0.0.1:8082";
    println!();
    println!("  To use with Claude Code, set:");
    println!("    {}", cyan(line));

    let profile = detect_shell_profile();
    let prompt = match &profile {
        Some(p) => format!("  Add to {}? [Y/n]: ", dim(&p.display().to_string())),
        None => "  Add to your shell profile? [Y/n]: ".into(),
    };

    print!("{prompt}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;

    if matches!(buf.trim().to_lowercase().as_str(), "n" | "no") {
        return Ok(());
    }

    let path = match profile {
        Some(p) => p,
        None => {
            println!("  {} Could not detect shell profile. Add manually.", dim("·"));
            return Ok(());
        }
    };

    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        if contents.contains("ANTHROPIC_BASE_URL") {
            println!("  {} Already set in {}", CHECK, dim(&path.display().to_string()));
            return Ok(());
        }
    }

    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    #[allow(unused_imports)]
    use std::io::Write as _;
    writeln!(f, "\n# Added by shunt")?;
    writeln!(f, "{line}")?;
    println!("  {} Added to {} — restart shell or: {}", green(CHECK),
        dim(&path.display().to_string()),
        cyan(&format!("source {}", path.display())));

    Ok(())
}

fn detect_shell_profile() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    if let Ok(shell) = std::env::var("SHELL") {
        if shell.contains("zsh")  { return Some(home.join(".zshrc")); }
        if shell.contains("fish") { return Some(home.join(".config/fish/config.fish")); }
        if shell.contains("bash") {
            let p = home.join(".bash_profile");
            return Some(if p.exists() { p } else { home.join(".bashrc") });
        }
    }
    for f in &[".zshrc", ".bashrc", ".bash_profile"] {
        let p = home.join(f);
        if p.exists() { return Some(p); }
    }
    None
}
