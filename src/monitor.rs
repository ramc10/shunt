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

use crate::term::{fmt_duration_ms, fmt_tokens};

// ---------------------------------------------------------------------------
// Status API response types (mirrors proxy.rs /status handler)
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
    #[serde(default)]
    today_input: u64,
    #[serde(default)]
    today_output: u64,
    #[serde(default)]
    today_cost_usd: f64,
    #[serde(default)]
    week_cost_usd: f64,
    #[serde(default)]
    all_time_cost_usd: f64,
}

#[derive(Debug, Deserialize)]
struct AccountStatus {
    name: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    provider: String,
    available: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    auth_failed: bool,
    #[serde(default)]
    utilization_5h: f64,
    #[serde(default)]
    reset_5h: Option<u64>,
    #[serde(default)]
    utilization_7d: f64,
    #[serde(default)]
    reset_7d: Option<u64>,
    #[serde(default)]
    cooldown_until_ms: u64,
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

const GREEN:    Color = Color::Indexed(154); // #afd700 bright lime-green
const DK_GREEN: Color = Color::Indexed(28);  // #008700 dark green
const BRAND:    Color = Color::Indexed(154); // #afd700 bright lime-green
const DIM:      Color = Color::Indexed(240); // #585858 gray
const YELLOW:   Color = Color::Indexed(220); // #ffd700 yellow
const RED:      Color = Color::Indexed(196); // #ff0000 red
const WHITE:    Color = Color::Indexed(253); // #dadada light gray
const CYAN:     Color = Color::Indexed(154); // #afd700 use green to stay on-theme

/// Per-account chart colours — distinct enough to tell apart at a glance.
const ACCOUNT_COLORS: &[Color] = &[
    Color::Indexed(154), // lime green  (brand)
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
// Interactive chart state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum TimeWindow {
    OneMin,
    FiveMin,
    FifteenMin,
    SixtyMin,
}

impl TimeWindow {
    fn ms(self) -> u64 {
        match self {
            Self::OneMin      => 60_000,
            Self::FiveMin     => 300_000,
            Self::FifteenMin  => 900_000,
            Self::SixtyMin    => 3_600_000,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::OneMin      => "1m",
            Self::FiveMin     => "5m",
            Self::FifteenMin  => "15m",
            Self::SixtyMin    => "1h",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::OneMin      => Self::FiveMin,
            Self::FiveMin     => Self::FifteenMin,
            Self::FifteenMin  => Self::SixtyMin,
            Self::SixtyMin    => Self::OneMin,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::OneMin      => Self::SixtyMin,
            Self::FiveMin     => Self::OneMin,
            Self::FifteenMin  => Self::FiveMin,
            Self::SixtyMin    => Self::FifteenMin,
        }
    }

    /// Number of equal-width buckets to divide the window into.
    fn bucket_count(self) -> usize { 60 }

    /// Width of each bucket in milliseconds.
    fn bucket_ms(self) -> u64 { self.ms() / self.bucket_count() as u64 }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ChartMetric {
    Tokens,
    Requests,
}

impl ChartMetric {
    fn label(self) -> &'static str {
        match self {
            Self::Tokens   => "tokens",
            Self::Requests => "reqs",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Tokens   => Self::Requests,
            Self::Requests => Self::Tokens,
        }
    }
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum FetchError {
    /// TCP connection refused — proxy is not running.
    NotRunning,
    /// Got a response but something else went wrong.
    Other(String),
}


// ---------------------------------------------------------------------------
// Picker overlay state
// ---------------------------------------------------------------------------

struct Picker {
    items: Vec<String>, // account names + "auto"
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
    fn up(&mut self) {
        self.cursor = if self.cursor == 0 { self.items.len() - 1 } else { self.cursor - 1 };
    }
    fn down(&mut self) {
        self.cursor = (self.cursor + 1) % self.items.len();
    }
    fn selected(&self) -> &str { &self.items[self.cursor] }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run_monitor(base_url: &str) -> Result<()> {
    let status_url = format!("{}/status", base_url.trim_end_matches('/'));
    let use_url    = format!("{}/use",    base_url.trim_end_matches('/'));

    // Install a panic hook that restores the terminal before printing the panic message,
    // so the terminal isn't left in raw/alternate-screen mode on crash.
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

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, terminal::EnterAlternateScreen, crossterm::cursor::Hide)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let mut state: Option<StatusResponse> = None;
    let mut fetch_err: Option<FetchError> = None;
    let mut last_fetch = Instant::now() - Duration::from_secs(10);
    let mut scroll: usize = 0;
    let mut picker: Option<Picker> = None;
    let mut show_help = false;
    let mut refresh_ms: u64 = 1_000;
    // Interactive chart state
    let mut chart_window = TimeWindow::FiveMin;
    let mut chart_metric = ChartMetric::Tokens;
    // Spinner frame counter for "not running" state
    let start_time = Instant::now();

