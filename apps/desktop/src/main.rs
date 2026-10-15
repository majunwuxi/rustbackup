use std::fs;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use backup_core::{
    expand_supported_variables, load_config, parse_config_json, save_config_atomic,
    validate_config, validate_task_paths, AppConfig, ApplicationConfig, BackupMode, BackupTask,
    ConfigError,
};
use backup_storage::{HistoryRepository, StateStore};
use serde::Serialize;
use tauri::{Manager, WindowEvent};

const DEFAULT_TASK_ID: &str = "work-documents";
const DEFAULT_CONFIG: &str = include_str!("../../../config/work-documents.json");
static NEXT_TASK_SUFFIX: AtomicU64 = AtomicU64::new(1);

#[cfg(windows)]
const GOOGLE_OAUTH_CLIENT_ID: &str = match option_env!("RUSTBACKUP_GOOGLE_OAUTH_CLIENT_ID") {
    Some(client_id) if !client_id.is_empty() => client_id,
    Some(_) => "",
    None => "647224236237-f4on9hiol9mf7jbnm365bbv0hvbqvbbf.apps.googleusercontent.com",
};
#[cfg(windows)]
const GOOGLE_OAUTH_CLIENT_SECRET: Option<&str> =
    option_env!("RUSTBACKUP_GOOGLE_OAUTH_CLIENT_SECRET");
#[cfg(windows)]
const GOOGLE_OAUTH_ENVIRONMENT: Option<&str> = option_env!("RUSTBACKUP_GOOGLE_OAUTH_ENVIRONMENT");

#[derive(Debug, Serialize)]
struct GoogleDriveConnectionStatus {
    connected: bool,
    email: Option<String>,
    client_secret_configured: bool,
    test_environment: bool,
}

#[derive(Debug, Serialize)]
struct BackupTaskView {
    task: BackupTask,
    recent_runs: Vec<BackupRunView>,
    manual_runs: Vec<ManualRunView>,
}

#[derive(Debug, Serialize)]
struct BackupRunView {
    run_id: String,
    started_at: String,
    finished_at: Option<String>,
    status: String,
    scanned_files: u64,
    changed_files: u64,
    error_summary: Option<String>,
}

#[derive(Debug, Serialize)]
struct ManualRunView {
    run_id: String,
    requested_at: String,
    status: String,
    result_status: Option<String>,
    error_summary: Option<String>,
}

#[derive(Debug, Serialize)]
struct HistoryEntry {
    version_id: String,
    storage_target: String,
    display_relative_path: String,
    created_at_unix_ms: i64,
    size_bytes: u64,
    is_latest: bool,
}

#[derive(Debug, serde::Deserialize, Serialize)]
struct AgentLogLine {
    timestamp_unix_ms: u128,
    task_id: Option<String>,
    level: String,
    message: String,
}

#[tauri::command]
async fn agent_status() -> Result<backup_ipc::AgentStatus, String> {
    tauri::async_runtime::spawn_blocking(backup_ipc::query_agent_status)
        .await
        .map_err(|_| "Agent 状态查询任务意外结束".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn list_backup_tasks() -> Result<Vec<BackupTaskView>, String> {
    let (_, config) = load_or_initialize_config()?;
    let store =
        StateStore::open_shared(&state_database_path()?).map_err(|error| error.to_string())?;
    config
        .tasks
        .into_iter()
        .map(|task| {
            let recent_runs = store
                .recent_backup_runs(&task.id, 5)
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|run| BackupRunView {
                    run_id: run.run_id,
                    started_at: run.started_at,
                    finished_at: run.finished_at,
                    status: run.status,
                    scanned_files: run.scanned_files,
                    changed_files: run.changed_files,
                    error_summary: run.error_summary,
                })
                .collect();
            let manual_runs = store
                .recent_task_runs(&task.id, 5)
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|run| ManualRunView {
                    run_id: run.run_id,
                    requested_at: run.requested_at,
                    status: run.status,
                    result_status: run.result_status,
                    error_summary: run.error_summary,
                })
                .collect();
            Ok(BackupTaskView {
                task,
                recent_runs,
                manual_runs,
            })
        })
        .collect()
}

