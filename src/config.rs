use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::oauth::OAuthCredential;

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
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            log_level: default_log_level(),
            upstream_url: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawAccount {
    name: String,
    #[serde(default = "default_plan_type")]
    plan_type: String,
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
}

#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub name: String,
    pub plan_type: String,
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

    let upstream_url = raw
        .server
        .upstream_url
        .clone()
        .or_else(|| std::env::var("SHUNT_UPSTREAM_URL").ok())
        .unwrap_or_else(|| "https://api.anthropic.com".into());

    let server = ServerConfig {
        host: raw.server.host,
        port: raw.server.port,
        log_level: raw.server.log_level,
        upstream_url,
    };

    if raw.accounts.is_empty() {
        bail!("Config has no accounts. Run `shunt setup` to add one.");
    }

    let store = CredentialsStore::load();

    let mut accounts = Vec::new();
    for a in &raw.accounts {
        // Resolve credential: env var → credentials store → auto-import primary
        let cred = if a.name == "main" || store.accounts.is_empty() {
            // Try the canonical Claude Code credentials as fallback for the primary account
            store
                .accounts
                .get(&a.name)
                .cloned()
                .or_else(crate::oauth::read_claude_credentials)
        } else {
            store.accounts.get(&a.name).cloned()
        };

        accounts.push(AccountConfig {
            name: a.name.clone(),
            plan_type: a.plan_type.clone(),
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