    loop {
        // Fetch status at the configured interval
        if last_fetch.elapsed() >= Duration::from_millis(refresh_ms) {
            match fetch_status(&status_url).await {
                Ok(s)  => { state = Some(s); fetch_err = None; }
                Err(e) => { fetch_err = Some(e); state = None; }
            }
            last_fetch = Instant::now();
        }

        terminal.draw(|f| {
            draw(f, &state, &fetch_err, scroll, base_url, &picker, show_help,
                 refresh_ms, start_time, chart_window, chart_metric)
        })?;

        // Poll for key events (non-blocking, 200ms timeout)
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                // Help overlay intercepts all keys
                if show_help {
                    show_help = false;
                    continue;
                }

                // Picker overlay active — intercept keys
                if let Some(ref mut p) = picker {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => { picker = None; }
                        KeyCode::Up   | KeyCode::Char('k') => p.up(),
                        KeyCode::Down | KeyCode::Char('j') => p.down(),
                        KeyCode::Enter => {
                            let chosen = p.selected().to_owned();
                            picker = None;
                            // POST /use — best-effort, ignore errors
                            let _ = reqwest::Client::new()
                                .post(&use_url)
                                .json(&serde_json::json!({ "account": chosen }))
                                .timeout(Duration::from_secs(3))
                                .send()
                                .await;
                            // Force immediate refresh
                            last_fetch = Instant::now() - Duration::from_secs(10);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Normal keys
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _)
                    | (KeyCode::Esc, _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                    (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                        scroll = scroll.saturating_add(1);
                    }
                    (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                        scroll = scroll.saturating_sub(1);
                    }
                    (KeyCode::Char('r'), _) => {
                        last_fetch = Instant::now() - Duration::from_secs(10);
                    }
                    (KeyCode::Char('u'), _) => {
                        if let Some(ref s) = state {
                            picker = Some(Picker::new(&s.accounts, s.pinned_account.as_deref()));
                        }
                    }
                    (KeyCode::Char('?'), _) => {
                        show_help = true;
                    }
                    // +/= increase refresh rate (halve interval, min 200ms)
                    (KeyCode::Char('+'), _) | (KeyCode::Char('='), _) => {
                        refresh_ms = (refresh_ms / 2).max(200);
                    }
                    // - decrease refresh rate (double interval, max 10s)
                    (KeyCode::Char('-'), _) => {
                        refresh_ms = (refresh_ms * 2).min(10_000);
                    }
                    // t / ] — cycle time window forward
                    (KeyCode::Char('t'), _) | (KeyCode::Char(']'), _) => {
                        chart_window = chart_window.next();
                    }
                    // [ — cycle time window backward
                    (KeyCode::Char('['), _) => {
                        chart_window = chart_window.prev();
                    }
                    // m — toggle metric (tokens ↔ requests)
                    (KeyCode::Char('m'), _) => {
                        chart_metric = chart_metric.next();
                    }
                    _ => {}
                }
            }
        }
    }