#[tauri::command]
fn create_backup_task() -> Result<BackupTask, String> {
    let (config_path, mut config) = load_or_initialize_config()?;
    let template_config = parse_config_json(DEFAULT_CONFIG).map_err(config_error_text)?;
    let mut task = template_config
        .tasks
        .into_iter()
        .find(|task| task.id == DEFAULT_TASK_ID)
        .ok_or_else(|| "RustBackup 默认任务模板缺失".to_owned())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "系统时钟无效".to_owned())?
        .as_millis();
    loop {
        let suffix = NEXT_TASK_SUFFIX.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("task-{now}-{suffix}");
        if !config.tasks.iter().any(|existing| existing.id == candidate) {
            task.id = candidate;
            break;
        }
    }
    task.name = "新建备份任务".to_owned();
    task.enabled = false;
    task.source.path = PathBuf::new();
    task.mode = BackupMode::Local;
    task.destinations.google_drive.enabled = false;
    task.destinations.google_drive.account_id = default_task(&config)?
        .destinations
        .google_drive
        .account_id
        .clone();
    task.destinations.google_drive.folder_id = None;
    task.destinations.google_drive.folder_name = format!("RustBackup/{}", task.id);
    config.tasks.push(task.clone());
    validate_config(&config).map_err(config_error_text)?;
    save_config_atomic(&config_path, &config).map_err(config_error_text)?;
    Ok(task)
}

#[tauri::command]
fn save_backup_task(task: BackupTask) -> Result<(), String> {
    let (config_path, mut config) = load_or_initialize_config()?;
    let existing = config
        .tasks
        .iter()
        .find(|existing| existing.id == task.id)
        .ok_or_else(|| "备份任务不存在；请刷新任务列表后重试".to_owned())?;
    if task.enabled != existing.enabled {
        return Err("请使用单独的启用、暂停或恢复操作更改任务状态".to_owned());
    }
    if existing.destinations.google_drive.account_id != task.destinations.google_drive.account_id
        || existing.destinations.google_drive.folder_id != task.destinations.google_drive.folder_id
    {
        return Err("不能在编辑任务时静默更换 Google 账号或云端目录绑定".to_owned());
    }
    config.tasks.retain(|existing| existing.id != task.id);
    config.tasks.push(task);
    validate_config(&config).map_err(config_error_text)?;
    save_config_atomic(&config_path, &config).map_err(config_error_text)
}

#[tauri::command]
fn set_backup_task_enabled(task_id: String, enabled: bool, confirmed: bool) -> Result<(), String> {
    let (config_path, mut config) = load_or_initialize_config()?;
    let task = config
        .tasks
        .iter_mut()
        .find(|task| task.id == task_id)
        .ok_or_else(|| "备份任务不存在；请刷新任务列表后重试".to_owned())?;
    if enabled && !confirmed {
        return Err("启用任务前必须确认扫描范围和目标目录".to_owned());
    }
    task.enabled = enabled;
    if enabled {
        validate_config(&config).map_err(config_error_text)?;
        let paths = validate_task_paths(
            config
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| "备份任务不存在".to_owned())?,
        )
        .map_err(|error| error.to_string())?;
        if let Some(destination) = paths.local_destination.as_ref() {
            fs::create_dir_all(destination).map_err(|error| {
                format!(
                    "无法创建已确认的本地备份目标 {}：{error}",
                    destination.display()
                )
            })?;
        }
        let task = config
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .ok_or_else(|| "备份任务不存在".to_owned())?;
        validate_task_paths(task).map_err(|error| error.to_string())?;
    }
    save_config_atomic(&config_path, &config).map_err(config_error_text)
}

#[tauri::command]
fn queue_backup_task(task_id: String) -> Result<ManualRunView, String> {
    let (_, config) = load_or_initialize_config()?;
    let task = config
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| "备份任务不存在；请刷新任务列表后重试".to_owned())?;
    if !task.enabled {
        return Err("请先启用任务，再将单次备份加入 Agent 队列".to_owned());
    }
    let store =
        StateStore::open_shared(&state_database_path()?).map_err(|error| error.to_string())?;
    let run = store
        .enqueue_task_run(&task.id, &task.name)
        .map_err(|error| error.to_string())?;
    Ok(ManualRunView {
        run_id: run.run_id,
        requested_at: run.requested_at,
        status: run.status,
        result_status: run.result_status,
        error_summary: run.error_summary,
    })
}

