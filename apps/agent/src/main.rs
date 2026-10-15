use std::time::Instant;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() == 1 && arguments[0] == "--status" {
        if let Err(error) = print_agent_status() {
            eprintln!("RustBackup Agent status unavailable: {error}");
            std::process::exit(1);
        }
        return;
    }
    if !arguments.is_empty() {
        eprintln!("Usage: rustbackup-agent [--status]");
        std::process::exit(2);
    }

    let started = Instant::now();
    match run_agent(started) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("RustBackup Agent stopped: {error}");
            std::process::exit(1);
        }
    }
}

fn print_agent_status() -> Result<(), String> {
    let status = backup_ipc::query_agent_status().map_err(|error| error.to_string())?;
    println!(
        "Agent is online ({:?}); uptime {}s; active tasks {}",
        status.state, status.uptime_seconds, status.active_tasks
    );
    Ok(())
}

#[cfg(windows)]
fn run_agent(started: Instant) -> Result<(), String> {
    let Some(_instance) =
        backup_ipc::acquire_agent_instance().map_err(|error| error.to_string())?
    else {
        println!("RustBackup Agent is already running for this Windows user.");
        return Ok(());
    };
    backup_ipc::serve_status_forever(started).map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn run_agent(_started: Instant) -> Result<(), String> {
    Err("RustBackup Agent is supported only on Windows".to_owned())
}
