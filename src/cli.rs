use anyhow::{bail, Context as _, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{config_path, config_template, credentials_path, log_path, pid_path, CredentialsStore};
use crate::oauth::{claude_credentials_path, read_claude_credentials, refresh_token, revoke_token, run_oauth_flow};
use crate::term::{self, bold, bold_white, brand_green, cyan, dark_green, dim, green, green_bold, red, yellow, CHECK, CROSS, DIAMOND, DOT, EMPTY};

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
        /// Enable debug-level logging (shows routing decisions and token refresh details)
        #[arg(long)]
        verbose: bool,
        /// Internal: running as background daemon (do not use directly)
        #[arg(long, hide = true)]
        daemon: bool,
    },
    /// Stop the running proxy daemon
    Stop,
    /// Restart the proxy daemon (stop then start)
    Restart {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print current config and proxy status
    Status {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Tail the proxy log file
    ///
    /// Examples:
    ///   shunt logs           — last 50 lines
    ///   shunt logs -f        — follow in real time
    ///   shunt logs -n 100    — last 100 lines
    Logs {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Follow log output in real time (like tail -f)
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,
    },
    /// Import the current Claude Code session as an additional account
    AddAccount {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Name for this account (e.g. "secondary", "work"). Prompted if omitted.
        name: Option<String>,
        /// Provider: "anthropic" or "openai". Prompted interactively if omitted.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Remove an account from the pool
    RemoveAccount {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Name of the account to remove (omit to pick interactively)
        name: Option<String>,
    },
    /// Enable remote access — expose the proxy to other devices
    Share {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Create a public tunnel via Cloudflare (works over any network, not just LAN)
        #[arg(long)]
        tunnel: bool,
        /// Disable remote access and revert to localhost-only
        #[arg(long)]
        stop: bool,
    },
    /// Log out of an account — clears stored credentials (keeps account in config)
    ///
    /// Examples:
    ///   shunt logout           — interactive picker
    ///   shunt logout work      — log out 'work'
    ///   shunt logout --all     — log out every account
    Logout {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Account name to log out. Omit to pick interactively.
        name: Option<String>,
        /// Log out all accounts at once
        #[arg(long)]
        all: bool,
    },
    /// Live fullscreen TUI dashboard — shows account utilization and request log
    Monitor {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Update shunt to the latest release
    Update,
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
    /// Upload credentials to the relay for transfer to another device
    ///
    /// Examples:
    ///   shunt push              — encrypt and upload, prints a transfer code
    Push {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Set up this device using a transfer code from `shunt push`
    ///
    /// Examples:
    ///   shunt login SH-a3f2b1c4d5e6f7a8b9
    Login {
        /// Transfer code printed by `shunt push` on another device
        code: String,
    },
    /// Print shell completion script
    ///
    /// Examples:
    ///   shunt completions zsh  >> ~/.zshrc
    ///   shunt completions bash >> ~/.bashrc
    ///   shunt completions fish > ~/.config/fish/completions/shunt.fish
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Setup { config } => cmd_setup(config).await,
        Command::Start { config, host, port, foreground, verbose, daemon } => cmd_start(config, host, port, foreground, verbose, daemon).await,
        Command::Stop => cmd_stop().await,
        Command::Restart { config } => cmd_restart(config).await,
        Command::Status { config } => cmd_status(config).await,
        Command::Logs { config, follow, lines } => cmd_logs(config, follow, lines).await,
        Command::AddAccount { config, name, provider } => cmd_add_account(config, name, provider.as_deref()).await,
        Command::RemoveAccount { config, name } => cmd_remove_account(config, name).await,
        Command::Logout { config, name, all } => cmd_logout(config, name, all).await,
        Command::Monitor { config } => cmd_monitor(config).await,
        Command::Update => cmd_update().await,
        Command::Share { config, tunnel, stop } => cmd_share(config, tunnel, stop).await,
        Command::Use { config, account } => cmd_use(config, account).await,
        Command::Push { config } => cmd_push(config).await,
        Command::Login { code } => cmd_login(code).await,
        Command::Completions { shell } => { cmd_completions(shell); Ok(()) }
    }
}

// ---------------------------------------------------------------------------
// setup
// ---------------------------------------------------------------------------

pub async fn cmd_setup(config_override: Option<PathBuf>) -> Result<()> {
    let config_p = config_override.clone().unwrap_or_else(config_path);

    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
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

async fn cmd_add_account(
    config_override: Option<PathBuf>,
    name_arg: Option<String>,
    provider_arg: Option<&str>,
) -> Result<()> {
    use crate::provider::Provider;

    let config_p = config_override.clone().unwrap_or_else(config_path);
    if !config_p.exists() {
        bail!("No config found. Run `shunt setup` first.");
    }

    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        "Add account".to_string(),
        String::new(),
    ]);

    // ── Step 1: choose provider ──────────────────────────────────────────────
    let provider = if let Some(p) = provider_arg {
        Provider::from_str(p)
    } else {
        let items = vec![
            term::SelectItem {
                label: format!("{}  {}",
                    bold("Claude Code"),
                    dim("(claude.ai — Anthropic)")),
                value: "anthropic".into(),
            },
            term::SelectItem {
                label: format!("{}  {}",
                    bold("Codex"),
                    dim("(chatgpt.com — OpenAI)")),
                value: "openai".into(),
            },
        ];
        match term::select("Which provider?", &items, 0) {
            Some(v) => Provider::from_str(&v),
            None => return Ok(()),
        }
    };

    println!();

    // ── Step 2: choose name ──────────────────────────────────────────────────
    let existing_config = std::fs::read_to_string(&config_p)?;
    let store = CredentialsStore::load();

    let (name, already_in_config) = if let Some(n) = name_arg {
        let in_config = existing_config.contains(&format!("name = \"{n}\""));
        let has_cred  = store.accounts.contains_key(&n);
        let is_expired = store.accounts.get(&n).map(|c| c.needs_refresh()).unwrap_or(false);
        let is_auth_failed = crate::state::StateStore::load(&crate::config::state_path())
            .account_states().get(&n).map(|s| s.auth_failed).unwrap_or(false);
        if in_config && has_cred && !is_expired && !is_auth_failed {
            bail!("Account '{}' already has a valid credential.", n);
        }
        (n, in_config)
    } else {
        // Check for existing config entries that are missing credentials for this provider
        let config = crate::config::load_config(config_override.as_deref())?;
        let missing: Vec<_> = config.accounts.iter()
            .filter(|a| a.provider == provider && a.credential.is_none())
            .collect();

        match missing.len() {
            1 => {
                println!("  {} Authorizing account {}", yellow("↻"), bold(&format!("'{}'", missing[0].name)));
                println!();
                (missing[0].name.clone(), true)
            }
            n if n > 1 => {
                let items: Vec<term::SelectItem> = missing.iter().map(|a| term::SelectItem {
                    label: bold(&a.name).to_string(),
                    value: a.name.clone(),
                }).collect();
                match term::select("Which account to authorize?", &items, 0) {
                    Some(v) => (v, true),
                    None => return Ok(()),
                }
            }
            _ => {
                // All configured — prompt for a new name
                print!("  {} Account name: ", dim("·"));
                use std::io::Write;
                std::io::stdout().flush().ok();
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let n = input.trim().to_string();
                if n.is_empty() { bail!("Account name cannot be empty."); }
                (n, false)
            }
        }
    };

    // ── Step 3: OAuth flow ───────────────────────────────────────────────────
    let mut cred = match provider {
        Provider::Anthropic => run_oauth_flow().await?,
        Provider::OpenAI    => crate::oauth::run_openai_oauth_flow().await?,
    };

    // Fetch email (non-fatal)
    let email = match provider {
        Provider::Anthropic => crate::oauth::fetch_account_email(&cred.access_token).await,
        Provider::OpenAI    => crate::oauth::fetch_openai_account_email(&cred.access_token).await,
    };
    if let Some(ref e) = email {
        println!("  {} Signed in as {}", green(CHECK), bold(e));
    }
    cred.email = email;

    // ── Step 4: persist ──────────────────────────────────────────────────────
    if !already_in_config {
        let mut config_text = existing_config;
        match provider {
            Provider::Anthropic => config_text.push_str(&format!(
                "\n[[accounts]]\nname = \"{name}\"\nplan_type = \"pro\"\n"
            )),
            Provider::OpenAI => config_text.push_str(&format!(
                "\n[[accounts]]\nname = \"{name}\"\nplan_type = \"pro\"\nprovider = \"openai\"\n"
            )),
        }
        std::fs::write(&config_p, &config_text)?;
    }

    let mut store = CredentialsStore::load();
    store.accounts.insert(name.clone(), cred.clone());
    store.save()?;

    // Keep ~/.codex/auth.json in sync so the Codex CLI works without re-login.
    if cred.id_token.is_some() {
        crate::oauth::write_codex_auth_file(&cred);
    }

    println!();
    println!("  {} Account {} added.", green(CHECK), bold(&format!("'{name}'")));
    offer_restart(config_override).await;
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
        let removable: Vec<_> = config.accounts.iter().collect();
        if removable.is_empty() {
            bail!("No accounts to remove.");
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

    let config_text = std::fs::read_to_string(&config_p)?;
    if !config_text.contains(&format!("name = \"{name}\"")) {
        bail!("Account '{name}' not found.");
    }

    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
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
    offer_restart(config_override).await;
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

async fn cmd_logout(config_override: Option<PathBuf>, name: Option<String>, all: bool) -> Result<()> {
    let config_p = config_override.clone().unwrap_or_else(config_path);
    if !config_p.exists() {
        bail!("No config found. Run `shunt setup` first.");
    }

    let config = crate::config::load_config(config_override.as_deref())?;

    // Collect account names to log out
    let names: Vec<String> = if all {
        config.accounts.iter()
            .filter(|a| a.credential.is_some())
            .map(|a| a.name.clone())
            .collect()
    } else if let Some(n) = name {
        if !config.accounts.iter().any(|a| a.name == n) {
            bail!("Account '{n}' not found.");
        }
        vec![n]
    } else {
        // Interactive picker — show only accounts that have credentials
        let with_cred: Vec<_> = config.accounts.iter()
            .filter(|a| a.credential.is_some())
            .collect();
        if with_cred.is_empty() {
            println!("  {} No logged-in accounts.", dim("·"));
            println!();
            return Ok(());
        }
        let items: Vec<term::SelectItem> = with_cred.iter().map(|a| {
            let email = a.credential.as_ref().and_then(|c| c.email.as_deref()).unwrap_or("");
            term::SelectItem {
                label: format!("{}  {}", bold(&pad(&a.name, 12)), dim(&pad(email, 32))),
                value: a.name.clone(),
            }
        }).collect();
        match term::select("Log out account:", &items, 0) {
            Some(v) => vec![v],
            None => return Ok(()),
        }
    };

    if names.is_empty() {
        println!("  {} No logged-in accounts.", dim("·"));
        println!();
        return Ok(());
    }

    let label = if names.len() == 1 {
        format!("account {}", bold(&format!("'{}'", names[0])))
    } else {
        format!("{} accounts", bold(&names.len().to_string()))
    };

    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        format!("Logging out {label}"),
        String::new(),
    ]);

    let mut store = CredentialsStore::load();

    for name in &names {
        // Revoke token on the server (best-effort)
        if let Some(cred) = store.accounts.get(name) {
            print!("  {} Revoking '{}' token… ", dim("↻"), name);
            use std::io::Write;
            std::io::stdout().flush().ok();
            if revoke_token(&cred.access_token).await {
                println!("{}", green("done"));
            } else {
                println!("{}", dim("(server did not confirm — cleared locally)"));
            }
        }

        // Remove credential from local store
        store.accounts.remove(name);
        println!("  {} Credential for '{}' removed", green(CHECK), name);
    }

    store.save()?;

    println!();
    println!("  {} Logged out {}.", green(CHECK), label);
    println!("  {} To re-authorize: {}", dim("·"), cyan("shunt add-account"));
    println!();
    Ok(())
}

/// Remove a `[[accounts]]` TOML block with the given name from config text.
/// Uses toml_edit for correct structured editing that handles comments and edge cases.
fn remove_account_block(config: &str, name: &str) -> String {
    let mut doc = match config.parse::<toml_edit::DocumentMut>() {
        Ok(d) => d,
        Err(_) => return config.to_owned(), // unparseable — leave unchanged
    };

    if let Some(item) = doc.get_mut("accounts") {
        if let Some(arr) = item.as_array_of_tables_mut() {
            // Collect indices to remove in reverse order so removal doesn't shift indices
            let to_remove: Vec<usize> = arr.iter()
                .enumerate()
                .filter(|(_, t)| t.get("name").and_then(|v| v.as_str()) == Some(name))
                .map(|(i, _)| i)
                .collect();
            for i in to_remove.into_iter().rev() {
                arr.remove(i);
            }
        }
    }

    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CONFIG: &str = r#"
[server]
port = 8082

[[accounts]]
name = "alice"
plan_type = "pro"

[[accounts]]
name = "bob"
plan_type = "max"

[[accounts]]
name = "charlie"
plan_type = "pro"
"#;

    #[test]
    fn test_remove_account_block_removes_target() {
        let result = remove_account_block(SAMPLE_CONFIG, "bob");
        // bob must be gone
        assert!(!result.contains("\"bob\"") && !result.contains("'bob'") && !result.contains("bob"),
            "removed account must not appear: {result}");
        // others must remain
        assert!(result.contains("alice"));
        assert!(result.contains("charlie"));
    }

    #[test]
    fn test_remove_account_block_preserves_others() {
        let result = remove_account_block(SAMPLE_CONFIG, "alice");
        assert!(!result.contains("alice"), "alice must be removed");
        assert!(result.contains("bob"),     "bob must remain");
        assert!(result.contains("charlie"), "charlie must remain");
    }

    #[test]
    fn test_remove_account_block_noop_when_not_found() {
        let result = remove_account_block(SAMPLE_CONFIG, "dave");
        // All three must still be present
        assert!(result.contains("alice"));
        assert!(result.contains("bob"));
        assert!(result.contains("charlie"));
    }

    #[test]
    fn test_remove_account_block_last_account() {
        let cfg = "[[accounts]]\nname = \"only\"\nplan_type = \"pro\"\n";
        let result = remove_account_block(cfg, "only");
        assert!(!result.contains("only"), "sole account must be removed");
    }

    #[test]
    fn test_remove_account_block_handles_unparseable_input() {
        let bad = "not valid [[toml{{ garbage";
        let result = remove_account_block(bad, "anything");
        // Must return input unchanged, not panic
        assert_eq!(result, bad);
    }

    #[test]
    fn test_remove_account_block_with_inline_comment() {
        let cfg = "[[accounts]]\nname = \"alice\" # main account\nplan_type = \"pro\"\n\n[[accounts]]\nname = \"bob\"\nplan_type = \"max\"\n";
        let result = remove_account_block(cfg, "alice");
        assert!(!result.contains("alice"));
        assert!(result.contains("bob"));
    }
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

async fn cmd_start(
    config_override: Option<PathBuf>,
    host_override: Option<String>,
    port_override: Option<u16>,
    foreground: bool,
    verbose: bool,
    daemon: bool,
) -> Result<()> {
    let config_p = config_override.clone().unwrap_or_else(config_path);

    // ── Daemon mode: internal re-exec, no user output ────────────────────────
    if daemon {
        if !config_p.exists() { return Ok(()); }
        let mut config = crate::config::load_config(config_override.as_deref())?;
        let host = host_override.unwrap_or_else(|| config.server.host.clone());
        let port = port_override.unwrap_or(config.server.port);

        for account in &mut config.accounts {
            if let Some(cred) = &account.credential {
                if cred.needs_refresh() {
                    if let Ok(Ok(fresh)) = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        account.provider.refresh_token(cred),
                    ).await {
                        let mut store = CredentialsStore::load();
                        store.accounts.insert(account.name.clone(), fresh.clone());
                        store.save().ok();
                        account.credential = Some(fresh);
                    }
                }
            }
        }

        let lp = log_path();
        let log_level = if verbose { "debug" } else { config.server.log_level.as_str() };
        crate::logging::prune_old_logs(&lp, 7);
        let _log_guard = crate::logging::setup(&lp, log_level)?;
        let state = crate::state::StateStore::load(&crate::config::state_path());
        write_pid();
        serve_all_providers(config, state, &host, port).await?;
        return Ok(());
    }

    // ── Auto-setup on first run ───────────────────────────────────────────────
    if !config_p.exists() {
        cmd_setup_auto(config_override.clone()).await?;
    }

    let config = crate::config::load_config(config_override.as_deref())?;
    let host = host_override.clone().unwrap_or_else(|| config.server.host.clone());
    let port = port_override.unwrap_or(config.server.port);

    // Kill any previous instance on this port
    for pid in port_pids(port) {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    }
    if !port_pids(port).is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(400));
    }

    // ── Foreground mode (debugging) ───────────────────────────────────────────
    if foreground {
        use std::io::Write as _;
        let mut config = config;
        let account_names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
        print_routing_header(&account_names, &[
            format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
            dim("foreground").to_string(),
        ]);
        for account in &mut config.accounts {
            if let Some(cred) = &account.credential {
                if cred.needs_refresh() {
                    print!("  {} Refreshing '{}'… ", yellow("↻"), account.name);
                    std::io::stdout().flush().ok();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        account.provider.refresh_token(cred),
                    ).await {
                        Ok(Ok(fresh)) => {
                            println!("{}", green("done"));
                            let mut store = CredentialsStore::load();
                            store.accounts.insert(account.name.clone(), fresh.clone());
                            store.save().ok();
                            account.credential = Some(fresh);
                        }
                        Ok(Err(e)) => println!("{}", yellow(&format!("failed ({})", e))),
                        Err(_)    => println!("{}", yellow("timed out")),
                    }
                }
            }
        }
        let lp = log_path();
        let log_level = if verbose { "debug" } else { config.server.log_level.as_str() };
        crate::logging::prune_old_logs(&lp, 7);
        let _log_guard = crate::logging::setup(&lp, log_level)?;
        let col = 13usize;
        for (p, addr) in listener_addrs(&config.accounts, &host, port) {
            println!("  {}  {} {}", dim(&pad("listening", col)), dim(&format!("[{p}]")), green_bold(&addr));
        }
        println!("  {}  {}", dim(&pad("logs", col)), dim(&lp.display().to_string()));
        println!();
        let state = crate::state::StateStore::load(&crate::config::state_path());
        write_pid();
        serve_all_providers(config, state, &host, port).await?;
        return Ok(());
    }

    // ── Background mode (default) ─────────────────────────────────────────────
    let exe = std::env::current_exe().context("cannot locate current executable")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("start").arg("--daemon");
    if let Some(ref p) = config_override { cmd.args(["--config", &p.display().to_string()]); }
    if let Some(ref h) = host_override   { cmd.args(["--host", h]); }
    if let Some(p) = port_override       { cmd.args(["--port", &p.to_string()]); }
    if verbose                           { cmd.arg("--verbose"); }
    cmd.stdin(std::process::Stdio::null())
       .stdout(std::process::Stdio::null())
       .stderr(std::process::Stdio::null())
       .spawn()
       .context("failed to start proxy in background")?;

    // Wait until the proxy is accepting connections (up to 8 s)
    let ready = wait_for_health(&host, port, 8).await;

    // Auto-write ANTHROPIC_BASE_URL to shell profile (silent if already there)
    auto_write_shell_export(port);

    let account_names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
    let status_line = if ready {
        format!("{}  {}  {}", green(DOT), green_bold("running"), cyan(&format!("http://{host}:{port}")))
    } else {
        format!("{}  {}  {}", yellow(DOT), yellow("starting"), dim(&format!("http://{host}:{port}")))
    };
    print_routing_header(&account_names, &[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        status_line,
    ]);

    Ok(())
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

