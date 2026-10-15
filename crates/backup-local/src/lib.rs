//! Safe local snapshot and mirror updates. The source is never modified and
//! destination deletions are deliberately not propagated.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use backup_core::{
    validate_task_paths, BackupMode, BackupTask, FilterSet, PathValidationError, ScanError,
    Scanner, SourceUnavailablePolicy,
};
use backup_storage::{BackupRunStatus, CommitOutcome, FileObservation, StateStore, StorageError};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tempfile::NamedTempFile;
use thiserror::Error;

const SNAPSHOT_ATTEMPTS: usize = 3;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_REPORTED_ISSUES: usize = 100;

#[derive(Debug, Error)]
pub enum LocalBackupError {
    #[error(transparent)]
    Paths(#[from] PathValidationError),
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("local destination root must already exist; it is created only after user confirmation: {0}")]
    DestinationMissing(PathBuf),
    #[error("local destination is not a regular directory: {0}")]
    DestinationInvalid(PathBuf),
    #[error("local destination contains an unsupported reparse point: {0}")]
    DestinationReparsePoint(PathBuf),
    #[error("destination path contains an unsafe component: {0}")]
    UnsafeDestinationPath(PathBuf),
    #[error("destination path is occupied by a non-file: {0}")]
    DestinationTargetInvalid(PathBuf),
    #[error("backup mode {0} is not implemented by the local M1 service")]
    UnsupportedMode(String),
    #[error("task {0} is not enabled")]
    TaskDisabled(String),
    #[error("filesystem operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("source snapshot did not remain stable: {0}")]
    UnstableSource(PathBuf),
    #[error("source snapshot checksum changed after scan: {0}")]
    SnapshotHashMismatch(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupSummary {
    pub run_id: String,
    pub status: String,
    pub scanned_files: u64,
    pub copied_files: u64,
    pub source_missing_files: u64,
    pub cloud_pending_files: u64,
    pub deletion_review_required: bool,
    pub issues: Vec<String>,
    pub omitted_issue_count: u64,
}

impl BackupSummary {
    fn new(run_id: String) -> Self {
        Self {
            run_id,
            status: "running".to_owned(),
            scanned_files: 0,
            copied_files: 0,
            source_missing_files: 0,
            cloud_pending_files: 0,
            deletion_review_required: false,
            issues: Vec::new(),
            omitted_issue_count: 0,
        }
    }

    fn add_issue(&mut self, issue: impl Into<String>) {
        if self.issues.len() < MAX_REPORTED_ISSUES {
            self.issues.push(issue.into());
        } else {
            self.omitted_issue_count += 1;
        }
    }
}

/// Runs a complete reconciliation and commits each stable file independently.
/// The destination root must exist already; this function never creates it.
pub fn backup_once(
    store: &mut StateStore,
    task: &BackupTask,
) -> Result<BackupSummary, LocalBackupError> {
    validate_mode(task)?;
    if !task.enabled {
        return Err(LocalBackupError::TaskDisabled(task.id.clone()));
    }

    store.register_task(&task.id, &task.name, task.enabled)?;
    let run_id = store.begin_backup_run(&task.id)?;
    let result = run_reconciliation(store, task, run_id.clone());
    match result {
        Ok(mut summary) => {
            summary.status = if !summary.issues.is_empty()
                || summary.omitted_issue_count > 0
                || summary.deletion_review_required
                || summary.cloud_pending_files > 0
            {
                "partial_success".to_owned()
            } else {
                "succeeded".to_owned()
            };
            let run_status = if summary.status == "succeeded" {
                BackupRunStatus::Succeeded
            } else {
                BackupRunStatus::PartialSuccess
            };
            store.finish_backup_run(
                &summary.run_id,
                run_status,
                summary.scanned_files,
                summary.copied_files,
                summary_error_text(&summary).as_deref(),
            )?;
            Ok(summary)
        }
        Err(error) => {
            let status = if should_pause_for_source(task, &error) {
                BackupRunStatus::Paused
            } else {
                BackupRunStatus::Error
            };
            store.finish_backup_run(&run_id, status, 0, 0, Some(&error.to_string()))?;
            Err(error)
        }
    }
}

fn validate_mode(task: &BackupTask) -> Result<(), LocalBackupError> {
    match task.mode {
        BackupMode::Local if task.destinations.local.enabled => Ok(()),
        BackupMode::Dual
            if task.destinations.local.enabled && task.destinations.google_drive.enabled =>
        {
            Ok(())
        }
        BackupMode::Bidirectional => Err(LocalBackupError::UnsupportedMode(
            "bidirectional".to_owned(),
        )),
        BackupMode::Cloud => Err(LocalBackupError::UnsupportedMode("cloud".to_owned())),
        BackupMode::Local | BackupMode::Dual => Err(LocalBackupError::UnsupportedMode(
            "task has no enabled local destination".to_owned(),
        )),
    }
}

fn run_reconciliation(
    store: &mut StateStore,
    task: &BackupTask,
    run_id: String,
) -> Result<BackupSummary, LocalBackupError> {
    let paths = validate_task_paths(task)?;
    let destination_root = paths.local_destination.ok_or_else(|| {
        LocalBackupError::DestinationMissing(task.destinations.local.path.clone())
    })?;
    let root_metadata = match fs::symlink_metadata(&destination_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(LocalBackupError::DestinationMissing(destination_root));
        }
        Err(source) => {
            return Err(LocalBackupError::Io {
                path: destination_root,
                source,
            });
        }
    };
    if is_reparse_point(&root_metadata) {
        return Err(LocalBackupError::DestinationReparsePoint(destination_root));
    }
    if !root_metadata.is_dir() {
        return Err(LocalBackupError::DestinationInvalid(destination_root));
    }
    let destination_root =
        fs::canonicalize(&destination_root).map_err(|source| LocalBackupError::Io {
            path: destination_root,
            source,
        })?;

    let mut summary = BackupSummary::new(run_id.clone());
    let scanner = Scanner::new(task)?;
    let filters = FilterSet::new(&task.filters).map_err(ScanError::from)?;
    let mut scan_complete = true;

    for item in scanner {
        let scanned = match item {
            Ok(scanned) => scanned,
            Err(error) => {
                scan_complete = false;
                summary.add_issue(error.to_string());
                continue;
            }
        };
        summary.scanned_files += 1;
        let display_path = scanned.relative_path.to_string_lossy().into_owned();
        let indexed = match store.record_file_seen(
            &task.id,
            &run_id,
            FileObservation {
                relative_path_key: &scanned.relative_path_key,
                display_relative_path: &display_path,
                size_bytes: scanned.size_bytes,
                modified_at_unix_ms: scanned.modified_at_unix_ms,
                blake3_hex: &scanned.blake3_hex,
            },
        ) {
            Ok(indexed) => indexed,
            Err(error) => {
                scan_complete = false;
                summary.add_issue(error.to_string());
                continue;
            }
        };

        if task.mode == BackupMode::Dual {
            summary.cloud_pending_files += 1;
        }
        if store.destination_revision(indexed.file_id, "local")? >= indexed.source_revision {
            continue;
        }

        let job_id = store.enqueue_local_copy(indexed.file_id, indexed.source_revision)?;
        if let Err(error) = store.start_job(&job_id) {
            summary.add_issue(format!("{}: {error}", display_path));
            continue;
        }

        let result = copy_scanned_file(
            store,
            &destination_root,
            &scanned,
            indexed.file_id,
            indexed.source_revision,
            &job_id,
        );
        match result {
            Ok(CommitOutcome::Committed) => summary.copied_files += 1,
            Ok(CommitOutcome::Stale) => summary.add_issue(format!(
                "{}: source revision was superseded before local commit",
                display_path
            )),
            Err(error) => {
                let message = error.to_string();
                store.retry_job(&job_id, &message)?;
                summary.add_issue(format!("{}: {message}", display_path));
            }
        }
    }

    if scan_complete {
        let unseen = store.unseen_files(&task.id, &run_id)?;
        let mut candidates = Vec::new();
        for file in unseen {
            if !filters.is_excluded(Path::new(&file.display_relative_path), false) {
                candidates.push(file);
            }
        }
        let tracked_count = summary.scanned_files as usize + candidates.len();
        let mass_deletion_suspected = candidates.len() >= 10
            && tracked_count > 0
            && candidates.len().saturating_mul(2) >= tracked_count;
        if mass_deletion_suspected {
            summary.deletion_review_required = true;
            summary.add_issue(format!(
                "{} of {} previously indexed files were not found; source-missing states require review",
                candidates.len(), tracked_count
            ));
        } else {
            for file in candidates {
                store.mark_source_missing(file.file_id)?;
                summary.source_missing_files += 1;
            }
        }
    } else {
        summary.add_issue("source scan was incomplete; missing-file states were left unchanged");
    }

    Ok(summary)
}

fn copy_scanned_file(
    store: &mut StateStore,
    destination_root: &Path,
    scanned: &backup_core::ScannedFile,
    file_id: i64,
    revision: i64,
    job_id: &str,
) -> Result<CommitOutcome, LocalBackupError> {
    let target = target_path(destination_root, &scanned.relative_path)?;
    let parent = target
        .parent()
        .ok_or_else(|| LocalBackupError::UnsafeDestinationPath(target.clone()))?;
    ensure_safe_target_parent(destination_root, parent)?;
    validate_existing_target(&target)?;

    let staged = create_stable_snapshot(&scanned.source_path, parent, &scanned.blake3_hex)?;
    let checksum = scanned.blake3_hex.clone();
    let target_file_id = target.to_string_lossy().into_owned();
    store
        .commit_local_if_current(
            job_id,
            file_id,
            revision,
            &checksum,
            &target_file_id,
            || {
                ensure_safe_target_parent(destination_root, parent).map_err(local_error_to_io)?;
                validate_existing_target(&target).map_err(local_error_to_io)?;
                staged
                    .persist(&target)
                    .map(|_| ())
                    .map_err(|error| error.error)
            },
        )
        .map_err(LocalBackupError::from)
}

fn validate_existing_target(target: &Path) -> Result<(), LocalBackupError> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if is_reparse_point(&metadata) => Err(
            LocalBackupError::DestinationReparsePoint(target.to_path_buf()),
        ),
        Ok(metadata) if !metadata.is_file() => Err(LocalBackupError::DestinationTargetInvalid(
            target.to_path_buf(),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LocalBackupError::Io {
            path: target.to_path_buf(),
            source,
        }),
    }
}

