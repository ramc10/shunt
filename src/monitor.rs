/// Live fullscreen TUI monitor for shunt.
///
/// Connects to the running proxy's /status endpoint and refreshes every second.
/// Press 'q' or Esc to exit, 's' for settings, '?' for help.
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table},
    Frame, Terminal,
};
use serde::Deserialize;
use std::{
    io::stdout,
    time::{Duration, Instant},
};

use crate::pricing::fmt_cost;
use crate::term::fmt_duration_ms;

// ---------------------------------------------------------------------------
// Status API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct StatusResponse {
    #[serde(default)]
    started_ms: Option<u64>,
    #[serde(default)]
    accounts: Vec<AccountStatus>,
    #[serde(default)]
    pinned_account: Option<String>,
    #[serde(default)]
    last_used_account: Option<String>,
    #[serde(default)]
    recent_requests: Vec<ReqLog>,
    #[serde(default)]
    #[allow(dead_code)]
    savings: Option<SavingsInfo>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[allow(dead_code)]
struct SavingsInfo {
    #[serde(default)] today_input: u64,
    #[serde(default)] today_output: u64,
    #[serde(default)] today_cost_usd: f64,
    #[serde(default)] week_cost_usd: f64,
    #[serde(default)] all_time_cost_usd: f64,
}

#[derive(Debug, Deserialize)]
struct AccountStatus {
    name: String,
    #[serde(default)] email: Option<String>,
    #[serde(default)] provider: String,
    available: bool,
    #[serde(default)] disabled: bool,
    #[serde(default)] auth_failed: bool,
    #[serde(default)] health_check_failed: bool,
    #[serde(default)] utilization_5h: f64,
    #[serde(default)] reset_5h: Option<u64>,
    #[serde(default)] status_5h: Option<String>,
    #[serde(default)] utilization_7d: f64,
    #[serde(default)] reset_7d: Option<u64>,
    #[serde(default)] status_7d: Option<String>,
    #[serde(default)] cooldown_until_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
struct ReqLog {
    ts_ms: u64,
    account: String,
    model: String,
    #[allow(dead_code)]
    status: u16,
    #[allow(dead_code)]
    input_tokens: u64,
    #[allow(dead_code)]
    output_tokens: u64,
    duration_ms: u64,
}

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

const GREEN:    Color = Color::Indexed(154);
const DK_GREEN: Color = Color::Indexed(28);
const BRAND:    Color = Color::Indexed(154);
const DIM:      Color = Color::Indexed(240);
const YELLOW:   Color = Color::Indexed(220);
const RED:      Color = Color::Indexed(196);
const WHITE:    Color = Color::Indexed(253);
const CYAN:     Color = Color::Indexed(154);

const ACCOUNT_COLORS: &[Color] = &[
    Color::Indexed(154), // lime green (brand)
    Color::Indexed(220), // bright yellow
    Color::Indexed(39),  // dodger blue
    Color::Indexed(213), // hot pink
    Color::Indexed(51),  // aqua
    Color::Indexed(208), // orange
    Color::Indexed(141), // medium purple
    Color::Indexed(85),  // sea green
];

fn style_brand()   -> Style { Style::default().fg(BRAND).add_modifier(Modifier::BOLD) }
fn style_green()   -> Style { Style::default().fg(GREEN) }
fn style_dkgreen() -> Style { Style::default().fg(DK_GREEN) }
fn style_dim()     -> Style { Style::default().fg(DIM) }
fn style_yellow()  -> Style { Style::default().fg(YELLOW) }
fn style_red()     -> Style { Style::default().fg(RED) }
fn style_white()   -> Style { Style::default().fg(WHITE) }
fn style_cyan()    -> Style { Style::default().fg(CYAN) }
#[allow(dead_code)]
fn style_bold()    -> Style { Style::default().add_modifier(Modifier::BOLD) }

// ---------------------------------------------------------------------------
// Focus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Focus {
    Accounts,
    Requests,
    History,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Accounts => Self::Requests,
            Self::Requests => Self::History,
            Self::History  => Self::Accounts,
        }
    }
    fn prev(self) -> Self {
        match self {
            Self::Accounts => Self::History,
            Self::Requests => Self::Accounts,
            Self::History  => Self::Requests,
        }
    }
}

// ---------------------------------------------------------------------------
// Time window
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum TimeWindow {
    FifteenMin,
    OneHour,
    SixHour,
    TwentyFourHour,
    ThreeDay,
    SevenDay,
}

impl TimeWindow {
    fn ms(self) -> u64 {
        match self {
            Self::FifteenMin    => 15 * 60_000,
            Self::OneHour       => 60 * 60_000,
            Self::SixHour       => 6  * 60 * 60_000,
            Self::TwentyFourHour=> 24 * 60 * 60_000,
            Self::ThreeDay      => 3  * 24 * 60 * 60_000,
            Self::SevenDay      => 7  * 24 * 60 * 60_000,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::FifteenMin     => "15m",
            Self::OneHour        => "1h",
            Self::SixHour        => "6h",
            Self::TwentyFourHour => "24h",
            Self::ThreeDay       => "3d",
            Self::SevenDay       => "7d",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::FifteenMin     => Self::OneHour,
            Self::OneHour        => Self::SixHour,
            Self::SixHour        => Self::TwentyFourHour,
            Self::TwentyFourHour => Self::ThreeDay,
            Self::ThreeDay       => Self::SevenDay,
            Self::SevenDay       => Self::FifteenMin,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::FifteenMin     => Self::SevenDay,
            Self::OneHour        => Self::FifteenMin,
            Self::SixHour        => Self::OneHour,
            Self::TwentyFourHour => Self::SixHour,
            Self::ThreeDay       => Self::TwentyFourHour,
            Self::SevenDay       => Self::ThreeDay,
        }
    }

    fn bucket_count(self) -> usize {
        match self {
            Self::FifteenMin     => 15,  // 1 min each
            Self::OneHour        => 12,  // 5 min each
            Self::SixHour        => 12,  // 30 min each
            Self::TwentyFourHour => 24,  // 1 h each
            Self::ThreeDay       => 18,  // 4 h each
            Self::SevenDay       => 14,  // 12 h each
        }
    }

