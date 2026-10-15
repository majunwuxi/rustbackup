use std::fs;
use std::path::{Component, Path, PathBuf};

use backup_drive::{DriveClient, DriveError};
use backup_storage::{DriveRestoreReceipt, StateStore, StorageError};
use tempfile::Builder;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DriveRestoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Drive(#[from] DriveError),
    #[error("cloud history version belongs to another Google account")]
    WrongAccount,
    #[error("restore destination must be an absolute, new directory beside an existing directory")]
    InvalidDestination,
    #[error("cloud history contains an unsafe relative path")]
    UnsafeRelativePath,
    #[error("restore filesystem operation failed")]
    Io(#[from] std::io::Error),
}

/// Restores one cloud history manifest into a new directory. It validates the
/// downloaded bytes before publishing the directory and records an audit row.
pub fn restore_drive_history_version(
    store: &mut StateStore,
    drive: &DriveClient,
    task_id: &str,
    manifest_id: &str,
    destination_directory: &Path,
) -> Result<DriveRestoreReceipt, DriveRestoreError> {
    let version = store
        .drive_history_version(task_id, manifest_id)?
        .ok_or_else(|| StorageError::VersionNotRecoverable(manifest_id.to_owned()))?;
    if version.account_id != drive.account_id() {
        return Err(DriveRestoreError::WrongAccount);
    }
    let relative = safe_relative_file(&version.display_relative_path)?;
    let parent = destination_directory
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or(DriveRestoreError::InvalidDestination)?;
    if !destination_directory.is_absolute() || fs::symlink_metadata(destination_directory).is_ok() {
        return Err(DriveRestoreError::InvalidDestination);
    }
    let parent_metadata =
        fs::symlink_metadata(parent).map_err(|_| DriveRestoreError::InvalidDestination)?;
    if !parent_metadata.is_dir() || super::is_reparse_point(&parent_metadata) {
        return Err(DriveRestoreError::InvalidDestination);
    }
    for ancestor in parent.ancestors() {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|_| DriveRestoreError::InvalidDestination)?;
        if super::is_reparse_point(&metadata) {
            return Err(DriveRestoreError::InvalidDestination);
        }
    }
    let parent = fs::canonicalize(parent).map_err(|_| DriveRestoreError::InvalidDestination)?;
    let destination_name = destination_directory
        .file_name()
        .ok_or(DriveRestoreError::InvalidDestination)?;
    let destination = parent.join(destination_name);
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(DriveRestoreError::InvalidDestination);
    }

    let audit = store.begin_drive_restore(task_id, manifest_id, &destination)?;
    let staged = Builder::new()
        .prefix(".rustbackup-restore-")
        .tempdir_in(&parent)?;
    let staged_file = staged.path().join(&relative);
    let staged_parent = staged_file
        .parent()
        .ok_or(DriveRestoreError::UnsafeRelativePath)?;
    fs::create_dir_all(staged_parent)?;

    let result = (|| -> Result<(), DriveRestoreError> {
        drive.download_verified_to(
            &version.drive_file_id,
            &version.content_hash,
            version.size_bytes,
            &staged_file,
        )?;
        fs::rename(staged.path(), &destination)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            store.finish_drive_restore(&audit.restore_id, true, None)?;
            Ok(audit)
        }
        Err(error) => {
            let _ = store.finish_drive_restore(
                &audit.restore_id,
                false,
                Some("cloud download verification or directory publish failed"),
            );
            Err(error)
        }
    }
}

fn safe_relative_file(value: &str) -> Result<PathBuf, DriveRestoreError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            let Component::Normal(name) = component else {
                return true;
            };
            let name = name.to_string_lossy();
            name.contains(':') || name.ends_with('.') || name.ends_with(' ')
        })
        || value.chars().any(char::is_control)
    {
        return Err(DriveRestoreError::UnsafeRelativePath);
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use backup_drive::{CredentialStore, DriveEndpoints, OAuthClient, OAuthEndpoints, OAuthError};
    use backup_storage::{DriveHistoryRecord, DriveTaskBinding, FileObservation};

    use super::*;

    #[derive(Default)]
    struct MemoryCredentials(Mutex<HashMap<String, String>>);

    impl CredentialStore for MemoryCredentials {
        fn get(&self, key: &str) -> Result<Option<String>, OAuthError> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        fn put(&self, key: &str, value: &str) -> Result<(), OAuthError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.to_owned());
            Ok(())
        }

        fn remove(&self, key: &str) -> Result<(), OAuthError> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn cloud_restore_refuses_a_different_bound_account() {
        let mut store = StateStore::open_in_memory().unwrap();
        store.register_task("task", "test", true).unwrap();
        let run = store.begin_backup_run("task").unwrap();
        let hash = blake3::hash(b"cloud version").to_hex().to_string();
        let file = store
            .record_file_seen(
                "task",
                &run,
                FileObservation {
                    relative_path_key: "file.txt",
                    display_relative_path: "file.txt",
                    size_bytes: b"cloud version".len() as u64,
                    modified_at_unix_ms: 1,
                    blake3_hex: &hash,
                },
            )
            .unwrap();
        store
            .bind_drive_task(&DriveTaskBinding {
                task_id: "task".to_owned(),
                account_id: "account-one".to_owned(),
                root_folder_id: "root-1".to_owned(),
                task_folder_id: "task-1".to_owned(),
                history_folder_id: "history-1".to_owned(),
                folder_name: "Work".to_owned(),
            })
            .unwrap();
        store
            .record_drive_history_version(DriveHistoryRecord {
                task_id: "task",
                file_id: file.file_id,
                source_revision: 1,
                drive_file_id: "remote-history-1",
                account_id: "account-one",
                relative_path_key: "file.txt",
                display_relative_path: "file.txt",
                content_hash: &hash,
                size_bytes: b"cloud version".len() as u64,
                created_at_unix_ms: 100,
            })
            .unwrap();
        let manifest_id = format!("drive:{}:1", file.file_id);
        let credentials = Arc::new(MemoryCredentials::default());
        let credential_store: Arc<dyn CredentialStore> = credentials;
        let oauth = Arc::new(
            OAuthClient::new(
                "desktop-client",
                "test-client-secret",
                OAuthEndpoints::default(),
                credential_store.clone(),
            )
            .unwrap(),
        );
        let drive = DriveClient::new(
            oauth,
            credential_store,
            "account-two",
            DriveEndpoints::default(),
        )
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("restore");
        let result =
            restore_drive_history_version(&mut store, &drive, "task", &manifest_id, &destination);
        assert!(matches!(result, Err(DriveRestoreError::WrongAccount)));
        assert!(!destination.exists());
    }
}