async fn cmd_stop() -> Result<()> {
    let pid_p = pid_path();
    let content = match std::fs::read_to_string(&pid_p) {
        Ok(c) => c,
        Err(_) => {
            println!("  {} Proxy is not running.", dim("·"));
            println!();
            return Ok(());
        }
    };
    let pid = match content.trim().parse::<u32>() {
        Ok(p) => p,
        Err(_) => {
            let _ = std::fs::remove_file(&pid_p);
            println!("  {} Proxy is not running.", dim("·"));
            println!();
            return Ok(());
        }
    };
    if !is_shunt_pid(pid) {
        let _ = std::fs::remove_file(&pid_p);
        println!("  {} Proxy is not running.", dim("·"));
        println!();
        return Ok(());
    }

    // SIGTERM — let axum drain connections cleanly
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };

    // Wait up to 3 s for clean exit, then SIGKILL
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !is_shunt_pid(pid) { break; }
    }
    if is_shunt_pid(pid) {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let _ = std::fs::remove_file(&pid_p);
    println!("  {} Proxy stopped.", green(CHECK));
    println!();
    Ok(())
}

fn is_shunt_pid(pid: u32) -> bool {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
    else { return false };
    String::from_utf8_lossy(&out.stdout).trim().contains("shunt")
}

// ---------------------------------------------------------------------------
// restart
// ---------------------------------------------------------------------------