    // Restore terminal
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
            if e.is_connect() || e.is_timeout() {
                FetchError::NotRunning
            } else {
                FetchError::Other(e.to_string())
            }
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
    scroll: usize,
    base_url: &str,
    picker: &Option<Picker>,
    show_help: bool,
    refresh_ms: u64,
    start_time: Instant,
    chart_window: TimeWindow,
    chart_metric: ChartMetric,
) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, chunks[0], state);

    match state {
        None => draw_connecting(f, chunks[1], error, base_url, start_time),
        Some(s) => draw_body(f, chunks[1], s, scroll, chart_window, chart_metric),
    }

    draw_footer(f, chunks[2], picker.is_some(), refresh_ms);

    if let Some(p) = picker {
        draw_picker(f, p, area);
    }

    if show_help {
        draw_help_overlay(f, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect, state: &Option<StatusResponse>) {
    let uptime_span = state
        .as_ref()
        .and_then(|s| s.started_ms)
        .map(|ms| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let elapsed = now_ms.saturating_sub(ms);
            format!("  up {}", fmt_duration_ms(elapsed))
        });

    let savings_span: Option<String> = state.as_ref().and_then(|s| {
        let sv = s.savings.as_ref()?;
        let today_tok = sv.today_input + sv.today_output;
        if today_tok == 0 && sv.all_time_cost_usd == 0.0 { return None; }
        let tok_str   = crate::term::fmt_tokens(today_tok);
        let cost_str  = crate::pricing::fmt_cost(sv.today_cost_usd);
        let week_str  = crate::pricing::fmt_cost(sv.week_cost_usd);
        Some(format!("  ·  today: {tok_str}  {cost_str}  ·  week: {week_str}"))
    });

    let mut spans = vec![
        Span::styled(" ◆ ", style_brand()),
        Span::styled("shunt", style_brand()),
        Span::styled("  monitor", style_dim()),
        Span::styled("  ·  live", Style::default().fg(GREEN)),
    ];
    if let Some(ref u) = uptime_span {
        spans.push(Span::styled(u.as_str(), style_dim()));
    }
    if let Some(ref sv) = savings_span {
        spans.push(Span::styled(sv.as_str(), style_dim()));
    }

    let title = Line::from(spans);
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(style_dkgreen());
    let p = Paragraph::new(title).block(block).alignment(Alignment::Left);
    f.render_widget(p, area);
}

fn sep() -> Span<'static> { Span::styled("  ·  ", Style::default().fg(DIM)) }

fn draw_footer(f: &mut Frame, area: Rect, picker_open: bool, refresh_ms: u64) {
    let hint = if picker_open {
        Line::from(vec![
            Span::styled(" ↑↓ navigate", style_dim()),
            sep(),
            Span::styled("enter", style_green()),
            Span::styled(" pin", style_dim()),
            sep(),
            Span::styled("esc", style_green()),
            Span::styled(" cancel", style_dim()),
        ])
    } else {
        let rate_str = if refresh_ms < 1_000 {
            format!("{}ms", refresh_ms)
        } else {
            format!("{}s", refresh_ms / 1_000)
        };
        Line::from(vec![
            Span::styled(" q", style_green()),
            Span::styled(" quit", style_dim()),
            sep(),
            Span::styled("r", style_green()),
            Span::styled(" refresh", style_dim()),
            sep(),
            Span::styled("u", style_green()),
            Span::styled(" pin", style_dim()),
            sep(),
            Span::styled("t", style_green()),
            Span::styled(" time", style_dim()),
            sep(),
            Span::styled("m", style_green()),
            Span::styled(" metric", style_dim()),
            sep(),
            Span::styled("+/-", style_green()),
            Span::styled(format!(" speed  {rate_str}"), style_dim()),
            sep(),
            Span::styled("?", style_green()),
            Span::styled(" help", style_dim()),
        ])
    };
    f.render_widget(Paragraph::new(hint), area);
}

fn is_remote_url(base_url: &str) -> bool {
    !base_url.contains("127.0.0.1") && !base_url.contains("localhost")
}

fn draw_connecting(
    f: &mut Frame,
    area: Rect,
    error: &Option<FetchError>,
    base_url: &str,
    start_time: Instant,
) {
    let remote = is_remote_url(base_url);

    let lines: Vec<Line> = match error {
        Some(FetchError::NotRunning) if remote => vec![
            Line::from(vec![
                Span::styled("✗ ", style_red()),
                Span::styled("Lost connection to host", style_white()),
            ]),
            Line::from(vec![
                Span::styled(format!("  {base_url}"), style_dim()),
            ]),
            Line::from(vec![]),
            Line::from(vec![
                Span::styled("  Is the host still running shunt?", style_dim()),
            ]),
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

    let p = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .block(Block::default());
    f.render_widget(p, area);
}

fn draw_body(
    f: &mut Frame,
    area: Rect,
    s: &StatusResponse,
    scroll: usize,
    chart_window: TimeWindow,
    chart_metric: ChartMetric,
) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    draw_accounts(f, chunks[0], s);
    draw_right_panel(f, chunks[1], s, scroll, chart_window, chart_metric);
}

// ---------------------------------------------------------------------------
// Right panel: request log (top) + history chart (bottom)
// ---------------------------------------------------------------------------

fn draw_right_panel(
    f: &mut Frame,
    area: Rect,
    s: &StatusResponse,
    scroll: usize,
    chart_window: TimeWindow,
    chart_metric: ChartMetric,
) {
    let halves = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);

    draw_request_log(f, halves[0], s, scroll);
    draw_history_chart(f, halves[1], s, chart_window, chart_metric);
}

