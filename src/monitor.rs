/// Live fullscreen TUI monitor for shunt.
///
/// Connects to the running proxy's /status endpoint and refreshes every second.
/// Press 'q' or Esc to exit.
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
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
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

const GREEN:     Color = Color::Rgb(0, 170, 0);
const DK_GREEN:  Color = Color::Rgb(0, 100, 0);
const BRAND:     Color = Color::Rgb(34, 139, 34);
const DIM:       Color = Color::Rgb(100, 100, 100);
const YELLOW:    Color = Color::Yellow;
const RED:       Color = Color::Red;
const WHITE:     Color = Color::White;
const CYAN:      Color = Color::Cyan;

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
// Entry point
// ---------------------------------------------------------------------------

pub async fn run_monitor(base_url: &str) -> Result<()> {
    let status_url = format!("{}/status", base_url.trim_end_matches('/'));

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, terminal::EnterAlternateScreen, crossterm::cursor::Hide)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let mut state: Option<StatusResponse> = None;
    let mut error_msg: Option<String> = None;
    let mut last_fetch = Instant::now() - Duration::from_secs(10); // fetch immediately
    let mut scroll: usize = 0; // scroll offset for request log

    loop {
        // Fetch status every second
        if last_fetch.elapsed() >= Duration::from_secs(1) {
            match fetch_status(&status_url).await {
                Ok(s) => { state = Some(s); error_msg = None; }
                Err(e) => { error_msg = Some(e.to_string()); }
            }
            last_fetch = Instant::now();
        }

        terminal.draw(|f| draw(f, &state, &error_msg, scroll, base_url))?;

        // Poll for key events (non-blocking, 200ms timeout)
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
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
                        // Force refresh
                        last_fetch = Instant::now() - Duration::from_secs(10);
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

async fn fetch_status(url: &str) -> Result<StatusResponse> {
    let resp = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(3))
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json().await?)
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, state: &Option<StatusResponse>, error: &Option<String>, scroll: usize, base_url: &str) {
    let area = f.area();

    // Root layout: header | body | footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(0),     // body
            Constraint::Length(1),  // footer
        ])
        .split(area);

    draw_header(f, chunks[0]);

    match state {
        None => draw_connecting(f, chunks[1], error, base_url),
        Some(s) => draw_body(f, chunks[1], s, scroll),
    }

    draw_footer(f, chunks[2]);
}

fn draw_header(f: &mut Frame, area: Rect) {
    let title = Line::from(vec![
        Span::styled("◉ ", style_brand()),
        Span::styled("shunt", style_brand()),
        Span::styled("  monitor", style_dim()),
        Span::styled("  ·  live", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
    ]);
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(style_dkgreen());
    let p = Paragraph::new(title).block(block).alignment(Alignment::Left);
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let hint = Line::from(vec![
        Span::styled(" q", style_green()),
        Span::styled(" quit  ", style_dim()),
        Span::styled("r", style_green()),
        Span::styled(" refresh  ", style_dim()),
        Span::styled("↑↓", style_green()),
        Span::styled(" scroll log", style_dim()),
    ]);
    let p = Paragraph::new(hint);
    f.render_widget(p, area);
}

fn draw_connecting(f: &mut Frame, area: Rect, error: &Option<String>, base_url: &str) {
    let msg = match error {
        Some(e) => Line::from(vec![
            Span::styled("✗ ", style_red()),
            Span::styled(format!("Cannot reach {base_url} — {e}"), style_dim()),
        ]),
        None => Line::from(vec![
            Span::styled("Connecting…", style_dim()),
        ]),
    };
    let p = Paragraph::new(msg)
        .alignment(Alignment::Center)
        .block(Block::default());
    f.render_widget(p, area);
}

fn draw_body(f: &mut Frame, area: Rect, s: &StatusResponse, scroll: usize) {
    // Split body: left = accounts, right = request log
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(45),
            Constraint::Percentage(55),
        ])
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
        let p = Paragraph::new(Line::from(vec![
            Span::styled("No accounts configured.", style_dim()),
        ]));
        f.render_widget(p, inner);
        return;
    }

    // Build rows for each account
    let mut lines: Vec<Line> = Vec::new();
    let pinned = s.pinned_account.as_deref().unwrap_or("");
    let last = s.last_used_account.as_deref().unwrap_or("");

    for acc in &s.accounts {
        // Account name line
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

        let name_line = Line::from(vec![
            Span::styled(format!(" {status_sym} "), status_style),
            Span::styled(&acc.name, Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
            routing_tag,
        ]);
        lines.push(name_line);

        // Email
        if let Some(email) = &acc.email {
            lines.push(Line::from(vec![
                Span::styled("   ", style_dim()),
                Span::styled(email.as_str(), style_dim()),
            ]));
        }

        // Utilization bar
        let util = acc.utilization_5h.clamp(0.0, 1.0);
        let bar_w = 24usize;
        let filled = (util * bar_w as f64).round() as usize;
        let bar_color = if util >= 0.9 { RED } else if util >= 0.6 { YELLOW } else { GREEN };
        let bar: String = format!(
            "{}{}",
            "█".repeat(filled),
            "░".repeat(bar_w.saturating_sub(filled))
        );
        let pct = format!("{:.0}%", util * 100.0);

        let reset_str = if let Some(reset_secs) = acc.reset_5h {
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if reset_secs > now_secs {
                let diff_ms = (reset_secs - now_secs) * 1000;
                format!("  resets in {}", fmt_duration_ms(diff_ms))
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        lines.push(Line::from(vec![
            Span::styled("   ", style_dim()),
            Span::styled(bar, Style::default().fg(bar_color)),
            Span::styled(format!(" {pct}"), style_dim()),
            Span::styled(reset_str, style_dim()),
        ]));

        // Tokens
        let tok_str = fmt_tokens(acc.total_tokens);
        lines.push(Line::from(vec![
            Span::styled("   ", style_dim()),
            Span::styled(format!("{tok_str} tokens this window"), style_dim()),
        ]));

        lines.push(Line::raw(""));
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

fn draw_request_log(f: &mut Frame, area: Rect, s: &StatusResponse, scroll: usize) {
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled("── ", style_dkgreen()),
            Span::styled("RECENT REQUESTS", style_bold()),
            Span::styled(" ──────────────────", style_dkgreen()),
        ]))
        .borders(Borders::NONE);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.recent_requests.is_empty() {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("  No requests yet.", style_dim()),
        ]));
        f.render_widget(p, inner);
        return;
    }

    // Column headers
    let header = Row::new(vec![
        Cell::from(Span::styled("time", style_dim())),
        Cell::from(Span::styled("account", style_dim())),
        Cell::from(Span::styled("model", style_dim())),
        Cell::from(Span::styled("in", style_dim())),
        Cell::from(Span::styled("out", style_dim())),
        Cell::from(Span::styled("dur", style_dim())),
    ]).height(1);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

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

            // Shorten model name
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
// Helpers
// ---------------------------------------------------------------------------

fn shorten_model(model: &str) -> String {
    // "claude-sonnet-4-5-20251001" → "sonnet-4.5"
    // "claude-opus-4-6" → "opus-4.6"
    let s = model.trim_start_matches("claude-");
    // Drop trailing date suffix like -20251001
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
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m", ms / 60_000)
    }
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
