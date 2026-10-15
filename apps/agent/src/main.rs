use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use backup_core::{load_config, parse_config_json, save_config_atomic, AppConfig, BackupTask};
use backup_local::{run_scheduled_with_drive, ManualRunCompletion, SchedulerReport};
use backup_storage::StateStore;
use serde::Serialize;

const DEFAULT_CONFIG: &str = include_str!("../../../config/work-documents.json");
const SUPERVISOR_POLL: Duration = Duration::from_millis(300);
const FAILED_WORKER_RETRY: Duration = Duration::from_secs(15);
const GOOGLE_OAUTH_CLIENT_ID: &str = match option_env!("RUSTBACKUP_GOOGLE_OAUTH_CLIENT_ID") {
    Some(client_id) if !client_id.is_empty() => client_id,
    _ => "647224236237-f4on9hiol9mf7jbnm365bbv0hvbqvbbf.apps.googleusercontent.com",
};

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

    match run_agent(Instant::now()) {
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
        "RustBackup Agent is online ({:?}); uptime {}s; active tasks {}",
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

    let active_tasks = Arc::new(AtomicU32::new(0));
    let supervisor_active_tasks = Arc::clone(&active_tasks);
    thread::Builder::new()
        .name("rustbackup-task-supervisor".to_owned())
        .spawn(move || run_task_supervisor(supervisor_active_tasks))
        .map_err(|error| format!("could not start the task supervisor: {error}"))?;

    backup_ipc::serve_status_forever_with_active_tasks(started, active_tasks)
        .map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn run_agent(_started: Instant) -> Result<(), String> {
    Err("RustBackup Agent is supported only on Windows".to_owned())
}

#[derive(Debug, Serialize)]
struct AgentLogEvent<'a> {
    timestamp_unix_ms: u128,
    task_id: Option<&'a str>,
    level: &'a str,
    message: &'a str,
}

struct Worker {
    task_id: String,
    fingerprint: String,
    shutdown: Sender<()>,
    manual_runs: Sender<String>,
    completed_manual_runs: Receiver<ManualRunCompletion>,
    join: JoinHandle<()>,
    stopping: bool,
}

#[cfg(windows)]
fn run_task_supervisor(active_tasks: Arc<AtomicU32>) {
    let paths = match AgentPaths::from_environment() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("RustBackup Agent cannot locate per-user data directories: {error}");
            return;
        }
    };

    if let Err(error) = prepare_agent_state(&paths) {
        log_event(
            &paths,
            None,
            "error",
            &format!("Agent state initialization failed: {error}"),
        );
        return;
    }
    log_event(&paths, None, "info", "Agent task supervisor started");

    let mut store = match StateStore::open_shared(&paths.database) {
        Ok(store) => store,
        Err(error) => {
            log_event(
                &paths,
                None,
                "error",
                &format!("Could not open Agent database: {error}"),
            );
            return;
        }
    };
    let mut workers = HashMap::<String, Worker>::new();
    let mut retry_after = HashMap::<String, Instant>::new();
    let mut last_config_error = None::<String>;

    loop {
        let config = match load_or_initialize_config(&paths.config) {
            Ok(config) => {
                last_config_error = None;
                config
            }
            Err(error) => {
                if last_config_error.as_deref() != Some(error.as_str()) {
                    log_event(
                        &paths,
                        None,
                        "error",
                        &format!("Task configuration is invalid: {error}"),
                    );
                    last_config_error = Some(error);
                }
                active_tasks.store(0, Ordering::Relaxed);
                thread::sleep(SUPERVISOR_POLL);
                continue;
            }
        };

        process_completed_manual_runs(&paths, &store, &mut workers);
        supervise_workers(&paths, &config, &mut store, &mut workers, &mut retry_after);
        dispatch_queued_manual_runs(&paths, &store, &config, &workers);
        let active = workers
            .values()
            .filter(|worker| !worker.stopping && !worker.join.is_finished())
            .count();
        active_tasks.store(u32::try_from(active).unwrap_or(u32::MAX), Ordering::Relaxed);
        thread::sleep(SUPERVISOR_POLL);
    }
}