fn draw_accounts(f: &mut Frame, area: Rect, s: &StatusResponse) {
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" accounts", style_dim()),
        ]))
        .borders(Borders::RIGHT)
        .border_style(style_dkgreen());

    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.accounts.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled("  no accounts configured", style_dim())));
        f.render_widget(p, inner);
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
            "openai"    => Span::styled("  [chatgpt]".to_string(), Style::default().fg(YELLOW)),
            other       => Span::styled(format!("  [{other}]"), Style::default().fg(CYAN)),
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

        // Cooldown countdown (only when actively cooling)
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if acc.cooldown_until_ms > now_ms {
            let remaining_ms = acc.cooldown_until_ms - now_ms;
            lines.push(Line::from(vec![
                Span::styled("   ⏸ cooldown  ", style_yellow()),
                Span::styled(
                    format!("resumes in {}", fmt_duration_ms(remaining_ms)),
                    style_yellow(),
                ),
            ]));
        }

        // Rate-limit bars — only Anthropic reports utilization windows.
        if acc.provider == "anthropic" || acc.provider.is_empty() {
            lines.push(util_bar_line("5h", acc.utilization_5h, acc.reset_5h));
            lines.push(util_bar_line("7d", acc.utilization_7d, acc.reset_7d));
        }

        lines.push(Line::raw(""));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn util_bar_line(label: &'static str, util: f64, reset: Option<u64>) -> Line<'static> {
    let util = util.clamp(0.0, 1.0);
    let bar_w = 20usize;
    let filled = (util * bar_w as f64).round() as usize;
    let bar_color = if util >= 0.9 { RED } else if util >= 0.6 { YELLOW } else { GREEN };
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w.saturating_sub(filled)));
    let pct = format!("{:.0}%", util * 100.0);

    let reset_str = reset.map(|reset_secs| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if reset_secs > now_secs {
            let diff_ms = (reset_secs - now_secs) * 1000;
            format!("  resets {}", fmt_duration_ms(diff_ms))
        } else {
            String::new()
        }
    }).unwrap_or_default();

    Line::from(vec![
        Span::styled(format!("   {label} "), style_dim()),
        Span::styled(bar, Style::default().fg(bar_color)),
        Span::styled(format!(" {pct}"), Style::default().fg(bar_color)),
        Span::styled(reset_str, style_dim()),
    ])
}

// ---------------------------------------------------------------------------
// History chart
// ---------------------------------------------------------------------------