fn local_error_to_io(error: LocalBackupError) -> io::Error {
    io::Error::other(error.to_string())
}

fn target_path(root: &Path, relative_path: &Path) -> Result<PathBuf, LocalBackupError> {
    let mut target = root.to_path_buf();
    let mut found = false;
    for component in relative_path.components() {
        let Component::Normal(name) = component else {
            return Err(LocalBackupError::UnsafeDestinationPath(
                relative_path.to_path_buf(),
            ));
        };
        target.push(name);
        found = true;
    }
    if !found {
        return Err(LocalBackupError::UnsafeDestinationPath(
            relative_path.to_path_buf(),
        ));
    }
    Ok(target)
}

fn ensure_safe_target_parent(root: &Path, parent: &Path) -> Result<(), LocalBackupError> {
    let root_metadata = fs::symlink_metadata(root).map_err(|source| LocalBackupError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    if is_reparse_point(&root_metadata) {
        return Err(LocalBackupError::DestinationReparsePoint(
            root.to_path_buf(),
        ));
    }
    if !root_metadata.is_dir() {
        return Err(LocalBackupError::DestinationInvalid(root.to_path_buf()));
    }
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| LocalBackupError::UnsafeDestinationPath(parent.to_path_buf()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(LocalBackupError::UnsafeDestinationPath(
                parent.to_path_buf(),
            ));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if is_reparse_point(&metadata) {
                    return Err(LocalBackupError::DestinationReparsePoint(current));
                }
                if !metadata.is_dir() {
                    return Err(LocalBackupError::DestinationInvalid(current));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|source| LocalBackupError::Io {
                    path: current.clone(),
                    source,
                })?;
                let metadata =
                    fs::symlink_metadata(&current).map_err(|source| LocalBackupError::Io {
                        path: current.clone(),
                        source,
                    })?;
                if is_reparse_point(&metadata) {
                    return Err(LocalBackupError::DestinationReparsePoint(current));
                }
                if !metadata.is_dir() {
                    return Err(LocalBackupError::DestinationInvalid(current));
                }
            }
            Err(source) => {
                return Err(LocalBackupError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn create_stable_snapshot(
    source: &Path,
    temp_parent: &Path,
    expected_hash: &str,
) -> Result<NamedTempFile, LocalBackupError> {
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let path_metadata =
            fs::symlink_metadata(source).map_err(|source_error| LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        if is_reparse_point(&path_metadata) || !path_metadata.is_file() {
            return Err(LocalBackupError::UnstableSource(source.to_path_buf()));
        }
        let mut input =
            open_source_no_follow(source).map_err(|source_error| LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        let input_metadata = input
            .metadata()
            .map_err(|source_error| LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        if is_reparse_point(&input_metadata) || !input_metadata.is_file() {
            return Err(LocalBackupError::UnstableSource(source.to_path_buf()));
        }
        let before = FileStamp::from_metadata(&input_metadata)
            .ok_or_else(|| LocalBackupError::UnstableSource(source.to_path_buf()))?;
        let mut staged =
            NamedTempFile::new_in(temp_parent).map_err(|source_error| LocalBackupError::Io {
                path: temp_parent.to_path_buf(),
                source: source_error,
            })?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|source_error| LocalBackupError::Io {
                    path: source.to_path_buf(),
                    source: source_error,
                })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            staged
                .write_all(&buffer[..read])
                .map_err(|source_error| LocalBackupError::Io {
                    path: temp_parent.to_path_buf(),
                    source: source_error,
                })?;
        }
        let after = FileStamp::from_metadata(&input.metadata().map_err(|source_error| {
            LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            }
        })?)
        .ok_or_else(|| LocalBackupError::UnstableSource(source.to_path_buf()))?;
        let path_after =
            fs::symlink_metadata(source).map_err(|source_error| LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        if is_reparse_point(&path_after) {
            return Err(LocalBackupError::UnstableSource(source.to_path_buf()));
        }
        let path_after = FileStamp::from_metadata(&path_after)
            .ok_or_else(|| LocalBackupError::UnstableSource(source.to_path_buf()))?;
        if before != after || after != path_after {
            continue;
        }
        let actual_hash = hasher.finalize().to_hex().to_string();
        if actual_hash != expected_hash {
            continue;
        }

        staged
            .as_file()
            .sync_all()
            .map_err(|source_error| LocalBackupError::Io {
                path: staged.path().to_path_buf(),
                source: source_error,
            })?;
        staged
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(|source_error| LocalBackupError::Io {
                path: staged.path().to_path_buf(),
                source: source_error,
            })?;
        let mut verify = blake3::Hasher::new();
        io::copy(staged.as_file_mut(), &mut verify).map_err(|source_error| {
            LocalBackupError::Io {
                path: staged.path().to_path_buf(),
                source: source_error,
            }
        })?;
        if verify.finalize().to_hex().as_str() != expected_hash {
            continue;
        }
        staged
            .as_file()
            .sync_all()
            .map_err(|source_error| LocalBackupError::Io {
                path: staged.path().to_path_buf(),
                source: source_error,
            })?;
        return Ok(staged);
    }
    Err(LocalBackupError::UnstableSource(source.to_path_buf()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
    size: u64,
    modified: SystemTime,
}

impl FileStamp {
    fn from_metadata(metadata: &fs::Metadata) -> Option<Self> {
        Some(Self {
            size: metadata.len(),
            modified: metadata.modified().ok()?,
        })
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn open_source_no_follow(path: &Path) -> io::Result<File> {
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

fn summary_error_text(summary: &BackupSummary) -> Option<String> {
    if summary.issues.is_empty() && summary.omitted_issue_count == 0 {
        None
    } else {
        let mut text = summary.issues.join("; ");
        if summary.omitted_issue_count > 0 {
            text.push_str(&format!(
                "; {} additional issues omitted",
                summary.omitted_issue_count
            ));
        }
        Some(text)
    }
}

#[derive(Clone, Debug)]
pub struct EventDebouncer {
    delay: Duration,
    deadlines: HashMap<PathBuf, Instant>,
    full_reconcile: bool,
}

impl EventDebouncer {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            deadlines: HashMap::new(),
            full_reconcile: false,
        }
    }

    pub fn observe_path(&mut self, path: PathBuf, now: Instant) {
        self.deadlines.insert(path, now + self.delay);
    }

    pub fn request_full_reconcile(&mut self) {
        self.full_reconcile = true;
    }

    pub fn take_ready(&mut self, now: Instant) -> bool {
        let ready_paths: Vec<_> = self
            .deadlines
            .iter()
            .filter_map(|(path, deadline)| (*deadline <= now).then_some(path.clone()))
            .collect();
        let has_ready_path = !ready_paths.is_empty();
        for path in ready_paths {
            self.deadlines.remove(&path);
        }
        let ready = has_ready_path || self.full_reconcile;
        self.full_reconcile = false;
        ready
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.values().copied().min()
    }
}

struct LocalWatcher {
    _watcher: RecommendedWatcher,
    events: Receiver<notify::Result<Event>>,
}

impl LocalWatcher {
    fn start(source: &Path) -> notify::Result<Self> {
        let (sender, events) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })?;
        watcher.watch(source, RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            events,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SchedulerReport {
    pub reconciliations: u64,
    pub watcher_error: Option<String>,
    pub last_backup_status: Option<String>,
    pub last_backup_error: Option<String>,
}

/// Runs startup, debounced watcher and periodic reconciliations until the
/// caller closes the shutdown channel. Watcher errors force a full scan and
/// remain visible in the returned report.
pub fn run_scheduled(
    store: &mut StateStore,
    task: &BackupTask,
    shutdown: &Receiver<()>,
) -> Result<SchedulerReport, LocalBackupError> {
    validate_mode(task)?;
    if !task.enabled {
        return Err(LocalBackupError::TaskDisabled(task.id.clone()));
    }
    let mut report = SchedulerReport::default();
    let mut watcher = None;
    let mut source = validated_source_or_pause(task, &mut report)?;
    let mut debouncer = EventDebouncer::new(Duration::from_millis(task.schedule.debounce_ms));
    if source.is_some()
        && matches!(
            task.schedule.schedule_type,
            backup_core::ScheduleType::Realtime
        )
    {
        attach_watcher(&mut watcher, &source, &mut report);
        if watcher.is_none() {
            debouncer.request_full_reconcile();
        }
    }

    if task.schedule.scan_on_startup {
        record_scheduled_backup(store, task, &mut report);
        report.reconciliations += 1;
    }
    let interval = Duration::from_secs(u64::from(task.schedule.reconcile_interval_minutes) * 60);
    let mut next_reconcile = Instant::now() + reconcile_delay(&report, interval);
    let mut next_source_check = Instant::now() + SOURCE_REAPPEAR_CHECK_INTERVAL;

    loop {
        match shutdown.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }

        let now = Instant::now();
        if now >= next_source_check {
            let was_available = source.is_some();
            source = validated_source_or_pause(task, &mut report)?;
            if source.is_none() {
                watcher = None;
                next_reconcile = now + SOURCE_REAPPEAR_CHECK_INTERVAL;
            } else if !was_available {
                debouncer.request_full_reconcile();
            }
            next_source_check = now + SOURCE_REAPPEAR_CHECK_INTERVAL;
        }
        if debouncer.take_ready(now) || now >= next_reconcile {
            record_scheduled_backup(store, task, &mut report);
            report.reconciliations += 1;
            next_reconcile = Instant::now() + reconcile_delay(&report, interval);
            source = validated_source_or_pause(task, &mut report)?;
            if source.is_none() {
                watcher = None;
            } else if watcher.is_none()
                && matches!(
                    task.schedule.schedule_type,
                    backup_core::ScheduleType::Realtime
                )
            {
                attach_watcher(&mut watcher, &source, &mut report);
            }
            continue;
        }

        let next_deadline = debouncer
            .next_deadline()
            .unwrap_or(next_reconcile)
            .min(next_reconcile);
        let timeout = next_deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(250));
        let Some(active_watcher) = watcher.as_ref() else {
            std::thread::sleep(timeout);
            continue;
        };
        match active_watcher.events.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                if event.need_rescan() || event.paths.is_empty() {
                    debouncer.request_full_reconcile();
                } else {
                    for path in event.paths {
                        if source
                            .as_ref()
                            .is_some_and(|root| path.strip_prefix(root).is_ok())
                        {
                            debouncer.observe_path(path, Instant::now());
                        } else {
                            debouncer.request_full_reconcile();
                        }
                    }
                }
            }
            Ok(Err(error)) => {
                report
                    .watcher_error
                    .get_or_insert_with(|| error.to_string());
                debouncer.request_full_reconcile();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                report
                    .watcher_error
                    .get_or_insert_with(|| "watcher event channel disconnected".to_owned());
                watcher = None;
                debouncer.request_full_reconcile();
            }
        }
    }
    Ok(report)
}

fn record_scheduled_backup(
    store: &mut StateStore,
    task: &BackupTask,
    report: &mut SchedulerReport,
) {
    match backup_once(store, task) {
        Ok(summary) => {
            report.last_backup_status = Some(summary.status);
            if !summary.issues.is_empty() {
                report.last_backup_error = Some(summary.issues.join("; "));
            } else {
                report.last_backup_error = None;
            }
        }
        Err(error) => {
            report.last_backup_status = Some(
                source_unavailable_status(task, &error)
                    .unwrap_or("error")
                    .to_owned(),
            );
            report.last_backup_error = Some(error.to_string());
        }
    }
}

const SOURCE_REAPPEAR_CHECK_INTERVAL: Duration = Duration::from_secs(30);

fn reconcile_delay(report: &SchedulerReport, normal_interval: Duration) -> Duration {
    if matches!(
        report.last_backup_status.as_deref(),
        Some("paused" | "retry_wait")
    ) {
        SOURCE_REAPPEAR_CHECK_INTERVAL
    } else {
        normal_interval
    }
}

fn validated_source_or_pause(
    task: &BackupTask,
    report: &mut SchedulerReport,
) -> Result<Option<PathBuf>, LocalBackupError> {
    match validate_task_paths(task) {
        Ok(paths) => Ok(Some(paths.source)),
        Err(error) if is_source_unavailable_path_error(&error) => {
            report.last_backup_status = Some(
                if matches!(
                    task.safety.on_source_unavailable,
                    SourceUnavailablePolicy::Pause
                ) {
                    "paused"
                } else {
                    "retry_wait"
                }
                .to_owned(),
            );
            report.last_backup_error = Some(error.to_string());
            Ok(None)
        }
        Err(error) => Err(LocalBackupError::Paths(error)),
    }
}

fn should_pause_for_source(task: &BackupTask, error: &LocalBackupError) -> bool {
    source_unavailable_status(task, error) == Some("paused")
}

fn source_unavailable_status(task: &BackupTask, error: &LocalBackupError) -> Option<&'static str> {
    let unavailable = match error {
        LocalBackupError::Paths(path_error) => is_source_unavailable_path_error(path_error),
        LocalBackupError::Scan(ScanError::PathValidation(path_error)) => {
            is_source_unavailable_path_error(path_error)
        }
        _ => false,
    };
    if !unavailable {
        return None;
    }
    Some(
        if matches!(
            task.safety.on_source_unavailable,
            SourceUnavailablePolicy::Pause
        ) {
            "paused"
        } else {
            "retry_wait"
        },
    )
}

fn is_source_unavailable_path_error(error: &PathValidationError) -> bool {
    matches!(
        error,
        PathValidationError::SourceIo { .. }
            | PathValidationError::SourceNotReadable { .. }
            | PathValidationError::NoExistingAncestor(_)
    )
}

fn attach_watcher(
    watcher: &mut Option<LocalWatcher>,
    source: &Option<PathBuf>,
    report: &mut SchedulerReport,
) {
    let Some(source) = source else {
        return;
    };
    match LocalWatcher::start(source) {
        Ok(started) => *watcher = Some(started),
        Err(error) => {
            report.watcher_error = Some(error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backup_core::{
        DestinationsConfig, FilterConfig, GoogleDriveDestination, LocalDestination,
        PermissionDeniedPolicy, SafetyConfig, ScheduleConfig, ScheduleType, SourceConfig,
        SourceUnavailablePolicy, VersioningConfig,
    };

    fn task(source: &Path, destination: &Path) -> BackupTask {
        BackupTask {
            id: "local-test".to_owned(),
            name: "local test".to_owned(),
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

    fn directories() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("create isolated test directory");
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).expect("create temporary source");
        fs::create_dir(&destination).expect("create temporary target");
        (temp, source, destination)
    }

    #[test]
    fn copies_new_and_changed_files_but_does_not_revise_unchanged_content() {
        let (_temp, source, destination) = directories();
        fs::write(source.join("hello.txt"), b"first").unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();

        let first = backup_once(&mut store, &task).unwrap();
        assert_eq!(first.status, "succeeded");
        assert_eq!(first.copied_files, 1);
        assert_eq!(fs::read(destination.join("hello.txt")).unwrap(), b"first");

        let second = backup_once(&mut store, &task).unwrap();
        assert_eq!(second.copied_files, 0);
        assert_eq!(second.status, "succeeded");

        fs::write(source.join("hello.txt"), b"second revision").unwrap();
        let third = backup_once(&mut store, &task).unwrap();
        assert_eq!(third.copied_files, 1);
        assert_eq!(
            fs::read(destination.join("hello.txt")).unwrap(),
            b"second revision"
        );
    }

    #[test]
    fn source_deletion_keeps_the_existing_local_copy() {
        let (_temp, source, destination) = directories();
        fs::write(source.join("keep.txt"), b"still available").unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();
        backup_once(&mut store, &task).unwrap();
        fs::remove_file(source.join("keep.txt")).unwrap();

        let result = backup_once(&mut store, &task).unwrap();
        assert_eq!(result.source_missing_files, 1);
        assert_eq!(
            fs::read(destination.join("keep.txt")).unwrap(),
            b"still available"
        );
    }

    #[test]
    fn changing_exclusions_does_not_mark_existing_target_missing() {
        let (_temp, source, destination) = directories();
        fs::write(source.join("retain.txt"), b"value").unwrap();
        let mut task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();
        backup_once(&mut store, &task).unwrap();
        task.filters.exclude.push("retain.txt".to_owned());

        let result = backup_once(&mut store, &task).unwrap();
        assert_eq!(result.source_missing_files, 0);
        assert_eq!(fs::read(destination.join("retain.txt")).unwrap(), b"value");
    }

    #[test]
    fn missing_destination_is_not_created_automatically() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("not-created");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("data.txt"), b"data").unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();

        assert!(matches!(
            backup_once(&mut store, &task),
            Err(LocalBackupError::DestinationMissing(_))
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn event_debouncer_waits_two_seconds_and_coalesces_paths() {
        let start = Instant::now();
        let mut debouncer = EventDebouncer::new(Duration::from_secs(2));
        debouncer.observe_path(PathBuf::from("a"), start);
        debouncer.observe_path(PathBuf::from("b"), start + Duration::from_millis(500));
        assert!(!debouncer.take_ready(start + Duration::from_millis(1_999)));
        assert!(debouncer.take_ready(start + Duration::from_millis(2_500)));
        assert!(!debouncer.take_ready(start + Duration::from_secs(3)));
    }

    #[test]
    fn full_reconcile_signal_is_immediately_ready() {
        let mut debouncer = EventDebouncer::new(Duration::from_secs(2));
        debouncer.request_full_reconcile();
        assert!(debouncer.take_ready(Instant::now()));
    }

    #[test]
    fn large_missing_batch_is_left_for_review() {
        let (_temp, source, destination) = directories();
        for index in 0..10 {
            fs::write(source.join(format!("file-{index}.txt")), b"value").unwrap();
        }
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();
        backup_once(&mut store, &task).unwrap();
        for entry in fs::read_dir(&source).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }

        let result = backup_once(&mut store, &task).unwrap();
        assert!(result.deletion_review_required);
        assert_eq!(result.source_missing_files, 0);
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 10);
    }

    #[test]
    fn failed_snapshot_keeps_existing_target_untouched() {
        let (_temp, source, destination) = directories();
        let original_target = destination.join("item.txt");
        fs::write(&original_target, b"previous good copy").unwrap();
        fs::write(source.join("item.txt"), b"new source value").unwrap();
        let expected_hash = blake3::hash(b"not the source hash").to_hex().to_string();
        let error = create_stable_snapshot(&source.join("item.txt"), &destination, &expected_hash)
            .expect_err("mismatching source snapshot must not be committed");
        assert!(matches!(error, LocalBackupError::UnstableSource(_)));
        assert_eq!(fs::read(original_target).unwrap(), b"previous good copy");
    }

    #[test]
    fn source_unavailable_policy_pauses_without_marking_missing() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let mut task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();
        assert!(backup_once(&mut store, &task).is_err());

        let mut report = SchedulerReport::default();
        assert!(validated_source_or_pause(&task, &mut report)
            .unwrap()
            .is_none());
        assert_eq!(report.last_backup_status.as_deref(), Some("paused"));

        task.safety.on_source_unavailable = SourceUnavailablePolicy::Retry;
        let mut retry_report = SchedulerReport::default();
        assert!(validated_source_or_pause(&task, &mut retry_report)
            .unwrap()
            .is_none());
        assert_eq!(
            retry_report.last_backup_status.as_deref(),
            Some("retry_wait")
        );
    }

    #[test]
    fn does_not_follow_destination_symlinked_parent() {
        let (_temp, source, destination) = directories();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/a.txt"), b"value").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(source.join("nested"), destination.join("nested")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(source.join("nested"), destination.join("nested"))
            .unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();

        let summary = backup_once(&mut store, &task).unwrap();
        assert_eq!(summary.copied_files, 0);
        assert_eq!(summary.status, "partial_success");
        assert!(summary
            .issues
            .iter()
            .any(|issue| issue.contains("reparse point")));
        assert!(
            !source.join("nested/a.txt").is_file()
                || fs::read(source.join("nested/a.txt")).unwrap() == b"value"
        );
    }
}