    fn bucket_ms(self) -> u64 {
        self.ms() / self.bucket_count() as u64
    }
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum FetchError {
    NotRunning,
    Other(String),
}

// ---------------------------------------------------------------------------
// Picker overlay
// ---------------------------------------------------------------------------

struct Picker {
    items: Vec<String>,
    cursor: usize,
}

impl Picker {
    fn new(accounts: &[AccountStatus], pinned: Option<&str>) -> Self {
        let mut items: Vec<String> = accounts.iter().map(|a| a.name.clone()).collect();
        items.push("auto".to_owned());
        let cursor = pinned
            .and_then(|p| items.iter().position(|i| i == p))
            .unwrap_or(items.len() - 1);
        Self { items, cursor }
    }
    fn up(&mut self)   { self.cursor = if self.cursor == 0 { self.items.len() - 1 } else { self.cursor - 1 }; }
    fn down(&mut self) { self.cursor = (self.cursor + 1) % self.items.len(); }
    fn selected(&self) -> &str { &self.items[self.cursor] }
}

/// (display name, description, model id or "" for auto/clear)
const MODEL_PRESETS: &[(&str, &str, &str)] = &[
    ("Auto",     "Let the client choose the model",         ""),
    ("Opus 4",   "Most capable · best for complex tasks",  "claude-opus-4-6"),
    ("Sonnet 4", "Balanced · fast and smart",               "claude-sonnet-4-6"),
    ("Haiku 4",  "Fastest · great for simple tasks",        "claude-haiku-4-5-20251001"),
];

struct ModelPicker {
    cursor: usize,
}

impl ModelPicker {
    fn new(current: Option<&str>) -> Self {
        let cursor = current
            .and_then(|m| MODEL_PRESETS.iter().position(|(_, _, id)| *id == m))
            .unwrap_or(0); // default to "Auto"
        Self { cursor }
    }
    fn up(&mut self)   { self.cursor = if self.cursor == 0 { MODEL_PRESETS.len() - 1 } else { self.cursor - 1 }; }
    fn down(&mut self) { self.cursor = (self.cursor + 1) % MODEL_PRESETS.len(); }
    fn selected_id(&self) -> &str { MODEL_PRESETS[self.cursor].2 }
}

/// (display name, description, strategy id, "" = auto/clear)
const STRATEGY_PRESETS: &[(&str, &str, &str)] = &[
    ("Maximus",  "Time-weighted dual-window scorer",       "maximus"),
    ("Reaper",   "Use-it-or-lose-it · drain expiring first", "reaper"),
    ("Carousel", "Fixed round-robin cycle",                "carousel"),
    ("Cushion",  "Lowest utilization · softest landing",   "cushion"),
];

struct StrategyPicker {
    cursor: usize,
}

impl StrategyPicker {
    fn new(current: Option<&str>) -> Self {
        let cursor = current
            .and_then(|s| STRATEGY_PRESETS.iter().position(|(_, _, id)| *id == s))
            .unwrap_or(0); // default to Maximus
        Self { cursor }
    }
    fn up(&mut self)   { self.cursor = if self.cursor == 0 { STRATEGY_PRESETS.len() - 1 } else { self.cursor - 1 }; }
    fn down(&mut self) { self.cursor = (self.cursor + 1) % STRATEGY_PRESETS.len(); }
    fn selected_id(&self) -> &str { STRATEGY_PRESETS[self.cursor].2 }
}

// ---------------------------------------------------------------------------
// Unified settings menu
// ---------------------------------------------------------------------------

const MENU_ITEMS: usize = 9;

struct Menu {
    cursor: usize,
}

impl Menu {
    fn new() -> Self { Self { cursor: 0 } }
    fn up(&mut self)   { self.cursor = self.cursor.checked_sub(1).unwrap_or(MENU_ITEMS - 1); }
    fn down(&mut self) { self.cursor = (self.cursor + 1) % MENU_ITEMS; }
}

const SPEED_PRESETS: &[u64] = &[200, 500, 1_000, 2_000, 5_000, 10_000];

struct SpeedPicker {
    cursor: usize,
}

impl SpeedPicker {
    fn new(current_ms: u64) -> Self {
        let cursor = SPEED_PRESETS.iter().position(|&s| s == current_ms).unwrap_or(2);
        Self { cursor }
    }
    fn up(&mut self)   { self.cursor = self.cursor.checked_sub(1).unwrap_or(SPEED_PRESETS.len() - 1); }
    fn down(&mut self) { self.cursor = (self.cursor + 1) % SPEED_PRESETS.len(); }
    fn selected(&self) -> u64 { SPEED_PRESETS[self.cursor] }
}

const BURST_LIMIT_PRESETS: &[u32] = &[0, 5, 8, 10, 12, 15, 20];

struct BurstLimitPicker {
    cursor: usize,
}

impl BurstLimitPicker {
    fn new(current: u32) -> Self {
        let cursor = BURST_LIMIT_PRESETS.iter().position(|&v| v == current).unwrap_or(3); // default to 10
        Self { cursor }
    }
    fn up(&mut self)   { self.cursor = self.cursor.checked_sub(1).unwrap_or(BURST_LIMIT_PRESETS.len() - 1); }
    fn down(&mut self) { self.cursor = (self.cursor + 1) % BURST_LIMIT_PRESETS.len(); }
    fn selected(&self) -> u32 { BURST_LIMIT_PRESETS[self.cursor] }
}

const FALLBACK_PRESETS: &[(&str, &str)] = &[
    ("auto", "Auto-detect from model"),
    ("off", "Disabled"),
    ("claude-sonnet-4-6", "Sonnet 4.6"),
    ("claude-haiku-4-5-20251001", "Haiku 4.5"),
];

struct FallbackPicker {
    cursor: usize,
}

impl FallbackPicker {
    fn new(current: Option<&str>) -> Self {
        let cursor = match current {
            None => 0, // auto
            Some("off") => 1,
            Some(m) => FALLBACK_PRESETS.iter().position(|(id, _)| *id == m).unwrap_or(0),
        };
        Self { cursor }
    }
    fn up(&mut self)   { self.cursor = self.cursor.checked_sub(1).unwrap_or(FALLBACK_PRESETS.len() - 1); }
    fn down(&mut self) { self.cursor = (self.cursor + 1) % FALLBACK_PRESETS.len(); }
    fn selected_id(&self) -> &str { FALLBACK_PRESETS[self.cursor].0 }
}

const EFFORT_PRESETS: &[(&str, &str)] = &[
    ("auto", "Passthrough (don't modify)"),
    ("low", "Low — fast & cheap"),
    ("medium", "Medium — balanced"),
    ("high", "High — default quality"),
    ("xhigh", "xHigh — multi-agent / ultracode"),
    ("max", "Max — maximum reasoning"),
];

struct EffortPicker {
    cursor: usize,
}

impl EffortPicker {
    fn new(current: Option<&str>) -> Self {
        let cursor = match current {
            None => 0, // auto/passthrough
            Some(e) => EFFORT_PRESETS.iter().position(|(id, _)| *id == e).unwrap_or(0),
        };
        Self { cursor }
    }
    fn up(&mut self)   { self.cursor = self.cursor.checked_sub(1).unwrap_or(EFFORT_PRESETS.len() - 1); }
    fn down(&mut self) { self.cursor = (self.cursor + 1) % EFFORT_PRESETS.len(); }
    fn selected_id(&self) -> &str { EFFORT_PRESETS[self.cursor].0 }
}

const THINKING_PRESETS: &[(&str, &str)] = &[
    ("auto", "Passthrough (don't modify)"),
    ("adaptive", "Adaptive — model decides when to think"),
    ("disabled", "Off — disable extended thinking"),
];

struct ThinkingPicker {
    cursor: usize,
}

impl ThinkingPicker {
    fn new(current: Option<&str>) -> Self {
        let cursor = match current {
            None => 0,
            Some(m) => THINKING_PRESETS.iter().position(|(id, _)| *id == m).unwrap_or(0),
        };
        Self { cursor }
    }
    fn up(&mut self)   { self.cursor = self.cursor.checked_sub(1).unwrap_or(THINKING_PRESETS.len() - 1); }
    fn down(&mut self) { self.cursor = (self.cursor + 1) % THINKING_PRESETS.len(); }
    fn selected_id(&self) -> &str { THINKING_PRESETS[self.cursor].0 }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run_monitor(base_url: &str) -> Result<()> {
    let base = base_url.trim_end_matches('/');
    let status_url = format!("{base}/status");
    let use_url    = format!("{base}/use");
    let model_url  = format!("{base}/model");
    let strategy_url = format!("{base}/strategy");
    let alerts_url = format!("{base}/alerts");
    let burst_limit_url = format!("{base}/burst-limit");
    let fallback_url = format!("{base}/fallback");
    let effort_url = format!("{base}/effort");
    let thinking_url = format!("{base}/thinking");

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        original_hook(info);
    }));

    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, terminal::EnterAlternateScreen, crossterm::cursor::Hide)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let mut state: Option<StatusResponse> = None;
    let mut fetch_err: Option<FetchError> = None;
    let mut last_fetch = Instant::now() - Duration::from_secs(10);
    let mut accounts_scroll: usize = 0;
    let mut requests_scroll: usize = 0;
    let mut picker: Option<Picker> = None;
    let mut model_picker: Option<ModelPicker> = None;
    let mut model_override: Option<String> = None;
    let mut strategy_picker: Option<StrategyPicker> = None;
    let mut current_strategy: Option<String> = None;
    let mut strategy_source: Option<String> = None;
    let mut alerts_muted = false;
    let mut show_help = false;
    let mut refresh_ms: u64 = 1_000;
    let mut menu: Option<Menu> = None;
    let mut speed_picker: Option<SpeedPicker> = None;
    let mut burst_limit_picker: Option<BurstLimitPicker> = None;
    let mut fallback_picker: Option<FallbackPicker> = None;
    let mut effort_picker: Option<EffortPicker> = None;
    let mut thinking_picker: Option<ThinkingPicker> = None;
    let mut current_burst_limit: u32 = 10;
    let mut current_fallback: Option<String> = None; // None = auto
    let mut current_effort: Option<String> = None; // None = passthrough
    let mut current_thinking: Option<String> = None; // None = passthrough
    let mut focus = Focus::Accounts;
    let mut chart_window = TimeWindow::FifteenMin;
    let start_time = Instant::now();

    loop {
        if last_fetch.elapsed() >= Duration::from_millis(refresh_ms) {
            match fetch_status(&status_url).await {
                Ok(s)  => { state = Some(s); fetch_err = None; }
                Err(e) => { fetch_err = Some(e); state = None; }
            }
            // Fetch model override in parallel (ignore errors — proxy may not support it yet)
            if let Ok(r) = reqwest::Client::new()
                .get(&model_url)
                .timeout(Duration::from_secs(2))
                .send().await
            {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    model_override = v["model"].as_str().map(|s| s.to_owned());
                }
            }
            // Fetch current routing strategy
            if let Ok(r) = reqwest::Client::new()
                .get(&strategy_url)
                .timeout(Duration::from_secs(2))
                .send().await
            {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    current_strategy = v["strategy"].as_str().map(|s| s.to_owned());
                    strategy_source = v["source"].as_str().map(|s| s.to_owned());
                }
            }
            // Fetch alerts mute state
            if let Ok(r) = reqwest::Client::new()
                .get(&alerts_url)
                .timeout(Duration::from_secs(2))
                .send().await
            {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    alerts_muted = v["muted"].as_bool().unwrap_or(false);
                }
            }
            // Fetch burst limit
            if let Ok(r) = reqwest::Client::new()
                .get(&burst_limit_url)
                .timeout(Duration::from_secs(2))
                .send().await
            {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    current_burst_limit = v["burst_rpm_limit"].as_u64().unwrap_or(10) as u32;
                }
            }
            // Fetch fallback model
            if let Ok(r) = reqwest::Client::new()
                .get(&fallback_url)
                .timeout(Duration::from_secs(2))
                .send().await
            {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    let src = v["source"].as_str().unwrap_or("auto");
                    if src == "auto" || v["fallback_model"].is_null() {
                        current_fallback = None;
                    } else if v["auto_disabled"].as_bool().unwrap_or(false) {
                        current_fallback = Some("off".to_owned());
                    } else {
                        current_fallback = v["fallback_model"].as_str().map(|s| s.to_owned());
                    }
                }
            }
            // Fetch effort override
            if let Ok(r) = reqwest::Client::new()
                .get(&effort_url)
                .timeout(Duration::from_secs(2))
                .send().await
            {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    let src = v["source"].as_str().unwrap_or("passthrough");
                    if src == "override" {
                        current_effort = v["effort"].as_str().map(|s| s.to_owned());
                    } else {
                        current_effort = None;
                    }
                }
            }
            // Fetch thinking override
            if let Ok(r) = reqwest::Client::new()
                .get(&thinking_url)
                .timeout(Duration::from_secs(2))
                .send().await
            {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    let src = v["source"].as_str().unwrap_or("passthrough");
                    if src == "override" {
                        current_thinking = v["thinking"].as_str().map(|s| s.to_owned());
                    } else {
                        current_thinking = None;
                    }
                }
            }
            last_fetch = Instant::now();
        }

        terminal.draw(|f| {
            draw(f, &state, &fetch_err, accounts_scroll, requests_scroll,
                 base_url, &picker, &model_picker, &model_override,
                 &strategy_picker, &current_strategy, &strategy_source,
                 alerts_muted, show_help, refresh_ms, focus, chart_window, start_time,
                 &menu, &speed_picker, &burst_limit_picker, &fallback_picker,
                 &effort_picker, &thinking_picker, current_burst_limit,
                 &current_fallback, &current_effort, &current_thinking)
        })?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if show_help {
                    show_help = false;
                    continue;
                }

                // Speed picker (launched from menu)
                if let Some(ref mut sp) = speed_picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { speed_picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => sp.up(),
                        KeyCode::Down | KeyCode::Char('j') => sp.down(),
                        KeyCode::Enter => {
                            refresh_ms = sp.selected();
                            speed_picker = None;
                            menu = None;
                        }
                        _ => {}
                    }
                    continue;
                }

                // Burst limit picker (launched from menu)
                if let Some(ref mut bp) = burst_limit_picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { burst_limit_picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => bp.up(),
                        KeyCode::Down | KeyCode::Char('j') => bp.down(),
                        KeyCode::Enter => {
                            let chosen = bp.selected();
                            burst_limit_picker = None;
                            menu = None;
                            let client = reqwest::Client::new();
                            if chosen == 0 {
                                let _ = client.post(&burst_limit_url)
                                    .json(&serde_json::json!({ "burst_rpm_limit": 0 }))
                                    .timeout(Duration::from_secs(3))
                                    .send().await;
                            } else {
                                let _ = client.post(&burst_limit_url)
                                    .json(&serde_json::json!({ "burst_rpm_limit": chosen }))
                                    .timeout(Duration::from_secs(3))
                                    .send().await;
                            }
                            current_burst_limit = chosen;
                            last_fetch = Instant::now() - Duration::from_secs(10);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Fallback picker (launched from menu)
                if let Some(ref mut fp) = fallback_picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { fallback_picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => fp.up(),
                        KeyCode::Down | KeyCode::Char('j') => fp.down(),
                        KeyCode::Enter => {
                            let chosen = fp.selected_id().to_owned();
                            fallback_picker = None;
                            menu = None;
                            let client = reqwest::Client::new();
                            match chosen.as_str() {
                                "auto" => {
                                    let _ = client.delete(&fallback_url)
                                        .timeout(Duration::from_secs(3))
                                        .send().await;
                                    current_fallback = None;
                                }
                                "off" => {
                                    let _ = client.post(&fallback_url)
                                        .json(&serde_json::json!({ "fallback_model": null }))
                                        .timeout(Duration::from_secs(3))
                                        .send().await;
                                    current_fallback = Some("off".to_owned());
                                }
                                model => {
                                    let _ = client.post(&fallback_url)
                                        .json(&serde_json::json!({ "fallback_model": model }))
                                        .timeout(Duration::from_secs(3))
                                        .send().await;
                                    current_fallback = Some(model.to_owned());
                                }
                            }
                            last_fetch = Instant::now() - Duration::from_secs(10);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Effort picker (launched from menu)
                if let Some(ref mut ep) = effort_picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { effort_picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => ep.up(),
                        KeyCode::Down | KeyCode::Char('j') => ep.down(),
                        KeyCode::Enter => {
                            let chosen = ep.selected_id().to_owned();
                            effort_picker = None;
                            menu = None;
                            let client = reqwest::Client::new();
                            if chosen == "auto" {
                                let _ = client.delete(&effort_url)
                                    .timeout(Duration::from_secs(3))
                                    .send().await;
                                current_effort = None;
                            } else {
                                let _ = client.post(&effort_url)
                                    .json(&serde_json::json!({ "effort": chosen }))
                                    .timeout(Duration::from_secs(3))
                                    .send().await;
                                current_effort = Some(chosen);
                            }
                            last_fetch = Instant::now() - Duration::from_secs(10);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Thinking picker (launched from menu)
                if let Some(ref mut tp) = thinking_picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { thinking_picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => tp.up(),
                        KeyCode::Down | KeyCode::Char('j') => tp.down(),
                        KeyCode::Enter => {
                            let chosen = tp.selected_id().to_owned();
                            thinking_picker = None;
                            menu = None;
                            let client = reqwest::Client::new();
                            if chosen == "auto" {
                                let _ = client.delete(&thinking_url)
                                    .timeout(Duration::from_secs(3))
                                    .send().await;
                                current_thinking = None;
                            } else {
                                let _ = client.post(&thinking_url)
                                    .json(&serde_json::json!({ "thinking": chosen }))
                                    .timeout(Duration::from_secs(3))
                                    .send().await;
                                current_thinking = Some(chosen);
                            }
                            last_fetch = Instant::now() - Duration::from_secs(10);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Account picker overlay (launched from menu)
                if let Some(ref mut p) = picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => p.up(),
                        KeyCode::Down | KeyCode::Char('j') => p.down(),
                        KeyCode::Enter => {
                            let chosen = p.selected().to_owned();
                            picker = None;
                            menu = None;
                            let _ = reqwest::Client::new()
                                .post(&use_url)
                                .json(&serde_json::json!({ "account": chosen }))
                                .timeout(Duration::from_secs(3))
                                .send()
                                .await;
                            last_fetch = Instant::now() - Duration::from_secs(10);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Model picker overlay (launched from menu)
                if let Some(ref mut mp) = model_picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { model_picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => mp.up(),
                        KeyCode::Down | KeyCode::Char('j') => mp.down(),
                        KeyCode::Enter => {
                            let chosen_id = mp.selected_id().to_owned();
                            model_picker = None;
                            menu = None;
                            let client = reqwest::Client::new();
                            if chosen_id.is_empty() {
                                let _ = client.delete(&model_url)
                                    .timeout(Duration::from_secs(3))
                                    .send().await;
                                model_override = None;
                            } else {
                                let _ = client.post(&model_url)
                                    .json(&serde_json::json!({ "model": chosen_id }))
                                    .timeout(Duration::from_secs(3))
                                    .send().await;
                                model_override = Some(chosen_id);
                            }
                            last_fetch = Instant::now() - Duration::from_secs(10);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Strategy picker overlay (launched from menu)
                if let Some(ref mut sp) = strategy_picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { strategy_picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => sp.up(),
                        KeyCode::Down | KeyCode::Char('j') => sp.down(),
                        KeyCode::Enter => {
                            let chosen_id = sp.selected_id().to_owned();
                            strategy_picker = None;
                            menu = None;
                            let client = reqwest::Client::new();
                            let _ = client.post(&strategy_url)
                                .json(&serde_json::json!({ "strategy": chosen_id }))
                                .timeout(Duration::from_secs(3))
                                .send().await;
                            current_strategy = Some(chosen_id);
                            strategy_source = Some("override".to_owned());
                            last_fetch = Instant::now() - Duration::from_secs(10);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Settings menu
                if let Some(ref mut m) = menu {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { menu = None; }
                        KeyCode::Up   | KeyCode::Char('k') => m.up(),
                        KeyCode::Down | KeyCode::Char('j') => m.down(),
                        KeyCode::Enter => {
                            match m.cursor {
                                0 => { // pin account
                                    if let Some(ref s) = state {
                                        picker = Some(Picker::new(&s.accounts, s.pinned_account.as_deref()));
                                    }
                                }
                                1 => { // set model
                                    model_picker = Some(ModelPicker::new(model_override.as_deref()));
                                }
                                2 => { // strategy
                                    strategy_picker = Some(StrategyPicker::new(current_strategy.as_deref()));
                                }
                                3 => { // toggle mute
                                    let new_muted = !alerts_muted;
                                    let _ = reqwest::Client::new()
                                        .post(&alerts_url)
                                        .json(&serde_json::json!({ "muted": new_muted }))
                                        .timeout(Duration::from_secs(3))
                                        .send().await;
                                    alerts_muted = new_muted;
                                    menu = None;
                                }
                                4 => { // refresh speed
                                    speed_picker = Some(SpeedPicker::new(refresh_ms));
                                }
                                5 => { // burst limit
                                    burst_limit_picker = Some(BurstLimitPicker::new(current_burst_limit));
                                }
                                6 => { // fallback model
                                    fallback_picker = Some(FallbackPicker::new(current_fallback.as_deref()));
                                }
                                7 => { // effort
                                    effort_picker = Some(EffortPicker::new(current_effort.as_deref()));
                                }
                                8 => { // thinking
                                    thinking_picker = Some(ThinkingPicker::new(current_thinking.as_deref()));
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _)
                    | (KeyCode::Esc, _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,

                    // Tab / Shift+Tab — cycle focus
                    (KeyCode::Tab, _) => { focus = focus.next(); }
                    (KeyCode::BackTab, _) => { focus = focus.prev(); }

                    // Scroll — routed to focused panel
                    (KeyCode::Down, _) | (KeyCode::Char('j'), _) => match focus {
                        Focus::Accounts => accounts_scroll = accounts_scroll.saturating_add(1),
                        Focus::Requests => requests_scroll = requests_scroll.saturating_add(1),
                        Focus::History  => chart_window = chart_window.next(),
                    },
                    (KeyCode::Up, _) | (KeyCode::Char('k'), _) => match focus {
                        Focus::Accounts => accounts_scroll = accounts_scroll.saturating_sub(1),
                        Focus::Requests => requests_scroll = requests_scroll.saturating_sub(1),
                        Focus::History  => chart_window = chart_window.prev(),
                    },

                    // Time window (always works when history is visible)
                    (KeyCode::Char('t'), _) | (KeyCode::Char(']'), _) => {
                        chart_window = chart_window.next();
                    }
                    (KeyCode::Char('['), _) => {
                        chart_window = chart_window.prev();
                    }

                    (KeyCode::Char('r'), _) => {
                        last_fetch = Instant::now() - Duration::from_secs(10);
                    }
                    (KeyCode::Char('s'), _) => {
                        menu = Some(Menu::new());
                    }
                    (KeyCode::Char('?'), _) => { show_help = true; }
                    _ => {}
                }
            }
        }
    }

    execute!(terminal.backend_mut(), terminal::LeaveAlternateScreen, crossterm::cursor::Show)?;
    terminal::disable_raw_mode()?;
    Ok(())
}

async fn fetch_status(url: &str) -> Result<StatusResponse, FetchError> {
    let resp = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() || e.is_timeout() { FetchError::NotRunning }
            else { FetchError::Other(e.to_string()) }
        })?
        .error_for_status()
        .map_err(|e| FetchError::Other(e.to_string()))?;

    resp.json::<StatusResponse>()
        .await
        .map_err(|e| FetchError::Other(format!("bad response: {e}")))
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut Frame,
    state: &Option<StatusResponse>,
    error: &Option<FetchError>,
    accounts_scroll: usize,
    requests_scroll: usize,
    base_url: &str,
    picker: &Option<Picker>,
    model_picker: &Option<ModelPicker>,
    model_override: &Option<String>,
    strategy_picker: &Option<StrategyPicker>,
    current_strategy: &Option<String>,
    strategy_source: &Option<String>,
    alerts_muted: bool,
    show_help: bool,
    refresh_ms: u64,
    focus: Focus,
    chart_window: TimeWindow,
    start_time: Instant,
    menu: &Option<Menu>,
    speed_picker: &Option<SpeedPicker>,
    burst_limit_picker: &Option<BurstLimitPicker>,
    fallback_picker: &Option<FallbackPicker>,
    effort_picker: &Option<EffortPicker>,
    thinking_picker: &Option<ThinkingPicker>,
    current_burst_limit: u32,
    current_fallback: &Option<String>,
    current_effort: &Option<String>,
    current_thinking: &Option<String>,
) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    draw_header(f, chunks[0], state, model_override, current_strategy, strategy_source, alerts_muted);

    match state {
        None    => draw_connecting(f, chunks[1], error, base_url, start_time),
        Some(s) => draw_body(f, chunks[1], s, accounts_scroll, requests_scroll, focus, chart_window),
    }

    let any_overlay = menu.is_some() || picker.is_some() || model_picker.is_some()
        || strategy_picker.is_some() || speed_picker.is_some()
        || burst_limit_picker.is_some() || fallback_picker.is_some()
        || effort_picker.is_some() || thinking_picker.is_some();
    draw_footer(f, chunks[2], any_overlay, focus);

    // Overlays — draw order: menu first (background), sub-pickers on top
    if let Some(m) = menu {
        draw_menu(f, m, state, model_override, current_strategy, alerts_muted, refresh_ms,
                  current_burst_limit, current_fallback, current_effort, current_thinking, area);
    }
    if let Some(p) = picker { draw_picker(f, p, current_strategy.as_deref(), area); }
    if let Some(mp) = model_picker { draw_model_picker(f, mp, model_override.as_deref(), area); }
    if let Some(sp) = strategy_picker { draw_strategy_picker(f, sp, current_strategy.as_deref(), area); }
    if let Some(sp) = speed_picker { draw_speed_picker(f, sp, refresh_ms, area); }
    if let Some(bp) = burst_limit_picker { draw_burst_limit_picker(f, bp, current_burst_limit, area); }
    if let Some(fp) = fallback_picker { draw_fallback_picker(f, fp, current_fallback, area); }
    if let Some(ep) = effort_picker { draw_effort_picker(f, ep, current_effort, area); }
    if let Some(tp) = thinking_picker { draw_thinking_picker(f, tp, current_thinking, area); }
    if show_help { draw_help_overlay(f, area); }
}

fn draw_header(f: &mut Frame, area: Rect, state: &Option<StatusResponse>, model_override: &Option<String>, current_strategy: &Option<String>, strategy_source: &Option<String>, alerts_muted: bool) {
    let uptime_span = state
        .as_ref()
        .and_then(|s| s.started_ms)
        .map(|ms| {
            let now_ms = now_ms();
            let elapsed = now_ms.saturating_sub(ms);
            format!("  up {}", fmt_duration_ms(elapsed))
        });

    let mut spans = vec![
        Span::styled(" ◆ ", style_brand()),
        Span::styled("shunt", style_brand()),
        Span::styled(format!(" v{}", env!("CARGO_PKG_VERSION")), style_dim()),
        Span::styled("  monitor", style_dim()),
        Span::styled("  ·  live", Style::default().fg(GREEN)),
    ];
    if let Some(ref u) = uptime_span {
        spans.push(Span::styled(u.as_str(), style_dim()));
    }
    if let Some(ref m) = model_override {
        spans.push(Span::styled("  ·  ", style_dim()));
        spans.push(Span::styled("model ", style_dim()));
        spans.push(Span::styled(shorten_model(m), style_yellow()));
    }
    if let Some(ref strat) = current_strategy {
        let is_override = strategy_source.as_deref() == Some("override");
        spans.push(Span::styled("  ·  ", style_dim()));
        spans.push(Span::styled("strategy ", style_dim()));
        spans.push(Span::styled(
            strat.clone(),
            if is_override { style_yellow() } else { style_dim() },
        ));
    }
    if let Some(sv) = state.as_ref().and_then(|s| s.savings.as_ref()).filter(|sv| sv.today_cost_usd > 0.001) {
        spans.push(Span::styled("  ·  ", style_dim()));
        spans.push(Span::styled("saved ", style_dim()));
        spans.push(Span::styled(fmt_cost(sv.today_cost_usd), style_green()));
        spans.push(Span::styled(" today", style_dim()));
    }
    if alerts_muted {
        spans.push(Span::styled("  ·  ", style_dim()));
        spans.push(Span::styled("alerts muted", style_red()));
    }

    let block = Block::default().borders(Borders::BOTTOM).border_style(style_dkgreen());
    f.render_widget(Paragraph::new(Line::from(spans)).block(block).alignment(Alignment::Left), area);
}

fn sep() -> Span<'static> { Span::styled("  ·  ", Style::default().fg(DIM)) }

fn draw_footer(f: &mut Frame, area: Rect, overlay_open: bool, focus: Focus) {
    let hint = if overlay_open {
        Line::from(vec![
            Span::styled(" ↑↓", style_green()), Span::styled(" navigate", style_dim()), sep(),
            Span::styled("enter", style_green()), Span::styled(" select", style_dim()), sep(),
            Span::styled("esc", style_green()), Span::styled(" back", style_dim()),
        ])
    } else {
        let scroll_hint = match focus {
            Focus::Accounts | Focus::Requests => Span::styled(" scroll", style_dim()),
            Focus::History  => Span::styled(" time", style_dim()),
        };
        Line::from(vec![
            Span::styled(" q", style_green()), Span::styled(" quit", style_dim()), sep(),
            Span::styled("tab", style_green()), Span::styled(" focus", style_dim()), sep(),
            Span::styled("↑↓", style_green()), scroll_hint, sep(),
            Span::styled("r", style_green()), Span::styled(" refresh", style_dim()), sep(),
            Span::styled("s", style_green()), Span::styled(" settings", style_dim()), sep(),
            Span::styled("?", style_green()), Span::styled(" help", style_dim()),
        ])
    };
    f.render_widget(Paragraph::new(hint), area);
}

fn is_remote_url(base_url: &str) -> bool {
    !base_url.contains("127.0.0.1") && !base_url.contains("localhost")
}

fn draw_connecting(f: &mut Frame, area: Rect, error: &Option<FetchError>, base_url: &str, start_time: Instant) {
    let remote = is_remote_url(base_url);
    let lines: Vec<Line> = match error {
        Some(FetchError::NotRunning) if remote => vec![
            Line::from(vec![Span::styled("✗ ", style_red()), Span::styled("Lost connection to host", style_white())]),
            Line::from(vec![Span::styled(format!("  {base_url}"), style_dim())]),
            Line::from(vec![]),
            Line::from(vec![Span::styled("  Is the host still running shunt?", style_dim())]),
            Line::from(vec![
                Span::styled("  Run ", style_dim()),
                Span::styled("shunt connect <new-code>", style_cyan()),
                Span::styled(" to reconnect.", style_dim()),
            ]),
        ],
        Some(FetchError::NotRunning) => {
            let frame = (start_time.elapsed().as_millis() / 120) as usize % SPINNER.len();
            vec![Line::from(vec![
                Span::styled(SPINNER[frame], style_dim()),
                Span::styled("  waiting for proxy  ·  run shunt start", style_dim()),
            ])]
        }
        Some(FetchError::Other(msg)) => vec![Line::from(vec![
            Span::styled("✗ ", style_red()),
            Span::styled(format!("cannot reach {base_url}  ·  {msg}"), style_dim()),
        ])],
        None => vec![Line::from(Span::styled("connecting…", style_dim()))],
    };
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

// ---------------------------------------------------------------------------
// Body — left: accounts, right: requests (top) + history chart (bottom)
// ---------------------------------------------------------------------------

fn draw_body(
    f: &mut Frame,
    area: Rect,
    s: &StatusResponse,
    accounts_scroll: usize,
    requests_scroll: usize,
    focus: Focus,
    chart_window: TimeWindow,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(cols[1]);

    draw_accounts(f, cols[0], s, accounts_scroll, focus == Focus::Accounts);
    draw_request_log(f, rows[0], s, requests_scroll, focus == Focus::Requests);
    draw_history_chart(f, rows[1], s, chart_window, focus == Focus::History);
}

// ---------------------------------------------------------------------------
// Panel: accounts
// ---------------------------------------------------------------------------

fn panel_border_style(focused: bool) -> Style {
    if focused { style_green() } else { style_dkgreen() }
}

fn panel_title_style(focused: bool) -> Style {
    if focused { style_green().add_modifier(Modifier::BOLD) } else { style_dim() }
}

fn draw_accounts(f: &mut Frame, area: Rect, s: &StatusResponse, scroll: usize, focused: bool) {
    let title_span = Span::styled(" accounts", panel_title_style(focused));
    let block = Block::default()
        .title(Line::from(vec![title_span]))
        .borders(Borders::RIGHT)
        .border_style(panel_border_style(focused));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.accounts.is_empty() {
        f.render_widget(Paragraph::new(Line::from(Span::styled("  no accounts configured", style_dim()))), inner);
        return;
    }

    let pinned = s.pinned_account.as_deref().unwrap_or("");
    let last   = s.last_used_account.as_deref().unwrap_or("");
    let mut lines: Vec<Line> = Vec::new();

    for acc in &s.accounts {
        let routing_tag = if acc.name == pinned {
            Span::styled("  pinned", style_yellow())
        } else if acc.name == last {
            Span::styled("  active", style_green())
        } else {
            Span::raw("")
        };

        let (status_sym, status_style) = if acc.disabled || acc.auth_failed {
            ("✗", style_red())
        } else if acc.health_check_failed {
            ("!", style_yellow())
        } else if !acc.available {
            ("↺", style_yellow())
        } else {
            ("✓", style_green())
        };

        let provider_tag: Span<'static> = match acc.provider.as_str() {
            "anthropic" | "" => Span::raw(""),
            "openai" => Span::styled("  [chatgpt]".to_string(), Style::default().fg(YELLOW)),
            other    => Span::styled(format!("  [{other}]"), Style::default().fg(CYAN)),
        };

        lines.push(Line::from(vec![
            Span::styled(format!(" {status_sym} "), status_style),
            Span::styled(acc.name.clone(), Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
            routing_tag,
            provider_tag,
        ]));

        if let Some(email) = &acc.email {
            lines.push(Line::from(vec![
                Span::styled("   ", style_dim()),
                Span::styled(email.as_str(), style_dim()),
            ]));
        }

        let now = now_ms();
        if acc.cooldown_until_ms > now {
            let rem = acc.cooldown_until_ms - now;
            lines.push(Line::from(vec![
                Span::styled("   ⏸ cooldown  ", style_yellow()),
                Span::styled(format!("resumes in {}", fmt_duration_ms(rem)), style_yellow()),
            ]));
        }

        if acc.provider == "anthropic" || acc.provider.is_empty() {
            if acc.utilization_5h > 0.0 || acc.reset_5h.is_some() {
                lines.push(util_bar_line("5h", acc.utilization_5h, acc.reset_5h, acc.status_5h.as_deref()));
            }
            if acc.utilization_7d > 0.0 || acc.reset_7d.is_some() {
                lines.push(util_bar_line("7d", acc.utilization_7d, acc.reset_7d, acc.status_7d.as_deref()));
            }
        }

        lines.push(Line::raw(""));
    }

    let visible = lines.into_iter().skip(scroll).collect::<Vec<_>>();
    f.render_widget(Paragraph::new(visible), inner);
}

fn util_bar_line(label: &'static str, util: f64, reset: Option<u64>, wstatus: Option<&str>) -> Line<'static> {
    let exhausted = wstatus == Some("exhausted");
    let util = util.clamp(0.0, 1.0);
    let bar_w = 20usize;
    // Fill shows REMAINING capacity — matches `shunt status` convention.
    let used  = (util * bar_w as f64).round() as usize;
    let free  = bar_w.saturating_sub(used);
    let bar_color = if exhausted || util >= 0.8 { RED } else if util >= 0.5 { YELLOW } else { GREEN };
    let bar = format!("{}{}", "█".repeat(free), "░".repeat(used));
    let rem_pct = ((1.0 - util) * 100.0).round() as u64;
    let pct: String = if exhausted {
        "exhausted".to_owned()
    } else {
        format!("{}% left", rem_pct)
    };

    let reset_str = reset.map(|reset_secs| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if reset_secs > now_secs {
            format!("  resets {}", fmt_duration_ms((reset_secs - now_secs) * 1000))
        } else { String::new() }
    }).unwrap_or_default();

    Line::from(vec![
        Span::styled(format!("   {label} "), style_dim()),
        Span::styled(bar, Style::default().fg(bar_color)),
        Span::styled(format!(" {pct}"), Style::default().fg(bar_color)),
        Span::styled(reset_str, style_dim()),
    ])
}

// ---------------------------------------------------------------------------
// Panel: request log
// ---------------------------------------------------------------------------

fn draw_request_log(f: &mut Frame, area: Rect, s: &StatusResponse, scroll: usize, focused: bool) {
    let now = now_ms();
    let req_per_min = s.recent_requests.iter()
        .filter(|r| now.saturating_sub(r.ts_ms) < 60_000)
        .count();
    let rate_str = if req_per_min > 0 { format!("  {req_per_min}/min") } else { String::new() };

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" requests", panel_title_style(focused)),
            Span::styled(rate_str, style_dim()),
        ]))
        .borders(Borders::BOTTOM)
        .border_style(panel_border_style(focused));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.recent_requests.is_empty() {
        f.render_widget(Paragraph::new(Line::from(Span::styled("  no requests yet", style_dim()))), inner);
        return;
    }

    let header = Row::new(vec![
        Cell::from(Span::styled("time", style_dim())),
        Cell::from(Span::styled("account", style_dim())),
        Cell::from(Span::styled("model", style_dim())),
        Cell::from(Span::styled("dur", style_dim())),
    ]).height(1);

    let rows: Vec<Row> = s.recent_requests.iter().skip(scroll).map(|r| {
        let age_ms = now.saturating_sub(r.ts_ms);
        let time_str = if age_ms < 60_000 {
            format!("{}s ago", age_ms / 1000)
        } else {
            format!("{} ago", fmt_duration_ms(age_ms))
        };
        Row::new(vec![
            Cell::from(Span::styled(time_str, style_dim())),
            Cell::from(Span::styled(&r.account, style_green())),
            Cell::from(Span::styled(shorten_model(&r.model), style_cyan())),
            Cell::from(Span::styled(fmt_dur_short(r.duration_ms), style_dim())),
        ])
    }).collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Min(16),
        Constraint::Length(7),
    ];

    f.render_widget(
        Table::new(rows, widths).header(header).row_highlight_style(style_green()).column_spacing(1),
        inner,
    );
}

// ---------------------------------------------------------------------------
// Panel: history chart (stacked bar)
// ---------------------------------------------------------------------------

fn draw_history_chart(f: &mut Frame, area: Rect, s: &StatusResponse, window: TimeWindow, focused: bool) {
    // Title: time-window selector inline
    let all_windows = [
        TimeWindow::FifteenMin, TimeWindow::OneHour, TimeWindow::SixHour,
        TimeWindow::TwentyFourHour, TimeWindow::ThreeDay, TimeWindow::SevenDay,
    ];
    let mut title_spans: Vec<Span> = vec![Span::styled(" history ", panel_title_style(focused))];
    for w in all_windows {
        if w == window {
            title_spans.push(Span::styled(
                format!("[{}]", w.label()),
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ));
        } else {
            title_spans.push(Span::styled(format!(" {} ", w.label()), style_dim()));
        }
    }

    let block = Block::default()
        .title(Line::from(title_spans))
        .borders(Borders::NONE)
        .border_style(panel_border_style(focused));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chart_h = inner.height as usize;
    let chart_w = inner.width as usize;
    if chart_h < 3 || chart_w < 4 { return; }

    // Reserve 1 row at bottom for x-axis time labels
    let bar_h = chart_h.saturating_sub(1);

    let now = now_ms();
    let window_ms = window.ms();
    let n_buckets = window.bucket_count();
    let bucket_ms = window.bucket_ms();

    let account_names: Vec<&str> = s.accounts.iter().map(|a| a.name.as_str()).collect();
    let n_accounts = account_names.len();

    // bucket_counts[bucket][account]
    let mut bucket_counts: Vec<Vec<u32>> = vec![vec![0u32; n_accounts.max(1)]; n_buckets];

    for req in &s.recent_requests {
        let age_ms = now.saturating_sub(req.ts_ms);
        if age_ms >= window_ms { continue; }
        if let Some(idx) = account_names.iter().position(|&n| n == req.account) {
            let b = (n_buckets - 1).saturating_sub((age_ms / bucket_ms) as usize);
            bucket_counts[b][idx] += 1;
        }
    }

    let max_total = bucket_counts.iter()
        .map(|b| b.iter().sum::<u32>())
        .max()
        .unwrap_or(0);

    // No data at all
    if max_total == 0 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  no requests in the last {}", window.label()), style_dim(),
            ))),
            inner,
        );
        return;
    }

    // Slot width: divide available width across buckets
    let slot_w = (chart_w / n_buckets).max(1);
    let bar_w  = slot_w.saturating_sub(1).max(1);

    // Build grid[row][col] = Option<Color>
    let mut grid: Vec<Vec<Option<Color>>> = vec![vec![None; chart_w]; bar_h];

    for (b, counts) in bucket_counts.iter().enumerate() {
        let x = b * slot_w;
        if x >= chart_w { break; }
        let x_end = (x + bar_w).min(chart_w);

        let bucket_total: u32 = counts.iter().sum();
        if bucket_total == 0 { continue; }

        let mut y_from_bottom: usize = 0;
        for (acc_idx, &count) in counts.iter().enumerate() {
            if count == 0 { continue; }
            // Height proportional to this account's share of the max bucket
            let seg_h = ((count as f64 / max_total as f64) * bar_h as f64).ceil() as usize;
            let seg_h = seg_h.max(1);
            let row_end   = bar_h.saturating_sub(y_from_bottom);
            let row_start = row_end.saturating_sub(seg_h);
            let color = ACCOUNT_COLORS[acc_idx % ACCOUNT_COLORS.len()];
            for row in row_start..row_end {
                for col in x..x_end {
                    grid[row][col] = Some(color);
                }
            }
            y_from_bottom += seg_h;
        }
    }

    // Render grid as Lines
    let mut lines: Vec<Line> = grid.iter().map(|row| {
        let mut spans: Vec<Span> = Vec::new();
        let mut cur_color: Option<Color> = row.first().copied().flatten();
        let mut buf = String::new();

        for &cell in row {
            if cell == cur_color {
                buf.push(if cell.is_some() { '█' } else { ' ' });
            } else {
                let style = cur_color.map(|c| Style::default().fg(c)).unwrap_or_default();
                spans.push(Span::styled(std::mem::take(&mut buf), style));
                cur_color = cell;
                buf.push(if cell.is_some() { '█' } else { ' ' });
            }
        }
        if !buf.is_empty() {
            let style = cur_color.map(|c| Style::default().fg(c)).unwrap_or_default();
            spans.push(Span::styled(buf, style));
        }
        Line::from(spans)
    }).collect();

    // X-axis label row: show bucket timestamps at start / mid / end
    let label_row = build_x_labels(chart_w, n_buckets, slot_w, window);
    lines.push(label_row);

    // Legend: one coloured dot per account that has data
    if n_accounts > 0 {
        let has_data: Vec<bool> = (0..n_accounts)
            .map(|i| bucket_counts.iter().any(|b| b[i] > 0))
            .collect();
        let mut legend_spans: Vec<Span> = vec![Span::styled(" ", style_dim())];
        for (i, name) in account_names.iter().enumerate() {
            if !has_data[i] { continue; }
            let color = ACCOUNT_COLORS[i % ACCOUNT_COLORS.len()];
            legend_spans.push(Span::styled("● ", Style::default().fg(color)));
            legend_spans.push(Span::styled(format!("{name}  "), style_dim()));
        }
        lines.push(Line::from(legend_spans));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn build_x_labels(chart_w: usize, n_buckets: usize, slot_w: usize, window: TimeWindow) -> Line<'static> {
    // Place labels at left edge, middle bucket, and right edge
    let mut label_chars: Vec<char> = vec![' '; chart_w];

    let place = |chars: &mut Vec<char>, pos: usize, label: &str| {
        for (i, ch) in label.chars().enumerate() {
            if pos + i < chars.len() { chars[pos + i] = ch; }
        }
    };

    let left_label  = format!("-{}", window.label());
    let mid_label   = format!("-{}", fmt_secs_label(window.ms() as f64 / 2000.0));
    let right_label = "now";

    place(&mut label_chars, 0, &left_label);
    let mid_pos = ((n_buckets / 2) * slot_w).saturating_sub(mid_label.len() / 2);
    place(&mut label_chars, mid_pos, &mid_label);
    let right_pos = chart_w.saturating_sub(right_label.len());
    place(&mut label_chars, right_pos, right_label);

    let s: String = label_chars.into_iter().collect();
    Line::from(Span::styled(s, style_dim()))
}

fn fmt_secs_label(secs: f64) -> String {
    if secs < 60.0 { format!("{:.0}s", secs) }
    else if secs < 3600.0 { format!("{:.0}m", secs / 60.0) }
    else if secs < 86400.0 { format!("{:.0}h", secs / 3600.0) }
    else { format!("{:.0}d", secs / 86400.0) }
}

// ---------------------------------------------------------------------------
// Picker overlay
// ---------------------------------------------------------------------------

fn draw_picker(f: &mut Frame, picker: &Picker, strategy: Option<&str>, area: Rect) {
    let h = (picker.items.len() + 4) as u16;
    let w = 36u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" pin account ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = picker.items.iter().enumerate().map(|(i, item)| {
        let is_sel = i == picker.cursor;
        let label = if item == "auto" {
            let strat = strategy.unwrap_or("auto");
            format!("  {} {} routing", if is_sel { "◆" } else { " " }, strat)
        } else {
            format!("  {} {}", if is_sel { "◆" } else { " " }, item)
        };
        let style = if is_sel { Style::default().fg(GREEN).add_modifier(Modifier::BOLD) } else { style_dim() };
        Row::new(vec![Cell::from(Span::styled(label, style))])
    }).collect();

    f.render_widget(Table::new(rows, [Constraint::Min(0)]).column_spacing(0), inner);
}

