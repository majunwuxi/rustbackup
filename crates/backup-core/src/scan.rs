#[cfg(test)]
use std::fs::File;
use std::fs::{self, ReadDir};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::paths::{is_reparse_point, open_read_no_follow};
use crate::{validate_task_paths, BackupTask, FilterError, FilterSet, PathValidationError};

const FILE_STABILITY_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedFile {
    pub source_path: PathBuf,
    pub relative_path: PathBuf,
    pub relative_path_key: String,
    pub size_bytes: u64,
    pub modified_at_unix_ms: i64,
    pub blake3_hex: String,
}

#[derive(Debug, Error)]
pub enum ScanError {
    #[error(transparent)]
    PathValidation(#[from] PathValidationError),
    #[error(transparent)]
    Filter(#[from] FilterError),
    #[error("filesystem operation failed for {path}: {error}")]
    Io {
        path: PathBuf,
        #[source]
        error: io::Error,
    },
    #[error("source path contains an unsupported reparse point: {0}")]
    ReparsePoint(PathBuf),
    #[error("source contains an unsupported filesystem entry: {0}")]
    UnsupportedEntry(PathBuf),
    #[error("source file changed while being scanned: {0}")]
    UnstableFile(PathBuf),
    #[error("relative path is empty, rooted, or contains traversal: {0}")]
    InvalidRelativePath(PathBuf),
    #[error("file timestamp is outside the supported range: {0}")]
    TimestampOutOfRange(PathBuf),
}

struct DirectoryFrame {
    absolute_path: PathBuf,
    relative_path: PathBuf,
    entries: ReadDir,
}

/// A depth-first streaming scanner. It retains only the active directory
/// stack and hashes each file through a bounded buffer.
pub struct Scanner {
    frames: Vec<DirectoryFrame>,
    recursive: bool,
    filters: FilterSet,
}

impl Scanner {
    pub fn new(task: &BackupTask) -> Result<Self, ScanError> {
        let paths = validate_task_paths(task)?;
        let filters = FilterSet::new(&task.filters)?;
        let entries = fs::read_dir(&paths.source).map_err(|error| ScanError::Io {
            path: paths.source.clone(),
            error,
        })?;
        Ok(Self {
            frames: vec![DirectoryFrame {
                absolute_path: paths.source,
                relative_path: PathBuf::new(),
                entries,
            }],
            recursive: task.source.recursive,
            filters,
        })
    }
}

impl Iterator for Scanner {
    type Item = Result<ScannedFile, ScanError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (directory_path, directory_relative_path, entry_result) = {
                let frame = self.frames.last_mut()?;
                (
                    frame.absolute_path.clone(),
                    frame.relative_path.clone(),
                    frame.entries.next(),
                )
            };

            let entry = match entry_result {
                None => {
                    self.frames.pop();
                    continue;
                }
                Some(Ok(entry)) => entry,
                Some(Err(error)) => {
                    return Some(Err(ScanError::Io {
                        path: directory_path,
                        error,
                    }));
                }
            };

            let absolute_path = entry.path();
            let relative_path = directory_relative_path.join(entry.file_name());
            let metadata = match fs::symlink_metadata(&absolute_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    return Some(Err(ScanError::Io {
                        path: absolute_path,
                        error,
                    }));
                }
            };

            if is_reparse_point(&metadata) {
                return Some(Err(ScanError::ReparsePoint(absolute_path)));
            }
            if metadata.is_dir() {
                if self.filters.is_excluded(&relative_path, true) {
                    continue;
                }
                if self.recursive {
                    match fs::read_dir(&absolute_path) {
                        Ok(entries) => self.frames.push(DirectoryFrame {
                            absolute_path,
                            relative_path,
                            entries,
                        }),
                        Err(error) => {
                            return Some(Err(ScanError::Io {
                                path: absolute_path,
                                error,
                            }));
                        }
                    }
                }
                continue;
            }
            if metadata.is_file() {
                if self.filters.is_excluded(&relative_path, false) {
                    continue;
                }
                return Some(scan_file(absolute_path, relative_path));
            }
            return Some(Err(ScanError::UnsupportedEntry(absolute_path)));
        }
    }
}

pub fn relative_path_key(relative_path: &Path) -> Result<String, ScanError> {
    let mut key = String::new();
    for component in relative_path.components() {
        let Component::Normal(name) = component else {
            return Err(ScanError::InvalidRelativePath(relative_path.to_path_buf()));
        };
        if !key.is_empty() {
            key.push('/');
        }
        key.extend(name.to_string_lossy().chars().flat_map(char::to_lowercase));
    }
    if key.is_empty() {
        return Err(ScanError::InvalidRelativePath(relative_path.to_path_buf()));
    }
    Ok(key)
}

