use anyhow::{bail, Context, Result};

/// Validate that an upstream URL uses http/https and does not point to
/// loopback or link-local addresses (SSRF guard).
/// Pass `allow_loopback = true` for Local-provider accounts (e.g. Ollama).
fn validate_upstream_url(url: &str, allow_loopback: bool) -> Result<()> {
    let parsed = url::Url::parse(url)
        .with_context(|| format!("Invalid upstream URL: {url}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => bail!("Upstream URL must use http or https, got scheme '{s}': {url}"),
    }
    if !allow_loopback {
        if let Some(host) = parsed.host_str() {
            let blocked = matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
                || host.starts_with("169.254.")
                || host.starts_with("fd");
            if blocked {
                bail!("Upstream URL must not point to loopback or link-local addresses: {url}");
            }
        }
    }
    Ok(())
}
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::credential::{deserialize_credential_map, Credential};
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

pub fn notify_log_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
        .join("notify.log")
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
    #[serde(deserialize_with = "deserialize_credential_map", default)]
    pub accounts: HashMap<String, Credential>,
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &p)?;
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
    /// Global model-name mapping: `"claude-sonnet-4-6" = "llama-3.3-70b-versatile"`
    /// Applied when routing Anthropic-format requests to non-Anthropic providers.
    #[serde(default)]
    model_mapping: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RawServer {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_control_port")]
    control_port: u16,
    #[serde(default = "default_log_level")]
    log_level: String,
    upstream_url: Option<String>,
    remote_key: Option<String>,
    relay_url: Option<String>,
    pub custom_domain: Option<String>,
    /// Conversation stickiness TTL in minutes (default: 10)
    sticky_ttl_minutes: Option<u64>,
    /// "use-it-or-lose-it" expiry window in minutes (default: 30)
    expiry_soon_minutes: Option<u64>,
    /// Account selection strategy: "earliest-expiry" (default), "round-robin", "least-utilized"
    routing_strategy: Option<String>,
    /// Upstream request timeout in seconds (default: 600)
    request_timeout_secs: Option<u64>,
    /// Per-IP rate limit in requests per minute (0 = disabled, default disabled).
    rate_limit_rpm: Option<u32>,
    /// Trust X-Real-IP / X-Forwarded-For headers for per-IP rate limiting.
    /// Set to true only when shunt sits behind a trusted reverse proxy (e.g. cloudflared).
    /// When false (default), all requests share one rate-limit bucket.
    trust_proxy_headers: Option<bool>,
    /// Enable periodic health-check probes for all accounts (default: true).
    health_check_enabled: Option<bool>,
    /// Seconds between health-check probe rounds (default: 300 = 5 min).
    health_check_interval_secs: Option<u64>,
    /// Per-account probe timeout in seconds (default: 10).
    health_check_timeout_secs: Option<u64>,
    /// URL of a shunt relay-server instance for multi-machine history aggregation.
    /// e.g. "http://relay.internal:3001"
    telemetry_url: Option<String>,
    /// Bearer token sent to the relay-server. Must match RELAY_TOKEN on the server.
    telemetry_token: Option<String>,
    /// Human-readable name for this shunt instance (shown in the relay dashboard).
    /// Defaults to the system hostname.
    instance_name: Option<String>,
    /// Per-account burst rate limit in requests per minute (0 = disabled, default disabled).
    /// When set, accounts approaching this limit are deprioritized in routing.
    burst_rpm_limit: Option<u32>,
    /// Fallback model to use when all accounts are on cooldown.
    /// If set, requests are retried with this model before waiting.
    fallback_model: Option<String>,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            control_port: default_control_port(),
            log_level: default_log_level(),
            upstream_url: None,
            remote_key: None,
            relay_url: None,
            custom_domain: None,
            sticky_ttl_minutes: None,
            expiry_soon_minutes: None,
            routing_strategy: None,
            request_timeout_secs: None,
            rate_limit_rpm: None,
            trust_proxy_headers: None,
            health_check_enabled: None,
            health_check_interval_secs: None,
            health_check_timeout_secs: None,
            telemetry_url: None,
            telemetry_token: None,
            instance_name: None,
            burst_rpm_limit: None,
            fallback_model: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawAccount {
    name: String,
    #[serde(default = "default_plan_type")]
    plan_type: String,
    /// "anthropic" (default) | "openai" / "codex" | "groq" | "mistral" | "local" | …
    #[serde(default)]
    provider: Option<String>,
    /// Inline API key (use api_key_env for better security).
    #[serde(default)]
    api_key: Option<String>,
    /// Name of an environment variable that holds the API key.
    #[serde(default)]
    api_key_env: Option<String>,
    /// Per-account upstream URL override (required for Local provider).
    #[serde(default)]
    upstream_url: Option<String>,
    /// Pin this account to a specific model, overriding global model_mapping
    /// and the provider's default_model(). Useful for mixing model tiers.
    #[serde(default)]
    model: Option<String>,
}

fn default_host() -> String { "127.0.0.1".into() }

pub fn default_instance_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "shunt".into())
}
fn default_port() -> u16 { 8082 }
fn default_control_port() -> u16 { 19081 }
fn default_log_level() -> String { "info".into() }
fn default_plan_type() -> String { "pro".into() }