fn draw_model_picker(f: &mut Frame, mp: &ModelPicker, current: Option<&str>, area: Rect) {
    let h = (MODEL_PRESETS.len() + 4) as u16;
    let w = 52u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" select model ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = MODEL_PRESETS.iter().enumerate().map(|(i, &(name, desc, id))| {
        let is_sel = i == mp.cursor;
        let is_current = current == Some(id) || (id.is_empty() && current.is_none());
        let bullet = if is_sel { "◆" } else { " " };
        let check  = if is_current { " ✓" } else { "  " };
        let name_style = if is_sel {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            style_white()
        };
        Row::new(vec![
            Cell::from(Span::styled(format!("  {bullet}"), style_dim())),
            Cell::from(Span::styled(format!("{name}{check}"), name_style)),
            Cell::from(Span::styled(desc, style_dim())),
        ])
    }).collect();

    f.render_widget(
        Table::new(rows, [Constraint::Length(4), Constraint::Length(12), Constraint::Min(0)])
            .column_spacing(1),
        inner,
    );
}

fn draw_strategy_picker(f: &mut Frame, sp: &StrategyPicker, current: Option<&str>, area: Rect) {
    let h = (STRATEGY_PRESETS.len() + 4) as u16;
    let w = 58u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" select routing strategy ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = STRATEGY_PRESETS.iter().enumerate().map(|(i, &(name, desc, id))| {
        let is_sel = i == sp.cursor;
        let is_current = current == Some(id);
        let bullet = if is_sel { "◆" } else { " " };
        let check  = if is_current { " ✓" } else { "  " };
        let name_style = if is_sel {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            style_white()
        };
        Row::new(vec![
            Cell::from(Span::styled(format!("  {bullet}"), style_dim())),
            Cell::from(Span::styled(format!("{name}{check}"), name_style)),
            Cell::from(Span::styled(desc, style_dim())),
        ])
    }).collect();

    f.render_widget(
        Table::new(rows, [Constraint::Length(4), Constraint::Length(14), Constraint::Min(0)])
            .column_spacing(1),
        inner,
    );
}