fn draw_history_chart(
    f: &mut Frame,
    area: Rect,
    s: &StatusResponse,
    window: TimeWindow,
    metric: ChartMetric,
) {
    // Build a title row that doubles as the time-window selector.
    // Highlight the active window in green, others dimmed.
    let all_windows = [
        TimeWindow::OneMin,
        TimeWindow::FiveMin,
        TimeWindow::FifteenMin,
        TimeWindow::SixtyMin,
    ];
    let mut title_spans: Vec<Span> = vec![Span::styled(" history ", style_dim())];
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
    title_spans.push(Span::styled("  ·  ", style_dim()));
    // Metric toggle indicator
    for m in [ChartMetric::Tokens, ChartMetric::Requests] {
        if m == metric {
            title_spans.push(Span::styled(
                format!("[{}]", m.label()),
                Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
            ));
        } else {
            title_spans.push(Span::styled(format!(" {} ", m.label()), style_dim()));
        }
    }

    let block = Block::default()
        .title(Line::from(title_spans))
        .borders(Borders::TOP)
        .border_style(style_dkgreen());

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Need at least a few rows and columns to render a meaningful chart.
    if inner.height < 4 || inner.width < 12 {
        return;
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let n_buckets = window.bucket_count();
    let bucket_ms = window.bucket_ms();
    let window_ms = window.ms();
    let window_secs = window_ms as f64 / 1000.0;
    let bucket_secs = bucket_ms as f64 / 1000.0;

    // Build per-account bucket data.
    let account_names: Vec<&str> = s.accounts.iter().map(|a| a.name.as_str()).collect();
    let n_accounts = account_names.len();

    // account_buckets[acc_idx][bucket_idx] = value
    let mut account_buckets: Vec<Vec<f64>> = vec![vec![0.0; n_buckets]; n_accounts.max(1)];

    for req in &s.recent_requests {
        let age_ms = now_ms.saturating_sub(req.ts_ms);
        if age_ms >= window_ms {
            continue;
        }
        let acc_idx = account_names.iter().position(|&n| n == req.account);
        if let Some(idx) = acc_idx {
            // bucket 0 = oldest, bucket n-1 = newest
            let b = (n_buckets - 1).saturating_sub((age_ms / bucket_ms) as usize);
            let val = match metric {
                ChartMetric::Tokens   => (req.input_tokens + req.output_tokens) as f64,
                ChartMetric::Requests => 1.0,
            };
            account_buckets[idx][b] += val;
        }
    }

    // Convert buckets to (x, y) points (x = seconds from window start).
    let all_points: Vec<Vec<(f64, f64)>> = account_buckets
        .iter()
        .map(|buckets| {
            buckets
                .iter()
                .enumerate()
                .map(|(b, &v)| (b as f64 * bucket_secs, v))
                .collect()
        })
        .collect();

    // Find the maximum value across all accounts for y-axis scaling.
    let max_val = all_points
        .iter()
        .flat_map(|pts| pts.iter().map(|(_, v)| *v))
        .fold(0.0_f64, f64::max)
        .max(1.0);

    // Only include accounts that have at least one non-zero bucket.
    let active_datasets: Vec<(usize, &str, &[(f64, f64)])> = all_points
        .iter()
        .enumerate()
        .filter(|(_, pts)| pts.iter().any(|(_, v)| *v > 0.0))
        .map(|(i, pts)| (i, account_names.get(i).copied().unwrap_or("?"), pts.as_slice()))
        .collect();

    if active_datasets.is_empty() {
        let msg = Line::from(Span::styled(
            format!("  no {} in the last {}", metric.label(), window.label()),
            style_dim(),
        ));
        f.render_widget(Paragraph::new(msg), inner);
        return;
    }

    let datasets: Vec<Dataset> = active_datasets
        .iter()
        .map(|(acc_idx, name, pts)| {
            let color = ACCOUNT_COLORS[acc_idx % ACCOUNT_COLORS.len()];
            Dataset::default()
                .name(*name)
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(color))
                .data(pts)
        })
        .collect();

    // X-axis labels: left = "-<window>", mid = "-<half>", right = "now"
    let half_label = fmt_secs_label(window_secs / 2.0);
    let x_labels = vec![
        Span::styled(format!("-{}", window.label()), style_dim()),
        Span::styled(format!("-{half_label}"), style_dim()),
        Span::styled("now", style_green()),
    ];

    // Y-axis labels: 0 at bottom, max at top
    let y_top_label = fmt_metric_val(max_val, metric);
    let y_mid_label = fmt_metric_val(max_val / 2.0, metric);
    let y_labels = vec![
        Span::styled("0", style_dim()),
        Span::styled(y_mid_label, style_dim()),
        Span::styled(y_top_label, style_dim()),
    ];

    let chart = Chart::new(datasets)
        .x_axis(
            Axis::default()
                .bounds([0.0, window_secs])
                .labels(x_labels)
                .style(style_dkgreen()),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, max_val])
                .labels(y_labels)
                .style(style_dkgreen()),
        );

    f.render_widget(chart, inner);
}

/// Format a duration in seconds as a compact human string (for axis labels).
fn fmt_secs_label(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.0}s", secs)
    } else if secs < 3600.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{:.0}h", secs / 3600.0)
    }
}

