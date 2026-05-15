use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::oauth::OAuthCredential;
use crate::provider::Provider;

pub const APP_NAME: &str = "shunt";

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
        .join("config.toml")
}

pub fn credentials_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
        .join("credentials.json")
}

pub fn state_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
        .join("state.json")
}

pub fn log_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
        .join("proxy.log")
}

pub fn pid_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
        .join("shunt.pid")
}

// ---------------------------------------------------------------------------
// Credentials store  (separate file from config — never commit this)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CredentialsStore {
    pub accounts: HashMap<String, OAuthCredential>,
}

impl CredentialsStore {
    pub fn load() -> Self {
        let p = credentials_path();
        if !p.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&p) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let p = credentials_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &p)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
        }
        // On Windows, restrict the file to the current user via icacls (best-effort).
        #[cfg(windows)]
        {
            if let Some(path_str) = p.to_str() {
                let username = std::env::var("USERNAME").unwrap_or_default();
                if !username.is_empty() {
                    let _ = std::process::Command::new("icacls")
                        .arg(path_str)
                        .arg("/inheritance:r")
                        .arg("/grant:r")
                        .arg(format!("{username}:F"))
                        .status();
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Raw TOML config types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    accounts: Vec<RawAccount>,
}

#[derive(Debug, Deserialize)]
struct RawServer {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_log_level")]
    log_level: String,
    upstream_url: Option<String>,
    remote_key: Option<String>,
    relay_url: Option<String>,
    /// Conversation stickiness TTL in minutes (default: 10)
    sticky_ttl_minutes: Option<u64>,
    /// "use-it-or-lose-it" expiry window in minutes (default: 30)
    expiry_soon_minutes: Option<u64>,
    /// Upstream request timeout in seconds (default: 600)
    request_timeout_secs: Option<u64>,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            log_level: default_log_level(),
            upstream_url: None,
            remote_key: None,
            relay_url: None,
            sticky_ttl_minutes: None,
            expiry_soon_minutes: None,
            request_timeout_secs: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawAccount {
    name: String,
    #[serde(default = "default_plan_type")]
    plan_type: String,
    /// "anthropic" (default) | "openai" / "codex"
    #[serde(default)]
    provider: Option<String>,
}

fn default_host() -> String { "127.0.0.1".into() }
fn default_port() -> u16 { 8082 }
fn default_log_level() -> String { "info".into() }
fn default_plan_type() -> String { "pro".into() }

// ---------------------------------------------------------------------------
// Resolved config types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub log_level: String,
    pub upstream_url: String,
    /// When set, remote requests must supply this value as `x-api-key`.
    pub remote_key: Option<String>,
    /// Relay URL for `shunt push` / `shunt login`. Overridable via SHUNT_RELAY_URL.
    pub relay_url: String,
    /// Conversation stickiness TTL in milliseconds.
    pub sticky_ttl_ms: u64,
    /// Accounts whose 5h window resets within this many seconds are preferred ("use-it-or-lose-it").
    pub expiry_soon_secs: u64,
    /// Upstream request timeout in seconds.
    pub request_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8082,
            log_level: "info".into(),
            upstream_url: "https://api.anthropic.com".into(),
            remote_key: None,
            relay_url: "https://relay.ramcharan.shop".into(),
            sticky_ttl_ms: 10 * 60 * 1000,
            expiry_soon_secs: 30 * 60,
            request_timeout_secs: 600,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub name: String,
    pub plan_type: String,
    pub provider: Provider,
    /// `None` when the account is in config but has no credential yet.
    /// These accounts are shown in status but skipped during proxying.
    pub credential: Option<OAuthCredential>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub accounts: Vec<AccountConfig>,
    pub config_file: PathBuf,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

pub fn load_config(path: Option<&Path>) -> Result<Config> {
    let p = path.map(PathBuf::from).unwrap_or_else(config_path);

    if !p.exists() {
        bail!(
            "Config not found: {}\nRun `shunt setup` to get started.",
            p.display()
        );
    }

    let raw_text = std::fs::read_to_string(&p)
        .with_context(|| format!("Failed to read config: {}", p.display()))?;

    let raw: RawConfig = toml::from_str(&raw_text)
        .with_context(|| format!("Failed to parse config: {}", p.display()))?;

    // Derive the default upstream URL from the first account's provider so that
    // an all-OpenAI config automatically points at api.openai.com without any
    // explicit `upstream_url` in the config file.
    let default_upstream = raw.accounts.first()
        .map(|a| a.provider.as_deref().map(Provider::from_str).unwrap_or_default())
        .unwrap_or_default()
        .default_upstream_url()
        .to_owned();

    let upstream_url = raw
        .server
        .upstream_url
        .clone()
        .or_else(|| std::env::var("SHUNT_UPSTREAM_URL").ok())
        .unwrap_or(default_upstream);

    let relay_url = raw
        .server
        .relay_url
        .clone()
        .or_else(|| std::env::var("SHUNT_RELAY_URL").ok())
        .unwrap_or_else(|| "https://relay.ramcharan.shop".into());

    let server = ServerConfig {
        host: raw.server.host,
        port: raw.server.port,
        log_level: raw.server.log_level,
        upstream_url,
        remote_key: raw.server.remote_key,
        relay_url,
        sticky_ttl_ms: raw.server.sticky_ttl_minutes.unwrap_or(10) * 60 * 1000,
        expiry_soon_secs: raw.server.expiry_soon_minutes.unwrap_or(30) * 60,
        request_timeout_secs: raw.server.request_timeout_secs.unwrap_or(600),
    };

    if raw.accounts.is_empty() {
        bail!("Config has no accounts. Run `shunt setup` to add one.");
    }

    let store = CredentialsStore::load();

    let mut accounts = Vec::new();
    for a in &raw.accounts {
        let provider = a.provider.as_deref().map(Provider::from_str).unwrap_or_default();

        // Resolve credential: stored credential first, then auto-import from provider's local CLI.
        let cred = store
            .accounts
            .get(&a.name)
            .cloned()
            .or_else(|| provider.read_local_credentials());

        accounts.push(AccountConfig {
            name: a.name.clone(),
            plan_type: a.plan_type.clone(),
            provider,
            credential: cred,
        });
    }

    Ok(Config { server, accounts, config_file: p })
}

// ---------------------------------------------------------------------------
// Config file template
// ---------------------------------------------------------------------------

pub fn config_template(accounts: &[(&str, &str)]) -> String {
    let mut out = String::from(
        "[server]\nhost = \"127.0.0.1\"\nport = 8082\nlog_level = \"info\"\n",
    );
    for (name, plan_type) in accounts {
        out.push_str(&format!(
            "\n[[accounts]]\nname = \"{name}\"\nplan_type = \"{plan_type}\"\n"
        ));
    }
    out
}