// ---------------------------------------------------------------------------
// Settings menu overlay
// ---------------------------------------------------------------------------

fn fmt_speed(ms: u64) -> String {
    if ms < 1_000 { format!("{}ms", ms) } else { format!("{}s", ms / 1_000) }
}

fn draw_menu(
    f: &mut Frame, m: &Menu,
    state: &Option<StatusResponse>,
    model_override: &Option<String>,
    current_strategy: &Option<String>,
    alerts_muted: bool,
    refresh_ms: u64,
    current_burst_limit: u32,
    current_fallback: &Option<String>,
    current_effort: &Option<String>,
    current_thinking: &Option<String>,
    area: Rect,
) {
    let h = (MENU_ITEMS + 4) as u16;
    let w = 40u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" settings ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let pinned = state.as_ref()
        .and_then(|s| s.pinned_account.as_deref())
        .unwrap_or("auto");
    let model = model_override.as_deref()
        .map(|m| shorten_model(m))
        .unwrap_or_else(|| "auto".into());
    let strategy = current_strategy.as_deref().unwrap_or("maximus");
    let mute_str = if alerts_muted { "muted" } else { "on" };
    let speed_str = fmt_speed(refresh_ms);

    let items: &[(&str, String)] = &[
        ("pin account",   pinned.to_owned()),
        ("set model",     model),
        ("strategy",      strategy.to_owned()),
        ("alerts",        mute_str.to_owned()),
        ("refresh speed", speed_str),
        ("burst limit",   if current_burst_limit == 0 { "off".to_owned() } else { format!("{}/min", current_burst_limit) }),
        ("fallback",      match current_fallback.as_deref() {
            None => "auto".to_owned(),
            Some("off") => "off".to_owned(),
            Some(m) => shorten_model(m),
        }),
        ("effort",        current_effort.as_deref().unwrap_or("auto").to_owned()),
        ("thinking",      match current_thinking.as_deref() {
            None => "auto".to_owned(),
            Some("disabled") => "off".to_owned(),
            Some(m) => m.to_owned(),
        }),
    ];

    let rows: Vec<Row> = items.iter().enumerate().map(|(i, (label, value))| {
        let is_sel = i == m.cursor;
        let bullet = if is_sel { "◆" } else { " " };
        let style = if is_sel {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            style_dim()
        };
        let val_style = if is_sel { style_yellow() } else { style_dim() };
        Row::new(vec![
            Cell::from(Span::styled(format!("  {bullet} {label}"), style)),
            Cell::from(Span::styled(value.as_str(), val_style)),
        ])
    }).collect();

    f.render_widget(
        Table::new(rows, [Constraint::Min(20), Constraint::Length(14)])
            .column_spacing(2),
        inner,
    );
}