async fn cmd_restart(config_override: Option<PathBuf>) -> Result<()> {
    cmd_stop().await?;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cmd_start(config_override, None, None, false, false, false).await
}

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

async fn cmd_logs(_config_override: Option<PathBuf>, follow: bool, lines: usize) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};

    let log = log_path();
    if !log.exists() {
        println!("  {} No log file found.", dim("·"));
        println!("  {} Start the proxy first: {}", dim("·"), cyan("shunt start"));
        println!();
        return Ok(());
    }

    let file = std::fs::File::open(&log)?;
    let mut reader = BufReader::new(file);

    // Use a ring buffer so we only keep the last N lines in memory
    // regardless of how large the log file is.
    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::with_capacity(lines + 1);
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        if ring.len() >= lines {
            ring.pop_front();
        }
        ring.push_back(std::mem::take(&mut line));
    }
    for l in &ring {
        print!("{l}");
    }
    std::io::stdout().flush().ok();

    if !follow {
        return Ok(());
    }

    // Follow mode — poll for new content
    eprintln!("{}", dim("--- following (Ctrl+C to stop) ---"));
    loop {
        line.clear();
        if reader.read_line(&mut line)? > 0 {
            print!("{line}");
            std::io::stdout().flush().ok();
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// push
// ---------------------------------------------------------------------------

async fn cmd_push(config_override: Option<PathBuf>) -> Result<()> {
    use crate::sync::{encrypt_bundle, generate_code, push_to_relay, SyncBundle};

    let config_p = config_override.clone().unwrap_or_else(config_path);
    if !config_p.exists() {
        bail!("No config found. Run `shunt setup` first.");
    }

    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        dim("Push credentials to relay").to_string(),
        String::new(),
    ]);

    let config = crate::config::load_config(config_override.as_deref())?;
    let relay_url = &config.server.relay_url;

    // Load raw config text + credentials
    let config_toml = std::fs::read_to_string(&config_p)?;
    let store = crate::config::CredentialsStore::load();

    if store.accounts.is_empty() {
        bail!("No credentials found. Run `shunt setup` or `shunt add-account` first.");
    }

    let n = store.accounts.len();
    let names: Vec<_> = store.accounts.keys().cloned().collect();
    println!("  {} Encrypting {} account{}…",
        dim("·"), bold(&n.to_string()),
        if n == 1 { "" } else { "s" });

    let bundle = SyncBundle { config_toml, accounts: store.accounts };
    let code = generate_code();
    let payload = encrypt_bundle(&bundle, &code)?;

    print!("  {} Uploading to relay… ", dim("↑"));
    use std::io::Write as _;
    std::io::stdout().flush().ok();

    push_to_relay(&code, &payload, relay_url).await?;
    println!("{}", green("done"));

    println!();
    println!("  {} Transfer code:", green(CHECK));
    println!();
    println!("      {}", bold_white(&code));
    println!();
    println!("  {} Accounts: {}", dim("·"), dim(&names.join(", ")));
    println!("  {} Expires in 24h — one-time use", dim("·"));
    println!();
    println!("  On the new device, run:");
    println!("    {}", cyan(&format!("shunt login {code}")));
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

async fn cmd_login(code: String) -> Result<()> {
    use crate::sync::{decrypt_bundle, pull_from_relay, validate_code};

    validate_code(&code)?;

    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        dim("Login — applying credentials from relay").to_string(),
        String::new(),
    ]);

    // Resolve relay URL: use existing config if present, else env var or default
    let relay_url = crate::config::load_config(None)
        .map(|c| c.server.relay_url.clone())
        .unwrap_or_else(|_| {
            std::env::var("SHUNT_RELAY_URL")
                .unwrap_or_else(|_| "https://relay.ramcharan.shop".into())
        });

    print!("  {} Downloading from relay… ", dim("↓"));
    use std::io::Write as _;
    std::io::stdout().flush().ok();

    let payload = pull_from_relay(&code, &relay_url).await?;
    println!("{}", green("done"));

    print!("  {} Decrypting… ", dim("·"));
    std::io::stdout().flush().ok();
    let bundle = decrypt_bundle(&payload, &code)?;
    println!("{}", green("done"));

    let config_p = config_path();
    let account_names: Vec<_> = bundle.accounts.keys().cloned().collect();

    // Strip sharing settings — remote_key and host=0.0.0.0 are tied to the
    // source device and must not carry over to a new local install.
    let config_toml: String = bundle.config_toml
        .lines()
        .filter(|l| !l.trim_start().starts_with("remote_key"))
        .map(|l| if l.trim() == "host = \"0.0.0.0\"" { "host = \"127.0.0.1\"" } else { l })
        .collect::<Vec<_>>()
        .join("\n") + "\n";

    // If config already exists, confirm overwrite
    if config_p.exists() {
        use std::io::{self, Write};
        print!("  {} Config already exists — overwrite? [y/N]: ", yellow("!"));
        io::stdout().flush()?;
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        if !matches!(buf.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("  {} Cancelled.", dim("·"));
            println!();
            return Ok(());
        }
    }

    // Write config
    if let Some(parent) = config_p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&config_p, &config_toml)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config_p, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("  {} Config written", green(CHECK));

    // Merge credentials (bundle wins; keeps any extra local accounts)
    let mut store = crate::config::CredentialsStore::load();
    for (name, cred) in bundle.accounts {
        store.accounts.insert(name, cred);
    }
    store.save()?;
    println!("  {} Credentials saved ({} accounts: {})",
        green(CHECK),
        account_names.len(),
        account_names.join(", "));

    offer_shell_export()?;

    println!();
    println!("  {} Run {} to start.", green(CHECK), cyan("shunt start"));
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// completions
// ---------------------------------------------------------------------------

