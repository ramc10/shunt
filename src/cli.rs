use anyhow::{bail, Context as _, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{config_path, config_template, credentials_path, log_path, pid_path, CredentialsStore};
use crate::credential::Credential;
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
    /// Manage accounts — add, remove, or log out (interactive menu)
    Config {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Import the current Claude Code session as an additional account
    #[command(hide = true)]
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
    #[command(hide = true)]
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
    #[command(hide = true)]
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
    /// Watch a remote shunt instance and fire local system notifications
    ///
    /// Run with no arguments on the machine running shunt to get a watch code,
    /// then enter that code on another device to receive notifications there.
    ///
    /// Examples:
    ///   shunt remote                  — host: generate a watch code
    ///   shunt remote RM-a3f2b1c4...  — client: connect with a watch code
    Remote {
        /// Watch code from `shunt remote` on the host. Omit to start hosting.
        code: Option<String>,
    },
    /// Connect this device to a remote shunt instance
    ///
    /// Fetches the proxy URL and API key for the given share code (printed by
    /// `shunt share` on the host) and writes them to your shell profile so
    /// Claude Code routes through the shared proxy automatically.
    ///
    /// Examples:
    ///   shunt connect SC-a3f2b1c4d5e6f7a8b9
    Connect {
        /// Share code printed by `shunt share` on the host
        code: String,
    },
    /// Update shunt to the latest release
    Update,
    /// Completely remove shunt — stops service, deletes config, removes binary
    Uninstall,
    /// Manage shunt as a system service (auto-start on login)
    ///
    /// Examples:
    ///   shunt service install    — register + start (called by install.sh)
    ///   shunt service uninstall  — stop + remove
    ///   shunt service status     — is service registered/running?
    Service {
        #[command(subcommand)]
        action: ServiceAction,
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

#[derive(Subcommand)]
enum ServiceAction {
    /// Register shunt as a login service and start it immediately
    Install,
    /// Stop and unregister the shunt login service
    Uninstall,
    /// Show whether the service is registered and running
    Status,
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
        Command::Config { config } => cmd_config(config).await,
        Command::AddAccount { config, name, provider } => cmd_add_account(config, name, provider.as_deref()).await,
        Command::RemoveAccount { config, name } => cmd_remove_account(config, name).await,
        Command::Logout { config, name, all } => cmd_logout(config, name, all).await,
        Command::Monitor { config } => cmd_monitor(config).await,
        Command::Remote { code } => cmd_remote(code).await,
        Command::Connect { code } => cmd_connect(code).await,
        Command::Update => cmd_update().await,
        Command::Share { config, tunnel, stop } => cmd_share(config, tunnel, stop).await,
        Command::Uninstall => cmd_uninstall().await,
        Command::Use { config, account } => cmd_use(config, account).await,
        Command::Service { action } => match action {
            ServiceAction::Install   => cmd_service_install().await,
            ServiceAction::Uninstall => cmd_service_uninstall().await,
            ServiceAction::Status    => cmd_service_status().await,
        },
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
    store.accounts.insert("main".into(), Credential::Oauth(cred));
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
// config  (unified account management)
// ---------------------------------------------------------------------------

async fn cmd_config(config_override: Option<PathBuf>) -> Result<()> {
    let config_p = config_override.clone().unwrap_or_else(config_path);
    if !config_p.exists() {
        bail!("No config found. Run `shunt setup` first.");
    }

    let items = vec![
        term::SelectItem { label: format!("{}  {}", bold("Add account"),     dim("connect a new account to the pool")),        value: "add".into() },
        term::SelectItem { label: format!("{}  {}", bold("Manage accounts"), dim("reauth, update config, or fix issues")),     value: "manage".into() },
        term::SelectItem { label: format!("{}  {}", bold("Remove account"),  dim("delete an account from the pool")),          value: "remove".into() },
        term::SelectItem { label: format!("{}  {}", bold("Log out"),         dim("clear credentials for an account")),         value: "logout".into() },
    ];

    println!();
    match term::select("Account management", &items, 0) {
        Some(v) if v == "add"    => cmd_add_account(config_override, None, None).await,
        Some(v) if v == "manage" => cmd_manage_account(config_override).await,
        Some(v) if v == "remove" => cmd_remove_account(config_override, None).await,
        Some(v) if v == "logout" => cmd_logout(config_override, None, false).await,
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// manage-account  (per-account edit / reauth)
// ---------------------------------------------------------------------------

async fn cmd_manage_account(config_override: Option<PathBuf>) -> Result<()> {
    use crate::provider::AuthKind;

    let config = crate::config::load_config(config_override.as_deref())?;
    if config.accounts.is_empty() {
        bail!("No accounts configured. Run `shunt config` → Add account.");
    }

    // ── Step 1: pick account ─────────────────────────────────────────────────
    let items: Vec<term::SelectItem> = config.accounts.iter().map(|a| {
        let tag = match a.provider.auth_kind() {
            AuthKind::OAuth  => {
                let ok = a.credential.as_ref().map(|c| !c.needs_refresh()).unwrap_or(false);
                if ok { dim("  oauth  ✓") } else { yellow("  oauth  !") }
            }
            AuthKind::ApiKey => dim("  api-key"),
            AuthKind::None   => dim("  local"),
        };
        term::SelectItem {
            label: format!("{}  {}{}", bold(&pad(&a.name, 14)), dim(&pad(a.credential.as_ref().and_then(|c| c.email()).unwrap_or(""), 32)), tag),
            value: a.name.clone(),
        }
    }).collect();

    println!();
    let name = match term::select("Which account?", &items, 0) {
        Some(v) => v,
        None => return Ok(()),
    };

    let account = config.accounts.iter().find(|a| a.name == name).unwrap();
    let provider = account.provider.clone();

    // ── Step 2: pick action ──────────────────────────────────────────────────
    let mut actions: Vec<term::SelectItem> = Vec::new();
    match provider.auth_kind() {
        AuthKind::OAuth => {
            actions.push(term::SelectItem { label: format!("{}  {}", bold("Re-authenticate"), dim("start a new OAuth session")),          value: "reauth".into() });
            actions.push(term::SelectItem { label: format!("{}  {}", bold("Log out"),         dim("clear stored credentials")),            value: "logout".into() });
        }
        AuthKind::ApiKey => {
            actions.push(term::SelectItem { label: format!("{}  {}", bold("Update API key"),  dim("replace stored key")),                  value: "apikey".into() });
        }
        AuthKind::None => {
            actions.push(term::SelectItem { label: format!("{}  {}", bold("Update upstream URL"), dim("change the local endpoint")),       value: "upstream".into() });
            actions.push(term::SelectItem { label: format!("{}  {}", bold("Update model"),        dim("set default model for this account")), value: "model".into() });
        }
    }
    actions.push(term::SelectItem { label: format!("{}  {}", bold("Remove account"), dim("delete from pool permanently")),                value: "remove".into() });

    println!();
    let action = match term::select(&format!("Manage  '{name}'"), &actions, 0) {
        Some(v) => v,
        None => return Ok(()),
    };

    println!();

    match action.as_str() {
        // ── Re-authenticate (OAuth) ──────────────────────────────────────────
        "reauth" => {
            print_splash(&[
                format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
                format!("Re-authenticating  '{name}'"),
                String::new(),
            ]);
            use crate::oauth::{run_oauth_flow, run_openai_oauth_flow, fetch_account_email, fetch_openai_account_email};
            use crate::provider::Provider;
            let mut cred = match provider {
                Provider::Anthropic => run_oauth_flow().await?,
                Provider::OpenAI    => run_openai_oauth_flow().await?,
                _ => unreachable!(),
            };
            let email = match provider {
                Provider::Anthropic => fetch_account_email(&cred.access_token).await,
                Provider::OpenAI    => fetch_openai_account_email(&cred.access_token).await,
                _ => None,
            };
            if let Some(ref e) = email { println!("  {} Signed in as {}", green(CHECK), bold(e)); }
            cred.email = email;
            if cred.id_token.is_some() { crate::oauth::write_codex_auth_file(&cred); }
            // Clear auth_failed state
            let state_p = crate::config::state_path();
            let state = crate::state::StateStore::load(&state_p);
            state.clear_auth_failed(&name);
            // Save credential
            let mut store = CredentialsStore::load();
            store.accounts.insert(name.clone(), Credential::Oauth(cred));
            store.save()?;
            println!();
            println!("  {} Account '{}' re-authenticated.", green(CHECK), bold(&name));
            offer_restart(config_override).await;
        }

        // ── Update API key ───────────────────────────────────────────────────
        "apikey" => {
            let env_hint = provider.api_key_env_var()
                .map(|v| format!(" (or set {} in your environment)", v))
                .unwrap_or_default();
            print!("  {} New API key{}: ", dim("·"), dim(&env_hint));
            use std::io::Write; std::io::stdout().flush().ok();
            let key = read_secret_line()?;
            if key.is_empty() { bail!("API key cannot be empty."); }
            let mut store = CredentialsStore::load();
            store.accounts.insert(name.clone(), Credential::Apikey { key });
            store.save()?;
            // Clear any auth_failed state
            let state_p = crate::config::state_path();
            let state = crate::state::StateStore::load(&state_p);
            state.clear_auth_failed(&name);
            println!("  {} API key updated for '{}'.", green(CHECK), bold(&name));
            offer_restart(config_override).await;
        }

        // ── Update upstream URL (Local) ──────────────────────────────────────
        "upstream" => {
            let current = account.upstream_url.as_deref().unwrap_or("(not set)");
            print!("  {} Upstream URL [{}]: ", dim("·"), dim(current));
            use std::io::{BufRead, Write}; std::io::stdout().flush().ok();
            let mut input = String::new();
            std::io::stdin().lock().read_line(&mut input)?;
            let url = input.trim().to_string();
            if url.is_empty() { bail!("URL cannot be empty."); }
            update_account_toml_field(config_override.as_deref(), &name, "upstream_url", &url)?;
            println!("  {} Upstream URL updated for '{}'.", green(CHECK), bold(&name));
            offer_restart(config_override).await;
        }

        // ── Update model (Local / any) ───────────────────────────────────────
        "model" => {
            let current = account.model.as_deref().unwrap_or("(not set)");
            print!("  {} Model [{}]: ", dim("·"), dim(current));
            use std::io::{BufRead, Write}; std::io::stdout().flush().ok();
            let mut input = String::new();
            std::io::stdin().lock().read_line(&mut input)?;
            let model = input.trim().to_string();
            if model.is_empty() { bail!("Model cannot be empty."); }
            update_account_toml_field(config_override.as_deref(), &name, "model", &model)?;
            println!("  {} Model updated for '{}'.", green(CHECK), bold(&name));
            offer_restart(config_override).await;
        }

        // ── Log out (OAuth) ──────────────────────────────────────────────────
        "logout" => {
            return cmd_logout(config_override, Some(name), false).await;
        }

        // ── Remove account ───────────────────────────────────────────────────
        "remove" => {
            return cmd_remove_account(config_override, Some(name)).await;
        }

        _ => {}
    }

    println!();
    Ok(())
}

/// Update a single string field inside the `[[accounts]]` block for `account_name`
/// in the TOML config file (using toml_edit for safe structured editing).
fn update_account_toml_field(config_override: Option<&std::path::Path>, account_name: &str, field: &str, value: &str) -> Result<()> {
    let config_p = config_override.map(|p| p.to_path_buf()).unwrap_or_else(config_path);
    let text = std::fs::read_to_string(&config_p)?;
    let mut doc = text.parse::<toml_edit::DocumentMut>()
        .context("Failed to parse config TOML")?;
    if let Some(item) = doc.get_mut("accounts") {
        if let Some(arr) = item.as_array_of_tables_mut() {
            for table in arr.iter_mut() {
                if table.get("name").and_then(|v| v.as_str()) == Some(account_name) {
                    table.insert(field, toml_edit::value(value));
                }
            }
        }
    }
    std::fs::write(&config_p, doc.to_string())?;
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
            term::SelectItem { label: format!("{}  {}", bold("Claude Code"), dim("(claude.ai — Anthropic)")), value: "anthropic".into() },
            term::SelectItem { label: format!("{}  {}  {}", bold("Codex"), yellow("[beta]"), dim("(chatgpt.com — OpenAI)")), value: "openai".into() },
            term::SelectItem { label: format!("{}  {}", bold("Groq"),        dim("(api.groq.com — API key)")),               value: "groq".into() },
            term::SelectItem { label: format!("{}  {}", bold("Mistral"),     dim("(api.mistral.ai — API key)")),             value: "mistral".into() },
            term::SelectItem { label: format!("{}  {}", bold("Together AI"), dim("(api.together.xyz — API key)")),           value: "together".into() },
            term::SelectItem { label: format!("{}  {}", bold("OpenRouter"),  dim("(openrouter.ai — API key)")),              value: "openrouter".into() },
            term::SelectItem { label: format!("{}  {}", bold("DeepSeek"),    dim("(api.deepseek.com — API key)")),           value: "deepseek".into() },
            term::SelectItem { label: format!("{}  {}", bold("Fireworks"),   dim("(api.fireworks.ai — API key)")),           value: "fireworks".into() },
            term::SelectItem { label: format!("{}  {}", bold("Gemini"),      dim("(generativelanguage.googleapis.com — API key)")), value: "gemini".into() },
            term::SelectItem { label: format!("{}  {}", bold("OpenAI API"),  dim("(api.openai.com — API key)")),             value: "openai-api".into() },
            term::SelectItem { label: format!("{}  {}", bold("Local"),       dim("(Ollama, LM Studio, etc. — no auth)")),   value: "local".into() },
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
        use crate::provider::AuthKind;
        // For OAuth providers: offer to re-auth existing uncredentialed accounts.
        // For API-key / Local: always prompt for a new name (credentials don't expire the same way).
        let missing_oauth: Vec<_> = if provider.auth_kind() == AuthKind::OAuth {
            let config = crate::config::load_config(config_override.as_deref())?;
            config.accounts.iter()
                .filter(|a| a.provider == provider && a.credential.is_none())
                .map(|a| a.name.clone())
                .collect()
        } else {
            vec![]
        };

        match missing_oauth.len() {
            1 => {
                println!("  {} Authorizing account {}", yellow("↻"), bold(&format!("'{}'", missing_oauth[0])));
                println!();
                (missing_oauth[0].clone(), true)
            }
            n if n > 1 => {
                let items: Vec<term::SelectItem> = missing_oauth.iter().map(|a| term::SelectItem {
                    label: bold(a).to_string(),
                    value: a.clone(),
                }).collect();
                match term::select("Which account to authorize?", &items, 0) {
                    Some(v) => (v, true),
                    None => return Ok(()),
                }
            }
            _ => {
                // Prompt for a new name
                let hint = format!("({} account name, e.g. \"{}\")", provider, provider.to_string().to_lowercase().replace(' ', "-"));
                print!("  {} Account name {}: ", dim("·"), dim(&hint));
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

    // ── Step 3: authenticate ─────────────────────────────────────────────────
    use crate::provider::AuthKind;
    let credential: Option<Credential> = match provider.auth_kind() {
        AuthKind::OAuth => {
            let mut cred = match provider {
                Provider::Anthropic => run_oauth_flow().await?,
                Provider::OpenAI    => crate::oauth::run_openai_oauth_flow().await?,
                _ => unreachable!(),
            };
            // Fetch email (non-fatal)
            let email = match provider {
                Provider::Anthropic => crate::oauth::fetch_account_email(&cred.access_token).await,
                Provider::OpenAI    => crate::oauth::fetch_openai_account_email(&cred.access_token).await,
                _ => None,
            };
            if let Some(ref e) = email {
                println!("  {} Signed in as {}", green(CHECK), bold(e));
            }
            cred.email = email;
            // Keep ~/.codex/auth.json in sync so the Codex CLI works without re-login.
            if cred.id_token.is_some() {
                crate::oauth::write_codex_auth_file(&cred);
            }
            Some(Credential::Oauth(cred))
        }
        AuthKind::ApiKey => {
            // Show env-var hint if available
            let env_hint = provider.api_key_env_var()
                .map(|v| format!(" (or set {} in your environment)", v))
                .unwrap_or_default();
            print!("  {} API key{}: ", dim("·"), dim(&env_hint));
            use std::io::Write;
            std::io::stdout().flush().ok();
            // Read key — use rpassword for masked input if available, otherwise plain readline
            let key = read_secret_line()?;
            if key.is_empty() { bail!("API key cannot be empty."); }
            println!("  {} API key saved.", green(CHECK));
            Some(Credential::Apikey { key })
        }
        AuthKind::None => {
            // Local provider — no credential needed, but we may need upstream_url
            None
        }
    };

    // For Local provider, prompt for upstream URL
    let upstream_url: Option<String> = if matches!(provider, Provider::Local) {
        print!("  {} Upstream URL (e.g. http://localhost:11434): ", dim("·"));
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let u = input.trim().to_string();
        if u.is_empty() { bail!("Upstream URL cannot be empty for local provider."); }
        Some(u)
    } else {
        None
    };

    // ── Step 4: persist ──────────────────────────────────────────────────────
    if !already_in_config {
        let mut config_text = existing_config;
        let mut block = format!("\n[[accounts]]\nname = \"{name}\"\n");
        if !matches!(provider, Provider::Anthropic) {
            block.push_str(&format!("provider = \"{provider}\"\n"));
        }
        if let Some(ref url) = upstream_url {
            block.push_str(&format!("upstream_url = \"{url}\"\n"));
        }
        config_text.push_str(&block);
        std::fs::write(&config_p, &config_text)?;
    }

    if let Some(cred) = credential {
        let mut store = CredentialsStore::load();
        store.accounts.insert(name.clone(), cred);
        store.save()?;
    }

    println!();
    println!("  {} Account {} added.", green(CHECK), bold(&format!("'{name}'")));
    offer_restart(config_override).await;
    println!();
    Ok(())
}

/// Read a line from stdin without echoing (for API keys). Falls back to
/// plain readline if the terminal doesn't support it.
fn read_secret_line() -> Result<String> {
    // Try rpassword-style: disable echo via termios, then restore.
    #[cfg(unix)]
    {
        use std::io::{BufRead, Write};
        // Disable echo
        let _ = std::process::Command::new("stty").arg("-echo").status();
        let mut out = std::io::stdout();
        let _ = out.flush();
        let stdin = std::io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        // Re-enable echo and print newline
        let _ = std::process::Command::new("stty").arg("echo").status();
        println!();
        return Ok(line.trim().to_string());
    }
    #[cfg(not(unix))]
    {
        use std::io::{BufRead, Write};
        let mut out = std::io::stdout();
        let _ = out.flush();
        let stdin = std::io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        return Ok(line.trim().to_string());
    }
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
            let email = a.credential.as_ref().and_then(|c| c.email()).unwrap_or("");
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

    if !term::confirm(&format!("Remove account '{name}'? This cannot be undone.")) {
        println!("  {} Cancelled.", dim("·"));
        println!();
        return Ok(());
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
            let email = a.credential.as_ref().and_then(|c| c.email()).unwrap_or("");
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

    // Reconfirm for --all or multi-account logout
    if names.len() > 1 {
        if !term::confirm(&format!("Log out all {} accounts? You will need to re-authorize each one.", names.len())) {
            println!("  {} Cancelled.", dim("·"));
            println!();
            return Ok(());
        }
    }

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
            if revoke_token(cred.access_token()).await {
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
                    if let Some(oauth) = cred.as_oauth() {
                        if let Ok(Ok(fresh)) = tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            account.provider.refresh_token(oauth),
                        ).await {
                            let mut store = CredentialsStore::load();
                            store.accounts.insert(account.name.clone(), Credential::Oauth(fresh.clone()));
                            store.save().ok();
                            account.credential = Some(Credential::Oauth(fresh));
                        }
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
    // Skip interactive setup when stdin is not a TTY (e.g. curl | sh) to
    // avoid blocking on macOS Keychain or OAuth prompts.
    let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) != 0 };
    if !config_p.exists() && stdin_is_tty {
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
                    if let Some(oauth) = cred.as_oauth() {
                        print!("  {} Refreshing '{}'… ", yellow("↻"), account.name);
                        std::io::stdout().flush().ok();
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            account.provider.refresh_token(oauth),
                        ).await {
                            Ok(Ok(fresh)) => {
                                println!("{}", green("done"));
                                let mut store = CredentialsStore::load();
                                store.accounts.insert(account.name.clone(), Credential::Oauth(fresh.clone()));
                                store.save().ok();
                                account.credential = Some(Credential::Oauth(fresh));
                            }
                            Ok(Err(e)) => println!("{}", yellow(&format!("failed ({})", e))),
                            Err(_)    => println!("{}", yellow("timed out")),
                        }
                    }
                }
            }
        }
        let lp = log_path();
        let log_level = if verbose { "debug" } else { config.server.log_level.as_str() };
        crate::logging::prune_old_logs(&lp, 7);
        let _log_guard = crate::logging::setup(&lp, log_level)?;
        let col = 13usize;
        println!("  {}  {} {}", dim(&pad("listening", col)), dim("[control]"),
            green_bold(&format!("http://{host}:{}", config.server.control_port)));
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

    // Wait until the control plane is accepting connections (up to 8 s)
    let control_port = config.server.control_port;
    let ready = wait_for_health(&host, control_port, 8).await;

    // Auto-write ANTHROPIC_BASE_URL to shell profile (silent if already there)
    auto_write_shell_export(port);

    let account_names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
    let status_line = if ready {
        format!("{}  {}  {}", green(DOT), green_bold("running"), cyan(&format!("http://{host}:{control_port}")))
    } else {
        format!("{}  {}  {}", yellow(DOT), yellow("starting"), dim(&format!("http://{host}:{control_port}")))
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
    store.accounts.insert("main".into(), Credential::Oauth(cred));
    store.save()?;

    Ok(())
}

async fn wait_for_health(host: &str, port: u16, timeout_secs: u64) -> bool {
    let url = format!("http://{host}:{port}/health");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap_or_default();
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        if client.get(&url).send().await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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

    // Fetch live status from the control plane (sees all accounts).
    let live: Option<serde_json::Value> = reqwest::get(
        format!("http://{}:{}/status", config.server.host, config.server.control_port)
    ).await.ok().and_then(|r| futures_executor_hack(r));

    // Back-fill missing emails (existing accounts set up before email support).
    // Fetch in parallel, persist any that are new.
    let mut store_dirty = false;
    let mut store = CredentialsStore::load();
    for acc in &mut config.accounts {
        if acc.credential.as_ref().map(|c| c.email().is_none()).unwrap_or(false) {
            let token = acc.credential.as_ref().map(|c| c.access_token().to_owned()).unwrap_or_default();
            if let Some(email) = crate::oauth::fetch_account_email(&token).await {
                if let Some(oauth) = acc.credential.as_mut().and_then(|c| c.as_oauth_mut()) {
                    oauth.email = Some(email.clone());
                }
                if let Some(stored) = store.accounts.get_mut(&acc.name) {
                    if let Some(oauth) = stored.as_oauth_mut() {
                        oauth.email = Some(email);
                        store_dirty = true;
                    }
                }
            }
        }
    }
    if store_dirty {
        store.save().ok();
    }

    // Build running address: show the control port when alive.
    let addr_str = if live.is_some() {
        cyan(&format!(":{}", config.server.control_port))
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
    let savings_line: Option<String> = live.as_ref().and_then(|v| {
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

    // Build per-provider account counts for the splash right panel.
    let provider_lines: Vec<String> = {
        let mut counts: Vec<(String, usize)> = vec![];
        for acc in &config.accounts {
            let label = match &acc.provider {
                crate::provider::Provider::Anthropic   => "Claude Code",
                crate::provider::Provider::OpenAI      => "Codex",
                crate::provider::Provider::OpenAIApi   => "OpenAI",
                crate::provider::Provider::OllamaCloud => "Ollama",
                crate::provider::Provider::Groq        => "Groq",
                crate::provider::Provider::Mistral     => "Mistral",
                crate::provider::Provider::Together    => "Together",
                crate::provider::Provider::OpenRouter  => "OpenRouter",
                crate::provider::Provider::DeepSeek    => "DeepSeek",
                crate::provider::Provider::Fireworks   => "Fireworks",
                crate::provider::Provider::Gemini      => "Gemini",
                crate::provider::Provider::Local       => "Local",
            };
            if let Some(entry) = counts.iter_mut().find(|(l, _)| l == label) {
                entry.1 += 1;
            } else {
                counts.push((label.to_string(), 1));
            }
        }
        let mut lines = vec![
            "accounts connected".to_string(),
            String::new(),
        ];
        lines.extend(counts.iter().map(|(label, n)| {
            let noun = if *n == 1 { "account" } else { "accounts" };
            format!("{n} {label} {noun}")
        }));
        lines
    };

    let title = format!("shunt  v{}", env!("CARGO_PKG_VERSION"));
    print_status_splash(&title, provider_lines);
    println!();

    let pinned_account = live.as_ref().and_then(|v| v["pinned"].as_str()).map(|s| s.to_owned());
    let last_used_account = live.as_ref().and_then(|v| v["last_used"].as_str()).map(|s| s.to_owned());

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
        let live_acc = live.as_ref()
            .and_then(|v| v["accounts"].as_array())
            .and_then(|arr| arr.iter().find(|a| a["name"] == acc.name));

        let status = live_acc.and_then(|a| a["status"].as_str()).unwrap_or("offline");

        let (status_icon, status_text): (String, String) = match status {
            "available"       => (green(CHECK), green("available")),
            "cooling"         => (yellow("↻"),  yellow("cooling")),
            "disabled"        => (red(CROSS),   red("disabled")),
            "reauth_required" => (red(CROSS),   red("session expired")),
            _ => {
                use crate::provider::AuthKind;
                match &acc.credential {
                    // Local/None-auth providers don't need a credential — show offline, not error.
                    None if acc.provider.auth_kind() == AuthKind::None
                                                  => (dim(EMPTY),   dim("offline")),
                    None                          => (red(CROSS),   red("no credential")),
                    Some(c) if c.needs_refresh()  => (yellow(CROSS), yellow("token expired")),
                    _                             => (dim(EMPTY),   dim("offline")),
                }
            }
        };

        let plan_label: &str = match &acc.provider {
            crate::provider::Provider::OpenAI => match acc.plan_type.to_lowercase().as_str() {
                "plus"  => "ChatGPT Plus [beta]",
                "pro"   => "ChatGPT Pro [beta]",
                "team"  => "ChatGPT Team [beta]",
                _       => "ChatGPT [beta]",
            },
            crate::provider::Provider::Anthropic => match acc.plan_type.to_lowercase().as_str() {
                "max" | "claude_max" => "Claude Max",
                "team"               => "Claude Team",
                _                    => "Claude Pro",
            },
            // API-key and Local providers don't have Claude plan tiers.
            _ => "",
        };
        let email_str = acc.credential.as_ref().and_then(|c| c.email()).unwrap_or("");

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
        let provider_label = match &acc.provider {
            crate::provider::Provider::Anthropic => String::new(),
            crate::provider::Provider::OpenAI    => "chatgpt".to_string(),
            p                                    => p.to_string(),
        };
        let provider_badge = if provider_label.is_empty() {
            String::new()
        } else {
            format!("  {}  {}", dim("·"), dim(&format!("[{provider_label}]")))
        };
        if !email_str.is_empty() {
            println!("{}", card_row(&format!("{}{}", dim(email_str), provider_badge)));
        } else if !provider_badge.is_empty() {
            println!("{}", card_row(&dim(&format!("[{provider_label}]"))));
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
        } else if acc.credential.is_none() && acc.provider.auth_kind() != crate::provider::AuthKind::None {
            println!("{}", card_row(&format!("{}  run {}",
                dim("·"), cyan(&format!("shunt add-account {}", acc.name)))));
        } else if status == "reauth_required" {
            println!("{}", card_row(&format!("{}  run {}",
                dim("·"), cyan(&format!("shunt add-account {}", acc.name)))));
        } else if live.is_some() && live_acc.is_some() {
            match &acc.provider {
                crate::provider::Provider::Anthropic =>
                    println!("{}", card_row(&dim("· quota data will appear after first request"))),
                crate::provider::Provider::Local => {
                    if acc.model.is_none() {
                        println!("{}", card_row(&dim(&format!(
                            "· tip: set model = \"your-model\" in config for this account"
                        ))));
                    }
                }
                _ =>
                    println!("{}", card_row(&dim("· quota tracking unavailable (provider doesn't report utilization)"))),
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
    let use_url = format!("http://{}:{}/use", config.server.host, config.server.control_port);

    // Fetch live state for utilization info
    let live: Option<serde_json::Value> = reqwest::get(
        &format!("http://{}:{}/status", config.server.host, config.server.control_port)
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

        let email = a.credential.as_ref().and_then(|c| c.email()).unwrap_or("");
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

/// Circuit shunt symbol: rectangle with wires extending left/right from the mid row,
/// and two legs going down from the bottom.
///
///   ·  ██████  ·
///   ███      ███   ← wire row (middle of box)
///   ·  ██████  ·
///   ·    █ █   ·   ← legs
fn build_logo_lines(h: usize, w: usize) -> Vec<String> {
    if h == 0 || w < 5 { return vec![]; }

    let box_l = w / 4;
    let box_r = w - w / 4;  // exclusive
    let leg_h = (h / 4).max(1);
    let box_h = h.saturating_sub(leg_h).max(2); // at least top + bottom row
    let wire_row = box_h / 2; // wire connects at vertical mid of box

    // Mirror from each side so legs are symmetric around centre.
    let leg1 = w / 3;
    let leg2 = w - w / 3 - 1;

    let mut out = Vec::new();
    for row in 0..h {
        let mut r = vec![' '; w];
        if row < box_h {
            let is_top = row == 0;
            let is_bot = row == box_h - 1;
            if is_top || is_bot {
                for j in box_l..box_r { r[j] = '█'; }
            } else {
                r[box_l]     = '█';
                r[box_r - 1] = '█';
            }
            if row == wire_row {
                for j in 0..box_l  { r[j] = '█'; }
                for j in box_r..w  { r[j] = '█'; }
            }
        } else {
            if leg1 < w { r[leg1] = '█'; }
            if leg2 < w { r[leg2] = '█'; }
        }
        out.push(r.into_iter().collect());
    }
    out
}

fn render_splash_frame(
    f: &mut ratatui::Frame,
    title_raw: &str,
    subtitle_raw: &str,
    right_lines: &[String],
) {
    use ratatui::{
        layout::{Constraint, Direction, Layout},
        style::{Color, Style},
        text::Line,
        widgets::{Block, Borders, Paragraph},
    };

    let brand    = Color::Indexed(154); // #afd700 bright lime-green
    let dim_col  = Color::Indexed(240); // #585858 gray
    let dk_green = Color::Indexed(28);  // #008700 dark green

    // Fixed-width box — does not stretch to fill the terminal.
    const BOX_W: u16 = 70;
    let full = f.area();
    let area = Layout::new(Direction::Horizontal, [
        Constraint::Length(BOX_W.min(full.width)),
        Constraint::Fill(1),
    ]).split(full)[0];

    // Outer bordered box.
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(dk_green))
        .title(Line::styled(format!(" {title_raw} "), Style::default().fg(brand)));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    const CONTENT_H: u16 = 4;
    const LOGO_W:    u16 = 10;

    // Main horizontal split: left half | separator | right half
    let cols = Layout::new(Direction::Horizontal, [
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ]).split(inner);
    let (left_area, sep_area, right_area) = (cols[0], cols[1], cols[2]);

    // Left: vertical centering around the content row.
    let has_sub = !subtitle_raw.is_empty();
    let left_v_constraints: Vec<Constraint> = if has_sub {
        vec![Constraint::Fill(1), Constraint::Length(CONTENT_H), Constraint::Fill(1), Constraint::Length(1)]
    } else {
        vec![Constraint::Fill(1), Constraint::Length(CONTENT_H), Constraint::Fill(1)]
    };
    let left_v = Layout::new(Direction::Vertical, left_v_constraints).split(left_area);
    let content_row = left_v[1];

    // Left content: logo centered horizontally within the left half
    let h = Layout::new(Direction::Horizontal, [
        Constraint::Fill(1),
        Constraint::Length(LOGO_W),
        Constraint::Fill(1),
    ]).split(content_row);

    let logo = build_logo_lines(CONTENT_H as usize, LOGO_W as usize);
    f.render_widget(
        Paragraph::new(logo.into_iter()
            .map(|l| Line::styled(l, Style::default().fg(brand)))
            .collect::<Vec<_>>()),
        h[1],
    );

    if has_sub {
        f.render_widget(
            Paragraph::new(subtitle_raw).style(Style::default().fg(dim_col)),
            left_v[3],
        );
    }

    // Vertical separator spanning full inner height.
    let sep_lines: Vec<Line> = (0..sep_area.height)
        .map(|_| Line::styled("│", Style::default().fg(dk_green)))
        .collect();
    f.render_widget(Paragraph::new(sep_lines), sep_area);

    // Right: custom lines (center-aligned) or static description (right-aligned).
    let static_desc: Vec<String> = vec![
        "Pool multiple Claude accounts".into(),
        "behind a single endpoint.".into(),
        "Maximise rate limits across".into(),
        "all accounts automatically.".into(),
    ];
    let (desc_lines, alignment) = if right_lines.is_empty() {
        (static_desc.as_slice(), ratatui::layout::Alignment::Center)
    } else {
        (right_lines, ratatui::layout::Alignment::Center)
    };
    let desc: Vec<Line> = desc_lines.iter()
        .map(|s| Line::styled(s.clone(), Style::default().fg(dim_col)))
        .collect();
    let desc_h = desc.len() as u16;
    // 1-col left spacer so text doesn't touch the separator.
    let right_inner = Layout::new(Direction::Horizontal, [
        Constraint::Length(1),
        Constraint::Fill(1),
    ]).split(right_area)[1];
    let right_v = Layout::new(Direction::Vertical, [
        Constraint::Fill(1),
        Constraint::Length(desc_h),
        Constraint::Fill(1),
    ]).split(right_inner);
    f.render_widget(
        Paragraph::new(desc).alignment(alignment),
        right_v[1],
    );
}


/// Print the splash using ratatui inline viewport — redraws live on resize.
fn print_splash(info: &[String]) {
    use ratatui::{backend::CrosstermBackend, Terminal, TerminalOptions, Viewport};
    use crossterm::{event::{self, Event}, terminal as cterm};
    use std::io::stdout;

    let title_raw    = info.get(0).map(|s| strip_ansi(s)).unwrap_or_default();
    let subtitle_raw = info.get(1).map(|s| strip_ansi(s)).unwrap_or_default();

    // Logo = 4 rows content + 2 border + 2 vertical padding + optional subtitle
    let splash_h: u16 = 4 + 2 + 2 + if subtitle_raw.is_empty() { 0 } else { 1 };

    let mut terminal = match Terminal::with_options(
        CrosstermBackend::new(stdout()),
        TerminalOptions { viewport: Viewport::Inline(splash_h) },
    ) {
        Ok(t) => t,
        Err(_) => {
            // Fallback: plain text header if ratatui fails (e.g. non-TTY).
            println!("\n  ◆  {}  {}\n", title_raw.trim(), subtitle_raw);
            return;
        }
    };

    let draw = |t: &mut Terminal<CrosstermBackend<std::io::Stdout>>| {
        t.draw(|f| render_splash_frame(f, &title_raw, &subtitle_raw, &[])).ok();
    };

    draw(&mut terminal);

    // Redraw on resize for up to 500 ms.
    let _ = cterm::enable_raw_mode();
    let dl = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        let rem = dl.saturating_duration_since(std::time::Instant::now());
        if rem.is_zero() { break; }
        if event::poll(rem).unwrap_or(false) {
            match event::read() {
                Ok(Event::Resize(_, _)) => draw(&mut terminal),
                _ => break,
            }
        } else { break; }
    }
    let _ = cterm::disable_raw_mode();
    let _ = terminal.show_cursor();
    // Ratatui leaves the cursor at the end of the inline viewport's last line.
    // \r resets to column 0 before \n moves down, so subsequent output is left-aligned.
    print!("\r\n");
}

/// Like print_splash but with custom right-side lines (used by cmd_status).
fn print_status_splash(title: &str, right_lines: Vec<String>) {
    use ratatui::{backend::CrosstermBackend, Terminal, TerminalOptions, Viewport};
    use crossterm::{event::{self, Event}, terminal as cterm};
    use std::io::stdout;

    // Ensure top and bottom Fill(1) each get ≥1 row:
    // inner_h = splash_h - 2; need inner_h >= content + 2 fills → splash_h >= len + 4
    let splash_h: u16 = (right_lines.len() as u16 + 4).max(8);
    let right = right_lines.clone();

    let mut terminal = match Terminal::with_options(
        CrosstermBackend::new(stdout()),
        TerminalOptions { viewport: Viewport::Inline(splash_h) },
    ) {
        Ok(t) => t,
        Err(_) => {
            println!("\n  ◆  {title}\n");
            for l in &right_lines { println!("     {l}"); }
            return;
        }
    };

    let draw = |t: &mut Terminal<CrosstermBackend<std::io::Stdout>>, r: &[String]| {
        t.draw(|f| render_splash_frame(f, title, "", r)).ok();
    };

    draw(&mut terminal, &right);

    let _ = cterm::enable_raw_mode();
    let dl = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        let rem = dl.saturating_duration_since(std::time::Instant::now());
        if rem.is_zero() { break; }
        if event::poll(rem).unwrap_or(false) {
            match event::read() {
                Ok(Event::Resize(_, _)) => draw(&mut terminal, &right),
                _ => break,
            }
        } else { break; }
    }
    let _ = cterm::disable_raw_mode();
    let _ = terminal.show_cursor();
    print!("\r\n");
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

    // Save all accounts for the control plane before the provider loop consumes them.
    let all_accounts = config.accounts.clone();
    let control_port = config.server.control_port;

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

        // The Anthropic proxy gets ALL accounts so non-Anthropic accounts (e.g. codex/chatgpt.com)
        // act as fallback when Anthropic accounts are exhausted. Each non-Anthropic account already
        // has upstream_url pre-populated (e.g. "https://chatgpt.com") by the config loader.
        let proxy_accounts = if provider == Provider::Anthropic {
            all_accounts.clone()
        } else {
            accounts
        };

        let provider_config = Config {
            accounts: proxy_accounts,
            server: ServerConfig {
                host: host.to_owned(),
                port,
                upstream_url: provider.default_upstream_url().to_owned(),
                ..config.server.clone()
            },
            config_file: config.config_file.clone(),
            model_mapping: config.model_mapping.clone(),
        };

        let anthropic_url = if provider == Provider::OpenAI {
            Some(format!("http://{}:{}", host, primary_port))
        } else {
            None
        };
        let (app, live_creds) = crate::proxy::create_proxy_app(provider_config.clone(), state.clone(), anthropic_url)?;
        let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
            .await
            .with_context(|| format!("cannot bind {host}:{port} for {provider_str} proxy"))?;

        let cfg_arc = std::sync::Arc::new(provider_config);
        tokio::spawn(crate::proxy::prefetch_rate_limits(cfg_arc.clone(), state.clone(), live_creds.clone()));
        tokio::spawn(crate::proxy::openai_token_refresh_loop(cfg_arc.clone(), state.clone(), live_creds.clone()));
        tokio::spawn(crate::proxy::cooldown_watcher(cfg_arc.clone(), state.clone(), live_creds.clone()));
        tokio::spawn(crate::proxy::recovery_watcher(cfg_arc, state.clone(), live_creds));
        handles.push(tokio::spawn(async move {
            axum::serve(listener, app).await
        }));
    }

    // Spawn the control plane — management endpoints with visibility into ALL accounts.
    let control_config = Config {
        accounts: all_accounts,
        server: ServerConfig {
            host: host.to_owned(),
            port: control_port,
            upstream_url: "https://api.anthropic.com".to_owned(),
            ..config.server.clone()
        },
        config_file: config.config_file.clone(),
        model_mapping: config.model_mapping.clone(),
    };
    let control_app = crate::proxy::create_control_app(control_config, state.clone())?;
    let control_listener = tokio::net::TcpListener::bind(format!("{host}:{control_port}"))
        .await
        .with_context(|| format!("cannot bind {host}:{control_port} for control plane"))?;
    handles.push(tokio::spawn(async move {
        axum::serve(control_listener, control_app).await
    }));

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
    let base_url = format!("http://{}:{}", config.server.host, config.server.control_port);

    // Quick check: is the proxy running? Hard 3-second timeout so we don't hang.
    let running = reqwest::Client::new()
        .get(format!("{base_url}/health"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .is_ok();
    if !running {
        println!();
        println!("  {} Proxy is not running.", red(CROSS));
        println!("  {} Start it first with {}.", dim("·"), cyan("shunt start"));
        println!();
        return Ok(());
    }

    crate::monitor::run_monitor(&base_url).await
}

// ---------------------------------------------------------------------------
// remote
// ---------------------------------------------------------------------------

async fn cmd_remote(code: Option<String>) -> Result<()> {
    // Host mode needs the local shunt URL; client mode only needs the relay URL.
    let (relay_url, local_url) = if code.is_none() {
        let config = crate::config::load_config(None)?;
        let local = format!("http://{}:{}", config.server.host, config.server.port);
        let relay = config.server.relay_url.clone();
        (Some(relay), local)
    } else {
        let relay_url = std::env::var("SHUNT_RELAY_URL").ok();
        (relay_url, String::new())
    };
    crate::remote::run_remote(code, relay_url, local_url).await
}

// update
// ---------------------------------------------------------------------------

async fn cmd_update() -> Result<()> {
    const REPO: &str = "ramc10/shunt";
    let current = env!("CARGO_PKG_VERSION");

    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{current}"))),
    ]);

    // Each status line is prefixed with \r so it starts at column 0 regardless
    // of where the cursor was left after the ratatui inline viewport.
    macro_rules! status {
        ($($arg:tt)*) => { println!("\r{}", format_args!($($arg)*)) };
    }

    status!("  {} Checking for updates…", dim("·"));

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

    // Compare versions numerically to correctly handle both upgrades and the
    // case where the installed build is newer than the latest GitHub release.
    if parse_version(latest) <= parse_version(current) {
        status!("  {} Already up to date ({})", green(CHECK), bold(&format!("v{current}")));
        println!();
        return Ok(());
    }

    status!("  {} Update available: {}  →  {}", green("↑"),
        dim(&format!("v{current}")), bold_white(&format!("v{latest}")));
    println!();

    // Detect platform
    let target = detect_update_target()?;
    let archive_name = format!("shunt-v{latest}-{target}.tar.gz");
    let url = format!(
        "https://github.com/{REPO}/releases/download/v{latest}/{archive_name}"
    );

    print!("\r  {} Downloading {}… ", dim("↓"), dim(&archive_name));
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

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // macOS: sign the temp file BEFORE replacing the live binary so Gatekeeper
    // never sees an unsigned binary on disk even if the process is killed mid-update.
    #[cfg(target_os = "macos")]
    {
        let p = tmp_path.display().to_string();
        std::process::Command::new("xattr").args(["-dr", "com.apple.quarantine", &p])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().ok();
        std::process::Command::new("codesign").args(["--force", "--deep", "--sign", "-", &p])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().ok();
    }

    // Atomic replace — new binary is already signed, so this is safe.
    std::fs::rename(&tmp_path, &exe_path)
        .context("Failed to replace binary (try running with sudo?)")?;

    status!("  {} Updated to {}", green(CHECK), bold_white(&format!("v{latest}")));
    println!();
    Ok(())
}

/// Parse a "major.minor.patch" version string into a comparable tuple.
/// Missing components default to 0.
fn parse_version(s: &str) -> (u32, u32, u32) {
    let mut it = s.split('.');
    let maj = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let min = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let pat = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (maj, min, pat)
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

    // If no flags given, show interactive menu
    // use an enum to track the chosen mode cleanly
    #[derive(Debug)]
    enum ShareMode { Lan, Tunnel, CustomDomain, Stop }

    let mode: ShareMode = if tunnel {
        ShareMode::Tunnel
    } else if stop {
        ShareMode::Stop
    } else {
        print_splash(&[
            format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
            dim("Remote sharing").to_string(),
            String::new(),
        ]);
        let top_items = vec![
            term::SelectItem {
                label: format!("{}  {}", bold("Local network (LAN)"),
                    dim("— same Wi-Fi only, no internet required")),
                value: "lan".into(),
            },
            term::SelectItem {
                label: format!("{}  {}", bold("Online"),
                    dim("— share over the internet")),
                value: "online".into(),
            },
            term::SelectItem {
                label: format!("{}  {}", bold("Stop sharing"),
                    dim("— revert to localhost-only")),
                value: "stop".into(),
            },
        ];
        match term::select("How do you want to share?", &top_items, 0).as_deref() {
            Some("lan")    => ShareMode::Lan,
            Some("stop")   => ShareMode::Stop,
            Some("online") => {
                // Sub-menu: temporary vs custom domain
                let existing_domain = crate::config::load_config(Some(&config_p))
                    .ok()
                    .and_then(|c| c.server.custom_domain.clone());
                let domain_label = match &existing_domain {
                    Some(d) => format!("{}  {}",
                        bold("Custom domain (permanent)"),
                        dim(&format!("— {} · your domain", d))),
                    None => format!("{}  {}",
                        bold("Custom domain (permanent)"),
                        dim("— your own domain, always-on")),
                };
                let online_items = vec![
                    term::SelectItem {
                        label: format!("{}  {}",
                            bold("Temporary (Cloudflare tunnel)"),
                            dim("— free, random URL, session only")),
                        value: "tunnel".into(),
                    },
                    term::SelectItem {
                        label: domain_label,
                        value: "custom".into(),
                    },
                ];
                match term::select("Online sharing type:", &online_items, 0).as_deref() {
                    Some("tunnel") => ShareMode::Tunnel,
                    Some("custom") => ShareMode::CustomDomain,
                    _ => return Ok(()),
                }
            }
            _ => return Ok(()),
        }
    };

    if matches!(mode, ShareMode::Stop) {
        // Reconfirm before disabling
        if !term::confirm("Stop sharing and revert to localhost-only?") {
            println!("  {} Cancelled.", dim("·"));
            println!();
            return Ok(());
        }

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

    // Generate or reuse existing remote key
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

    let (port, relay_url, saved_domain) = match crate::config::load_config(Some(&config_p)) {
        Ok(cfg) => {
            let relay = std::env::var("SHUNT_RELAY_URL")
                .unwrap_or_else(|_| cfg.server.relay_url.clone());
            (cfg.server.port, relay, cfg.server.custom_domain)
        }
        Err(_) => (8082u16,
            std::env::var("SHUNT_RELAY_URL")
                .unwrap_or_else(|_| "https://relay.ramcharan.shop".to_string()),
            None),
    };

    match mode {
        ShareMode::Tunnel => {
            print_splash(&[
                format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
                dim("Starting Cloudflare tunnel…").to_string(),
                String::new(),
            ]);
            println!("  {} Make sure the proxy is running: {}", dim("·"), cyan("shunt start"));
            println!();

            let url = start_cloudflare_tunnel(port)?;
            share_and_print(&url, &key, &relay_url, "Tunnel active", &[
                format!("  {} Code expires in 10 minutes — one-time use", dim("·")),
                format!("  {} Tunnel is active — keep this terminal open.", dim("·")),
                format!("  {} Press Ctrl+C to stop.", dim("·")),
            ]).await;

            tokio::signal::ctrl_c().await.ok();
            println!("\n  {} Tunnel closed.", dim("·"));
        }

        ShareMode::CustomDomain => {
            // Resolve domain: use saved, or prompt + save
            let domain = if let Some(d) = saved_domain {
                d
            } else {
                use std::io::Write;
                println!();
                println!("  {} Enter your domain URL (e.g. {}): ",
                    dim("·"), dim("https://shunt.mysite.com"));
                print!("    ");
                std::io::stdout().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let domain = input.trim().trim_end_matches('/').to_string();
                if domain.is_empty() {
                    bail!("No domain entered.");
                }
                if !domain.starts_with("http") {
                    bail!("Domain must start with http:// or https://");
                }
                // Save to config
                let mut cfg_text = std::fs::read_to_string(&config_p)?;
                cfg_text = insert_into_server_section(&cfg_text,
                    &format!("custom_domain = \"{domain}\""));
                std::fs::write(&config_p, &cfg_text)?;
                println!("  {} Saved {} to config.", green(CHECK), cyan(&domain));
                domain
            };

            share_and_print(&domain, &key, &relay_url, "Online sharing (custom domain)", &[
                format!("  {} Code expires in 10 minutes — one-time use", dim("·")),
                format!("  {} Make sure {} is pointing to port {} on this machine.",
                    dim("·"), cyan(&domain), port),
                format!("  {} Restart to apply: {}", dim("·"), cyan("shunt start")),
                format!("  {} To stop sharing:  {}", dim("·"), cyan("shunt share --stop")),
            ]).await;
        }

        ShareMode::Lan => {
            let ip = local_ip().unwrap_or_else(|| "<your-ip>".to_string());
            let base_url = format!("http://{ip}:{port}");

            share_and_print(&base_url, &key, &relay_url, "Remote sharing enabled (LAN)", &[
                format!("  {} Code expires in 10 minutes — one-time use", dim("·")),
                format!("  {} Both devices must be on the same network.", dim("·")),
                format!("  {} Restart to apply: {}", dim("·"), cyan("shunt start")),
                format!("  {} To stop sharing:  {}", dim("·"), cyan("shunt share --stop")),
            ]).await;
        }

        ShareMode::Stop => unreachable!(),
    }

    Ok(())
}

/// Push share code to relay and print the result (code or fallback manual instructions).
async fn share_and_print(base_url: &str, key: &str, relay_url: &str, subtitle: &str, hints: &[String]) {
    let share_code = crate::sync::generate_share_code();
    match crate::sync::push_share(&share_code, base_url, key, relay_url).await {
        Ok(()) => {
            print_splash(&[
                format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
                dim(subtitle).to_string(),
                String::new(),
            ]);
            println!("  {}  Share code:\n", green(CHECK));
            println!("      {}\n", cyan(&share_code));
            println!("  {} On the other device, run:", dim("·"));
            println!("       {}", cyan(&format!("shunt connect {share_code}")));
            println!();
            for hint in hints { println!("{hint}"); }
            println!();
        }
        Err(e) => {
            // Relay unavailable — fall back to manual env var instructions
            print_splash(&[
                format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
                dim(subtitle).to_string(),
                String::new(),
            ]);
            println!("  Set on the remote device:\n");
            println!("    {}{}", dim("export ANTHROPIC_BASE_URL="), cyan(base_url));
            println!("    {}{}", dim("export ANTHROPIC_API_KEY="), cyan(key));
            println!();
            println!("  {} (share code unavailable: {e})", dim("·"));
            for hint in hints { println!("{hint}"); }
            println!();
        }
    }
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

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

async fn cmd_connect(code: String) -> Result<()> {
    use std::io::{self, Write};

    crate::sync::validate_share_code(&code)?;

    let relay_url = std::env::var("SHUNT_RELAY_URL")
        .unwrap_or_else(|_| "https://relay.ramcharan.shop".to_string());

    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        dim("Connecting to remote shunt…").to_string(),
        String::new(),
    ]);

    println!("  {} Fetching credentials for {}…", dim("·"), cyan(&code));
    println!();

    let (base_url, api_key) = crate::sync::pull_share(&code, &relay_url).await?;

    println!("  {}  Retrieved:", green(CHECK));
    println!("      {} {}", dim("ANTHROPIC_BASE_URL ="), cyan(&base_url));
    println!("      {} {}", dim("ANTHROPIC_API_KEY  ="), cyan(&format!("{}…", &api_key[..api_key.len().min(12)])));
    println!();

    // --- Offer to write to shell profile ---
    let profile = detect_shell_profile();
    let prompt = match &profile {
        Some(p) => format!("  Write to {}? [Y/n]: ", dim(&p.display().to_string())),
        None => "  Write to shell profile? [Y/n]: ".into(),
    };
    print!("{prompt}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;

    if !matches!(buf.trim().to_lowercase().as_str(), "n" | "no") {
        match profile {
            Some(p) => {
                write_connect_vars_to_profile(&p, &base_url, &api_key)?;
            }
            None => {
                println!("  {} Could not detect shell profile. Set manually:", dim("·"));
                println!("      export ANTHROPIC_BASE_URL={base_url}");
                println!("      export ANTHROPIC_API_KEY={api_key}");
            }
        }
    }

    // --- Write to Claude Code settings.json ---
    if let Err(e) = write_claude_settings(&base_url, &api_key) {
        println!("  {} Could not write ~/.claude/settings.json: {e}", dim("·"));
    } else {
        println!("  {} Written to {}", green(CHECK), dim("~/.claude/settings.json"));
    }

    println!();
    println!("  {} Done! Restart shell or run: {}", green(CHECK),
        cyan(detect_shell_profile()
            .map(|p| format!("source {}", p.display()))
            .unwrap_or_else(|| "source ~/.zshrc".to_string()).as_str()));
    println!();

    Ok(())
}

/// Write ANTHROPIC_BASE_URL and ANTHROPIC_API_KEY to a shell profile, replacing
/// existing entries in-place or appending if absent.
fn write_connect_vars_to_profile(profile: &std::path::Path, base_url: &str, api_key: &str) -> Result<()> {
    use std::io::Write as _;

    let url_line = format!("export ANTHROPIC_BASE_URL={base_url}");
    let key_line = format!("export ANTHROPIC_API_KEY={api_key}");

    if profile.exists() {
        let contents = std::fs::read_to_string(profile)?;
        let has_url = contents.contains("ANTHROPIC_BASE_URL");
        let has_key = contents.contains("ANTHROPIC_API_KEY");

        if has_url || has_key {
            // Replace in-place
            let updated: String = contents
                .lines()
                .map(|l| {
                    if l.contains("ANTHROPIC_BASE_URL") {
                        url_line.as_str()
                    } else if l.contains("ANTHROPIC_API_KEY") {
                        key_line.as_str()
                    } else {
                        l
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            // Append any var that wasn't already there
            let mut final_content = updated;
            if !has_url {
                final_content.push_str(&format!("{url_line}\n"));
            }
            if !has_key {
                final_content.push_str(&format!("{key_line}\n"));
            }
            std::fs::write(profile, &final_content)?;
            println!("  {} Updated {} — {}", green(CHECK),
                dim(&profile.display().to_string()),
                cyan("ANTHROPIC_BASE_URL + ANTHROPIC_API_KEY"));
            return Ok(());
        }
    }

    // Append both vars
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(profile)?;
    writeln!(f, "\n# Added by shunt connect")?;
    writeln!(f, "{url_line}")?;
    writeln!(f, "{key_line}")?;
    println!("  {} Added to {} — {}", green(CHECK),
        dim(&profile.display().to_string()),
        cyan("ANTHROPIC_BASE_URL + ANTHROPIC_API_KEY"));
    Ok(())
}

/// Write ANTHROPIC_BASE_URL and ANTHROPIC_API_KEY into ~/.claude/settings.json
/// under the `env` key, creating the file if absent.
fn write_claude_settings(base_url: &str, api_key: &str) -> Result<()> {
    let home = dirs::home_dir().context("Cannot find home directory")?;
    let settings_path = home.join(".claude").join("settings.json");

    let mut root: serde_json::Value = if settings_path.exists() {
        let text = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Object(Default::default()))
    } else {
        serde_json::Value::Object(Default::default())
    };

    let obj = root.as_object_mut().context("settings.json root is not an object")?;
    let env = obj.entry("env").or_insert(serde_json::Value::Object(Default::default()));
    let env_obj = env.as_object_mut().context("settings.json 'env' is not an object")?;
    env_obj.insert("ANTHROPIC_BASE_URL".to_string(), serde_json::Value::String(base_url.to_string()));
    env_obj.insert("ANTHROPIC_API_KEY".to_string(), serde_json::Value::String(api_key.to_string()));

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&settings_path, serde_json::to_string_pretty(&root)?)?;
    Ok(())
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

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

async fn cmd_uninstall() -> Result<()> {
    use std::io::Write as _;

    // ── Collect what exists ───────────────────────────────────────────────────
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("shunt");

    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("shunt");

    let exe = std::env::current_exe().ok();

    // Shell profile line to remove
    let shell_profile = detect_shell_profile();
    let profile_has_export = shell_profile.as_ref().and_then(|p| {
        std::fs::read_to_string(p).ok()
    }).map(|s| s.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:")).unwrap_or(false);

    #[cfg(target_os = "macos")]
    let service_plist = {
        let p = service_plist_path();
        if p.exists() { Some(p) } else { None }
    };
    #[cfg(not(target_os = "macos"))]
    let service_plist: Option<PathBuf> = None;

    #[cfg(target_os = "linux")]
    let service_unit = {
        let p = service_unit_path();
        if p.exists() { Some(p) } else { None }
    };
    #[cfg(not(target_os = "linux"))]
    let service_unit: Option<PathBuf> = None;

    // ── Show plan ─────────────────────────────────────────────────────────────
    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        red("Uninstall").to_string(),
        String::new(),
    ]);

    println!("  This will permanently remove:");
    println!();

    if service_plist.is_some() || service_unit.is_some() {
        println!("  {}  Stop and unregister login service", red("✕"));
    }

    if config_dir.exists() {
        println!("  {}  {} {}", red("✕"), dim("delete"), cyan(&config_dir.display().to_string()));
    }
    if data_dir.exists() && data_dir != config_dir {
        println!("  {}  {} {}", red("✕"), dim("delete"), cyan(&data_dir.display().to_string()));
    }
    if let Some(ref p) = shell_profile {
        if profile_has_export {
            println!("  {}  {} ANTHROPIC_BASE_URL from {}", red("✕"), dim("remove"), cyan(&p.display().to_string()));
        }
    }
    if let Some(ref exe_path) = exe {
        println!("  {}  {} {}", red("✕"), dim("delete"), cyan(&exe_path.display().to_string()));
    }

    println!();

    // ── Reconfirm ─────────────────────────────────────────────────────────────
    if !term::confirm("Are you sure you want to completely uninstall shunt?") {
        println!("  {} Cancelled.", dim("·"));
        println!();
        return Ok(());
    }

    // Second confirmation — type "uninstall"
    println!();
    print!("  {} Type {} to confirm: ", dim("·"), bold("uninstall"));
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    if buf.trim() != "uninstall" {
        println!("  {} Cancelled.", dim("·"));
        println!();
        return Ok(());
    }

    println!();

    // ── Execute ───────────────────────────────────────────────────────────────

    // 1. Stop + unregister service
    #[cfg(target_os = "macos")]
    if let Some(ref p) = service_plist {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &p.display().to_string()])
            .output();
        let _ = std::fs::remove_file(p);
        println!("  {} Login service removed", green(CHECK));
    }
    #[cfg(target_os = "linux")]
    if let Some(ref p) = service_unit {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", "shunt"])
            .output();
        let _ = std::fs::remove_file(p);
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        println!("  {} Login service removed", green(CHECK));
    }

    // 2. Config + credentials dir
    if config_dir.exists() {
        std::fs::remove_dir_all(&config_dir)
            .with_context(|| format!("failed to remove {}", config_dir.display()))?;
        println!("  {} Config removed  {}", green(CHECK), dim(&config_dir.display().to_string()));
    }

    // 3. Data dir (logs, state, pid) — skip if same as config_dir (macOS)
    if data_dir.exists() && data_dir != config_dir {
        std::fs::remove_dir_all(&data_dir)
            .with_context(|| format!("failed to remove {}", data_dir.display()))?;
        println!("  {} Data removed    {}", green(CHECK), dim(&data_dir.display().to_string()));
    }

    // 4. Shell profile — strip ANTHROPIC_BASE_URL lines
    if let Some(ref profile_path) = shell_profile {
        if profile_has_export {
            if let Ok(contents) = std::fs::read_to_string(profile_path) {
                let cleaned: String = contents
                    .lines()
                    .filter(|l| {
                        !l.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:")
                            && *l != "# Added by shunt"
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // Preserve trailing newline if original had one
                let cleaned = if contents.ends_with('\n') {
                    format!("{cleaned}\n")
                } else {
                    cleaned
                };
                std::fs::write(profile_path, cleaned)?;
                println!("  {} Shell export removed  {}", green(CHECK),
                    dim(&profile_path.display().to_string()));
            }
        }
    }

    // 5. Binary — do this last so error messages can still print
    if let Some(exe_path) = exe {
        // Spawn a tiny shell to delete the binary after this process exits
        let path_str = exe_path.display().to_string();
        std::process::Command::new("sh")
            .args(["-c", &format!("sleep 0.3 && rm -f '{path_str}'")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok();
        println!("  {} Binary removed   {}", green(CHECK), dim(&exe_path.display().to_string()));
    }

    println!();
    println!("  {} shunt fully removed.", green(CHECK));
    println!("  {} Run {} to clear the proxy from this shell session.", dim("·"), cyan("unset ANTHROPIC_BASE_URL"));
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// service
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn service_plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Library/LaunchAgents/sh.shunt.proxy.plist")
}

#[cfg(target_os = "linux")]
fn service_unit_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config/systemd/user/shunt.service")
}

/// Write the platform service file and enable it to run at login.
/// Write the platform service file and attempt to activate it.
/// Returns `true` if the service was successfully loaded/started by the init
/// system, `false` if the plist/unit was written but activation was skipped
/// or timed out (e.g. SSH session without a GUI bootstrap context).
fn register_service() -> Result<bool> {
    let exe = std::env::current_exe().context("cannot locate current executable")?;
    let exe_str = exe.display().to_string();

    #[cfg(target_os = "macos")]
    {
        let plist_path = service_plist_path();
        let plist_was_present = plist_path.exists();
        if let Some(parent) = plist_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let plist = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>sh.shunt.proxy</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe_str}</string>
    <string>start</string>
    <string>--foreground</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{home}/Library/Logs/shunt.log</string>
  <key>StandardErrorPath</key>
  <string>{home}/Library/Logs/shunt.log</string>
</dict>
</plist>
"#,
            exe_str = exe_str,
            home = dirs::home_dir().unwrap_or_default().display(),
        );
        std::fs::write(&plist_path, &plist)?;

        // launchctl hangs in SSH sessions without a GUI bootstrap context.
        // Wrap both unload and load in threads with timeouts.
        let plist_str = plist_path.display().to_string();

        // Unload only if a plist was already there (i.e. this is a reinstall)
        if plist_was_present {
            let p = plist_str.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = std::process::Command::new("launchctl")
                    .args(["unload", &p])
                    .output();
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(std::time::Duration::from_secs(4));
        }

        // Load
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ok = std::process::Command::new("launchctl")
                .args(["load", "-w", &plist_str])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            let _ = tx.send(ok);
        });

        let loaded = rx
            .recv_timeout(std::time::Duration::from_secs(4))
            .unwrap_or(false);

        return Ok(loaded);
    }

    #[cfg(target_os = "linux")]
    {
        let unit_path = service_unit_path();
        if let Some(parent) = unit_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let unit = format!(
            "[Unit]\nDescription=shunt Claude Code proxy\nAfter=network.target\n\n\
             [Service]\nExecStart={exe_str} start --foreground\nRestart=always\nRestartSec=5\n\n\
             [Install]\nWantedBy=default.target\n"
        );
        std::fs::write(&unit_path, &unit)?;

        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();

        let out = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "shunt"])
            .output()
            .context("failed to run systemctl")?;

        return Ok(out.status.success());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("Service management is only supported on macOS and Linux.");

    #[allow(unreachable_code)]
    Ok(false)
}

async fn cmd_service_install() -> Result<()> {
    print_splash(&[
        format!("{}  {}", brand_green("shunt"), dim(&format!("v{}", env!("CARGO_PKG_VERSION")))),
        dim("Service install"),
        String::new(),
    ]);

    // 1. Ensure config + credentials exist.
    //    If stdin is not a TTY (e.g. curl | sh), skip interactive setup to
    //    avoid blocking on keychain/OAuth. The service is still registered and
    //    the proxy started; user runs `shunt setup` in a terminal to finish.
    let config_p = config_path();
    let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) != 0 };
    if !config_p.exists() {
        if stdin_is_tty {
            cmd_setup_auto(None).await?;
        } else {
            println!("  {} No config — run {} in a terminal to import credentials",
                yellow("·"), cyan("shunt setup"));
        }
    }

    // 2. Read port from config for shell export
    let port = crate::config::load_config(None)
        .map(|c| c.server.port)
        .unwrap_or(8082);

    // 3. Register the platform service
    print!("  {} Registering login service… ", dim("·"));
    use std::io::Write as _;
    std::io::stdout().flush().ok();
    let service_loaded = register_service()?;
    if service_loaded {
        println!("{}", green("done"));
    } else {
        println!("{}", dim("skipped (SSH session — activates on next login)"));
    }

    // 4. If launchd/systemd couldn't activate the service (e.g. SSH session
    //    without a GUI bootstrap context), start the proxy directly.
    if !service_loaded {
        print!("  {} Starting proxy… ", dim("·"));
        std::io::stdout().flush().ok();
        let exe = std::env::current_exe().context("cannot locate current executable")?;
        let _ = std::process::Command::new(&exe)
            .args(["start", "--daemon"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    // 5. Write shell export silently
    auto_write_shell_export(port);

    // 6. Wait for proxy to be healthy
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let config = crate::config::load_config(None).ok();
    let host = config.as_ref().map(|c| c.server.host.clone()).unwrap_or_else(|| "127.0.0.1".into());
    let running = wait_for_health(&host, port, 8).await;
    if !service_loaded {
        println!("{}", if running { green("done").to_string() } else { dim("starting…").to_string() });
    }

    println!();
    if running {
        println!("  {}  {}  {}", green(DOT), green_bold("proxy running"),
            cyan(&format!("http://{host}:{port}")));
    } else {
        println!("  {}  {} — proxy starting in background",
            yellow(DOT), yellow("starting"));
    }

    #[cfg(target_os = "macos")]
    if service_loaded {
        println!("  {}  LaunchAgent registered — starts automatically at login", green(CHECK));
    } else {
        println!("  {}  LaunchAgent written — will activate on next login", yellow("·"));
        println!("  {}  To activate now (in a GUI session): {}",
            dim("·"), cyan("launchctl load -w ~/Library/LaunchAgents/sh.shunt.proxy.plist"));
    }
    #[cfg(target_os = "linux")]
    if service_loaded {
        println!("  {}  systemd user unit registered — starts automatically at login", green(CHECK));
    } else {
        println!("  {}  systemd unit written — run {} to activate",
            yellow("·"), cyan("systemctl --user enable --now shunt"));
    }

    println!();
    println!("  {} To unregister: {}", dim("·"), cyan("shunt service uninstall"));
    println!();

    Ok(())
}

async fn cmd_service_uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = service_plist_path();
        if plist_path.exists() {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &plist_path.display().to_string()])
                .output();
            std::fs::remove_file(&plist_path)
                .context("failed to remove plist")?;
            println!("  {} Service unregistered.", green(CHECK));
        } else {
            println!("  {} Service not registered.", dim("·"));
        }
    }

    #[cfg(target_os = "linux")]
    {
        let unit_path = service_unit_path();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", "shunt"])
            .output();
        if unit_path.exists() {
            std::fs::remove_file(&unit_path)
                .context("failed to remove unit file")?;
        }
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        println!("  {} Service unregistered.", green(CHECK));
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("Service management is only supported on macOS and Linux.");

    println!();
    Ok(())
}

async fn cmd_service_status() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = service_plist_path();
        let registered = plist_path.exists();
        if registered {
            println!("  {} Registered  {}", green(CHECK), dim(&plist_path.display().to_string()));
        } else {
            println!("  {} Not registered (run {})", dim("·"), cyan("shunt service install"));
        }

        // Check if launchd considers it running
        let out = std::process::Command::new("launchctl")
            .args(["list", "sh.shunt.proxy"])
            .output();
        let running = out.map(|o| o.status.success()).unwrap_or(false);
        if running {
            println!("  {} Running (launchd)", green(DOT));
        } else {
            println!("  {} Not running", dim(DOT));
        }
    }

    #[cfg(target_os = "linux")]
    {
        let unit_path = service_unit_path();
        let registered = unit_path.exists();
        if registered {
            println!("  {} Registered  {}", green(CHECK), dim(&unit_path.display().to_string()));
        } else {
            println!("  {} Not registered (run {})", dim("·"), cyan("shunt service install"));
        }

        let out = std::process::Command::new("systemctl")
            .args(["--user", "is-active", "shunt"])
            .output();
        let active = out.map(|o| o.status.success()).unwrap_or(false);
        if active {
            println!("  {} Running (systemd)", green(DOT));
        } else {
            println!("  {} Not running", dim(DOT));
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    println!("  {} Service management is only supported on macOS and Linux.", dim("·"));

    println!();
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