fn draw_speed_picker(f: &mut Frame, sp: &SpeedPicker, current_ms: u64, area: Rect) {
    let h = (SPEED_PRESETS.len() + 4) as u16;
    let w = 30u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" refresh speed ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = SPEED_PRESETS.iter().enumerate().map(|(i, &ms)| {
        let is_sel = i == sp.cursor;
        let is_current = ms == current_ms;
        let bullet = if is_sel { "◆" } else { " " };
        let check  = if is_current { " ✓" } else { "" };
        let label = fmt_speed(ms);
        let style = if is_sel {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            style_dim()
        };
        Row::new(vec![
            Cell::from(Span::styled(format!("  {bullet} {label}{check}"), style)),
        ])
    }).collect();

    f.render_widget(Table::new(rows, [Constraint::Min(0)]).column_spacing(0), inner);
}

fn draw_burst_limit_picker(f: &mut Frame, bp: &BurstLimitPicker, current: u32, area: Rect) {
    let h = (BURST_LIMIT_PRESETS.len() + 4) as u16;
    let w = 30u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" burst limit ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = BURST_LIMIT_PRESETS.iter().enumerate().map(|(i, &val)| {
        let is_sel = i == bp.cursor;
        let is_current = val == current;
        let bullet = if is_sel { "◆" } else { " " };
        let check  = if is_current { " ✓" } else { "" };
        let label = if val == 0 { "off".to_owned() } else { format!("{}/min", val) };
        let style = if is_sel {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            style_dim()
        };
        Row::new(vec![
            Cell::from(Span::styled(format!("  {bullet} {label}{check}"), style)),
        ])
    }).collect();

    f.render_widget(Table::new(rows, [Constraint::Min(0)]).column_spacing(0), inner);
}

