use std::error::Error;
use std::fs;
use std::io;

#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use backup_core::{parse_config_json, BackupMode};
#[cfg(windows)]
use backup_drive::{
    CredentialStore, DriveClient, DriveEndpoints, OAuthClient, OAuthEndpoints,
    PlatformCredentialStore,
};
#[cfg(windows)]
use backup_local::{backup_once_with_drive, restore_drive_history_version};
#[cfg(windows)]
use backup_storage::StateStore;

#[cfg(windows)]
const DEFAULT_TASK_ID: &str = "work-documents";
#[cfg(windows)]
const GOOGLE_OAUTH_CLIENT_ID: &str = match option_env!("RUSTBACKUP_GOOGLE_OAUTH_CLIENT_ID") {
    Some(client_id) if !client_id.is_empty() => client_id,
    _ => "647224236237-f4on9hiol9mf7jbnm365bbv0hvbqvbbf.apps.googleusercontent.com",
};
#[cfg(windows)]
const VERSION_ONE: &[u8] = b"RustBackup disposable Drive smoke test, version one.\n";
#[cfg(windows)]
const VERSION_TWO: &[u8] = b"RustBackup disposable Drive smoke test, version two after update.\n";

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(not(windows))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the live Drive smoke test is supported only on Windows",
    )
    .into());

    #[cfg(windows)]
    run_live_smoke()
}

#[cfg(windows)]
fn run_live_smoke() -> Result<(), Box<dyn Error>> {
    if option_env!("RUSTBACKUP_GOOGLE_OAUTH_ENVIRONMENT") == Some("production") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the live smoke test refuses to run against a production OAuth environment",
        )
        .into());
    }

    if !std::env::args().any(|argument| argument == "--confirm-live-drive-writes") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "this test creates a dedicated folder and disposable test files in the authorized Google Drive account; rerun with --confirm-live-drive-writes",
        )
        .into());
    }

    let account_id = connected_account_id()?;
    let platform_credentials = PlatformCredentialStore::new()?;
    let client_secret_key = format!("oauth-client-secret:{GOOGLE_OAUTH_CLIENT_ID}");
    let client_secret = match option_env!("RUSTBACKUP_GOOGLE_OAUTH_CLIENT_SECRET") {
        Some(secret) if !secret.is_empty() => secret.to_owned(),
        _ => platform_credentials
            .get(&client_secret_key)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "OAuth client secret is unavailable",
                )
            })?,
    };

    let credentials: Arc<dyn CredentialStore> = Arc::new(platform_credentials);
    let oauth = Arc::new(OAuthClient::new(
        GOOGLE_OAUTH_CLIENT_ID,
        client_secret,
        OAuthEndpoints::default(),
        Arc::clone(&credentials),
    )?);
    let drive = DriveClient::new(
        oauth,
        credentials,
        account_id.clone(),
        DriveEndpoints::default(),
    )?;
    drive.check_authorized()?;

    let (test_root, test_id) = create_test_root()?;
    let source = test_root.join("source");
    let local = test_root.join("local-mirror");
    let state_path = test_root.join("state.sqlite3");
    fs::create_dir(&source)?;
    fs::create_dir(&local)?;
    let sample_file = source.join("sample.txt");
    fs::write(&sample_file, VERSION_ONE)?;

    let mut config = parse_config_json(include_str!("../../../config/work-documents.json"))?;
    let mut task = config
        .tasks
        .drain(..)
        .find(|task| task.id == DEFAULT_TASK_ID)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "default task template is missing",
            )
        })?;
    let task_id = format!("drive-live-{test_id}");
    let folder_name = format!("RustBackup-Test-{test_id}");
    task.id = task_id.clone();
    task.name = folder_name.clone();
    task.enabled = true;
    task.mode = BackupMode::Dual;
    task.source.path = source;
    task.destinations.local.enabled = true;
    task.destinations.local.path = local;
    task.destinations.google_drive.enabled = true;
    task.destinations.google_drive.account_id = Some(account_id);
    task.destinations.google_drive.folder_id = None;
    task.destinations.google_drive.folder_name = folder_name.clone();

    let mut store = StateStore::open(&state_path)?;

    let first = backup_once_with_drive(&mut store, &task, Some(&drive))?;
    require_succeeded("initial upload", &first)?;
    if first.scanned_files != 1 || first.cloud_uploaded_files != 1 {
        return Err(test_failure(format!(
            "initial upload expected one scanned and uploaded file, got scanned={} uploaded={}",
            first.scanned_files, first.cloud_uploaded_files
        )));
    }

    let retry = backup_once_with_drive(&mut store, &task, Some(&drive))?;
    require_succeeded("unchanged retry", &retry)?;
    if retry.cloud_uploaded_files != 0 {
        return Err(test_failure(format!(
            "unchanged retry uploaded {} files instead of reusing committed state",
            retry.cloud_uploaded_files
        )));
    }

    fs::write(&sample_file, VERSION_TWO)?;
    let update = backup_once_with_drive(&mut store, &task, Some(&drive))?;
    require_succeeded("content update", &update)?;
    if update.cloud_uploaded_files != 1 || update.cloud_history_versions_created != 1 {
        return Err(test_failure(format!(
            "content update expected one upload and one history copy, got uploaded={} history={}",
            update.cloud_uploaded_files, update.cloud_history_versions_created
        )));
    }

    let indexed_files = store.indexed_files(&task_id)?;
    if indexed_files.len() != 1 {
        return Err(test_failure(format!(
            "expected one indexed smoke-test file, got {}",
            indexed_files.len()
        )));
    }
    let file_id = indexed_files[0].file_id;
    let current_file = drive
        .find_current_file(&task_id, file_id)?
        .ok_or_else(|| test_failure("updated Drive file was not found"))?;
    let binding = store
        .drive_task_binding(&task_id)?
        .ok_or_else(|| test_failure("Drive task folder binding was not persisted"))?;
    if !current_file
        .parents
        .iter()
        .any(|parent| parent == &binding.task_folder_id)
    {
        return Err(test_failure(
            "updated Drive file is outside the dedicated app-managed test folder",
        ));
    }

    let history = store.list_drive_history_versions(&task_id, file_id)?;
    if history.len() != 1 {
        return Err(test_failure(format!(
            "expected one recoverable prior version, got {}",
            history.len()
        )));
    }
    let restore_destination = test_root.join("restored-version-one");
    restore_drive_history_version(
        &mut store,
        &drive,
        &task_id,
        &history[0].manifest_id,
        &restore_destination,
    )?;
    let restored = fs::read(restore_destination.join("sample.txt"))?;
    if restored != VERSION_ONE {
        return Err(test_failure(
            "restored historical bytes do not match version one",
        ));
    }

    println!("Live Google Drive smoke test passed.");
    println!("Temporary local test root: {}", test_root.display());
    println!("Dedicated Drive task folder: {folder_name}");
    println!("Results: first upload=1, unchanged retry=0 duplicate uploads, update=1 with history, historical restore verified.");
    println!("The disposable Drive test files were left in that dedicated folder for inspection.");
    Ok(())
}

