use shunt::term::{bold_white, brand_green, cyan, dim};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let is_start = args.iter().any(|a| a == "start");
    let is_foreground = args.iter().any(|a| a == "--foreground");
    let is_daemon = args.iter().any(|a| a == "--daemon");

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

    // Safety check: verify the PID actually belongs to a shunt process before
    // killing it. If the daemon died and the OS recycled its PID to something
    // else (e.g. the user's shell), we must not kill it.
    if !pid_is_shunt(old_pid) { return; }

    // SIGKILL via libc — no subprocess, instant, cannot hang
    unsafe { libc::kill(old_pid as i32, libc::SIGKILL) };
    // Give the OS 400ms to reclaim the port
    std::thread::sleep(std::time::Duration::from_millis(400));
}

/// Returns true if the given PID is a shunt process.
/// Uses `ps` to check the command name — cross-platform enough for macOS/Linux.
fn pid_is_shunt(pid: u32) -> bool {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
    else {
        return false;
    };
    let comm = String::from_utf8_lossy(&out.stdout);
    comm.trim().contains("shunt")
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

    // Collect original args, replace "start" with "start --_daemon"
    let mut child_args: Vec<String> = std::env::args()
        .skip(1) // skip argv[0]
        .collect();
    if !child_args.iter().any(|a| a == "--daemon") {
        child_args.push("--daemon".into());
    }

    use std::os::unix::process::CommandExt;
    let log_file2 = log_file.try_clone().ok();

    let result = unsafe {
        std::process::Command::new(&exe)
            .args(&child_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_file2.unwrap_or_else(|| {
                std::fs::OpenOptions::new()
                    .create(true).append(true).open(&log_path).unwrap()
            })))
            // setsid() creates a new session: detaches from the controlling
            // terminal entirely so the daemon survives terminal close / logout.
            .pre_exec(|| {
                libc::setsid();
                Ok(())
            })
            .spawn()
    };

    match result {
        Ok(_child) => {
            let addrs = load_addrs();
            println!();
            println!("  {}  {}  {}",
                brand_green("◆"),
                bold_white("shunt"),
                bold_white("started"));
            for (provider, addr) in &addrs {
                let label = format!("{provider:<12}");
                println!("  {}  {}  {}", dim("·"), dim(&label), cyan(addr));
            }
            println!("  {}  run {} for account details",
                dim("·"), cyan("shunt status"));
            println!();
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Warning: could not daemonize ({e}), starting in foreground");
            // fall through — tokio will start and run normally
        }
    }
}

/// Returns `(provider_label, url)` for each provider found in the config.
/// Falls back to just the Anthropic default if the config can't be loaded.
fn load_addrs() -> Vec<(String, String)> {
    use shunt::provider::Provider;

    let Ok(cfg) = shunt::config::load_config(None) else {
        return vec![("anthropic".into(), "http://127.0.0.1:8082".into())];
    };

    let host = &cfg.server.host;
    let primary_port = cfg.server.port;

    use std::collections::BTreeSet;
    let providers: BTreeSet<String> = cfg.accounts.iter()
        .map(|a| a.provider.to_string())
        .collect();

    providers.into_iter().map(|p| {
        let port = match Provider::from_str(&p) {
            Provider::Anthropic => primary_port,
            other => other.default_port(),
        };
        (p, format!("http://{host}:{port}"))
    }).collect()
}