#[tauri::command]
fn list_backup_history(task_id: String) -> Result<Vec<HistoryEntry>, String> {
    let (_, config) = load_or_initialize_config()?;
    let task = config
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| "备份任务不存在；请刷新任务列表后重试".to_owned())?;
    let store =
        StateStore::open_shared(&state_database_path()?).map_err(|error| error.to_string())?;
    let mut history = Vec::new();

    if task.destinations.local.enabled {
        let destination = expand_supported_variables(&task.destinations.local.path)
            .map_err(|error| error.to_string())?;
        let parent = destination.parent().unwrap_or(&destination);
        let repository_path = parent.join(".rustbackup").join(&task.id);
        if repository_path.exists() {
            let repository = HistoryRepository::open_existing(&repository_path)
                .map_err(|error| error.to_string())?;
            history.extend(
                repository
                    .list_recent_task_versions(&store, &task.id, 100)
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .map(|version| HistoryEntry {
                        version_id: version.version_id,
                        storage_target: "local".to_owned(),
                        display_relative_path: version.display_relative_path,
                        created_at_unix_ms: version.created_at_unix_ms,
                        size_bytes: version.size_bytes,
                        is_latest: version.is_latest,
                    }),
            );
        }
    }
    history.extend(
        store
            .list_recent_drive_history_versions(&task.id, 100)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|version| HistoryEntry {
                version_id: version.manifest_id,
                storage_target: "google_drive".to_owned(),
                display_relative_path: version.display_relative_path,
                created_at_unix_ms: version.created_at_unix_ms,
                size_bytes: version.size_bytes,
                is_latest: false,
            }),
    );
    history.sort_by_key(|entry| std::cmp::Reverse(entry.created_at_unix_ms));
    history.truncate(200);
    Ok(history)
}

#[tauri::command]
fn restore_backup_history_version(
    task_id: String,
    version_id: String,
    storage_target: String,
    destination_parent: PathBuf,
) -> Result<String, String> {
    let (_, config) = load_or_initialize_config()?;
    let task = config
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| "备份任务不存在；请刷新任务列表后重试".to_owned())?;
    let destination = new_restore_directory(&destination_parent)?;
    let mut store =
        StateStore::open_shared(&state_database_path()?).map_err(|error| error.to_string())?;

    match storage_target.as_str() {
        "local" => {
            let local_destination = expand_supported_variables(&task.destinations.local.path)
                .map_err(|error| error.to_string())?;
            let repository_parent = local_destination.parent().unwrap_or(&local_destination);
            let repository_path = repository_parent.join(".rustbackup").join(&task.id);
            let repository = HistoryRepository::open_existing(&repository_path)
                .map_err(|error| error.to_string())?;
            repository
                .restore_version(&mut store, &version_id, &destination, false)
                .map_err(|error| error.to_string())?;
        }
        "google_drive" => {
            #[cfg(windows)]
            {
                let drive = build_drive_client(task)?
                    .ok_or_else(|| "此任务没有已授权的 Google Drive 账号".to_owned())?;
                backup_local::restore_drive_history_version(
                    &mut store,
                    &drive,
                    &task.id,
                    &version_id,
                    &destination,
                )
                .map_err(|error| error.to_string())?;
            }
            #[cfg(not(windows))]
            return Err("Google Drive 历史恢复仅支持 Windows".to_owned());
        }
        _ => return Err("历史存储目标无效".to_owned()),
    }
    Ok(destination.to_string_lossy().into_owned())
}

#[tauri::command]
fn get_application_settings() -> Result<ApplicationConfig, String> {
    let (_, config) = load_or_initialize_config()?;
    Ok(config.application)
}