#[cfg(windows)]
fn connected_account_id() -> Result<String, Box<dyn Error>> {
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(user_config_path()?)?)?;
    value["tasks"]
        .as_array()
        .and_then(|tasks| {
            tasks
                .iter()
                .find(|task| task["id"].as_str() == Some(DEFAULT_TASK_ID))
        })
        .and_then(|task| task["destinations"]["google_drive"]["account_id"].as_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "RustBackup has no connected Google account; connect it in the desktop app first",
            )
            .into()
        })
}

#[cfg(windows)]
fn user_config_path() -> Result<PathBuf, Box<dyn Error>> {
    let app_data = std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Windows APPDATA is unavailable"))?;
    Ok(Path::new(&app_data).join("RustBackup").join("config.json"))
}

#[cfg(windows)]
fn create_test_root() -> Result<(PathBuf, String), Box<dyn Error>> {
    let test_base = std::env::temp_dir().join("RustBackupTests");
    fs::create_dir_all(&test_base)?;
    let canonical_test_base = fs::canonicalize(&test_base)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let test_id = format!("{timestamp}-{}", std::process::id());
    let test_root = test_base.join(format!("drive-{test_id}"));
    fs::create_dir(&test_root)?;
    let canonical_test_root = fs::canonicalize(&test_root)?;
    if !canonical_test_root.starts_with(&canonical_test_base) {
        return Err(test_failure(
            "generated test path escaped %TEMP%\\RustBackupTests",
        ));
    }
    Ok((test_root, test_id))
}

#[cfg(windows)]
fn require_succeeded(
    stage: &str,
    summary: &backup_local::BackupSummary,
) -> Result<(), Box<dyn Error>> {
    if summary.status != "succeeded" {
        return Err(test_failure(format!(
            "{stage} ended with status {}; issues: {}",
            summary.status,
            summary.issues.join("; ")
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn test_failure(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::other(message.into()).into()
}