fn draw_fallback_picker(f: &mut Frame, fp: &FallbackPicker, current: &Option<String>, area: Rect) {
    let h = (FALLBACK_PRESETS.len() + 4) as u16;
    let w = 42u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" fallback model ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = FALLBACK_PRESETS.iter().enumerate().map(|(i, &(id, desc))| {
        let is_sel = i == fp.cursor;
        let is_current = match (id, current.as_deref()) {
            ("auto", None) => true,
            ("off", Some("off")) => true,
            (m, Some(c)) if m == c => true,
            _ => false,
        };
        let bullet = if is_sel { "◆" } else { " " };
        let check  = if is_current { " ✓" } else { "  " };
        let name_style = if is_sel {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            style_white()
        };
        Row::new(vec![
            Cell::from(Span::styled(format!("  {bullet}"), style_dim())),
            Cell::from(Span::styled(format!("{desc}{check}"), name_style)),
        ])
    }).collect();

    f.render_widget(
        Table::new(rows, [Constraint::Length(4), Constraint::Min(0)])
            .column_spacing(1),
        inner,
    );
}

fn draw_effort_picker(f: &mut Frame, ep: &EffortPicker, current: &Option<String>, area: Rect) {
    let h = (EFFORT_PRESETS.len() + 4) as u16;
    let w = 38u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" effort level ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = EFFORT_PRESETS.iter().enumerate().map(|(i, &(id, desc))| {
        let is_sel = i == ep.cursor;
        let is_current = match (id, current.as_deref()) {
            ("auto", None) => true,
            (e, Some(c)) if e == c => true,
            _ => false,
        };
        let bullet = if is_sel { "◆" } else { " " };
        let check  = if is_current { " ✓" } else { "  " };
        let name_style = if is_sel {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            style_white()
        };
        Row::new(vec![
            Cell::from(Span::styled(format!("  {bullet}"), style_dim())),
            Cell::from(Span::styled(format!("{desc}{check}"), name_style)),
        ])
    }).collect();

    f.render_widget(
        Table::new(rows, [Constraint::Length(4), Constraint::Min(0)])
            .column_spacing(1),
        inner,
    );
}

