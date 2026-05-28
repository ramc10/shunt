use std::io::Write;

/// Write a terminal bell + iTerm2 Growl notification to the controlling terminal.
///
/// - Bell (`\x07`): audible/visual alert in any terminal emulator (iTerm2, Warp, Terminal.app, …).
/// - iTerm2 Growl (`\x1b]9;…\x07`): native macOS notification popup in iTerm2.
/// - Warp: bell triggers a tab badge automatically.
///
/// Writes to `/dev/tty` so it reaches the user even when stdout/stderr are
/// redirected to a log file.
pub fn terminal_notify(title: &str, body: &str) {
    let msg = if body.is_empty() {
        title.to_owned()
    } else {
        format!("{title}: {body}")
    };
    // Strip control characters to prevent escape-code injection.
    let msg: String = msg.chars().filter(|c| !c.is_control()).collect();

    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        // Bell, then iTerm2 Growl notification escape sequence.
        let _ = write!(tty, "\x07\x1b]9;{msg}\x07");
    }
}

/// Fire a macOS system notification AND a terminal bell + iTerm2 notification.
/// `sound` is a macOS alert sound name ("Basso", "Ping", "Glass", etc.)
pub fn notify(title: &str, body: &str, sound: &str) {
    terminal_notify(title, body);

    #[cfg(target_os = "macos")]
    {
        // Use double-quoted AppleScript strings with explicit escaping.
        // Rust's {:?} does not escape AppleScript backtick substitution, so we
        // must use explicit escaping instead.
        fn escape(s: &str) -> String {
            s.replace('\\', "\\\\").replace('"', "\\\"")
        }
        let script = format!(
            r#"display notification "{}" with title "{}" sound name "{}""#,
            escape(body),
            escape(title),
            escape(sound),
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (title, body, sound);
}