#[tauri::command]
fn save_application_settings(settings: ApplicationConfig) -> Result<(), String> {
    let (config_path, mut config) = load_or_initialize_config()?;
    let original_config = config.clone();
    let original_startup = config.application.start_with_windows;
    config.application = settings;
    validate_config(&config).map_err(config_error_text)?;
    save_config_atomic(&config_path, &config).map_err(config_error_text)?;
    if let Err(registration_error) =
        sync_agent_startup_registration(config.application.start_with_windows)
    {
        let config_rollback = save_config_atomic(&config_path, &original_config)
            .map_err(config_error_text)
            .err();
        let startup_rollback = sync_agent_startup_registration(original_startup).err();
        let rollback_errors = [config_rollback, startup_rollback]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if rollback_errors.is_empty() {
            return Err(registration_error);
        }
        return Err(format!(
            "{registration_error}; 设置回滚也未完全成功：{}",
            rollback_errors.join("；")
        ));
    }
    Ok(())
}

#[tauri::command]
fn get_recent_agent_logs() -> Result<Vec<AgentLogLine>, String> {
    let path = agent_log_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(path).map_err(|error| format!("无法读取 Agent 日志：{error}"))?;
    Ok(content
        .lines()
        .rev()
        .take(100)
        .filter_map(|line| serde_json::from_str::<AgentLogLine>(line).ok())
        .collect())
}

#[tauri::command]
fn google_drive_connection_status() -> Result<GoogleDriveConnectionStatus, String> {
    let (_, config) = load_or_initialize_config()?;
    let task = default_task(&config)?;
    Ok(GoogleDriveConnectionStatus {
        connected: task.destinations.google_drive.account_id.is_some(),
        email: None,
        client_secret_configured: google_oauth_client_secret_configured()?,
        test_environment: google_oauth_test_environment(),
    })
}

fn google_oauth_test_environment() -> bool {
    #[cfg(windows)]
    {
        GOOGLE_OAUTH_ENVIRONMENT != Some("production")
    }

    #[cfg(not(windows))]
    {
        true
    }
}

