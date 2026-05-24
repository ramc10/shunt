/// Fire a macOS system notification. No-op on other platforms.
/// `sound` is a macOS alert sound name ("Basso", "Ping", "Glass", etc.)
pub fn notify(title: &str, body: &str, sound: &str) {
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
