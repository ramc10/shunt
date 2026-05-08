/// Terminal formatting helpers — ANSI colors, alignment, and status symbols.
///
/// All color output is suppressed when stdout is not a TTY (e.g. piped to a file).
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// TTY detection
// ---------------------------------------------------------------------------

fn is_tty() -> bool {
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(|| {
        // SAFETY: just calling libc isatty
        #[cfg(unix)]
        unsafe { libc::isatty(1) != 0 }
        #[cfg(not(unix))]
        false
    })
}

// ---------------------------------------------------------------------------
// ANSI codes
// ---------------------------------------------------------------------------

pub fn bold(s: &str) -> String {
    if is_tty() { format!("\x1b[1m{s}\x1b[0m") } else { s.to_owned() }
}

pub fn dim(s: &str) -> String {
    if is_tty() { format!("\x1b[2m{s}\x1b[0m") } else { s.to_owned() }
}

pub fn green(s: &str) -> String {
    if is_tty() { format!("\x1b[32m{s}\x1b[0m") } else { s.to_owned() }
}

pub fn yellow(s: &str) -> String {
    if is_tty() { format!("\x1b[33m{s}\x1b[0m") } else { s.to_owned() }
}

pub fn red(s: &str) -> String {
    if is_tty() { format!("\x1b[31m{s}\x1b[0m") } else { s.to_owned() }
}

pub fn cyan(s: &str) -> String {
    if is_tty() { format!("\x1b[36m{s}\x1b[0m") } else { s.to_owned() }
}

pub fn bold_white(s: &str) -> String {
    if is_tty() { format!("\x1b[1;97m{s}\x1b[0m") } else { s.to_owned() }
}

/// 256-colour dark forest green — used for borders and decorative chrome.
pub fn dark_green(s: &str) -> String {
    if is_tty() { format!("\x1b[38;5;28m{s}\x1b[0m") } else { s.to_owned() }
}

/// Bold bright green — used for account names in the routing diagram.
pub fn green_bold(s: &str) -> String {
    if is_tty() { format!("\x1b[1;32m{s}\x1b[0m") } else { s.to_owned() }
}