fn google_oauth_client_secret_configured() -> Result<bool, String> {
    #[cfg(windows)]
    {
        if let Some(secret) = GOOGLE_OAUTH_CLIENT_SECRET {
            validate_google_oauth_client_secret(secret)
                .map_err(|_| "OAuth 发布构建配置无效".to_owned())?;
            return Ok(true);
        }

        use backup_drive::{CredentialStore, PlatformCredentialStore};

        let credentials =
            PlatformCredentialStore::new().map_err(|_| "Windows 凭据管理器不可用".to_owned())?;
        credentials
            .get(&google_oauth_client_secret_key())
            .map(|secret| secret.is_some())
            .map_err(|_| "无法读取 Windows 凭据管理器".to_owned())
    }

    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

#[cfg(windows)]
fn google_oauth_client_secret_key() -> String {
    format!("oauth-client-secret:{GOOGLE_OAUTH_CLIENT_ID}")
}

fn validate_google_oauth_client_secret(secret: &str) -> Result<(), String> {
    if secret.is_empty()
        || secret.trim() != secret
        || secret.len() > 1024
        || secret.chars().any(char::is_control)
    {
        return Err(
            "客户端密钥无效；请粘贴 Google Cloud 中的 OAuth Desktop 客户端密钥。".to_owned(),
        );
    }
    Ok(())
}

#[tauri::command]
fn store_google_oauth_client_secret(secret: String) -> Result<(), String> {
    validate_google_oauth_client_secret(&secret)?;

    #[cfg(windows)]
    {
        use backup_drive::{CredentialStore, PlatformCredentialStore};

        let credentials =
            PlatformCredentialStore::new().map_err(|_| "Windows 凭据管理器不可用".to_owned())?;
        credentials
            .put(&google_oauth_client_secret_key(), &secret)
            .map_err(|_| "无法保存到 Windows 凭据管理器".to_owned())
    }

    #[cfg(not(windows))]
    {
        let _ = secret;
        Err("Google Drive 授权仅支持 Windows".to_owned())
    }
}

#[tauri::command]
async fn connect_google_drive(
    app: tauri::AppHandle,
) -> Result<GoogleDriveConnectionStatus, String> {
    #[cfg(windows)]
    {
        connect_google_drive_windows(app).await
    }

    #[cfg(not(windows))]
    {
        let _ = app;
        Err("Google Drive authorization is supported only on Windows".to_owned())
    }
}

#[tauri::command]
async fn reauthorize_google_drive(
    app: tauri::AppHandle,
) -> Result<GoogleDriveConnectionStatus, String> {
    #[cfg(windows)]
    {
        clear_google_drive_local_auth()?;
        connect_google_drive_windows(app).await
    }

    #[cfg(not(windows))]
    {
        let _ = app;
        Err("Google Drive authorization is supported only on Windows".to_owned())
    }
}

#[cfg(windows)]
async fn connect_google_drive_windows(
    app: tauri::AppHandle,
) -> Result<GoogleDriveConnectionStatus, String> {
    use std::sync::Arc;

    use backup_drive::{
        begin_authorization, CredentialStore, OAuthClient, OAuthEndpoints, PlatformCredentialStore,
    };
    use tauri_plugin_opener::OpenerExt;

    let (_, existing_config) = load_or_initialize_config()?;
    if default_task(&existing_config)?
        .destinations
        .google_drive
        .account_id
        .is_some()
    {
        return Err("此任务已绑定 Google 账号；当前版本不会自动切换绑定账号。".to_owned());
    }

    let platform_credentials =
        PlatformCredentialStore::new().map_err(|_| "Windows 凭据管理器不可用".to_owned())?;
    let client_secret = load_google_oauth_client_secret(&platform_credentials)?;
    let credentials: Arc<dyn CredentialStore> = Arc::new(platform_credentials);
    let oauth_endpoints = OAuthEndpoints::default();
    let oauth = Arc::new(
        OAuthClient::new(
            GOOGLE_OAUTH_CLIENT_ID,
            client_secret,
            oauth_endpoints.clone(),
            credentials,
        )
        .map_err(|error| error.to_string())?,
    );
    let (authorization_url, session) =
        begin_authorization(GOOGLE_OAUTH_CLIENT_ID, &oauth_endpoints)
            .map_err(|error| error.to_string())?;

    app.opener()
        .open_url(&authorization_url, None::<&str>)
        .map_err(|error| format!("无法打开系统浏览器：{error}"))?;

    let oauth_for_exchange = Arc::clone(&oauth);
    let identity = tauri::async_runtime::spawn_blocking(move || {
        let authorization = session.wait_for_code(Duration::from_secs(300))?;
        oauth_for_exchange.complete_authorization(authorization)
    })
    .await
    .map_err(|_| "Google 授权处理任务意外结束".to_owned())?
    .map_err(|error| error.to_string())?;

    let (config_path, mut config) = load_or_initialize_config()?;
    set_default_task_google_account(&mut config, &identity.subject)?;
    save_config_atomic(&config_path, &config).map_err(config_error_text)?;

    Ok(GoogleDriveConnectionStatus {
        connected: true,
        email: identity.email,
        client_secret_configured: true,
        test_environment: google_oauth_test_environment(),
    })
}

#[cfg(windows)]
fn load_google_oauth_client_secret(
    credentials: &dyn backup_drive::CredentialStore,
) -> Result<String, String> {
    match GOOGLE_OAUTH_CLIENT_SECRET {
        Some(secret) if validate_google_oauth_client_secret(secret).is_ok() => {
            Ok(secret.to_owned())
        }
        Some(_) => Err("此应用的 OAuth 构建配置无效。".to_owned()),
        None => credentials
            .get(&google_oauth_client_secret_key())
            .map_err(|_| "无法读取 Windows 凭据管理器".to_owned())?
            .ok_or_else(|| {
                "开发构建请先将 OAuth Desktop 客户端密钥保存到 Windows 凭据管理器。".to_owned()
            }),
    }
}

#[cfg(windows)]
fn clear_google_drive_local_auth() -> Result<(), String> {
    use std::sync::Arc;

    use backup_drive::{CredentialStore, OAuthClient, OAuthEndpoints, PlatformCredentialStore};

    let (config_path, mut config) = load_or_initialize_config()?;
    let account_id = default_task(&config)?
        .destinations
        .google_drive
        .account_id
        .clone();

    if let Some(account_id) = account_id {
        let platform_credentials =
            PlatformCredentialStore::new().map_err(|_| "Windows 凭据管理器不可用".to_owned())?;
        let client_secret = load_google_oauth_client_secret(&platform_credentials)?;
        let credentials: Arc<dyn CredentialStore> = Arc::new(platform_credentials);
        let oauth = OAuthClient::new(
            GOOGLE_OAUTH_CLIENT_ID,
            client_secret,
            OAuthEndpoints::default(),
            credentials,
        )
        .map_err(|error| error.to_string())?;
        oauth
            .revoke_local_credentials(&account_id)
            .map_err(|error| error.to_string())?;
    }

    clear_default_task_google_account(&mut config)?;
    save_config_atomic(&config_path, &config).map_err(config_error_text)
}

fn load_or_initialize_config() -> Result<(PathBuf, AppConfig), String> {
    let path = user_config_path()?;
    if path.exists() {
        let config = load_config(&path).map_err(config_error_text)?;
        return Ok((path, config));
    }

    let config = parse_config_json(DEFAULT_CONFIG).map_err(config_error_text)?;
    save_config_atomic(&path, &config).map_err(config_error_text)?;
    Ok((path, config))
}

fn user_config_path() -> Result<PathBuf, String> {
    let app_data = std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Windows APPDATA path is unavailable".to_owned())?;
    user_config_path_in(Path::new(&app_data))
}

fn user_config_path_in(app_data: &Path) -> Result<PathBuf, String> {
    if app_data.as_os_str().is_empty() {
        return Err("Windows APPDATA path is unavailable".to_owned());
    }
    Ok(app_data.join("RustBackup").join("config.json"))
}

fn state_database_path() -> Result<PathBuf, String> {
    let app_data = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Windows LOCALAPPDATA path is unavailable".to_owned())?;
    Ok(PathBuf::from(app_data).join("RustBackup").join("state.db"))
}

fn agent_log_path() -> Result<PathBuf, String> {
    Ok(state_database_path()?
        .parent()
        .ok_or_else(|| "RustBackup data directory is unavailable".to_owned())?
        .join("logs")
        .join("agent.log"))
}

fn new_restore_directory(parent: &Path) -> Result<PathBuf, String> {
    if !parent.is_absolute() {
        return Err("恢复位置必须是绝对路径".to_owned());
    }
    let metadata =
        fs::symlink_metadata(parent).map_err(|error| format!("无法访问选择的恢复位置：{error}"))?;
    if !metadata.is_dir() || is_reparse_point(&metadata) {
        return Err("恢复位置必须是已存在的普通目录，不能是重解析点".to_owned());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "系统时钟无效".to_owned())?
        .as_millis();
    for _ in 0..16 {
        let suffix = NEXT_TASK_SUFFIX.fetch_add(1, Ordering::Relaxed);
        let destination = parent.join(format!("RustBackup-Restore-{now}-{suffix}"));
        if !destination.exists() {
            return Ok(destination);
        }
    }
    Err("无法为恢复结果分配一个新的目录名称".to_owned())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn sync_agent_startup_registration(enabled: bool) -> Result<(), String> {
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "RustBackupAgent";
    if enabled {
        let executable = std::env::current_exe()
            .map_err(|error| format!("无法解析 RustBackup 程序路径：{error}"))?;
        let value = format!("\"{}\" --login-startup", executable.to_string_lossy());
        let output = ProcessCommand::new("reg.exe")
            .args([
                "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &value, "/f",
            ])
            .output()
            .map_err(|error| format!("无法设置当前用户登录启动：{error}"))?;
        if !output.status.success() {
            return Err(format!(
                "Windows 拒绝设置当前用户登录启动：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    } else {
        let query = ProcessCommand::new("reg.exe")
            .args(["query", RUN_KEY, "/v", VALUE_NAME])
            .output()
            .map_err(|error| format!("无法检查当前用户登录启动：{error}"))?;
        if !query.status.success() {
            return Ok(());
        }
        let output = ProcessCommand::new("reg.exe")
            .args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])
            .output()
            .map_err(|error| format!("无法移除当前用户登录启动：{error}"))?;
        if !output.status.success() {
            return Err(format!(
                "Windows 拒绝移除当前用户登录启动：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
fn sync_agent_startup_registration(enabled: bool) -> Result<(), String> {
    if enabled {
        Err("登录启动设置仅支持 Windows".to_owned())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn build_drive_client(task: &BackupTask) -> Result<Option<backup_drive::DriveClient>, String> {
    use std::sync::Arc;

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
    let client_secret = match GOOGLE_OAUTH_CLIENT_SECRET {
        Some(secret) if !secret.is_empty() => secret.to_owned(),
        _ => platform_credentials
            .get(&client_secret_key)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "OAuth Desktop 客户端密钥尚未安全保存".to_owned())?,
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

#[cfg(not(windows))]
fn build_drive_client(_task: &BackupTask) -> Result<Option<backup_drive::DriveClient>, String> {
    Ok(None)
}

fn default_task(config: &AppConfig) -> Result<&BackupTask, String> {
    config
        .tasks
        .iter()
        .find(|task| task.id == DEFAULT_TASK_ID)
        .ok_or_else(|| "RustBackup default task is missing from the configuration".to_owned())
}

fn set_default_task_google_account(config: &mut AppConfig, account_id: &str) -> Result<(), String> {
    if account_id.is_empty() || account_id.len() > 256 || account_id.chars().any(char::is_control) {
        return Err("Google returned an invalid account identifier".to_owned());
    }

    let task = config
        .tasks
        .iter_mut()
        .find(|task| task.id == DEFAULT_TASK_ID)
        .ok_or_else(|| "RustBackup default task is missing from the configuration".to_owned())?;

    match task.destinations.google_drive.account_id.as_deref() {
        None => {
            task.destinations.google_drive.account_id = Some(account_id.to_owned());
            Ok(())
        }
        Some(existing) if existing == account_id => Ok(()),
        Some(_) => Err("此任务已绑定其他 Google 账号，拒绝静默更换绑定。".to_owned()),
    }
}

fn clear_default_task_google_account(config: &mut AppConfig) -> Result<(), String> {
    let task = config
        .tasks
        .iter_mut()
        .find(|task| task.id == DEFAULT_TASK_ID)
        .ok_or_else(|| "RustBackup default task is missing from the configuration".to_owned())?;
    task.destinations.google_drive.account_id = None;
    Ok(())
}

fn config_error_text(error: ConfigError) -> String {
    error.to_string()
}

#[cfg(windows)]
fn start_agent_process(app: &tauri::AppHandle) -> Result<(), String> {
    if backup_ipc::query_agent_status().is_ok() {
        return Ok(());
    }
    use tauri_plugin_shell::ShellExt;

    match app.shell().sidecar("rustbackup-agent") {
        Ok(command) => match command.spawn() {
            Ok((_events, _child)) => Ok(()),
            Err(sidecar_error) => start_sibling_agent().map_err(|fallback_error| {
                format!("sidecar 启动失败：{sidecar_error}；备用启动也失败：{fallback_error}")
            }),
        },
        Err(sidecar_error) => start_sibling_agent().map_err(|fallback_error| {
            format!("sidecar 不可用：{sidecar_error}；备用启动也失败：{fallback_error}")
        }),
    }
}

#[cfg(windows)]
fn start_sibling_agent() -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    let desktop = std::env::current_exe().map_err(|error| error.to_string())?;
    let agent = desktop.with_file_name("rustbackup-agent.exe");
    if !agent.is_file() {
        return Err("安装目录或开发输出目录中找不到 rustbackup-agent.exe".to_owned());
    }
    ProcessCommand::new(agent)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn start_agent_process(_app: &tauri::AppHandle) -> Result<(), String> {
    Err("RustBackup Agent is supported only on Windows".to_owned())
}

fn install_system_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| std::io::Error::other("RustBackup tray icon is missing"))?;
    let open = MenuItem::with_id(app, "open", "打开 RustBackup", true, None::<&str>)?;
    let exit = MenuItem::with_id(app, "exit", "退出界面（后台备份继续）", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &exit])?;
    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main_window(app),
            "exit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn main() {
    #[cfg(windows)]
    if std::env::args().any(|argument| argument == "--clear-google-auth") {
        match clear_google_drive_local_auth() {
            Ok(()) => println!("已清除此设备保存的 Google 登录令牌和账号绑定。"),
            Err(error) => {
                eprintln!("清理 Google 登录状态失败：{error}");
                std::process::exit(1);
            }
        }
        return;
    }

    let started_at_login = std::env::args().any(|argument| argument == "--login-startup");
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .setup(move |app| {
            install_system_tray(app)?;
            let (_, config) = load_or_initialize_config().map_err(std::io::Error::other)?;
            if let Err(error) =
                sync_agent_startup_registration(config.application.start_with_windows)
            {
                eprintln!("RustBackup login startup setup failed: {error}");
            }
            if let Err(error) = start_agent_process(app.handle()) {
                eprintln!("RustBackup Agent startup failed: {error}");
            }
            if started_at_login && config.application.start_minimized {
                if let Some(window) = app.get_webview_window("main") {
                    window.hide()?;
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let should_hide = load_or_initialize_config()
                    .map(|(_, config)| config.application.minimize_to_tray)
                    .unwrap_or(true);
                if should_hide {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            agent_status,
            list_backup_tasks,
            create_backup_task,
            save_backup_task,
            set_backup_task_enabled,
            queue_backup_task,
            list_backup_history,
            restore_backup_history_version,
            get_application_settings,
            save_application_settings,
            get_recent_agent_logs,
            google_drive_connection_status,
            store_google_oauth_client_secret,
            connect_google_drive,
            reauthorize_google_drive
        ])
        .run(tauri::generate_context!())
        .expect("failed to start RustBackup desktop");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_binding_keeps_default_task_disabled_and_source_empty() {
        let mut config = parse_config_json(DEFAULT_CONFIG).expect("default config is valid");
        set_default_task_google_account(&mut config, "google-subject-123")
            .expect("account should be bound");

        let task = default_task(&config).expect("default task exists");
        assert_eq!(
            task.destinations.google_drive.account_id.as_deref(),
            Some("google-subject-123")
        );
        assert!(!task.enabled);
        assert!(task.source.path.as_os_str().is_empty());
    }

    #[test]
    fn account_binding_rejects_silent_account_replacement() {
        let mut config = parse_config_json(DEFAULT_CONFIG).expect("default config is valid");
        set_default_task_google_account(&mut config, "google-subject-one")
            .expect("first account should be bound");

        let error = set_default_task_google_account(&mut config, "google-subject-two")
            .expect_err("a different account must require an explicit rebind flow");

        assert!(error.contains("拒绝静默更换绑定"));
        assert_eq!(
            default_task(&config)
                .expect("default task exists")
                .destinations
                .google_drive
                .account_id
                .as_deref(),
            Some("google-subject-one")
        );
    }

    #[test]
    fn clearing_google_account_binding_keeps_backup_task_disabled_and_source_empty() {
        let mut config = parse_config_json(DEFAULT_CONFIG).expect("default config is valid");
        set_default_task_google_account(&mut config, "google-subject-123")
            .expect("account should be bound");

        clear_default_task_google_account(&mut config).expect("account binding should clear");

        let task = default_task(&config).expect("default task exists");
        assert!(task.destinations.google_drive.account_id.is_none());
        assert!(!task.enabled);
        assert!(task.source.path.as_os_str().is_empty());
    }

    #[test]
    fn account_binding_rejects_invalid_provider_values() {
        let mut config = parse_config_json(DEFAULT_CONFIG).expect("default config is valid");
        assert!(set_default_task_google_account(&mut config, "bad\nsubject").is_err());
        assert!(set_default_task_google_account(&mut config, &"x".repeat(257)).is_err());
    }

    #[test]
    fn user_config_path_is_scoped_to_rustbackup_app_data() {
        let app_data = Path::new(r"C:\Users\Test\AppData\Roaming");
        assert_eq!(
            user_config_path_in(app_data).expect("valid APPDATA path"),
            app_data.join("RustBackup").join("config.json")
        );
        assert!(user_config_path_in(Path::new("")).is_err());
    }

    #[test]
    fn client_secret_validation_rejects_empty_whitespace_control_and_long_values() {
        assert!(validate_google_oauth_client_secret("GOCSPX-example-secret").is_ok());
        assert!(validate_google_oauth_client_secret("").is_err());
        assert!(validate_google_oauth_client_secret(" leading-space").is_err());
        assert!(validate_google_oauth_client_secret("secret\nvalue").is_err());
        assert!(validate_google_oauth_client_secret(&"x".repeat(1025)).is_err());
    }
}
