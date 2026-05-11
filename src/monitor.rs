/// Live fullscreen TUI monitor for shunt.
///
/// Connects to the running proxy's /status endpoint and refreshes every second.
/// Press 'q' or Esc to exit, 'u' to pick an account to pin.
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
}

#[derive(Debug, Deserialize)]
struct AccountStatus {
    name: String,
    #[serde(default)]
    email: Option<String>,
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
    total_tokens: u64,
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

const GREEN:    Color = Color::Rgb(0, 170, 0);
const DK_GREEN: Color = Color::Rgb(0, 100, 0);
const BRAND:    Color = Color::Rgb(34, 139, 34);
const DIM:      Color = Color::Rgb(100, 100, 100);
const YELLOW:   Color = Color::Yellow;
const RED:      Color = Color::Red;
const WHITE:    Color = Color::White;
const CYAN:     Color = Color::Cyan;

fn style_brand()   -> Style { Style::default().fg(BRAND).add_modifier(Modifier::BOLD) }
fn style_green()   -> Style { Style::default().fg(GREEN) }
fn style_dkgreen() -> Style { Style::default().fg(DK_GREEN) }
fn style_dim()     -> Style { Style::default().fg(DIM) }
fn style_yellow()  -> Style { Style::default().fg(YELLOW) }
fn style_red()     -> Style { Style::default().fg(RED) }
fn style_white()   -> Style { Style::default().fg(WHITE) }
fn style_cyan()    -> Style { Style::default().fg(CYAN) }
fn style_bold()    -> Style { Style::default().add_modifier(Modifier::BOLD) }

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
    // Spinner frame counter for "not running" state
    let start_time = Instant::now();