/// Format a metric value compactly for the y-axis label.
fn fmt_metric_val(v: f64, metric: ChartMetric) -> String {
    match metric {
        ChartMetric::Requests => format!("{:.0}", v),
        ChartMetric::Tokens => {
            if v >= 1_000_000.0 {
                format!("{:.1}M", v / 1_000_000.0)
            } else if v >= 1_000.0 {
                format!("{:.0}k", v / 1_000.0)
            } else {
                format!("{:.0}", v)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request log (right panel — unchanged)
// ---------------------------------------------------------------------------

fn draw_request_log(f: &mut Frame, area: Rect, s: &StatusResponse, scroll: usize) {
    // Calculate requests per minute from last 60s
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let req_per_min = s.recent_requests.iter()
        .filter(|r| now_ms.saturating_sub(r.ts_ms) < 60_000)
        .count();
    let rate_str = if req_per_min > 0 {
        format!("  {req_per_min}/min")
    } else {
        String::new()
    };

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" requests", style_dim()),
            Span::styled(rate_str, style_dim()),
        ]))
        .borders(Borders::NONE);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.recent_requests.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled("  no requests yet", style_dim())));
        f.render_widget(p, inner);
        return;
    }

    let header = Row::new(vec![
        Cell::from(Span::styled("time", style_dim())),
        Cell::from(Span::styled("account", style_dim())),
        Cell::from(Span::styled("model", style_dim())),
        Cell::from(Span::styled("in", style_dim())),
        Cell::from(Span::styled("out", style_dim())),
        Cell::from(Span::styled("dur", style_dim())),
    ]).height(1);

    let rows: Vec<Row> = s.recent_requests
        .iter()
        .skip(scroll)
        .map(|r| {
            let age_ms = now_ms.saturating_sub(r.ts_ms);
            let time_str = if age_ms < 60_000 {
                format!("{}s ago", age_ms / 1000)
            } else {
                format!("{} ago", fmt_duration_ms(age_ms))
            };
            let model_short = shorten_model(&r.model);
            Row::new(vec![
                Cell::from(Span::styled(time_str, style_dim())),
                Cell::from(Span::styled(&r.account, style_green())),
                Cell::from(Span::styled(model_short, style_cyan())),
                Cell::from(Span::styled(fmt_tokens(r.input_tokens), style_white())),
                Cell::from(Span::styled(fmt_tokens(r.output_tokens), style_white())),
                Cell::from(Span::styled(fmt_dur_short(r.duration_ms), style_dim())),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Min(16),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(7),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(style_green())
        .column_spacing(1);

    f.render_widget(table, inner);
}

// ---------------------------------------------------------------------------
// Picker overlay
// ---------------------------------------------------------------------------

fn draw_picker(f: &mut Frame, picker: &Picker, area: Rect) {
    let h = (picker.items.len() + 4) as u16;
    let w = 36u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" pin account ", style_dim()),
        ]))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = picker.items.iter().enumerate().map(|(i, item)| {
        let is_sel = i == picker.cursor;
        let label = if item == "auto" {
            format!("  {} auto routing", if is_sel { "◆" } else { " " })
        } else {
            format!("  {} {}", if is_sel { "◆" } else { " " }, item)
        };
        let style = if is_sel {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            style_dim()
        };
        Row::new(vec![Cell::from(Span::styled(label, style))])
    }).collect();

    let table = Table::new(rows, [Constraint::Min(0)]).column_spacing(0);
    f.render_widget(table, inner);
}

// ---------------------------------------------------------------------------
// Help overlay
// ---------------------------------------------------------------------------

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let lines: &[(&str, &str)] = &[
        ("q / Esc",  "quit"),
        ("r",        "force refresh"),
        ("u",        "pin account"),
        ("↑ / k",   "scroll log up"),
        ("↓ / j",   "scroll log down"),
        ("+  / =",  "faster refresh rate"),
        ("-",        "slower refresh rate"),
        ("t / ]",   "next time window"),
        ("[",        "prev time window"),
        ("m",        "toggle tokens/requests"),
        ("?",        "toggle this help"),
        ("any key",  "close help"),
    ];

    let h = (lines.len() + 4) as u16;
    let w = 42u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup_area = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" shortcuts ", style_dim()),
        ]))
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

    let table = Table::new(rows, [Constraint::Length(14), Constraint::Min(0)])
        .column_spacing(1);
    f.render_widget(table, inner);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn shorten_model(model: &str) -> String {
    let s = model.trim_start_matches("claude-");
    let s = if let Some(idx) = s.rfind('-') {
        let suffix = &s[idx + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            &s[..idx]
        } else {
            s
        }
    } else {
        s
    };
    s.to_owned()
}

fn fmt_dur_short(ms: u64) -> String {
    if ms < 1_000 { format!("{ms}ms") }
    else if ms < 60_000 { format!("{:.1}s", ms as f64 / 1_000.0) }
    else { format!("{}m", ms / 60_000) }
}
