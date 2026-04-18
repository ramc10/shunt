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

/// Format a duration in ms as "Xh Ym" or "Ym" or "< 1m"
pub fn fmt_duration_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let mins = secs / 60;
    if mins == 0 {
        return "< 1m".into();
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours == 0 {
        format!("{mins}m")
    } else if rem_mins == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {rem_mins}m")
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
            "\r\n  \x1b[2m↑↓ navigate  ·  1-{} jump  ·  enter select  ·  esc cancel\x1b[0m\r\n",
            items.len()
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
