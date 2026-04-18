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