fn draw_thinking_picker(f: &mut Frame, tp: &ThinkingPicker, current: &Option<String>, area: Rect) {
    let h = (THINKING_PRESETS.len() + 4) as u16;
    let w = 42u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" thinking mode ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = THINKING_PRESETS.iter().enumerate().map(|(i, &(id, desc))| {
        let is_sel = i == tp.cursor;
        let is_current = match (id, current.as_deref()) {
            ("auto", None) => true,
            (m, Some(c)) if m == c => true,
            _ => false,
        };
        let bullet = if is_sel { "◆" } else { " " };
        let check  = if is_current { " ✓" } else { "  " };
        let name_style = if is_sel {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            style_white()
        };
        Row::new(vec![
            Cell::from(Span::styled(format!("  {bullet}"), style_dim())),
            Cell::from(Span::styled(format!("{desc}{check}"), name_style)),
        ])
    }).collect();

    f.render_widget(
        Table::new(rows, [Constraint::Length(4), Constraint::Min(0)])
            .column_spacing(1),
        inner,
    );
}

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let lines: &[(&str, &str)] = &[
        ("q / Esc",  "quit"),
        ("tab",      "cycle panel focus"),
        ("↑ / k",   "scroll up / prev time"),
        ("↓ / j",   "scroll down / next time"),
        ("r",        "force refresh"),
        ("s",        "open settings"),
        ("t / ]",   "next time window"),
        ("[",        "prev time window"),
        ("?",        "close help"),
    ];

    let h = (lines.len() + 4) as u16;
    let w = 42u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Line::from(Span::styled(" shortcuts ", style_dim())))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = lines.iter().map(|(key, desc)| {
        Row::new(vec![
            Cell::from(Span::styled(format!("  {key}"), style_green())),
            Cell::from(Span::styled(format!("  {desc}"), style_dim())),
        ])
    }).collect();

    f.render_widget(
        Table::new(rows, [Constraint::Length(14), Constraint::Min(0)]).column_spacing(1),
        inner,
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use crate::state::now_ms_pub as now_ms;

fn shorten_model(model: &str) -> String {
    let s = model.trim_start_matches("claude-");
    let s = if let Some(idx) = s.rfind('-') {
        let suffix = &s[idx + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) { &s[..idx] } else { s }
    } else { s };
    s.to_owned()
}

fn fmt_dur_short(ms: u64) -> String {
    if ms < 1_000 { format!("{ms}ms") }
    else if ms < 60_000 { format!("{:.1}s", ms as f64 / 1_000.0) }
    else { format!("{}m", ms / 60_000) }
}