fn cmd_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    clap_complete::generate(shell, &mut Cli::command(), "shunt", &mut std::io::stdout());
}

/// Non-interactive setup called from `cmd_start`.
/// Imports the existing Claude Code session silently.
/// The only user interaction is the OAuth code paste if no session exists.
async fn cmd_setup_auto(config_override: Option<PathBuf>) -> Result<()> {
    let config_p = config_override.clone().unwrap_or_else(config_path);

    let mut cred = match crate::oauth::read_claude_credentials() {
        Some(mut c) => {
            if c.needs_refresh() {
                if let Ok(fresh) = refresh_token(&c).await { c = fresh; }
            }
            c
        }
        None => {
            // No session on disk — run the full OAuth flow (user pastes code)
            println!("  {} No Claude Code session found — opening browser for login…", yellow("·"));
            crate::oauth::run_oauth_flow().await?
        }
    };

    let plan = crate::oauth::read_claude_session_info()
        .map(|s| s.plan)
        .unwrap_or_else(|| "pro".to_string());

    cred.email = crate::oauth::fetch_account_email(&cred.access_token).await;

    if let Some(parent) = config_p.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::write(&config_p, crate::config::config_template(&[("main", &plan)]))?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config_p, std::fs::Permissions::from_mode(0o600))?;
    }

    let mut store = CredentialsStore::default();
    store.accounts.insert("main".into(), cred);
    store.save()?;

    Ok(())
}

async fn wait_for_health(host: &str, port: u16, timeout_secs: u64) -> bool {
    let url = format!("http://{host}:{port}/health");
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        if reqwest::get(&url).await.map(|r| r.status().is_success()).unwrap_or(false) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    false
}

