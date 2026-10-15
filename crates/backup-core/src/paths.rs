use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::BackupTask;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedTaskPaths {
    pub source: PathBuf,
    pub local_destination: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum PathValidationError {
    #[error("task is disabled")]
    TaskDisabled,
    #[error("source path is empty")]
    EmptySource,
    #[error("following symbolic links or reparse points is unsupported")]
    FollowSymlinksUnsupported,
    #[error("path must be absolute: {0}")]
    RelativePath(PathBuf),
    #[error("unsupported environment variable {0} in path")]
    UnsupportedEnvironmentVariable(String),
    #[error("environment variable {0} is not defined")]
    MissingEnvironmentVariable(String),
    #[error("path contains an unmatched percent sign: {0}")]
    MalformedEnvironmentVariable(PathBuf),
    #[error("path component is a symbolic link or reparse point: {0}")]
    ReparsePoint(PathBuf),
    #[error("source does not exist or cannot be inspected: {path}: {source}")]
    SourceIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("source is not a directory: {0}")]
    SourceNotDirectory(PathBuf),
    #[error("source directory is not readable: {path}: {source}")]
    SourceNotReadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("local destination is not a directory: {0}")]
    DestinationNotDirectory(PathBuf),
    #[error("local destination cannot be inspected: {path}: {source}")]
    DestinationIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "source and local destination are equal or nested: {source_path} <-> {destination_path}"
    )]
    OverlappingPaths {
        source_path: PathBuf,
        destination_path: PathBuf,
    },
    #[error("no existing ancestor for path: {0}")]
    NoExistingAncestor(PathBuf),
}

/// Expands the one supported Windows variable. Unknown or unresolved variables
/// are rejected instead of being passed to path APIs literally.
pub fn expand_supported_variables(path: &Path) -> Result<PathBuf, PathValidationError> {
    let input = path.to_string_lossy();
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(relative_start) = input[cursor..].find('%') {
        let start = cursor + relative_start;
        output.push_str(&input[cursor..start]);
        let value_start = start + 1;
        let Some(relative_end) = input[value_start..].find('%') else {
            return Err(PathValidationError::MalformedEnvironmentVariable(
                path.to_path_buf(),
            ));
        };
        let end = value_start + relative_end;
        let name = &input[value_start..end];
        if !name.eq_ignore_ascii_case("USERPROFILE") {
            return Err(PathValidationError::UnsupportedEnvironmentVariable(
                name.to_owned(),
            ));
        }
        let value = std::env::var_os("USERPROFILE").ok_or_else(|| {
            PathValidationError::MissingEnvironmentVariable("USERPROFILE".to_owned())
        })?;
        output.push_str(&value.to_string_lossy());
        cursor = end + 1;
    }
    output.push_str(&input[cursor..]);
    Ok(PathBuf::from(output))
}

pub fn validate_task_paths(task: &BackupTask) -> Result<ValidatedTaskPaths, PathValidationError> {
    if !task.enabled {
        return Err(PathValidationError::TaskDisabled);
    }
    if task.source.path.as_os_str().is_empty() {
        return Err(PathValidationError::EmptySource);
    }
    if task.source.follow_symlinks {
        return Err(PathValidationError::FollowSymlinksUnsupported);
    }

    let source_input = expand_supported_variables(&task.source.path)?;
    let source = canonicalize_without_reparse(&source_input)?;
    let source_metadata = fs::metadata(&source).map_err(|error| PathValidationError::SourceIo {
        path: source.clone(),
        source: error,
    })?;
    if !source_metadata.is_dir() {
        return Err(PathValidationError::SourceNotDirectory(source));
    }
    drop(
        fs::read_dir(&source).map_err(|error| PathValidationError::SourceNotReadable {
            path: source.clone(),
            source: error,
        })?,
    );

    let local_destination = if task.destinations.local.enabled {
        let destination_input = expand_supported_variables(&task.destinations.local.path)?;
        let destination = canonicalize_without_reparse(&destination_input)?;
        match fs::metadata(&destination) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(PathValidationError::DestinationNotDirectory(destination));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(PathValidationError::DestinationIo {
                    path: destination,
                    source,
                });
            }
        }
        if paths_overlap(&source, &destination) {
            return Err(PathValidationError::OverlappingPaths {
                source_path: source,
                destination_path: destination,
            });
        }
        Some(destination)
    } else {
        None
    };

    Ok(ValidatedTaskPaths {
        source,
        local_destination,
    })
}