    loop {
        // Fetch status every second
        if last_fetch.elapsed() >= Duration::from_secs(1) {
            match fetch_status(&status_url).await {
                Ok(s)  => { state = Some(s); fetch_err = None; }
                Err(e) => { fetch_err = Some(e); state = None; }
            }
            last_fetch = Instant::now();
        }

        terminal.draw(|f| {
            draw(f, &state, &fetch_err, scroll, base_url, &picker, start_time)
        })?;

        // Poll for key events (non-blocking, 200ms timeout)
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
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

fn draw(
    f: &mut Frame,
    state: &Option<StatusResponse>,
    error: &Option<FetchError>,
    scroll: usize,
    base_url: &str,
    picker: &Option<Picker>,
    start_time: Instant,
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
        Some(s) => draw_body(f, chunks[1], s, scroll),
    }

    draw_footer(f, chunks[2], picker.is_some());

    if let Some(p) = picker {
        draw_picker(f, p, area);
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

    let mut spans = vec![
        Span::styled("◉ ", style_brand()),
        Span::styled("shunt", style_brand()),
        Span::styled("  monitor", style_dim()),
        Span::styled("  ·  live", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
    ];
    if let Some(ref u) = uptime_span {
        spans.push(Span::styled(u.as_str(), style_dim()));
    }

    let title = Line::from(spans);
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(style_dkgreen());
    let p = Paragraph::new(title).block(block).alignment(Alignment::Left);
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, picker_open: bool) {
    let hint = if picker_open {
        Line::from(vec![
            Span::styled(" ↑↓", style_green()),
            Span::styled(" navigate  ", style_dim()),
            Span::styled("enter", style_green()),
            Span::styled(" pin  ", style_dim()),
            Span::styled("esc", style_green()),
            Span::styled(" cancel", style_dim()),
        ])
    } else {
        Line::from(vec![
            Span::styled(" q", style_green()),
            Span::styled(" quit  ", style_dim()),
            Span::styled("r", style_green()),
            Span::styled(" refresh  ", style_dim()),
            Span::styled("u", style_green()),
            Span::styled(" pin account  ", style_dim()),
            Span::styled("↑↓", style_green()),
            Span::styled(" scroll log", style_dim()),
        ])
    };
    f.render_widget(Paragraph::new(hint), area);
}

fn draw_connecting(
    f: &mut Frame,
    area: Rect,
    error: &Option<FetchError>,
    base_url: &str,
    start_time: Instant,
) {
    let msg = match error {
        Some(FetchError::NotRunning) => {
            let frame = (start_time.elapsed().as_millis() / 120) as usize % SPINNER.len();
            Line::from(vec![
                Span::styled(SPINNER[frame], style_dim()),
                Span::styled(
                    format!("  waiting for proxy at {base_url}  ·  run shunt start"),
                    style_dim(),
                ),
            ])
        }
        Some(FetchError::Other(msg)) => Line::from(vec![
            Span::styled("✗ ", style_red()),
            Span::styled(
                format!("Cannot reach {base_url} — {msg}"),
                style_dim(),
            ),
        ]),
        None => Line::from(Span::styled("Connecting…", style_dim())),
    };

    let p = Paragraph::new(msg)
        .alignment(Alignment::Center)
        .block(Block::default());
    f.render_widget(p, area);
}

fn draw_body(f: &mut Frame, area: Rect, s: &StatusResponse, scroll: usize) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    draw_accounts(f, chunks[0], s);
    draw_request_log(f, chunks[1], s, scroll);
}

fn draw_accounts(f: &mut Frame, area: Rect, s: &StatusResponse) {
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled("── ", style_dkgreen()),
            Span::styled("ACCOUNTS", style_bold()),
            Span::styled(" ──────────────────", style_dkgreen()),
        ]))
        .borders(Borders::RIGHT)
        .border_style(style_dkgreen());

    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.accounts.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled("No accounts configured.", style_dim())));
        f.render_widget(p, inner);
        return;
    }

    let pinned = s.pinned_account.as_deref().unwrap_or("");
    let last   = s.last_used_account.as_deref().unwrap_or("");

    let mut lines: Vec<Line> = Vec::new();

    for acc in &s.accounts {
        let routing_tag = if acc.name == pinned {
            Span::styled(" [pinned]", style_yellow())
        } else if acc.name == last {
            Span::styled(" [active]", style_green())
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

        lines.push(Line::from(vec![
            Span::styled(format!(" {status_sym} "), status_style),
            Span::styled(&acc.name, Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
            routing_tag,
        ]));

        if let Some(email) = &acc.email {
            lines.push(Line::from(vec![
                Span::styled("   ", style_dim()),
                Span::styled(email.as_str(), style_dim()),
            ]));
        }

        // 5h bar
        lines.push(util_bar_line("5h", acc.utilization_5h, acc.reset_5h));
        // 7d bar
        if acc.utilization_7d > 0.0 || acc.reset_7d.is_some() {
            lines.push(util_bar_line("7d", acc.utilization_7d, acc.reset_7d));
        }

        let tok_str = fmt_tokens(acc.total_tokens);
        lines.push(Line::from(vec![
            Span::styled("   ", style_dim()),
            Span::styled(format!("{tok_str} tokens this window"), style_dim()),
        ]));

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
        Span::styled(format!(" {pct}"), style_dim()),
        Span::styled(reset_str, style_dim()),
    ])
}

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
            Span::styled("── ", style_dkgreen()),
            Span::styled("RECENT REQUESTS", style_bold()),
            Span::styled(rate_str, style_dim()),
            Span::styled(" ──────────────────", style_dkgreen()),
        ]))
        .borders(Borders::NONE);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.recent_requests.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled("  No requests yet.", style_dim())));
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
            Span::styled("── ", style_dkgreen()),
            Span::styled("PIN ACCOUNT", style_bold()),
            Span::styled(" ──", style_dkgreen()),
        ]))
        .borders(Borders::ALL)
        .border_style(style_dkgreen());

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let rows: Vec<Row> = picker.items.iter().enumerate().map(|(i, item)| {
        let is_sel = i == picker.cursor;
        let label = if item == "auto" {
            format!("  {} auto routing", if is_sel { "▶" } else { " " })
        } else {
            format!("  {} {}", if is_sel { "▶" } else { " " }, item)
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

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{}k", n / 1_000)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

fn fmt_dur_short(ms: u64) -> String {
    if ms < 1_000 { format!("{ms}ms") }
    else if ms < 60_000 { format!("{:.1}s", ms as f64 / 1_000.0) }
    else { format!("{}m", ms / 60_000) }
}

fn fmt_duration_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs == 0 { return "0s".into(); }
    let mins = secs / 60;
    if mins == 0 { return format!("{secs}s"); }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours == 0 { return format!("{mins}m"); }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if days == 0 {
        if rem_mins == 0 { format!("{hours}h") } else { format!("{hours}h {rem_mins}m") }
    } else if rem_hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d {rem_hours}h")
    }
}