fn auto_write_shell_export(port: u16) {
    use std::io::Write;
    let line = format!("export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}");
    let Some(profile) = detect_shell_profile() else { return };

    if profile.exists() {
        if let Ok(contents) = std::fs::read_to_string(&profile) {
            if contents.contains(&line) {
                // Already exactly correct — nothing to do.
                return;
            }
            if contents.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:") {
                // Has the variable but with a different port — update it in-place.
                let updated: String = contents
                    .lines()
                    .map(|l| {
                        if l.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:") {
                            line.as_str()
                        } else {
                            l
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n";
                if std::fs::write(&profile, updated).is_ok() {
                    println!("  {} {} updated to port {}  → {}",
                        green(CHECK), cyan("ANTHROPIC_BASE_URL"), port,
                        dim(&profile.display().to_string()));
                }
                return;
            }
            if contents.contains("ANTHROPIC_BASE_URL") {
                // Set to something else (e.g. remote URL) — leave it alone.
                return;
            }
        }
    }

    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&profile) {
        writeln!(f, "\n# Added by shunt").ok();
        writeln!(f, "{line}").ok();
        println!("  {} {} → {}",
            green(CHECK), cyan("ANTHROPIC_BASE_URL"),
            dim(&profile.display().to_string()));
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn cmd_status(config_override: Option<PathBuf>) -> Result<()> {
    let mut config = crate::config::load_config(config_override.as_deref())?;
    let _primary_url = format!("http://{}:{}", config.server.host, config.server.port);

    // Fetch live status from every provider's proxy (each runs on its own port).
    // provider_label → serde_json::Value
    let provider_urls = listener_addrs(&config.accounts, &config.server.host, config.server.port);
    let mut live_by_provider: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for (label, url) in &provider_urls {
        if let Some(v) = reqwest::get(format!("{url}/status")).await.ok()
            .and_then(|r| futures_executor_hack(r))
        {
            live_by_provider.insert(label.clone(), v);
        }
    }

    // Primary proxy (Anthropic) drives the overall running/stopped display.
    let live: Option<&serde_json::Value> = live_by_provider
        .get(&crate::provider::Provider::Anthropic.to_string())
        .or_else(|| live_by_provider.values().next());

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

    // Build running address list: ":8082" or ":8082 · :8083"
    let addr_str = if !live_by_provider.is_empty() {
        let parts: Vec<String> = provider_urls.iter()
            .filter(|(label, _)| live_by_provider.contains_key(label.as_str()))
            .map(|(_, url)| {
                let port = url.rsplit(':').next().unwrap_or("?");
                cyan(&format!(":{port}"))
            })
            .collect();
        parts.join(&dim("  ·  "))
    } else {
        String::new()
    };

    let proxy_line = if live.is_some() {
        format!("{}  {}  {}", green(DOT), green_bold("running"), addr_str)
    } else {
        let log_hint = if log_path().exists() {
            format!("  {}  {}", dim("·"), dim("shunt logs for details"))
        } else {
            String::new()
        };
        format!("{}  {}  {}{}", dim(EMPTY), dim("stopped"), dim("shunt start"), log_hint)
    };

    let account_names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
    // Build savings summary if proxy is running and has data.
    let savings_line: Option<String> = live.and_then(|v| {
        let s = v.get("savings")?;
        let today_in  = s["today_input"].as_u64().unwrap_or(0);
        let today_out = s["today_output"].as_u64().unwrap_or(0);
        let today_cost = s["today_cost_usd"].as_f64().unwrap_or(0.0);
        let all_cost   = s["all_time_cost_usd"].as_f64().unwrap_or(0.0);
        if today_in + today_out == 0 && all_cost == 0.0 { return None; }
        let today_tok = crate::term::fmt_tokens(today_in + today_out);
        let cost_str  = crate::pricing::fmt_cost(today_cost);
        let all_str   = crate::pricing::fmt_cost(all_cost);
        Some(format!("{}  today {}  {}  {}  all time {}",
            dim("·"), dim(&today_tok), dim(&cost_str), dim("·"), dim(&all_str)))
    });

    print_routing_header(&account_names, &[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        proxy_line,
    ]);

    if let Some(ref line) = savings_line {
        println!("  {line}");
        println!();
    }

    let pinned_account = live.and_then(|v| v["pinned"].as_str()).map(|s| s.to_owned());
    let last_used_account = live.and_then(|v| v["last_used"].as_str()).map(|s| s.to_owned());

    // Pinned notice
    if let Some(ref pinned) = pinned_account {
        println!("  {}  pinned to {}",
            yellow(DIAMOND), bold(pinned));
        println!("  {}  run {} to restore auto routing",
            dim("·"), cyan("shunt use auto"));
        println!();
    }

    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()).unwrap_or(0);

    for acc in &config.accounts {
        let live_acc = live_by_provider.get(&acc.provider.to_string())
            .and_then(|v| v["accounts"].as_array())
            .and_then(|arr| arr.iter().find(|a| a["name"] == acc.name));

        let status = live_acc.and_then(|a| a["status"].as_str()).unwrap_or("offline");

        let (status_icon, status_text): (String, String) = match status {
            "available"       => (green(CHECK), green("available")),
            "cooling"         => (yellow("↻"),  yellow("cooling")),
            "disabled"        => (red(CROSS),   red("disabled")),
            "reauth_required" => (red(CROSS),   red("session expired")),
            _ => match &acc.credential {
                None                          => (red(CROSS),   red("no credential")),
                Some(c) if c.needs_refresh()  => (yellow(CROSS), yellow("token expired")),
                _                             => (dim(EMPTY),   dim("offline")),
            },
        };

        let plan_label = if acc.provider == crate::provider::Provider::OpenAI {
            match acc.plan_type.to_lowercase().as_str() {
                "plus"  => "ChatGPT Plus",
                "pro"   => "ChatGPT Pro",
                "team"  => "ChatGPT Team",
                _       => "ChatGPT",
            }
        } else {
            match acc.plan_type.to_lowercase().as_str() {
                "max" | "claude_max" => "Claude Max",
                "team"               => "Claude Team",
                _                    => "Claude Pro",
            }
        };
        let email_str = acc.credential.as_ref().and_then(|c| c.email.as_deref()).unwrap_or("");

        // ── routing tag ─────────────────────────────────────
        let is_pinned  = pinned_account.as_deref() == Some(&acc.name);
        let is_last    = !is_pinned && last_used_account.as_deref() == Some(&acc.name);
        let (routing_tag, tag_vis_len): (String, usize) = if is_pinned {
            (format!("  {}", yellow("pinned")), 8)
        } else if is_last {
            (format!("  {}", green("active")), 8)
        } else {
            (String::new(), 0)
        };

        // ── account header (name + tag + plan) ──────────────
        println!("{}", card_header(&acc.name, &green_bold(&acc.name), &routing_tag, tag_vis_len, plan_label));

        // ── email + provider badge row ───────────────────────
        let is_openai = acc.provider == crate::provider::Provider::OpenAI;
        let provider_badge = if is_openai { format!("  {}  {}", dim("·"), dim("openai")) } else { String::new() };
        if !email_str.is_empty() {
            println!("{}", card_row(&format!("{}{}", dim(email_str), provider_badge)));
        } else if is_openai {
            println!("{}", card_row(&dim("openai")));
        }

        println!();

        // ── status ───────────────────────────────────────────
        println!("{}", card_row(&format!("{}  {}", status_icon, status_text)));

        // ── rate limit bars ──────────────────────────────────
        if let Some(rl) = live_acc.and_then(|a| a["rate_limit"].as_object()) {
            let util_5h   = rl.get("utilization_5h").and_then(|v| v.as_f64());
            let reset_5h  = rl.get("reset_5h").and_then(|v| v.as_u64());
            let status_5h = rl.get("status_5h").and_then(|v| v.as_str()).unwrap_or("allowed");
            let util_7d   = rl.get("utilization_7d").and_then(|v| v.as_f64());
            let reset_7d  = rl.get("reset_7d").and_then(|v| v.as_u64());
            let status_7d = rl.get("status_7d").and_then(|v| v.as_str()).unwrap_or("allowed");

            let window_row = |label: &str, util: Option<f64>, reset: Option<u64>, wstatus: &str| {
                if reset.map(|t| t <= now_secs).unwrap_or(false) {
                    let ago = reset.map(|t| format!(
                        "  {} ago", term::fmt_duration_ms(now_secs.saturating_sub(t) * 1000)
                    )).unwrap_or_default();
                    println!("{}", card_row(&format!(
                        "{}  {}  {}{}",
                        dim(label), green(&"─".repeat(20)), green("fresh"), dim(&ago)
                    )));
                } else if let Some(u) = util {
                    let rem = 100u64.saturating_sub((u * 100.0) as u64);
                    let bar = util_bar(u, 20);
                    let reset_str = reset.and_then(|t| secs_until(t))
                        .map(|s| format!("  ·  resets in {}", term::fmt_duration_ms(s * 1000)))
                        .unwrap_or_default();
                    let pct = if wstatus == "exhausted" {
                        red("exhausted")
                    } else {
                        format!("{}% left", bold(&rem.to_string()))
                    };
                    println!("{}", card_row(&format!(
                        "{}  {}  {}{}",
                        dim(label), bar, pct, dim(&reset_str)
                    )));
                }
            };

            if util_5h.is_some() || reset_5h.is_some() {
                window_row("5h", util_5h, reset_5h, status_5h);
            }
            if util_7d.is_some() || reset_7d.is_some() {
                window_row("7d", util_7d, reset_7d, status_7d);
            }
        } else if acc.credential.is_none() {
            println!("{}", card_row(&format!("{}  run {}",
                dim("·"), cyan(&format!("shunt add-account {}", acc.name)))));
        } else if status == "reauth_required" {
            println!("{}", card_row(&format!("{}  run {}",
                dim("·"), cyan(&format!("shunt add-account {}", acc.name)))));
        } else if live.is_some() && live_acc.is_some() {
            if acc.provider == crate::provider::Provider::Anthropic {
                println!("{}", card_row(&dim("· quota data will appear after first request")));
            } else {
                println!("{}", card_row(&dim("· quota tracking unavailable (OpenAI doesn't report utilization)")));
            }
        }

        // ── separator ────────────────────────────────────────
        println!();
        println!("{}", card_sep());
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
        let pin = if is_pinned { format!("  {}", yellow("pinned")) } else { String::new() };

        term::SelectItem {
            label: format!("{}  {}  {}{}", bold(&pad(&a.name, 12)), dim(&pad(email, 32)), status_str, pin),
            value: a.name.clone(),
        }
    }).collect();

    let auto_marker = if current_pinned.is_none() { format!("  {}", yellow("active")) } else { String::new() };
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
            // Proxy not running — persist directly to the state file so it
            // takes effect when the proxy next starts.
            write_pinned_to_state(if is_auto { None } else { Some(chosen.clone()) });
            if is_auto {
                println!("  {} Automatic routing saved  ·  {}", green(CHECK),
                    dim("applies on next shunt start"));
            } else {
                println!("  {} Pinned to {}  ·  {}", green(CHECK), bold(&chosen),
                    dim("applies on next shunt start"));
            }
            println!();
        }
    }
    Ok(())
}