fn canonicalize_without_reparse(path: &Path) -> Result<PathBuf, PathValidationError> {
    if !path.is_absolute() {
        return Err(PathValidationError::RelativePath(path.to_path_buf()));
    }

    let mut cursor = PathBuf::new();
    let mut last_existing = None;
    let mut missing_component_seen = false;
    for component in path.components() {
        cursor.push(component.as_os_str());
        if missing_component_seen {
            continue;
        }
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if is_reparse_point(&metadata) {
                    return Err(PathValidationError::ReparsePoint(cursor));
                }
                last_existing = Some(cursor.clone());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_component_seen = true;
            }
            Err(source) => {
                return Err(PathValidationError::DestinationIo {
                    path: cursor,
                    source,
                });
            }
        }
    }

    let existing =
        last_existing.ok_or_else(|| PathValidationError::NoExistingAncestor(path.to_path_buf()))?;
    let canonical =
        fs::canonicalize(&existing).map_err(|source| PathValidationError::DestinationIo {
            path: existing.clone(),
            source,
        })?;
    let remainder = path
        .strip_prefix(&existing)
        .map_err(|_| PathValidationError::NoExistingAncestor(path.to_path_buf()))?;
    Ok(canonical.join(remainder))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = comparison_components(left);
    let right = comparison_components(right);
    component_prefix(&left, &right) || component_prefix(&right, &left)
}

fn comparison_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| match component {
            Component::Prefix(prefix) => prefix.as_os_str().to_string_lossy().to_lowercase(),
            Component::RootDir => "\\".to_owned(),
            Component::CurDir => ".".to_owned(),
            Component::ParentDir => "..".to_owned(),
            Component::Normal(name) => name.to_string_lossy().to_lowercase(),
        })
        .collect()
}

fn component_prefix(prefix: &[String], path: &[String]) -> bool {
    prefix.len() <= path.len() && prefix.iter().zip(path).all(|(left, right)| left == right)
}

#[cfg(windows)]
pub(crate) fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

pub(crate) fn open_read_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BackupMode, DestinationsConfig, FilterConfig, GoogleDriveDestination, LocalDestination,
        PermissionDeniedPolicy, SafetyConfig, ScheduleConfig, ScheduleType, SourceConfig,
        SourceUnavailablePolicy, VersioningConfig,
    };

    fn local_task(source: PathBuf, destination: PathBuf) -> BackupTask {
        BackupTask {
            id: "isolated-test".to_owned(),
            name: "isolated test".to_owned(),
            enabled: true,
            source: SourceConfig {
                path: source,
                recursive: true,
                follow_symlinks: false,
            },
            mode: BackupMode::Local,
            schedule: ScheduleConfig {
                schedule_type: ScheduleType::Manual,
                debounce_ms: 2_000,
                reconcile_interval_minutes: 60,
                scan_on_startup: false,
            },
            destinations: DestinationsConfig {
                local: LocalDestination {
                    enabled: true,
                    path: destination,
                },
                google_drive: GoogleDriveDestination {
                    enabled: false,
                    account_id: None,
                    folder_id: None,
                    folder_name: String::new(),
                },
            },
            versioning: VersioningConfig {
                enabled: true,
                retention_days: 7,
                keep_latest: true,
            },
            filters: FilterConfig {
                exclude: Vec::new(),
                case_insensitive: true,
            },
            safety: SafetyConfig {
                propagate_deletions: false,
                preserve_deleted_files: true,
                verify_before_commit: true,
                on_source_unavailable: SourceUnavailablePolicy::Pause,
                on_permission_denied: PermissionDeniedPolicy::Retry,
            },
        }
    }

    #[test]
    fn nested_local_destination_is_rejected() {
        let directory = tempfile::tempdir().expect("create isolated test directory");
        let source = directory.path().join("source");
        fs::create_dir(&source).expect("create temporary source");
        let destination = source.join("backup");

        let error = validate_task_paths(&local_task(source, destination)).expect_err("nested path");
        assert!(matches!(
            error,
            PathValidationError::OverlappingPaths { .. }
        ));
    }

    #[test]
    fn separate_temporary_source_and_destination_are_accepted() {
        let directory = tempfile::tempdir().expect("create isolated test directory");
        let source = directory.path().join("source");
        let destination = directory.path().join("backup");
        fs::create_dir(&source).expect("create temporary source");

        let paths = validate_task_paths(&local_task(source, destination))
            .expect("separate temporary paths should be valid");
        let expected_destination = fs::canonicalize(directory.path())
            .expect("canonicalize temporary test directory")
            .join("backup");
        assert_eq!(paths.local_destination, Some(expected_destination));
    }

    #[test]
    fn unknown_environment_variables_are_rejected() {
        let error = expand_supported_variables(Path::new("%SOME_OTHER_VARIABLE%\\Backup"))
            .expect_err("unknown variables must be rejected");
        assert!(matches!(
            error,
            PathValidationError::UnsupportedEnvironmentVariable(_)
        ));
    }

    #[test]
    fn empty_source_is_rejected_before_any_path_access() {
        let task = local_task(PathBuf::new(), PathBuf::from("C:\\not-used"));
        assert!(matches!(
            validate_task_paths(&task),
            Err(PathValidationError::EmptySource)
        ));
    }
}
