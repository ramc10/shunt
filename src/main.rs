fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let is_start = args.iter().any(|a| a == "start");
    let is_foreground = args.iter().any(|a| a == "--foreground");
    let is_daemon = args.iter().any(|a| a == "--_daemon");

    if is_start && !is_daemon {
        // Kill any existing instance BEFORE doing anything else.
        // Must be synchronous — no runtime, no async, no hangs possible.
        preflight_kill();

        if !is_foreground {
            // Daemonize by re-execing self with --_daemon (avoids fork() issues on macOS).
            // The child runs the server in the background; we print a status line and exit.
            spawn_daemon();
            // spawn_daemon() exits — we never reach here unless spawn failed.
        }
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(shunt::cli::run())
}

fn preflight_kill() {
    let pid_path = shunt::config::pid_path();
    let Ok(content) = std::fs::read_to_string(&pid_path) else { return };
    let Ok(old_pid) = content.trim().parse::<u32>() else { return };
    if old_pid == std::process::id() { return; }

    // SIGKILL via libc — no subprocess, instant, cannot hang
    unsafe { libc::kill(old_pid as i32, libc::SIGKILL) };
    // Give the OS 400ms to reclaim the port
    std::thread::sleep(std::time::Duration::from_millis(400));
}

/// Re-exec self with --_daemon flag so the child runs the server.
/// Opens the log file for the child's stdout/stderr, prints a brief status
/// line to the terminal, then exits so the shell prompt returns.
fn spawn_daemon() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Warning: cannot locate executable ({e}), starting in foreground");
            return; // fall through to foreground
        }
    };

    let log_path = shunt::config::log_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let log_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Warning: cannot open log file ({e}), starting in foreground");
            return;
        }
    };

    let addr = load_addr();

    // Collect original args, replace "start" with "start --_daemon"
    let mut child_args: Vec<String> = std::env::args()
        .skip(1) // skip argv[0]
        .collect();
    if !child_args.iter().any(|a| a == "--_daemon") {
        child_args.push("--_daemon".into());
    }

    use std::os::unix::process::CommandExt;
    let log_file2 = log_file.try_clone().ok();

    let result = std::process::Command::new(&exe)
        .args(&child_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file2.unwrap_or_else(|| {
            std::fs::OpenOptions::new()
                .create(true).append(true).open(&log_path).unwrap()
        })))
        // Detach from the current process group so SIGHUP doesn't reach child
        .process_group(0)
        .spawn();

    match result {
        Ok(_child) => {
            println!();
            println!("  shunt started  ·  {addr}");
            println!("  shunt status   to see live info");
            println!();
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Warning: could not daemonize ({e}), starting in foreground");
            // fall through — tokio will start and run normally
        }
    }
}

fn load_addr() -> String {
    if let Ok(cfg) = shunt::config::load_config(None) {
        format!("http://{}:{}", cfg.server.host, cfg.server.port)
    } else {
        "http://127.0.0.1:8082".into()
    }
}
