mod config;
mod filter;
mod paths;
mod scan;

pub use config::{
    load_config, migrate_config_value, parse_config_json, save_config_atomic, validate_config,
    AppConfig, ApplicationConfig, BackupMode, BackupTask, ConfigError, DestinationsConfig,
    FilterConfig, GoogleDriveDestination, LocalDestination, PermissionDeniedPolicy, SafetyConfig,
    ScheduleConfig, ScheduleType, SourceConfig, SourceUnavailablePolicy, VersioningConfig,
    CURRENT_SCHEMA_VERSION, HISTORY_RETENTION_DAYS, MAX_PARALLEL_TRANSFERS,
};
pub use filter::{FilterError, FilterSet};
pub use paths::{
    expand_supported_variables, validate_task_paths, PathValidationError, ValidatedTaskPaths,
};
pub use scan::{relative_path_key, ScanError, ScannedFile, Scanner};
