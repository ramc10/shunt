/// Live fullscreen TUI monitor for shunt.
///
/// Connects to the running proxy's /status endpoint and refreshes every second.
/// Press 'q' or Esc to exit, 'u' to pick an account to pin, '?' for help.
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
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Cell, Chart, Clear, Dataset, GraphType, Paragraph, Row, Table},
    Frame, Terminal,
};
use serde::Deserialize;
use std::{
    io::stdout,
    time::{Duration, Instant},
};

use crate::notify::terminal_notify;
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
    savings: Option<SavingsInfo>,
}

#[derive(Debug, Deserialize, Default, Clone)]
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
    input_tokens: u64,
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
// Entry point
// ---------------------------------------------------------------------------

pub async fn run_monitor(base_url: &str) -> Result<()> {
    let base = base_url.trim_end_matches('/');
    let status_url = format!("{base}/status");
    let use_url    = format!("{base}/use");
    let model_url  = format!("{base}/model");
    let strategy_url = format!("{base}/strategy");

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
    let mut show_help = false;
    let mut refresh_ms: u64 = 1_000;
    let mut focus = Focus::Accounts;
    let mut chart_window = TimeWindow::FifteenMin;
    let start_time = Instant::now();
    // Track the newest request timestamp we've seen so far; used to detect new
    // requests each poll cycle and fire terminal bell + iTerm2 notifications.
    // Initialised to 0 so the very first fetch is used to establish a baseline
    // without triggering bells for historical requests.
    let mut last_seen_req_ts: u64 = 0;
    let mut notif_baseline_set = false;

    loop {
        if last_fetch.elapsed() >= Duration::from_millis(refresh_ms) {
            match fetch_status(&status_url).await {
                Ok(s)  => {
                    // Detect requests that arrived since the last poll.
                    let max_ts = s.recent_requests.iter().map(|r| r.ts_ms).max().unwrap_or(0);
                    if notif_baseline_set && max_ts > last_seen_req_ts {
                        let new: Vec<&ReqLog> = s.recent_requests.iter()
                            .filter(|r| r.ts_ms > last_seen_req_ts)
                            .collect();
                        if new.len() == 1 {
                            terminal_notify("shunt", &new[0].model);
                        } else {
                            terminal_notify("shunt", &format!("{} new requests", new.len()));
                        }
                    }
                    if max_ts > 0 {
                        last_seen_req_ts = last_seen_req_ts.max(max_ts);
                        notif_baseline_set = true;
                    }
                    state = Some(s); fetch_err = None;
                }
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
            last_fetch = Instant::now();
        }

        terminal.draw(|f| {
            draw(f, &state, &fetch_err, accounts_scroll, requests_scroll,
                 base_url, &picker, &model_picker, &model_override,
                 &strategy_picker, &current_strategy, &strategy_source,
                 show_help, refresh_ms, focus, chart_window, start_time)
        })?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if show_help {
                    show_help = false;
                    continue;
                }

                // Account picker overlay
                if let Some(ref mut p) = picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => p.up(),
                        KeyCode::Down | KeyCode::Char('j') => p.down(),
                        KeyCode::Enter => {
                            let chosen = p.selected().to_owned();
                            picker = None;
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

                // Model picker overlay
                if let Some(ref mut mp) = model_picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { model_picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => mp.up(),
                        KeyCode::Down | KeyCode::Char('j') => mp.down(),
                        KeyCode::Enter => {
                            let chosen_id = mp.selected_id().to_owned();
                            model_picker = None;
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

                // Strategy picker overlay
                if let Some(ref mut sp) = strategy_picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { strategy_picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => sp.up(),
                        KeyCode::Down | KeyCode::Char('j') => sp.down(),
                        KeyCode::Enter => {
                            let chosen_id = sp.selected_id().to_owned();
                            strategy_picker = None;
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
                    (KeyCode::Char('u'), _) => {
                        if let Some(ref s) = state {
                            picker = Some(Picker::new(&s.accounts, s.pinned_account.as_deref()));
                        }
                    }
                    (KeyCode::Char('m'), _) => {
                        model_picker = Some(ModelPicker::new(model_override.as_deref()));
                    }
                    (KeyCode::Char('s'), _) => {
                        strategy_picker = Some(StrategyPicker::new(current_strategy.as_deref()));
                    }
                    (KeyCode::Char('?'), _) => { show_help = true; }
                    (KeyCode::Char('+'), _) | (KeyCode::Char('='), _) => {
                        refresh_ms = (refresh_ms / 2).max(200);
                    }
                    (KeyCode::Char('-'), _) => {
                        refresh_ms = (refresh_ms * 2).min(10_000);
                    }
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
    show_help: bool,
    refresh_ms: u64,
    focus: Focus,
    chart_window: TimeWindow,
    start_time: Instant,
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

    draw_header(f, chunks[0], state, model_override, current_strategy, strategy_source);

    match state {
        None    => draw_connecting(f, chunks[1], error, base_url, start_time),
        Some(s) => draw_body(f, chunks[1], s, accounts_scroll, requests_scroll, focus, chart_window),
    }

    draw_footer(f, chunks[2], picker.is_some() || model_picker.is_some() || strategy_picker.is_some(), refresh_ms, focus);

    if let Some(p) = picker { draw_picker(f, p, current_strategy.as_deref(), area); }
    if let Some(mp) = model_picker { draw_model_picker(f, mp, model_override.as_deref(), area); }
    if let Some(sp) = strategy_picker { draw_strategy_picker(f, sp, current_strategy.as_deref(), area); }
    if show_help { draw_help_overlay(f, area); }
}

fn draw_header(f: &mut Frame, area: Rect, state: &Option<StatusResponse>, model_override: &Option<String>, current_strategy: &Option<String>, strategy_source: &Option<String>) {
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

    let block = Block::default().borders(Borders::BOTTOM).border_style(style_dkgreen());
    f.render_widget(Paragraph::new(Line::from(spans)).block(block).alignment(Alignment::Left), area);
}

fn sep() -> Span<'static> { Span::styled("  ·  ", Style::default().fg(DIM)) }

fn draw_footer(f: &mut Frame, area: Rect, picker_open: bool, refresh_ms: u64, focus: Focus) {
    let hint = if picker_open {
        Line::from(vec![
            Span::styled(" ↑↓ navigate", style_dim()), sep(),
            Span::styled("enter", style_green()), Span::styled(" pin", style_dim()), sep(),
            Span::styled("esc", style_green()), Span::styled(" cancel", style_dim()),
        ])
    } else {
        let rate_str = if refresh_ms < 1_000 { format!("{}ms", refresh_ms) } else { format!("{}s", refresh_ms / 1_000) };
        let scroll_hint = match focus {
            Focus::Accounts | Focus::Requests => Span::styled(" scroll", style_dim()),
            Focus::History  => Span::styled(" time", style_dim()),
        };
        Line::from(vec![
            Span::styled(" q", style_green()), Span::styled(" quit", style_dim()), sep(),
            Span::styled("tab", style_green()), Span::styled(" focus", style_dim()), sep(),
            Span::styled("↑↓", style_green()), scroll_hint, sep(),
            Span::styled("r", style_green()), Span::styled(" refresh", style_dim()), sep(),
            Span::styled("u", style_green()), Span::styled(" pin", style_dim()), sep(),
            Span::styled("m", style_green()), Span::styled(" model", style_dim()), sep(),
            Span::styled("s", style_green()), Span::styled(" strategy", style_dim()), sep(),
            Span::styled("+/-", style_green()), Span::styled(format!(" speed  {rate_str}"), style_dim()), sep(),
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
// Help overlay
// ---------------------------------------------------------------------------

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let lines: &[(&str, &str)] = &[
        ("q / Esc",  "quit"),
        ("tab",      "cycle panel focus"),
        ("↑ / k",   "scroll up / prev time"),
        ("↓ / j",   "scroll down / next time"),
        ("r",        "force refresh"),
        ("u",        "pin account"),
        ("m",        "override model"),
        ("s",        "switch routing strategy"),
        ("t / ]",   "next time window"),
        ("[",        "prev time window"),
        ("+  / =",  "faster refresh"),
        ("-",        "slower refresh"),
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

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