/// Write a pinned account directly into the state file (used when proxy is not running).
fn write_pinned_to_state(account: Option<String>) {
    let path = crate::config::state_path();
    let mut data: serde_json::Value = path.exists()
        .then(|| std::fs::read_to_string(&path).ok())
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    data["pinned_account"] = match account {
        Some(a) => serde_json::Value::String(a),
        None => serde_json::Value::Null,
    };
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    let tmp = path.with_extension("tmp");
    if let Ok(text) = serde_json::to_string_pretty(&data) {
        let _ = std::fs::write(&tmp, text);
        let _ = std::fs::rename(&tmp, &path);
    }
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

/// Clean header: ◆ followed by title and optional subtitle, then a rule.
fn print_splash(info: &[String]) {
    println!();
    let title    = info.get(0).map(|s| s.as_str()).unwrap_or("");
    let subtitle = info.get(1).map(|s| s.as_str()).unwrap_or("");

    println!("  {}  {}", brand_green(DIAMOND), title);
    if !subtitle.is_empty() {
        println!("       {}", subtitle);
    }
    let w = strip_ansi(title).chars().count()
        .max(strip_ansi(subtitle).chars().count())
        .max(18) + 3;
    println!("  {}", dim(&"─".repeat(w)));
    println!();
}

// ---------------------------------------------------------------------------
// Account card helpers  (used by cmd_status)
// ---------------------------------------------------------------------------

/// Target visible width for account header lines and separators.
const CARD_W: usize = 58;

/// Account header: "  ◆  name  tag                     Plan"
fn card_header(name: &str, name_c: &str, routing_tag: &str, tag_vis: usize, plan: &str) -> String {
    // Visible prefix: "  ◆  " = 5, then name (name.len()), then tag (tag_vis)
    let left_vis = 5 + name.len() + tag_vis;
    let gap = CARD_W.saturating_sub(left_vis + plan.len());
    format!("  {}  {}{}{}{}", brand_green(DIAMOND), name_c, routing_tag, " ".repeat(gap), dim(plan))
}

/// An indented content row: "    content"
fn card_row(content: &str) -> String {
    format!("    {content}")
}

/// Thin separator line between accounts.
fn card_sep() -> String {
    format!("  {}", dim(&"─".repeat(CARD_W - 2)))
}

/// Routing diagram — account names in bold green, connectors in dark green.
///
/// 1 account:           2 accounts:          3+ accounts:
///   main  ─→  [info]    main ─┐ →  [info]    main ─┐
///             [info1]   work ─┘     [info1]   work ─┼─→  [info]
///                                             sec  ─┘     [info1]
fn print_routing_header(account_names: &[&str], info: &[String]) {
    println!();
    let n = account_names.len();
    let name_w = account_names.iter().map(|s| s.len()).max().unwrap_or(4);
    let info0 = info.get(0).map(|s| s.as_str()).unwrap_or("");
    let info1 = info.get(1).map(|s| s.as_str()).unwrap_or("");

    match n {
        0 => {
            // No accounts yet — clean two-line header
            println!("  {}  {}", brand_green(DIAMOND), info0);
            if !info1.is_empty() {
                println!("       {}", info1);
            }
        }
        1 => {
            // "  name  ─→  info0"  (info1 indented to same column)
            let indent = name_w + 8; // 2 + name + 2 + "─→" + 2
            println!("  {}  {}  {}", green_bold(account_names[0]), dark_green("─→"), info0);
            if !info1.is_empty() {
                println!("  {}{}", " ".repeat(indent), info1);
            }
        }
        2 => {
            // "  name0 ─┐ →  info0"
            // "  name1 ─┘     info1"
            println!("  {}  {} {}  {}",
                green_bold(&pad(account_names[0], name_w)),
                dark_green("─┐"), dark_green("→"), info0);
            println!("  {}  {}    {}",
                green_bold(&pad(account_names[1], name_w)),
                dark_green("─┘"), info1);
        }
        3 => {
            // "  name0 ─┐"
            // "  name1 ─┼─→  info0"
            // "  name2 ─┘     info1"
            println!("  {}  {}", green_bold(&pad(account_names[0], name_w)), dark_green("─┐"));
            println!("  {}  {}  {}",
                green_bold(&pad(account_names[1], name_w)),
                dark_green("─┼─→"), info0);
            println!("  {}  {}    {}",
                green_bold(&pad(account_names[2], name_w)),
                dark_green("─┘"), info1);
        }
        _ => {
            // "  name0      ─┐"
            // "  + N more   ─┼─→  info0"
            // "  nameN      ─┘     info1"
            let more = dim(&pad(&format!("+ {} more", n - 2), name_w));
            println!("  {}  {}", green_bold(&pad(account_names[0], name_w)), dark_green("─┐"));
            println!("  {}  {}  {}", more, dark_green("─┼─→"), info0);
            println!("  {}  {}    {}",
                green_bold(&pad(account_names[n - 1], name_w)),
                dark_green("─┘"), info1);
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

// ---------------------------------------------------------------------------
// Multi-provider listener helpers
// ---------------------------------------------------------------------------

/// Returns `(provider_label, url)` pairs for every provider present in accounts,
/// using `primary_port` for Anthropic and each provider's default port for others.
fn listener_addrs(
    accounts: &[crate::config::AccountConfig],
    host: &str,
    primary_port: u16,
) -> Vec<(String, String)> {
    use crate::provider::Provider;
    use std::collections::BTreeSet;

    let providers: BTreeSet<String> = accounts.iter()
        .map(|a| a.provider.to_string())
        .collect();

    providers.into_iter().map(|p| {
        let port = match Provider::from_str(&p) {
            Provider::Anthropic => primary_port,
            other => other.default_port(),
        };
        (p.clone(), format!("http://{host}:{port}"))
    }).collect()
}

/// Bind a listener and spawn an axum server for each provider group found in
/// `config.accounts`. All servers run concurrently; the function returns when
/// the first one stops (error or clean shutdown).
async fn serve_all_providers(
    config: crate::config::Config,
    state: crate::state::StateStore,
    host: &str,
    primary_port: u16,
) -> anyhow::Result<()> {
    use crate::config::{Config, ServerConfig};
    use crate::provider::Provider;
    use std::collections::HashMap;

    // Group accounts by provider.
    let mut by_provider: HashMap<String, Vec<crate::config::AccountConfig>> = HashMap::new();
    for account in config.accounts {
        by_provider.entry(account.provider.to_string()).or_default().push(account);
    }

    let mut handles = Vec::new();

    for (provider_str, accounts) in by_provider {
        let provider = Provider::from_str(&provider_str);
        let port = match provider {
            Provider::Anthropic => primary_port,
            ref other => other.default_port(),
        };

        let provider_config = Config {
            accounts,
            server: ServerConfig {
                host: host.to_owned(),
                port,
                upstream_url: provider.default_upstream_url().to_owned(),
                ..config.server.clone()
            },
            config_file: config.config_file.clone(),
        };

        let anthropic_url = if provider == Provider::OpenAI {
            Some(format!("http://{}:{}", host, primary_port))
        } else {
            None
        };
        let (app, live_creds) = crate::proxy::create_app_with_state(provider_config.clone(), state.clone(), anthropic_url)?;
        let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
            .await
            .with_context(|| format!("cannot bind {host}:{port} for {provider_str} proxy"))?;

        let cfg_arc = std::sync::Arc::new(provider_config);
        tokio::spawn(crate::proxy::prefetch_rate_limits(cfg_arc.clone(), state.clone()));
        tokio::spawn(crate::proxy::openai_token_refresh_loop(cfg_arc.clone(), state.clone(), live_creds.clone()));
        tokio::spawn(crate::proxy::recovery_watcher(cfg_arc, state.clone(), live_creds));
        handles.push(tokio::spawn(async move {
            axum::serve(listener, app).await
        }));
    }

    if handles.is_empty() {
        return Ok(());
    }

    // Wait until the first listener stops, then exit (whole daemon restarts on error).
    let (result, _idx, _rest) = futures_util::future::select_all(handles).await;
    result??;
    Ok(())
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

/// Pad a string to display width using spaces (strips ANSI codes first; handles Unicode).
fn pad(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let visible_width = UnicodeWidthStr::width(strip_ansi(s).as_str());
    if visible_width >= width {
        s.to_owned()
    } else {
        format!("{s}{}", " ".repeat(width - visible_width))
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

// ---------------------------------------------------------------------------
// monitor
// ---------------------------------------------------------------------------

async fn cmd_monitor(config_override: Option<PathBuf>) -> Result<()> {
    let config = crate::config::load_config(config_override.as_deref())?;
    let base_url = format!("http://{}:{}", config.server.host, config.server.port);

    // Quick check: is the proxy running?
    if reqwest::get(format!("{base_url}/health")).await.is_err() {
        println!();
        println!("  {} Proxy is not running.", red(CROSS));
        println!("  {} Start it first with {}.", dim("·"), cyan("shunt start"));
        println!();
        return Ok(());
    }

    crate::monitor::run_monitor(&base_url).await
}

// update
// ---------------------------------------------------------------------------

async fn cmd_update() -> Result<()> {
    const REPO: &str = "ramc10/shunt";
    let current = env!("CARGO_PKG_VERSION");

    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{current}"))),
        dim("Checking for updates…").to_string(),
        String::new(),
    ]);

    // Fetch latest release from GitHub API
    let client = reqwest::Client::builder()
        .user_agent("shunt-updater")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let api_url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = client.get(&api_url).send().await
        .context("Failed to reach GitHub API")?;

    if !resp.status().is_success() {
        bail!("GitHub API returned {}", resp.status());
    }

    let json: serde_json::Value = resp.json().await?;
    let latest_tag = json["tag_name"].as_str().context("Missing tag_name in release")?;
    let latest = latest_tag.trim_start_matches('v');

    if latest == current {
        println!("  {} Already up to date ({})", green(CHECK), bold(&format!("v{current}")));
        println!();
        return Ok(());
    }

    println!("  {} Update available: {}  →  {}", green("↑"),
        dim(&format!("v{current}")), bold_white(&format!("v{latest}")));
    println!();

    // Detect platform
    let target = detect_update_target()?;
    let archive_name = format!("shunt-v{latest}-{target}.tar.gz");
    let url = format!(
        "https://github.com/{REPO}/releases/download/v{latest}/{archive_name}"
    );

    print!("  {} Downloading {}… ", dim("↓"), dim(&archive_name));
    use std::io::Write as _;
    std::io::stdout().flush().ok();

    let resp = client.get(&url).send().await
        .context("Download request failed")?;

    if !resp.status().is_success() {
        bail!("Download failed: HTTP {} for {url}", resp.status());
    }

    let bytes = resp.bytes().await
        .context("Failed to read download")?;

    // Sanity-check: gzip magic bytes are 0x1f 0x8b
    if bytes.len() < 2 || bytes[0] != 0x1f || bytes[1] != 0x8b {
        bail!(
            "Downloaded file does not look like a gzip archive ({} bytes, first bytes: {:02x?})",
            bytes.len(), &bytes[..bytes.len().min(4)]
        );
    }

    println!("{}", green("done"));

    // Extract binary from tarball into a temp file next to the current exe
    let exe_path = std::env::current_exe().context("Cannot locate current executable")?;
    let tmp_path = exe_path.with_extension("tmp");

    extract_binary_from_tarball(&bytes, &tmp_path)
        .context("Failed to extract binary from archive")?;

    // Replace current executable atomically
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp_path, &exe_path)
        .context("Failed to replace binary (try running with sudo?)")?;

    // macOS: remove quarantine and ad-hoc sign so Gatekeeper allows unsigned binaries
    #[cfg(target_os = "macos")]
    {
        let p = exe_path.display().to_string();
        std::process::Command::new("xattr").args(["-d", "com.apple.quarantine", &p]).status().ok();
        std::process::Command::new("codesign").args(["--force", "--deep", "--sign", "-", &p]).status().ok();
    }

    println!("  {} Updated to {}", green(CHECK), bold_white(&format!("v{latest}")));
    println!();
    Ok(())
}

fn detect_update_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos",  "aarch64") => Ok("aarch64-apple-darwin"),
        ("linux",  "x86_64")  => Ok("x86_64-unknown-linux-gnu"),
        ("linux",  "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        (os, arch) => bail!("No pre-built binary for {os}/{arch}. Build from source: cargo install shunt-proxy"),
    }
}

fn extract_binary_from_tarball(data: &[u8], dest: &std::path::Path) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.file_name().and_then(|n| n.to_str()) == Some("shunt") {
            let mut out = std::fs::File::create(dest)?;
            std::io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    bail!("Binary 'shunt' not found in archive")
}

// ---------------------------------------------------------------------------
// share
// ---------------------------------------------------------------------------

async fn cmd_share(config_override: Option<PathBuf>, tunnel: bool, stop: bool) -> Result<()> {
    let config_p = config_override.unwrap_or_else(config_path);
    if !config_p.exists() {
        bail!("No config found. Run `shunt setup` first.");
    }

    let mut text = std::fs::read_to_string(&config_p)?;

    if stop {
        text = text.lines()
            .filter(|l| !l.trim_start().starts_with("remote_key"))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.ends_with('\n') { text.push('\n'); }
        text = text.replace("host = \"0.0.0.0\"", "host = \"127.0.0.1\"");
        std::fs::write(&config_p, &text)?;

        print_splash(&[
            format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
            dim("Remote sharing disabled").to_string(),
            String::new(),
        ]);
        println!("  {} Restart to apply: {}", dim("·"), cyan("shunt start"));
        println!();
        return Ok(());
    }

    // Generate or reuse existing key
    let key = match extract_remote_key(&text) {
        Some(k) => k,
        None => {
            let k = generate_remote_key();
            text = insert_into_server_section(&text, &format!("remote_key = \"{k}\""));
            k
        }
    };

    // Ensure host is 0.0.0.0
    if text.contains("host = \"127.0.0.1\"") {
        text = text.replace("host = \"127.0.0.1\"", "host = \"0.0.0.0\"");
    }

    std::fs::write(&config_p, &text)?;

    let port = crate::config::load_config(Some(&config_p))
        .map(|c| c.server.port)
        .unwrap_or(8082);

    if tunnel {
        // Cloudflare quick tunnel — works over any network, no account needed
        print_splash(&[
            format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
            dim("Starting Cloudflare tunnel…").to_string(),
            String::new(),
        ]);

        println!("  {} Make sure the proxy is running: {}", dim("·"), cyan("shunt start"));
        println!();

        let url = start_cloudflare_tunnel(port)?;

        println!("  {}  Set on the remote device:\n", green(CHECK));
        println!("    {}{}",
            dim("export ANTHROPIC_BASE_URL="),
            cyan(&url),
        );
        println!("    {}{}", dim("export ANTHROPIC_API_KEY="), cyan(&key));
        println!();
        println!("  {} Tunnel is active — keep this terminal open.", dim("·"));
        println!("  {} Press Ctrl+C to stop.", dim("·"));
        println!();

        // Block until the user kills it
        tokio::signal::ctrl_c().await.ok();
        println!("\n  {} Tunnel closed.", dim("·"));
    } else {
        let ip = local_ip().unwrap_or_else(|| "<your-ip>".to_string());

        print_splash(&[
            format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
            dim("Remote sharing enabled (LAN)").to_string(),
            String::new(),
        ]);

        println!("  Set on the remote device:\n");
        println!("    {}{}",
            dim("export ANTHROPIC_BASE_URL="),
            cyan(&format!("http://{ip}:{port}")),
        );
        println!("    {}{}", dim("export ANTHROPIC_API_KEY="), cyan(&key));
        println!();
        println!("  {} Both devices must be on the same network.", dim("·"));
        println!("  {} For any network: {}", dim("·"), cyan("shunt share --tunnel"));
        println!("  {} Restart to apply: {}", dim("·"), cyan("shunt start"));
        println!("  {} To stop sharing:  {}", dim("·"), cyan("shunt share --stop"));
        println!();
    }

    Ok(())
}

/// Spawn `cloudflared tunnel --url http://localhost:{port}`, wait for the public URL,
/// and return it. The cloudflared process is left running in the background.
fn start_cloudflare_tunnel(port: u16) -> Result<String> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let mut child = Command::new("cloudflared")
        .args(["tunnel", "--url", &format!("http://localhost:{port}")])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "cloudflared not found.\n\n  Install it:\n    brew install cloudflared\n  or: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/"
                )
            } else {
                anyhow::anyhow!("Failed to start cloudflared: {e}")
            }
        })?;

    let stderr = child.stderr.take().expect("stderr was piped");
    let reader = BufReader::new(stderr);

    for line in reader.lines() {
        let line = line?;
        if let Some(url) = extract_cloudflare_url(&line) {
            // Leave the child running — it will be killed when the process exits
            std::mem::forget(child);
            return Ok(url);
        }
    }

    bail!("cloudflared exited before providing a tunnel URL")
}

