use std::path::{Path, PathBuf};
use std::time::Duration;

use backup_core::{
    load_config, parse_config_json, save_config_atomic, AppConfig, BackupTask, ConfigError,
};
use serde::Serialize;

const DEFAULT_TASK_ID: &str = "work-documents";
const DEFAULT_CONFIG: &str = include_str!("../../../config/work-documents.json");

#[cfg(windows)]
const GOOGLE_OAUTH_CLIENT_ID: &str =
    "647224236237-f4on9hiol9mf7jbnm365bbv0hvbqvbbf.apps.googleusercontent.com";

#[derive(Debug, Serialize)]
struct GoogleDriveConnectionStatus {
    connected: bool,
    email: Option<String>,
}

#[tauri::command]
fn google_drive_connection_status() -> Result<GoogleDriveConnectionStatus, String> {
    let (_, config) = load_or_initialize_config()?;
    let task = default_task(&config)?;
    Ok(GoogleDriveConnectionStatus {
        connected: task.destinations.google_drive.account_id.is_some(),
        email: None,
    })
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

    let credentials: Arc<dyn CredentialStore> =
        Arc::new(PlatformCredentialStore::new().map_err(|error| error.to_string())?);
    let oauth_endpoints = OAuthEndpoints::default();
    let oauth = Arc::new(
        OAuthClient::new(GOOGLE_OAUTH_CLIENT_ID, oauth_endpoints.clone(), credentials)
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
    })
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

fn config_error_text(error: ConfigError) -> String {
    error.to_string()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            google_drive_connection_status,
            connect_google_drive
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
}