fn scan_file(absolute_path: PathBuf, relative_path: PathBuf) -> Result<ScannedFile, ScanError> {
    for _ in 0..FILE_STABILITY_ATTEMPTS {
        let path_metadata =
            fs::symlink_metadata(&absolute_path).map_err(|error| ScanError::Io {
                path: absolute_path.clone(),
                error,
            })?;
        if is_reparse_point(&path_metadata) {
            return Err(ScanError::ReparsePoint(absolute_path));
        }
        if !path_metadata.is_file() {
            return Err(ScanError::UnsupportedEntry(absolute_path));
        }

        let mut file = open_read_no_follow(&absolute_path).map_err(|error| ScanError::Io {
            path: absolute_path.clone(),
            error,
        })?;
        let before = file.metadata().map_err(|error| ScanError::Io {
            path: absolute_path.clone(),
            error,
        })?;
        if is_reparse_point(&before) || !before.is_file() {
            return Err(ScanError::ReparsePoint(absolute_path));
        }
        let before_stamp = file_stamp(&before, &absolute_path)?;

        let mut hasher = blake3::Hasher::new();
        io::copy(&mut file, &mut hasher).map_err(|error| ScanError::Io {
            path: absolute_path.clone(),
            error,
        })?;

        let after = file.metadata().map_err(|error| ScanError::Io {
            path: absolute_path.clone(),
            error,
        })?;
        let after_stamp = file_stamp(&after, &absolute_path)?;
        let current_path_metadata =
            fs::symlink_metadata(&absolute_path).map_err(|error| ScanError::Io {
                path: absolute_path.clone(),
                error,
            })?;
        if is_reparse_point(&current_path_metadata) {
            return Err(ScanError::ReparsePoint(absolute_path));
        }
        let current_path_stamp = file_stamp(&current_path_metadata, &absolute_path)?;

        if before_stamp != after_stamp || after_stamp != current_path_stamp {
            continue;
        }
        let modified_at_unix_ms = timestamp_millis(after_stamp.modified, &absolute_path)?;
        return Ok(ScannedFile {
            source_path: absolute_path,
            relative_path_key: relative_path_key(&relative_path)?,
            relative_path,
            size_bytes: after_stamp.size,
            modified_at_unix_ms,
            blake3_hex: hasher.finalize().to_hex().to_string(),
        });
    }

    Err(ScanError::UnstableFile(absolute_path))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
    size: u64,
    modified: SystemTime,
}

fn file_stamp(metadata: &fs::Metadata, path: &Path) -> Result<FileStamp, ScanError> {
    let modified = metadata.modified().map_err(|error| ScanError::Io {
        path: path.to_path_buf(),
        error,
    })?;
    Ok(FileStamp {
        size: metadata.len(),
        modified,
    })
}

fn timestamp_millis(time: SystemTime, path: &Path) -> Result<i64, ScanError> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis())
            .map_err(|_| ScanError::TimestampOutOfRange(path.to_path_buf())),
        Err(error) => {
            let millis = i64::try_from(error.duration().as_millis())
                .map_err(|_| ScanError::TimestampOutOfRange(path.to_path_buf()))?;
            Ok(-millis)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::{
        BackupMode, DestinationsConfig, FilterConfig, GoogleDriveDestination, LocalDestination,
        PermissionDeniedPolicy, SafetyConfig, ScheduleConfig, ScheduleType, SourceConfig,
        SourceUnavailablePolicy, VersioningConfig,
    };

    fn task(source: &Path, destination: &Path) -> BackupTask {
        BackupTask {
            id: "scan-test".to_owned(),
            name: "scan test".to_owned(),
            enabled: true,
            source: SourceConfig {
                path: source.to_path_buf(),
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
                    path: destination.to_path_buf(),
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
                exclude: vec![
                    "*.tmp".to_owned(),
                    "~$*".to_owned(),
                    ".git".to_owned(),
                    "node_modules".to_owned(),
                ],
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
    fn scan_hashes_files_and_prunes_excluded_directories() {
        let directory = tempfile::tempdir().expect("create isolated test directory");
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::create_dir(&source).expect("create temporary source");
        fs::create_dir(&destination).expect("create temporary destination");
        fs::create_dir_all(source.join("nested/.git")).expect("create excluded directory");
        fs::create_dir_all(source.join("nested/node_modules/pkg"))
            .expect("create excluded directory");

        let mut keep = File::create(source.join("keep.txt")).expect("create temporary file");
        keep.write_all(b"keep").expect("write temporary file");
        let mut excluded =
            File::create(source.join("nested/skip.TMP")).expect("create excluded temporary file");
        excluded.write_all(b"skip").expect("write excluded file");
        File::create(source.join("nested/.git/config"))
            .expect("create file under excluded directory");
        File::create(source.join("nested/node_modules/pkg/index.js"))
            .expect("create file under excluded directory");

        let files = Scanner::new(&task(&source, &destination))
            .expect("initialize scan")
            .collect::<Result<Vec<_>, _>>()
            .expect("scan should succeed");

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path_key, "keep.txt");
        assert_eq!(
            files[0].blake3_hex,
            blake3::hash(b"keep").to_hex().to_string()
        );
    }

    #[test]
    fn relative_keys_normalize_separator_and_case() {
        let key = relative_path_key(Path::new("Folder/Report.TXT")).expect("valid relative path");
        assert_eq!(key, "folder/report.txt");
        assert!(relative_path_key(Path::new("../outside.txt")).is_err());
    }
}
