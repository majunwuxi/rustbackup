use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::filter::{FilterError, FilterSet};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
pub const MAX_PARALLEL_TRANSFERS: u8 = 16;
pub const HISTORY_RETENTION_DAYS: u32 = 7;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub schema_version: u32,
    pub application: ApplicationConfig,
    pub tasks: Vec<BackupTask>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationConfig {
    pub start_with_windows: bool,
    pub start_minimized: bool,
    pub minimize_to_tray: bool,
    pub language: String,
    pub max_parallel_transfers: u8,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupTask {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub source: SourceConfig,
    pub mode: BackupMode,
    pub schedule: ScheduleConfig,
    pub destinations: DestinationsConfig,
    pub versioning: VersioningConfig,
    pub filters: FilterConfig,
    pub safety: SafetyConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupMode {
    Local,
    Cloud,
    Dual,
    Bidirectional,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub path: std::path::PathBuf,
    pub recursive: bool,
    pub follow_symlinks: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    #[serde(rename = "type")]
    pub schedule_type: ScheduleType,
    pub debounce_ms: u64,
    pub reconcile_interval_minutes: u32,
    pub scan_on_startup: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleType {
    Realtime,
    Hourly,
    Daily,
    Manual,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationsConfig {
    pub local: LocalDestination,
    pub google_drive: GoogleDriveDestination,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalDestination {
    pub enabled: bool,
    pub path: std::path::PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleDriveDestination {
    pub enabled: bool,
    pub account_id: Option<String>,
    pub folder_id: Option<String>,
    pub folder_name: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VersioningConfig {
    pub enabled: bool,
    pub retention_days: u32,
    pub keep_latest: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilterConfig {
    pub exclude: Vec<String>,
    pub case_insensitive: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyConfig {
    pub propagate_deletions: bool,
    pub preserve_deleted_files: bool,
    pub verify_before_commit: bool,
    pub on_source_unavailable: SourceUnavailablePolicy,
    pub on_permission_denied: PermissionDeniedPolicy,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceUnavailablePolicy {
    Pause,
    Retry,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDeniedPolicy {
    Pause,
    Retry,
    Fail,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("configuration JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("configuration has no schema_version")]
    MissingSchemaVersion,
    #[error("unsupported configuration schema version {0}")]
    UnsupportedSchemaVersion(u64),
    #[error("max_parallel_transfers must be between 1 and 16")]
    InvalidParallelTransferCount,
    #[error(
        "task id {0} is invalid; use 1-64 lowercase ASCII letters, digits, underscore, or hyphen"
    )]
    InvalidTaskId(String),
    #[error("task id {0} is duplicated")]
    DuplicateTaskId(String),
    #[error("enabled task {0} must have a source directory")]
    EmptyEnabledSource(String),
    #[error("task {0} has an empty local destination path")]
    EmptyLocalDestination(String),
    #[error("task {0} requires its local destination to be enabled")]
    LocalDestinationRequired(String),
    #[error("task {0} requires its Google Drive destination to be enabled")]
    DriveDestinationRequired(String),
    #[error("task {0} must have at least one destination")]
    NoDestination(String),
    #[error("task {0} enables versioning but retention_days must be exactly 7")]
    InvalidRetention(String),
    #[error("task {0} uses bidirectional mode, which is unsupported in this release")]
    UnsupportedMode(String),
    #[error("task {0} must have a Google Drive folder name")]
    EmptyDriveFolderName(String),
    #[error("task {0} must have a non-empty name")]
    EmptyTaskName(String),
    #[error("task {0} has invalid exclude patterns: {1}")]
    InvalidFilter(String, #[source] FilterError),
    #[error("task {0} schedule intervals must be greater than zero")]
    InvalidScheduleInterval(String),
}

pub fn parse_config_json(json: &str) -> Result<AppConfig, ConfigError> {
    let value: Value = serde_json::from_str(json)?;
    let value = migrate_config_value(value)?;
    let config: AppConfig = serde_json::from_value(value)?;
    validate_config(&config)?;
    Ok(config)
}

pub fn load_config(path: &Path) -> Result<AppConfig, ConfigError> {
    let json = fs::read_to_string(path)?;
    parse_config_json(&json)
}

/// Migration entry point. Add a version-to-version transform here before
/// accepting a new schema version; unknown versions are never guessed.
pub fn migrate_config_value(value: Value) -> Result<Value, ConfigError> {
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or(ConfigError::MissingSchemaVersion)?;
    if version != u64::from(CURRENT_SCHEMA_VERSION) {
        return Err(ConfigError::UnsupportedSchemaVersion(version));
    }
    Ok(value)
}

pub fn validate_config(config: &AppConfig) -> Result<(), ConfigError> {
    if config.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchemaVersion(u64::from(
            config.schema_version,
        )));
    }
    if !(1..=MAX_PARALLEL_TRANSFERS).contains(&config.application.max_parallel_transfers) {
        return Err(ConfigError::InvalidParallelTransferCount);
    }

    let mut task_ids = HashSet::with_capacity(config.tasks.len());
    for task in &config.tasks {
        if !is_valid_task_id(&task.id) {
            return Err(ConfigError::InvalidTaskId(task.id.clone()));
        }
        if !task_ids.insert(task.id.as_str()) {
            return Err(ConfigError::DuplicateTaskId(task.id.clone()));
        }
        if task.name.trim().is_empty() {
            return Err(ConfigError::EmptyTaskName(task.id.clone()));
        }
        if task.enabled && task.source.path.as_os_str().is_empty() {
            return Err(ConfigError::EmptyEnabledSource(task.id.clone()));
        }
        if task.schedule.debounce_ms == 0 || task.schedule.reconcile_interval_minutes == 0 {
            return Err(ConfigError::InvalidScheduleInterval(task.id.clone()));
        }
        if task.destinations.local.enabled && task.destinations.local.path.as_os_str().is_empty() {
            return Err(ConfigError::EmptyLocalDestination(task.id.clone()));
        }
        if task.destinations.google_drive.enabled
            && task.destinations.google_drive.folder_name.trim().is_empty()
        {
            return Err(ConfigError::EmptyDriveFolderName(task.id.clone()));
        }
        if task.versioning.enabled && task.versioning.retention_days != HISTORY_RETENTION_DAYS {
            return Err(ConfigError::InvalidRetention(task.id.clone()));
        }
        FilterSet::new(&task.filters)
            .map_err(|error| ConfigError::InvalidFilter(task.id.clone(), error))?;
        if !task.destinations.local.enabled && !task.destinations.google_drive.enabled {
            return Err(ConfigError::NoDestination(task.id.clone()));
        }

        match &task.mode {
            BackupMode::Local if !task.destinations.local.enabled => {
                return Err(ConfigError::LocalDestinationRequired(task.id.clone()));
            }
            BackupMode::Cloud if !task.destinations.google_drive.enabled => {
                return Err(ConfigError::DriveDestinationRequired(task.id.clone()));
            }
            BackupMode::Dual
                if !task.destinations.local.enabled || !task.destinations.google_drive.enabled =>
            {
                if !task.destinations.local.enabled {
                    return Err(ConfigError::LocalDestinationRequired(task.id.clone()));
                }
                return Err(ConfigError::DriveDestinationRequired(task.id.clone()));
            }
            BackupMode::Bidirectional if task.enabled => {
                return Err(ConfigError::UnsupportedMode(task.id.clone()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_valid_task_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

pub fn save_config_atomic(path: &Path, config: &AppConfig) -> Result<(), ConfigError> {
    validate_config(config)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;

    let json = serde_json::to_vec_pretty(config)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&json)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;

    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = File::open(parent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_CONFIG: &str = include_str!("../../../config/work-documents.json");

    #[test]
    fn default_config_round_trips_and_remains_disabled() {
        let config = parse_config_json(DEFAULT_CONFIG).expect("default config should be valid");
        assert_eq!(config.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(config.tasks.len(), 1);
        assert!(!config.tasks[0].enabled);
        assert!(config.tasks[0].source.path.as_os_str().is_empty());

        let serialized = serde_json::to_string(&config).expect("serialize config");
        let restored = parse_config_json(&serialized).expect("parse serialized config");
        assert_eq!(restored, config);
    }

    #[test]
    fn unknown_enum_values_are_rejected() {
        let invalid = DEFAULT_CONFIG.replace("\"dual\"", "\"sync\"");
        assert!(matches!(
            parse_config_json(&invalid),
            Err(ConfigError::Json(_))
        ));
    }

    #[test]
    fn enabled_task_cannot_use_an_empty_source_path() {
        let invalid = DEFAULT_CONFIG.replace("\"enabled\": false", "\"enabled\": true");
        assert!(matches!(
            parse_config_json(&invalid),
            Err(ConfigError::EmptyEnabledSource(_))
        ));
    }

    #[test]
    fn enabled_versioning_requires_the_project_seven_day_retention() {
        let invalid = DEFAULT_CONFIG.replace("\"retention_days\": 7", "\"retention_days\": 30");
        assert!(matches!(
            parse_config_json(&invalid),
            Err(ConfigError::InvalidRetention(_))
        ));
    }

    #[test]
    fn unsupported_schema_versions_are_not_guessed() {
        let invalid = DEFAULT_CONFIG.replace("\"schema_version\": 1", "\"schema_version\": 2");
        assert!(matches!(
            parse_config_json(&invalid),
            Err(ConfigError::UnsupportedSchemaVersion(2))
        ));
    }

    #[test]
    fn atomic_save_replaces_existing_file_in_test_directory() {
        let directory = tempfile::tempdir().expect("create isolated test directory");
        let path = directory.path().join("config.json");
        let config = parse_config_json(DEFAULT_CONFIG).expect("default config should be valid");

        save_config_atomic(&path, &config).expect("save config");
        let restored = load_config(&path).expect("load saved config");

        assert_eq!(restored, config);
        save_config_atomic(&path, &config).expect("replace existing config");
        assert_eq!(load_config(&path).expect("reload saved config"), config);
    }
}