fn extract_cloudflare_url(line: &str) -> Option<String> {
    // cloudflared prints the URL in a line like:
    //   INF | https://random-words.trycloudflare.com |
    // or just contains the URL somewhere in the log line
    let lower = line.to_lowercase();
    if lower.contains("trycloudflare.com") || lower.contains("cfargotunnel.com") {
        // Extract the https:// URL from the line
        if let Some(start) = line.find("https://") {
            let rest = &line[start..];
            let end = rest.find(|c: char| c.is_whitespace() || c == '|' || c == '"')
                .unwrap_or(rest.len());
            return Some(rest[..end].trim_end_matches('/').to_owned());
        }
    }
    None
}

fn generate_remote_key() -> String {
    hex::encode(crate::oauth::rand_bytes::<16>())
}

fn extract_remote_key(config: &str) -> Option<String> {
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with("remote_key") {
            return line.split('=')
                .nth(1)
                .map(|s| s.trim().trim_matches('"').to_owned());
        }
    }
    None
}

fn insert_into_server_section(config: &str, line: &str) -> String {
    // Insert just before the first [[accounts]] block
    if let Some(pos) = config.find("\n[[accounts]]") {
        let (before, after) = config.split_at(pos);
        format!("{before}\n{line}{after}")
    } else {
        format!("{config}\n{line}\n")
    }
}

fn local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

/// If the proxy is currently running, offer to restart it immediately.
async fn offer_restart(config_override: Option<PathBuf>) {
    use std::io::Write;
    let Ok(cfg) = crate::config::load_config(config_override.as_deref()) else { return };
    let health_url = format!("http://{}:{}/health", cfg.server.host, cfg.server.port);
    let running = reqwest::get(&health_url).await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if !running { return; }

    print!("  {} Proxy is running — restart now? [Y/n]: ", dim("·"));
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf).ok();
    if matches!(buf.trim().to_lowercase().as_str(), "n" | "no") {
        println!("  {} Run {} when ready.", dim("·"), cyan("shunt restart"));
        return;
    }
    if let Err(e) = cmd_restart(config_override).await {
        println!("  {} Restart failed: {e}", red(CROSS));
    }
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