/// Bold medium green — the primary brand colour for the "shunt" wordmark.
pub fn brand_green(s: &str) -> String {
    if is_tty() { format!("\x1b[1;38;5;34m{s}\x1b[0m") } else { s.to_owned() }
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

pub const CHECK:   &str = "✓";
pub const CROSS:   &str = "✗";
pub const DOT:     &str = "●";
pub const EMPTY:   &str = "○";
pub const DASH:    &str = "—";
pub const ARROW:   &str = "→";

// ---------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------

/// Horizontal rule, dimmed
pub fn rule(width: usize) -> String {
    dim(&"─".repeat(width))
}

/// Print a section header like:  ── ACCOUNTS ──────────────────
pub fn section(label: &str) {
    let header = format!("{} {} {}", dim("──"), bold(label), dim(&"─".repeat(44 - label.len())));
    println!("{header}");
}

/// Format a duration in ms dynamically:
///   >= 24h  → "Xd Yh" / "Xd"
///   >= 1h   → "Xh Ym" / "Xh"
///   >= 1m   → "Xm"
///   < 1m    → "Xs"
pub fn fmt_duration_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs == 0 {
        return "0s".into();
    }
    let mins = secs / 60;
    if mins == 0 {
        return format!("{}s", secs);
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours == 0 {
        return format!("{mins}m");
    }
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

// ---------------------------------------------------------------------------
// Interactive select menu
// ---------------------------------------------------------------------------

/// An item in the interactive select menu.
pub struct SelectItem {
    /// What the user sees (may contain ANSI codes)
    pub label: String,
    /// Value returned on selection
    pub value: String,
}

/// Show an interactive, arrow-key-navigable menu and return the chosen value.
///
/// Controls:
///   ↑ / k      — move up
///   ↓ / j      — move down
///   1–9        — jump to item N
///   Enter      — confirm selection
///   Esc / q    — cancel (returns None)
pub fn select(prompt: &str, items: &[SelectItem], initial: usize) -> Option<String> {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyModifiers},
        execute,
        terminal::{self, ClearType},
    };
    use std::io::{stdout, Write};

    if items.is_empty() {
        return None;
    }

    let mut selected = initial.min(items.len() - 1);
    let mut stdout = stdout();

    // Enter raw mode so keystrokes are read immediately without Enter
    terminal::enable_raw_mode().ok()?;
    execute!(stdout, cursor::Hide).ok();

    let render = |sel: usize, out: &mut dyn Write| {
        // Clear all lines we're about to draw
        let _ = write!(out, "\r\n  {prompt}\r\n\r\n");
        for (i, item) in items.iter().enumerate() {
            if i == sel {
                let _ = write!(out, "  \x1b[1;36m▶\x1b[0m  \x1b[1m{}\x1b[0m\r\n", item.label);
            } else {
                let _ = write!(out, "     {}\r\n", item.label);
            }
        }
        let _ = write!(
            out,
            "\r\n  \x1b[2m↑ ↓  navigate  ·  enter  select  ·  esc  cancel\x1b[0m\r\n",
        );
        let _ = out.flush();
    };

    // Initial render
    let lines_drawn = items.len() + 5; // header + blank + items + blank + hint
    render(selected, &mut stdout);

    let result = loop {
        match event::read() {
            Ok(Event::Key(key)) => {
                // Move cursor back to top of our block
                execute!(
                    stdout,
                    cursor::MoveUp(lines_drawn as u16),
                    cursor::MoveToColumn(0),
                    terminal::Clear(ClearType::FromCursorDown),
                ).ok();

                match (key.code, key.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => break None,
                    (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => break None,
                    (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                        selected = if selected == 0 { items.len() - 1 } else { selected - 1 };
                        render(selected, &mut stdout);
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                        selected = (selected + 1) % items.len();
                        render(selected, &mut stdout);
                    }
                    (KeyCode::Char(c), _) if c.is_ascii_digit() => {
                        let n = c as usize - '0' as usize;
                        if n >= 1 && n <= items.len() {
                            selected = n - 1;
                            render(selected, &mut stdout);
                        }
                    }
                    (KeyCode::Enter, _) => {
                        // Clear the menu block and leave a one-line confirmation
                        execute!(
                            stdout,
                            cursor::MoveUp(lines_drawn as u16),
                            cursor::MoveToColumn(0),
                            terminal::Clear(ClearType::FromCursorDown),
                        ).ok();
                        break Some(items[selected].value.clone());
                    }
                    _ => { render(selected, &mut stdout); }
                }
            }
            _ => {}
        }
    };

    execute!(stdout, cursor::Show).ok();
    terminal::disable_raw_mode().ok();
    println!();
    result
}

#[cfg(test)]
mod tests {
    use super::fmt_duration_ms;

    #[test]
    fn test_fmt_duration_ms() {
        assert_eq!(fmt_duration_ms(0),              "0s");
        assert_eq!(fmt_duration_ms(500),            "0s");
        assert_eq!(fmt_duration_ms(1_000),          "1s");
        assert_eq!(fmt_duration_ms(45_000),         "45s");
        assert_eq!(fmt_duration_ms(59_000),         "59s");
        assert_eq!(fmt_duration_ms(60_000),         "1m");
        assert_eq!(fmt_duration_ms(90_000),         "1m");   // 1m 30s → "1m"
        assert_eq!(fmt_duration_ms(30 * 60_000),    "30m");
        assert_eq!(fmt_duration_ms(60 * 60_000),    "1h");
        assert_eq!(fmt_duration_ms(90 * 60_000),    "1h 30m");
        assert_eq!(fmt_duration_ms(5 * 3600_000),   "5h");
        assert_eq!(fmt_duration_ms(5 * 3600_000 + 30 * 60_000), "5h 30m");
        assert_eq!(fmt_duration_ms(24 * 3600_000),  "1d");
        assert_eq!(fmt_duration_ms(48 * 3600_000),  "2d");
        assert_eq!(fmt_duration_ms(25 * 3600_000),  "1d 1h");
        assert_eq!(fmt_duration_ms(7 * 24 * 3600_000), "7d");
    }
}

/// Format a large token count as "1.2k" / "34k" / "1.1M" / raw
pub fn fmt_tokens(n: u64) -> String {
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