// ---------------------------------------------------------------------------
// Resolved config types
// ---------------------------------------------------------------------------

/// Account-selection algorithm used when no sticky or pinned account applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoutingStrategy {
    /// Harvest every token before the window expires — use-it-or-lose-it.
    /// Drains accounts whose quota windows expire soonest first, then prefers
    /// the account with the most remaining quota. Maximises total token usage over time.
    /// Config: `"reaper"`
    Reaper,
    /// Spins through accounts in a fixed round-robin cycle, ignoring quota state.
    /// Config: `"carousel"`
    Carousel,
    /// Always routes to the account with the softest landing — the most remaining
    /// capacity across both 5h and 7d windows (binding window primary, secondary as tiebreak).
    /// Config: `"cushion"`
    Cushion,
    /// Time-weighted dual-window optimizer. Scores each account as:
    ///   health_5h = 1 - (time_fraction_5h × util_5h)
    ///   health_7d = 1 - (time_fraction_7d × util_7d)
    ///   score     = health_5h × health_7d
    /// where time_fraction = secs_to_reset / window_duration (0 = resetting now, 1 = just started).
    /// Accounts for how much quota remains AND how soon each window refreshes.
    /// Config: `"maximus"`
    #[default]
    Maximus,
}

impl RoutingStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reaper  => "reaper",
            Self::Carousel => "carousel",
            Self::Cushion  => "cushion",
            Self::Maximus  => "maximus",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "reaper" | "earliest-expiry" | "earliest_expiry" => Some(Self::Reaper),
            "carousel" | "round-robin" | "round_robin" => Some(Self::Carousel),
            "cushion" | "most-available" | "most_available" => Some(Self::Cushion),
            "maximus" => Some(Self::Maximus),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Port for the control plane (/status, /use, /health) — sees all accounts.
    pub control_port: u16,
    pub log_level: String,
    pub upstream_url: String,
    /// When set, remote requests must supply this value as `x-api-key`.
    pub remote_key: Option<String>,
    /// Relay URL for `shunt push` / `shunt login`. Overridable via SHUNT_RELAY_URL.
    pub relay_url: String,
    /// Custom domain for permanent online sharing (e.g. https://shunt.mysite.com).
    pub custom_domain: Option<String>,
    /// Conversation stickiness TTL in milliseconds.
    pub sticky_ttl_ms: u64,
    /// Accounts whose 5h window resets within this many seconds are preferred ("use-it-or-lose-it").
    pub expiry_soon_secs: u64,
    /// Which routing algorithm to use for account selection.
    pub routing_strategy: RoutingStrategy,
    /// Upstream request timeout in seconds.
    pub request_timeout_secs: u64,
    /// Per-IP rate limit in requests per minute (0 = disabled, default disabled).
    pub rate_limit_rpm: u32,
    /// Trust X-Real-IP for per-IP rate limiting (only when behind a trusted proxy).
    pub trust_proxy_headers: bool,
    /// Enable periodic health-check probes for all accounts.
    pub health_check_enabled: bool,
    /// Seconds between health-check probe rounds.
    pub health_check_interval_secs: u64,
    /// Per-account probe timeout in seconds.
    pub health_check_timeout_secs: u64,
    /// Optional relay-server URL for cross-instance history aggregation.
    pub telemetry_url: Option<String>,
    /// Bearer token for the relay-server.
    pub telemetry_token: Option<String>,
    /// Identifier for this shunt instance sent in telemetry payloads.
    pub instance_name: String,
    /// Per-account burst rate limit in requests per minute (0 = disabled).
    pub burst_rpm_limit: u32,
    /// Fallback model when all accounts are on cooldown.
    pub fallback_model: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8082,
            control_port: 19081,
            log_level: "info".into(),
            upstream_url: "https://api.anthropic.com".into(),
            remote_key: None,
            relay_url: "https://relay.ramcharan.shop".into(),
            custom_domain: None,
            sticky_ttl_ms: 10 * 60 * 1000,
            expiry_soon_secs: 30 * 60,
            routing_strategy: RoutingStrategy::Maximus,
            request_timeout_secs: 600,
            rate_limit_rpm: 0,
            trust_proxy_headers: false,
            health_check_enabled: true,
            health_check_interval_secs: 300,
            health_check_timeout_secs: 10,
            telemetry_url: None,
            telemetry_token: None,
            instance_name: default_instance_name(),
            burst_rpm_limit: 10,
            fallback_model: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub name: String,
    pub plan_type: String,
    pub provider: Provider,
    /// `None` when the account has no credential.
    /// OAuth accounts: None means reauth required (shown as auth_failed).
    /// ApiKey accounts: None means key not yet configured.
    /// Local accounts: None is normal (no auth required).
    pub credential: Option<Credential>,
    /// Override the upstream base URL for this account.
    /// `None` means use `config.server.upstream_url` (primary provider) or
    /// `provider.default_upstream_url()` (non-primary provider).
    pub upstream_url: Option<String>,
    /// Pin this account to a specific model name.
    /// Overrides both `model_mapping` and `provider.default_model()`.
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub accounts: Vec<AccountConfig>,
    pub config_file: PathBuf,
    /// Global model-name overrides: claude model → provider model.
    /// e.g. `"claude-sonnet-4-6" → "llama-3.3-70b-versatile"`
    pub model_mapping: HashMap<String, String>,
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
    let primary_provider_derived = raw.accounts.first()
        .map(|a| a.provider.as_deref().map(Provider::from_str).unwrap_or_default())
        .unwrap_or_default();
    let default_upstream = primary_provider_derived.default_upstream_url().to_owned();

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

    let telemetry_url = raw.server.telemetry_url.clone()
        .or_else(|| std::env::var("SHUNT_TELEMETRY_URL").ok());
    let telemetry_token = raw.server.telemetry_token.clone()
        .or_else(|| std::env::var("SHUNT_TELEMETRY_TOKEN").ok());
    let instance_name = raw.server.instance_name.clone()
        .or_else(|| std::env::var("SHUNT_INSTANCE_NAME").ok())
        .unwrap_or_else(default_instance_name);

    // #6 SSRF: validate the server-level upstream URL.
    // Allow loopback only when the URL was derived from a Local provider's default
    // (e.g. an all-Ollama config); explicit upstream_url entries are never allowed to
    // use loopback unless explicitly set via SHUNT_UPSTREAM_URL (trust the operator).
    let server_url_is_local_derived = raw.server.upstream_url.is_none()
        && std::env::var("SHUNT_UPSTREAM_URL").is_err()
        && matches!(primary_provider_derived, Provider::Local);
    validate_upstream_url(&upstream_url, server_url_is_local_derived)
        .with_context(|| "server.upstream_url failed validation")?;

    let server = ServerConfig {
        host: raw.server.host,
        port: raw.server.port,
        control_port: raw.server.control_port,
        log_level: raw.server.log_level,
        upstream_url,
        remote_key: raw.server.remote_key,
        relay_url,
        custom_domain: raw.server.custom_domain,
        sticky_ttl_ms: raw.server.sticky_ttl_minutes.unwrap_or(10) * 60 * 1000,
        expiry_soon_secs: raw.server.expiry_soon_minutes.unwrap_or(30) * 60,
        routing_strategy: raw.server.routing_strategy.as_deref()
            .and_then(RoutingStrategy::from_str)
            .unwrap_or_default(),
        request_timeout_secs: raw.server.request_timeout_secs.unwrap_or(600),
        rate_limit_rpm: raw.server.rate_limit_rpm.unwrap_or(0),
        trust_proxy_headers: raw.server.trust_proxy_headers.unwrap_or(false),
        health_check_enabled: raw.server.health_check_enabled.unwrap_or(true),
        health_check_interval_secs: raw.server.health_check_interval_secs.unwrap_or(300),
        health_check_timeout_secs: raw.server.health_check_timeout_secs.unwrap_or(10),
        telemetry_url,
        telemetry_token,
        instance_name,
        burst_rpm_limit: raw.server.burst_rpm_limit.unwrap_or(10),
        fallback_model: raw.server.fallback_model,
    };

    if raw.accounts.is_empty() {
        bail!("Config has no accounts. Run `shunt setup` to add one.");
    }

    let store = CredentialsStore::load();

    // primary_provider_derived was already computed above for the server URL derivation.
    let primary_provider = primary_provider_derived;

    let mut accounts = Vec::new();
    for a in &raw.accounts {
        let provider = a.provider.as_deref().map(Provider::from_str).unwrap_or_default();

        // Resolve credential.
        //
        // OAuth providers (Anthropic, OpenAI): credentials.json first, then
        // auto-import from the provider's local CLI tool.
        //
        // API-key providers: credentials.json first, then inline api_key field,
        // then api_key_env field, then the provider's well-known env var.
        let cred: Option<Credential> = store.accounts.get(&a.name).cloned()
            .or_else(|| {
                // Inline api_key from TOML (less secure, but convenient for testing).
                a.api_key.as_deref().map(|k| {
                    tracing::warn!(account = %a.name, "Inline api_key in config.toml is insecure — use api_key_env instead");
                    Credential::Apikey { key: k.to_owned() }
                })
            })
            .or_else(|| {
                // api_key_env: name of env var holding the key.
                a.api_key_env.as_deref()
                    .and_then(|var| std::env::var(var).ok())
                    .map(|k| Credential::Apikey { key: k })
            })
            .or_else(|| {
                // Auto-import from provider's CLI tool (OAuth providers) or
                // well-known env var (API-key providers).
                provider.read_local_credentials()
            });

        // Upstream URL: per-account override from TOML takes priority, then
        // non-primary-provider accounts get the provider's default URL so
        // the forwarder knows where to send requests.
        let is_local = matches!(provider, Provider::Local);
        if let Some(ref url) = a.upstream_url {
            // #6 SSRF: allow loopback only for Local provider (e.g. Ollama at localhost).
            validate_upstream_url(url, is_local)
                .with_context(|| format!("account '{}' upstream_url failed validation", a.name))?;
        }
        let acct_upstream = a.upstream_url.clone().or_else(|| {
            if provider != primary_provider {
                Some(provider.default_upstream_url().to_owned())
            } else {
                None
            }
        });

        accounts.push(AccountConfig {
            name: a.name.clone(),
            plan_type: a.plan_type.clone(),
            provider,
            credential: cred,
            upstream_url: acct_upstream,
            model: a.model.clone(),
        });
    }

    Ok(Config { server, accounts, config_file: p, model_mapping: raw.model_mapping })
}

// ---------------------------------------------------------------------------
// Config file template
// ---------------------------------------------------------------------------

pub fn config_template(accounts: &[(&str, &str)]) -> String {
    let mut out = String::from(
        "[server]\nhost = \"127.0.0.1\"\nport = 8082\ncontrol_port = 19081\nlog_level = \"info\"\n",
    );
    for (name, plan_type) in accounts {
        out.push_str(&format!(
            "\n[[accounts]]\nname = \"{name}\"\nplan_type = \"{plan_type}\"\n"
        ));
    }
    out
}
