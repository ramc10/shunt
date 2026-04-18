fn main() -> anyhow::Result<()> {
    // Pre-flight: kill any existing shunt process BEFORE starting tokio.
    // Must be synchronous — no runtime, no async, no hangs possible.
    let args: Vec<String> = std::env::args().collect();
    let is_start = args.iter().any(|a| a == "start");
    let foreground = args.iter().any(|a| a == "--foreground");

    if is_start {
        preflight_kill();
        if !foreground {
            // Daemonize: fork, parent prints splash + exits, child runs server.
            daemonize();
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

/// Fork: parent prints a brief started message and exits (freeing the terminal).
/// Child redirects stdio and continues as the background server.
#[cfg(unix)]
fn daemonize() {
    // Load minimal config info for the parent's status line (best-effort)
    let addr = load_addr();

    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            // fork failed — continue in foreground
            return;
        }
        if pid > 0 {
            // Parent: print started message, return terminal to user, exit
            println!();
            println!("  shunt started  ·  {}", addr);
            println!("  shunt status   for live info");
            println!();
            std::process::exit(0);
        }
        // Child: detach from terminal, redirect stdio to /dev/null (logs go to file)
        libc::setsid();
        let devnull = libc::open(
            b"/dev/null\0".as_ptr() as *const libc::c_char,
            libc::O_RDWR,
        );
        if devnull >= 0 {
            libc::dup2(devnull, 0); // stdin
            libc::dup2(devnull, 1); // stdout
            libc::dup2(devnull, 2); // stderr
            libc::close(devnull);
        }
    }
}

#[cfg(not(unix))]
fn daemonize() {}

fn load_addr() -> String {
    if let Ok(cfg) = shunt::config::load_config(None) {
        format!("http://{}:{}", cfg.server.host, cfg.server.port)
    } else {
        "http://127.0.0.1:8082".into()
    }
}