#[cfg(windows)]
struct AgentPaths {
    config: PathBuf,
    database: PathBuf,
    log: PathBuf,
}

#[cfg(windows)]
impl AgentPaths {
    fn from_environment() -> Result<Self, String> {
        let roaming = std::env::var_os("APPDATA")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "APPDATA is not set".to_owned())?;
        let local = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "LOCALAPPDATA is not set".to_owned())?;
        let roaming = PathBuf::from(roaming).join("RustBackup");
        let local = PathBuf::from(local).join("RustBackup");
        Ok(Self {
            config: roaming.join("config.json"),
            database: local.join("state.db"),
            log: local.join("logs").join("agent.log"),
        })
    }
}

#[cfg(windows)]
fn prepare_agent_state(paths: &AgentPaths) -> Result<(), String> {
    let config = load_or_initialize_config(&paths.config)?;
    let store = StateStore::open(&paths.database).map_err(|error| error.to_string())?;
    let recovered = store
        .recover_interrupted_task_runs()
        .map_err(|error| error.to_string())?;
    if recovered > 0 {
        log_event(
            paths,
            None,
            "warning",
            &format!("Requeued {recovered} interrupted manual backup request(s)"),
        );
    }
    for task in config.tasks {
        if let Err(error) = store.register_task(&task.id, &task.name, task.enabled) {
            return Err(format!("could not register task {}: {error}", task.id));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn load_or_initialize_config(path: &Path) -> Result<AppConfig, String> {
    if path.exists() {
        return load_config(path).map_err(|error| error.to_string());
    }
    let config = parse_config_json(DEFAULT_CONFIG).map_err(|error| error.to_string())?;
    save_config_atomic(path, &config).map_err(|error| error.to_string())?;
    Ok(config)
}

#[cfg(windows)]
fn supervise_workers(
    paths: &AgentPaths,
    config: &AppConfig,
    store: &mut StateStore,
    workers: &mut HashMap<String, Worker>,
    retry_after: &mut HashMap<String, Instant>,
) {
    let desired = config
        .tasks
        .iter()
        .map(|task| (task.id.as_str(), task))
        .collect::<HashMap<_, _>>();

    let worker_ids = workers.keys().cloned().collect::<Vec<_>>();
    for task_id in worker_ids {
        let Some(worker) = workers.get_mut(&task_id) else {
            continue;
        };
        let should_run = desired
            .get(task_id.as_str())
            .is_some_and(|task| task.enabled);
        let changed = should_run
            && desired
                .get(task_id.as_str())
                .and_then(|task| serde_json::to_string(task).ok())
                .as_deref()
                != Some(worker.fingerprint.as_str());
        if (!should_run || changed) && !worker.stopping {
            let _ = worker.shutdown.send(());
            worker.stopping = true;
            retry_after.remove(&task_id);
            log_event(
                paths,
                Some(&task_id),
                "info",
                "Task is stopping after a configuration change",
            );
        }

        if worker.join.is_finished() {
            let worker = workers.remove(&task_id).expect("worker id exists");
            if let Err(error) = worker.join.join() {
                log_event(
                    paths,
                    Some(&task_id),
                    "error",
                    &format!("Task worker panicked: {error:?}"),
                );
            }
            if let Err(error) = store.requeue_running_task_runs(&task_id) {
                log_event(
                    paths,
                    Some(&task_id),
                    "error",
                    &format!("Could not requeue interrupted work: {error}"),
                );
            }
            if should_run && !changed {
                retry_after.insert(task_id.clone(), Instant::now() + FAILED_WORKER_RETRY);
                log_event(
                    paths,
                    Some(&task_id),
                    "warning",
                    "Task worker stopped; retry is scheduled",
                );
            }
        }
    }

    for task in config.tasks.iter().filter(|task| task.enabled) {
        if workers.contains_key(&task.id)
            || retry_after
                .get(&task.id)
                .is_some_and(|deadline| Instant::now() < *deadline)
        {
            continue;
        }
        retry_after.remove(&task.id);
        match spawn_task_worker(paths, task) {
            Ok(worker) => {
                log_event(paths, Some(&task.id), "info", "Task worker started");
                workers.insert(task.id.clone(), worker);
            }
            Err(error) => {
                log_event(
                    paths,
                    Some(&task.id),
                    "error",
                    &format!("Could not start task: {error}"),
                );
                retry_after.insert(task.id.clone(), Instant::now() + FAILED_WORKER_RETRY);
            }
        }
    }
}

#[cfg(windows)]
fn spawn_task_worker(paths: &AgentPaths, task: &BackupTask) -> Result<Worker, String> {
    let fingerprint = serde_json::to_string(task).map_err(|error| error.to_string())?;
    let task = task.clone();
    let database = paths.database.clone();
    let log_path = paths.log.clone();
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let (manual_tx, manual_rx) = mpsc::channel();
    let (completed_tx, completed_rx) = mpsc::channel();
    let worker_task_id = task.id.clone();
    let join = thread::Builder::new()
        .name(format!("rustbackup-task-{}", task.id))
        .spawn(move || {
            let worker_paths = AgentPaths {
                config: PathBuf::new(),
                database,
                log: log_path,
            };
            run_task_worker(&worker_paths, task, shutdown_rx, manual_rx, completed_tx);
        })
        .map_err(|error| error.to_string())?;

    Ok(Worker {
        task_id: worker_task_id,
        fingerprint,
        shutdown: shutdown_tx,
        manual_runs: manual_tx,
        completed_manual_runs: completed_rx,
        join,
        stopping: false,
    })
}

#[cfg(windows)]
fn run_task_worker(
    paths: &AgentPaths,
    task: BackupTask,
    shutdown: Receiver<()>,
    manual_runs: Receiver<String>,
    completed_manual_runs: Sender<ManualRunCompletion>,
) {
    let mut store = match StateStore::open_shared(&paths.database) {
        Ok(store) => store,
        Err(error) => {
            log_event(
                paths,
                Some(&task.id),
                "error",
                &format!("Could not open task database: {error}"),
            );
            return;
        }
    };
    if let Err(error) = store.register_task(&task.id, &task.name, task.enabled) {
        log_event(
            paths,
            Some(&task.id),
            "error",
            &format!("Could not register task: {error}"),
        );
        return;
    }

    let drive = match build_drive_client(&task) {
        Ok(drive) => drive,
        Err(error) => {
            log_event(
                paths,
                Some(&task.id),
                "warning",
                &format!("Drive branch is waiting for authorization: {error}"),
            );
            None
        }
    };
    let mut report_changed = |report: &SchedulerReport| {
        let detail = report.last_backup_error.as_deref().unwrap_or_else(|| {
            report
                .last_backup_status
                .as_deref()
                .unwrap_or("scheduler ready")
        });
        let level = if report.last_backup_status.as_deref() == Some("succeeded") {
            "info"
        } else {
            "warning"
        };
        log_event(paths, Some(&task.id), level, detail);
    };
    if let Err(error) = run_scheduled_with_drive(
        &mut store,
        &task,
        &shutdown,
        drive.as_ref(),
        Some(&manual_runs),
        Some(&completed_manual_runs),
        Some(&mut report_changed),
    ) {
        log_event(
            paths,
            Some(&task.id),
            "error",
            &format!("Scheduler stopped: {error}"),
        );
    }
}

#[cfg(windows)]
fn dispatch_queued_manual_runs(
    paths: &AgentPaths,
    store: &StateStore,
    config: &AppConfig,
    workers: &HashMap<String, Worker>,
) {
    for task in config.tasks.iter().filter(|task| task.enabled) {
        let Some(worker) = workers.get(&task.id).filter(|worker| !worker.stopping) else {
            continue;
        };
        let queued = match store.queued_task_runs(&task.id) {
            Ok(queued) => queued,
            Err(error) => {
                log_event(
                    paths,
                    Some(&task.id),
                    "error",
                    &format!("Could not read task queue: {error}"),
                );
                continue;
            }
        };
        for request in queued {
            let claimed = match store.claim_task_run(&request.run_id) {
                Ok(claimed) => claimed,
                Err(error) => {
                    log_event(
                        paths,
                        Some(&task.id),
                        "error",
                        &format!("Could not claim queued backup: {error}"),
                    );
                    continue;
                }
            };
            if claimed && worker.manual_runs.send(request.run_id.clone()).is_err() {
                let _ = store.requeue_task_run(&request.run_id);
                log_event(
                    paths,
                    Some(&task.id),
                    "warning",
                    "Task stopped before the queued backup began",
                );
                break;
            }
        }
    }
}

#[cfg(windows)]
fn process_completed_manual_runs(
    paths: &AgentPaths,
    store: &StateStore,
    workers: &mut HashMap<String, Worker>,
) {
    for worker in workers.values_mut() {
        while let Ok(completion) = worker.completed_manual_runs.try_recv() {
            if let Err(error) = store.finish_task_run(
                &completion.run_id,
                &completion.status,
                completion.error_summary.as_deref(),
            ) {
                log_event(
                    paths,
                    Some(&worker.task_id),
                    "error",
                    &format!("Could not complete queued backup: {error}"),
                );
            }
        }
    }
}

#[cfg(windows)]
fn build_drive_client(task: &BackupTask) -> Result<Option<backup_drive::DriveClient>, String> {
    use backup_drive::{
        CredentialStore, DriveClient, DriveEndpoints, OAuthClient, OAuthEndpoints,
        PlatformCredentialStore,
    };

    if !task.destinations.google_drive.enabled {
        return Ok(None);
    }
    let Some(account_id) = task.destinations.google_drive.account_id.as_deref() else {
        return Ok(None);
    };
    let platform_credentials = PlatformCredentialStore::new().map_err(|error| error.to_string())?;
    let client_secret_key = format!("oauth-client-secret:{GOOGLE_OAUTH_CLIENT_ID}");
    let client_secret = match option_env!("RUSTBACKUP_GOOGLE_OAUTH_CLIENT_SECRET") {
        Some(secret) if !secret.is_empty() => secret.to_owned(),
        _ => platform_credentials
            .get(&client_secret_key)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "OAuth Desktop client secret is not configured".to_owned())?,
    };
    let credentials: Arc<dyn CredentialStore> = Arc::new(platform_credentials);
    let oauth = Arc::new(
        OAuthClient::new(
            GOOGLE_OAUTH_CLIENT_ID,
            client_secret,
            OAuthEndpoints::default(),
            Arc::clone(&credentials),
        )
        .map_err(|error| error.to_string())?,
    );
    DriveClient::new(oauth, credentials, account_id, DriveEndpoints::default())
        .map(Some)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn log_event(paths: &AgentPaths, task_id: Option<&str>, level: &str, message: &str) {
    let timestamp_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let safe_message = message
        .chars()
        .filter(|character| !character.is_control())
        .take(2048)
        .collect::<String>();
    let event = AgentLogEvent {
        timestamp_unix_ms,
        task_id,
        level,
        message: &safe_message,
    };
    let Ok(line) = serde_json::to_vec(&event) else {
        return;
    };
    if let Some(parent) = paths.log.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)
    {
        let _ = file.write_all(&line).and_then(|()| file.write_all(b"\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_fingerprint_excludes_no_task_configuration() {
        let mut config = parse_config_json(DEFAULT_CONFIG).unwrap();
        let task = config.tasks.first().unwrap().clone();
        let before = serde_json::to_string(&task).unwrap();
        config.tasks[0].name.push_str(" updated");
        let after = serde_json::to_string(&config.tasks[0]).unwrap();
        assert_ne!(before, after);
    }
}
