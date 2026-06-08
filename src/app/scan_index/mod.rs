//! SQLite-backed incremental scan index for normal reports.

mod aggregates;
mod config;
mod metadata;
mod records;
mod schema;
mod snapshot;

use super::model::{ReportKind, ScannerParallelism, UsageTotals};
use super::report::{
    ReportBuilder, SessionScanTarget, resolve_scan_worker_count,
    scan_selected_session_targets_with_observer,
};
use super::scan_runtime::{ScanBatchRunner, ScanObserver};
use super::session_log::{
    RawUsage, SessionParseCheckpoint,
    scan_session_file_from_checkpoint_with_observer_and_bytes_and_format,
};
use chrono::NaiveDate;
use chrono_tz::Tz;
use eyre::{Result, WrapErr, eyre};
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime};

use aggregates::{FileAggregateSet, replace_aggregate_rows};
pub(in crate::app) use config::{ScanIndexConfig, default_scan_index_path};
use metadata::{ObservedFile, ParsedContentHash, content_hash_prefix};
use records::{
    ScannedCacheEntry, ScannedFile, StoredFileRecord, update_file_record_conditionally,
    upsert_file_record,
};
use schema::initialize_schema;
use snapshot::ScanIndexSnapshot;

/// Timeout used for `SQLite` lock waits.
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_millis(500);
/// Cached files modified in this window are rechecked before aggregate reuse.
const RECENT_SESSION_REVALIDATION_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Inputs needed to scan selected targets with a persistent index.
pub(in crate::app) struct IndexedScanRequest<'a, R>
where
    R: ScanBatchRunner,
{
    /// Selected session files.
    pub(in crate::app) selected_files: &'a [SessionScanTarget],
    /// Scanner worker configuration.
    pub(in crate::app) parallelism: ScannerParallelism,
    /// Report kind being built.
    pub(in crate::app) kind: ReportKind,
    /// Grouping timezone.
    pub(in crate::app) timezone: Tz,
    /// Inclusive lower date bound.
    pub(in crate::app) since: Option<NaiveDate>,
    /// Inclusive upper date bound.
    pub(in crate::app) until: Option<NaiveDate>,
    /// Runtime scan observer provider.
    pub(in crate::app) scan_runner: &'a R,
    /// Scan-index configuration.
    pub(in crate::app) config: &'a ScanIndexConfig,
}

/// Scan selected targets with a best-effort persistent index.
pub(in crate::app) fn scan_selected_session_targets_with_index<R>(
    request: &IndexedScanRequest<'_, R>,
) -> Result<ReportBuilder>
where
    R: ScanBatchRunner,
{
    if !request.config.enabled {
        return scan_selected_session_targets_without_index(request);
    }

    let path = request
        .config
        .path
        .clone()
        .unwrap_or_else(default_scan_index_path);
    let mut index = match ScanIndex::open(&path) {
        Ok(index) => index,
        Err(error) => {
            warn_scan_index(&format!(
                "failed to open scan index {}; falling back to full scan: {error:#}",
                path.display()
            ));
            return scan_selected_session_targets_without_index(request);
        }
    };

    indexed_scan_selected_targets(&mut index, request, true)
}

/// Update the persistent index for selected targets without report fallback scans.
pub(in crate::app) fn update_selected_session_targets_in_index<R>(
    request: &IndexedScanRequest<'_, R>,
) -> Result<()>
where
    R: ScanBatchRunner,
{
    if !request.config.enabled || request.selected_files.is_empty() {
        return Ok(());
    }

    let path = request
        .config
        .path
        .clone()
        .unwrap_or_else(default_scan_index_path);
    let mut index = ScanIndex::open(&path)?;
    let _ = indexed_scan_selected_targets(&mut index, request, false)?;
    Ok(())
}

/// Scan selected targets without using the persistent index.
fn scan_selected_session_targets_without_index<R>(
    request: &IndexedScanRequest<'_, R>,
) -> Result<ReportBuilder>
where
    R: ScanBatchRunner,
{
    request
        .scan_runner
        .run_batch(request.selected_files.len(), |observer| {
            scan_selected_session_targets_with_observer(
                request.selected_files,
                request.parallelism,
                request.kind,
                request.timezone,
                request.since,
                request.until,
                observer,
            )
        })
}

/// Perform the indexed scan once the `SQLite` connection is available.
fn indexed_scan_selected_targets<R>(
    index: &mut ScanIndex,
    request: &IndexedScanRequest<'_, R>,
    fallback_to_full_scan: bool,
) -> Result<ReportBuilder>
where
    R: ScanBatchRunner,
{
    let mut builder =
        ReportBuilder::new(request.kind, request.timezone, request.since, request.until);
    let session_keys = request
        .selected_files
        .iter()
        .map(|target| target.session_id.as_str())
        .collect::<Vec<_>>();
    let mut snapshot = match ScanIndexSnapshot::load_selected(
        &index.connection,
        request.timezone,
        &session_keys,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            if !fallback_to_full_scan {
                return Err(error).wrap_err("failed to read scan index");
            }
            warn_scan_index(&format!(
                "failed to read scan index; falling back to full scan: {error:#}"
            ));
            return scan_selected_session_targets_without_index(request);
        }
    };
    let mut scan_tasks = Vec::new();
    let cache_revalidation_now = SystemTime::now();

    for target in request.selected_files {
        match ScanIndex::plan_target(&mut snapshot, target, cache_revalidation_now) {
            Ok(IndexedTargetPlan::Cached { aggregates }) => {
                aggregates.merge_into_report_builder(&mut builder, target);
            }
            Ok(IndexedTargetPlan::Append {
                target,
                record,
                cached_aggregates,
            }) => {
                scan_tasks.push(IndexedScanTask::Append {
                    target,
                    record,
                    cached_aggregates,
                });
            }
            Ok(IndexedTargetPlan::Rebuild {
                target,
                previous,
                content_changed,
            }) => {
                scan_tasks.push(IndexedScanTask::Rebuild {
                    target,
                    previous,
                    content_changed,
                });
            }
            Err(error) => {
                if !fallback_to_full_scan {
                    return Err(error).wrap_err("failed to read scan index");
                }
                warn_scan_index(&format!(
                    "failed to read scan index; falling back to full scan: {error:#}"
                ));
                return scan_selected_session_targets_without_index(request);
            }
        }
    }

    let completed = request
        .scan_runner
        .run_batch(scan_tasks.len(), |observer| {
            scan_index_tasks_with_observer(
                &scan_tasks,
                request.parallelism,
                request.timezone,
                observer,
            )
        })?;
    for task in completed {
        task.merge_into_report_builder(&mut builder);
        index.write_completed_task(request.timezone, &task);
    }

    Ok(builder)
}

/// Print one non-fatal scan-index warning.
fn warn_scan_index(message: &str) {
    eprintln!("Warning: {message}");
}

/// `SQLite` scan-index connection.
struct ScanIndex {
    /// Open `SQLite` connection.
    connection: Connection,
}

impl ScanIndex {
    /// Open and initialize the scan-index database.
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).wrap_err_with(|| {
                format!("failed to create scan index directory {}", parent.display())
            })?;
        }

        let mut connection = Connection::open(path)
            .wrap_err_with(|| format!("failed to open scan index {}", path.display()))?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        initialize_schema(&mut connection)?;
        Ok(Self { connection })
    }

    /// Build a scan plan for one selected session target.
    fn plan_target(
        snapshot: &mut ScanIndexSnapshot,
        target: &SessionScanTarget,
        cache_revalidation_now: SystemTime,
    ) -> Result<IndexedTargetPlan> {
        let observed = ObservedFile::from_target(target);
        let Some(record) = snapshot.take_record(&target.session_id) else {
            return Ok(IndexedTargetPlan::Rebuild {
                target: target.clone(),
                previous: None,
                content_changed: true,
            });
        };
        if !record.is_compatible_with(&observed) {
            return Ok(IndexedTargetPlan::Rebuild {
                target: target.clone(),
                previous: Some(record),
                content_changed: true,
            });
        }

        let aggregates = snapshot.take_aggregates(&record);
        if record.metadata.same_contents_as(&observed.metadata) {
            if !should_revalidate_cached_target(target, cache_revalidation_now) {
                return Ok(Self::plan_with_matching_metadata(
                    target.clone(),
                    record,
                    aggregates,
                ));
            }
            return Self::plan_with_fresh_metadata(target, record, aggregates);
        }

        match aggregates {
            AggregateLoad::Valid(cached_aggregates) if record.can_append_to(&observed) => {
                Ok(IndexedTargetPlan::Append {
                    target: target.clone(),
                    record,
                    cached_aggregates,
                })
            }
            AggregateLoad::Valid(_) | AggregateLoad::MissingOrInvalid => {
                Ok(IndexedTargetPlan::Rebuild {
                    target: target.clone(),
                    previous: Some(record),
                    content_changed: true,
                })
            }
        }
    }

    /// Build a plan after metadata has been accepted as unchanged.
    fn plan_with_matching_metadata(
        target: SessionScanTarget,
        record: StoredFileRecord,
        aggregates: AggregateLoad,
    ) -> IndexedTargetPlan {
        match aggregates {
            AggregateLoad::Valid(aggregates) => IndexedTargetPlan::Cached { aggregates },
            AggregateLoad::MissingOrInvalid => IndexedTargetPlan::Rebuild {
                target,
                previous: Some(record),
                content_changed: false,
            },
        }
    }

    /// Re-check metadata immediately before reusing cached aggregates.
    fn plan_with_fresh_metadata(
        target: &SessionScanTarget,
        record: StoredFileRecord,
        aggregates: AggregateLoad,
    ) -> Result<IndexedTargetPlan> {
        let refreshed = target.refresh_metadata()?;
        let observed = ObservedFile::from_target(&refreshed);
        if !record.is_compatible_with(&observed) {
            return Ok(IndexedTargetPlan::Rebuild {
                target: refreshed,
                previous: Some(record),
                content_changed: true,
            });
        }

        if record.metadata.same_contents_as(&observed.metadata) {
            return Ok(Self::plan_with_matching_metadata(
                refreshed, record, aggregates,
            ));
        }

        match aggregates {
            AggregateLoad::Valid(cached_aggregates) if record.can_append_to(&observed) => {
                Ok(IndexedTargetPlan::Append {
                    target: refreshed,
                    record,
                    cached_aggregates,
                })
            }
            AggregateLoad::Valid(_) | AggregateLoad::MissingOrInvalid => {
                Ok(IndexedTargetPlan::Rebuild {
                    target: refreshed,
                    previous: Some(record),
                    content_changed: true,
                })
            }
        }
    }

    /// Best-effort write for one completed scan task.
    fn write_completed_task(&mut self, timezone: Tz, task: &CompletedScanTask) {
        let result = match task {
            CompletedScanTask::Append {
                record, scanned, ..
            } => self.write_appended_file(timezone, record, scanned),
            CompletedScanTask::Rebuild {
                previous,
                content_changed,
                scanned,
            } => self.write_rebuilt_file(timezone, previous.as_ref(), *content_changed, scanned),
        };
        if let Err(error) = result {
            warn_scan_index(&format!("failed to update scan index: {error:#}"));
        }
    }

    /// Persist an append update if no other writer advanced the same row first.
    fn write_appended_file(
        &mut self,
        timezone: Tz,
        record: &StoredFileRecord,
        scanned: &ScannedFile,
    ) -> Result<()> {
        let Some(cache_entry) = scanned.cache_entry.as_ref() else {
            return Err(eyre!(
                "scan-index metadata was unavailable after append scan"
            ));
        };
        let generation = record.generation.saturating_add(1);
        let transaction = self.connection.transaction()?;
        let changed = update_file_record_conditionally(
            &transaction,
            record,
            generation,
            cache_entry,
            &scanned.aggregates,
        )?;
        if changed == 1 {
            replace_aggregate_rows(
                &transaction,
                record.session_key.as_str(),
                timezone,
                generation,
                true,
                &scanned.aggregates,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Persist a rebuilt file and its aggregate rows.
    fn write_rebuilt_file(
        &mut self,
        timezone: Tz,
        previous: Option<&StoredFileRecord>,
        content_changed: bool,
        scanned: &ScannedFile,
    ) -> Result<()> {
        let Some(cache_entry) = scanned.cache_entry.as_ref() else {
            return Err(eyre!("scan-index metadata was unavailable after full scan"));
        };
        let generation = match (previous, content_changed) {
            (Some(record), true) => record.generation.saturating_add(1),
            (Some(record), false) => record.generation,
            (None, _) => 1,
        };
        let transaction = self.connection.transaction()?;
        let changed = if let Some(record) = previous {
            update_file_record_conditionally(
                &transaction,
                record,
                generation,
                cache_entry,
                &scanned.aggregates,
            )?
        } else {
            upsert_file_record(&transaction, generation, cache_entry, &scanned.aggregates)?;
            1
        };
        if changed == 1 {
            replace_aggregate_rows(
                &transaction,
                cache_entry.session_key.as_str(),
                timezone,
                generation,
                content_changed,
                &scanned.aggregates,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

/// Return whether a cached file is recent enough to warrant a close-to-use stat.
fn should_revalidate_cached_target(target: &SessionScanTarget, now: SystemTime) -> bool {
    let Some(modified) = target.modified else {
        return true;
    };
    now.duration_since(modified)
        .map_or(true, |age| age <= RECENT_SESSION_REVALIDATION_WINDOW)
}

/// High-level cache decision for one selected target.
enum IndexedTargetPlan {
    /// The target is unchanged and all aggregates are reusable.
    Cached {
        /// Cached local-day aggregates.
        aggregates: FileAggregateSet,
    },
    /// The target only grew and the indexed prefix was validated.
    Append {
        /// Refreshed file target.
        target: SessionScanTarget,
        /// Stored file record used to validate the write.
        record: StoredFileRecord,
        /// Cached aggregates from the previous parsed prefix.
        cached_aggregates: FileAggregateSet,
    },
    /// The target must be fully scanned.
    Rebuild {
        /// File target to scan.
        target: SessionScanTarget,
        /// Previous stored record, when one existed.
        previous: Option<StoredFileRecord>,
        /// Whether the file contents changed rather than only missing this timezone's aggregates.
        content_changed: bool,
    },
}

/// Result of loading aggregate rows for one record.
enum AggregateLoad {
    /// Aggregate rows are valid for the current file generation.
    Valid(FileAggregateSet),
    /// Aggregate rows are absent or do not match file-level totals.
    MissingOrInvalid,
}

/// One parse task required by an indexed report.
#[derive(Clone)]
enum IndexedScanTask {
    /// Parse only the appended suffix.
    Append {
        /// Selected file target.
        target: SessionScanTarget,
        /// Stored file record at classification time.
        record: StoredFileRecord,
        /// Cached aggregates from the parsed prefix.
        cached_aggregates: FileAggregateSet,
    },
    /// Parse the entire selected file.
    Rebuild {
        /// Selected file target.
        target: SessionScanTarget,
        /// Previous stored record, when one existed.
        previous: Option<StoredFileRecord>,
        /// Whether the file contents changed.
        content_changed: bool,
    },
}

impl IndexedScanTask {
    /// Return the scheduling weight for this task.
    fn bytes(&self) -> u64 {
        match self {
            Self::Append { target, record, .. } => {
                target.bytes.saturating_sub(record.checkpoint.offset)
            }
            Self::Rebuild { target, .. } => target.bytes,
        }
    }
}

/// One completed parse task.
enum CompletedScanTask {
    /// Completed append scan.
    Append {
        /// Stored file record at classification time.
        record: StoredFileRecord,
        /// Full aggregate set used for cache writes.
        scanned: ScannedFile,
    },
    /// Completed full rebuild scan.
    Rebuild {
        /// Previous stored record, when one existed.
        previous: Option<StoredFileRecord>,
        /// Whether the file contents changed.
        content_changed: bool,
        /// Full scan result.
        scanned: ScannedFile,
    },
}

impl CompletedScanTask {
    /// Merge this completed task's report-visible contribution.
    fn merge_into_report_builder(&self, builder: &mut ReportBuilder) {
        match self {
            Self::Append { scanned, .. } | Self::Rebuild { scanned, .. } => {
                scanned
                    .aggregates
                    .merge_into_report_builder(builder, &scanned.target);
            }
        }
    }
}

/// Scan all tasks using the requested parallelism.
fn scan_index_tasks_with_observer<O>(
    tasks: &[IndexedScanTask],
    parallelism: ScannerParallelism,
    timezone: Tz,
    observer: &O,
) -> Result<Vec<CompletedScanTask>>
where
    O: ScanObserver,
{
    if tasks.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = resolve_scan_worker_count(parallelism, tasks.len());
    if worker_count == 1 {
        return scan_index_task_chunk(tasks, timezone, observer);
    }

    let chunks = balanced_index_scan_chunks(tasks, worker_count);
    thread::scope(|scope| -> Result<Vec<CompletedScanTask>> {
        let mut chunks = chunks.into_iter();
        let first_chunk = chunks
            .next()
            .ok_or_else(|| eyre!("missing initial indexed scan chunk"))?;
        let handles = chunks
            .map(|chunk| {
                let observer = observer.clone();
                scope.spawn(move || scan_index_task_chunk(&chunk, timezone, &observer))
            })
            .collect::<Vec<_>>();

        let mut completed = scan_index_task_chunk(&first_chunk, timezone, observer)?;
        for handle in handles {
            completed.extend(
                handle
                    .join()
                    .map_err(|_| eyre!("indexed scan worker panicked"))??,
            );
        }
        Ok(completed)
    })
}

/// Scan one chunk of indexed tasks.
fn scan_index_task_chunk<O>(
    tasks: &[IndexedScanTask],
    timezone: Tz,
    observer: &O,
) -> Result<Vec<CompletedScanTask>>
where
    O: ScanObserver,
{
    tasks
        .iter()
        .map(|task| scan_index_task(task, timezone, observer))
        .collect()
}

/// Scan one indexed task.
fn scan_index_task<O>(
    task: &IndexedScanTask,
    timezone: Tz,
    observer: &O,
) -> Result<CompletedScanTask>
where
    O: ScanObserver,
{
    match task {
        IndexedScanTask::Append {
            target,
            record,
            cached_aggregates,
        } => {
            if !content_hash_prefix(&target.path, record.checkpoint.offset)
                .is_ok_and(|actual| actual == record.content_hash)
            {
                let scanned = scan_file_aggregates(
                    target,
                    timezone,
                    &SessionParseCheckpoint::default(),
                    observer,
                )?;
                return Ok(CompletedScanTask::Rebuild {
                    previous: Some(record.clone()),
                    content_changed: true,
                    scanned,
                });
            }

            let appended = scan_file_aggregates(target, timezone, &record.checkpoint, observer)?;
            let mut full_aggregates = cached_aggregates.clone();
            full_aggregates.merge(appended.aggregates.clone());
            let scanned = appended.with_aggregates(full_aggregates);
            Ok(CompletedScanTask::Append {
                record: record.clone(),
                scanned,
            })
        }
        IndexedScanTask::Rebuild {
            target,
            previous,
            content_changed,
        } => {
            let scanned = scan_file_aggregates(
                target,
                timezone,
                &SessionParseCheckpoint::default(),
                observer,
            )?;
            Ok(CompletedScanTask::Rebuild {
                previous: previous.clone(),
                content_changed: *content_changed,
                scanned,
            })
        }
    }
}

/// Split scan-index tasks into roughly balanced chunks.
fn balanced_index_scan_chunks(
    tasks: &[IndexedScanTask],
    worker_count: usize,
) -> Vec<Vec<IndexedScanTask>> {
    let worker_count = worker_count.clamp(1, tasks.len());
    let mut chunks = (0..worker_count)
        .map(|_| WeightedIndexChunk::default())
        .collect::<Vec<_>>();
    let mut ordered = tasks.iter().collect::<Vec<_>>();
    ordered.sort_unstable_by_key(|task| std::cmp::Reverse(task.bytes()));
    for task in ordered {
        let chunk = chunks
            .iter_mut()
            .min_by_key(|chunk| (chunk.bytes, chunk.tasks.len()))
            .expect("worker count is at least one");
        chunk.bytes = chunk.bytes.saturating_add(task.bytes());
        chunk.tasks.push(task.clone());
    }
    chunks
        .into_iter()
        .map(|chunk| chunk.tasks)
        .filter(|chunk| !chunk.is_empty())
        .collect()
}

/// Weighted chunk used while balancing indexed scan work.
#[derive(Default)]
struct WeightedIndexChunk {
    /// Tasks assigned to this chunk.
    tasks: Vec<IndexedScanTask>,
    /// Sum of task byte weights.
    bytes: u64,
}

/// Scan one file from the given checkpoint and aggregate parsed events by local day.
fn scan_file_aggregates<O>(
    target: &SessionScanTarget,
    timezone: Tz,
    start_checkpoint: &SessionParseCheckpoint,
    observer: &O,
) -> Result<ScannedFile>
where
    O: ScanObserver,
{
    let mut aggregates = FileAggregateSet::default();
    let mut parsed_hash = ParsedContentHash::default();
    let checkpoint = scan_session_file_from_checkpoint_with_observer_and_bytes_and_format(
        &target.path,
        &target.session_id,
        target.file_format,
        start_checkpoint,
        observer,
        |bytes| parsed_hash.observe(bytes),
        |event| aggregates.observe(event, timezone),
    )?;
    observer.on_file_complete();
    let content_hash = if start_checkpoint.offset == 0 {
        (target.file_format.is_compressed() || parsed_hash.offset() == checkpoint.offset)
            .then(|| parsed_hash.finish())
    } else {
        content_hash_prefix(&target.path, checkpoint.offset).ok()
    };
    let cache_entry = content_hash.and_then(|content_hash| {
        target.refresh_metadata().ok().and_then(|refreshed| {
            scanned_metadata_matches_target(&checkpoint, target, &refreshed)
                .then(|| ScannedCacheEntry::from_scan(&refreshed, &checkpoint, content_hash))
        })
    });
    Ok(ScannedFile {
        target: target.clone(),
        aggregates,
        cache_entry,
    })
}

/// Return whether the post-scan metadata still describes the selected target bytes.
fn scanned_metadata_matches_target(
    checkpoint: &SessionParseCheckpoint,
    target: &SessionScanTarget,
    refreshed: &SessionScanTarget,
) -> bool {
    refreshed.bytes == checkpoint.offset
        && refreshed.metadata.mtime_ns == target.metadata.mtime_ns
        && refreshed.metadata.dev == target.metadata.dev
        && refreshed.metadata.ino == target.metadata.ino
        && refreshed.metadata.ctime_ns == target.metadata.ctime_ns
}

/// Convert a `u64` into `SQLite`'s signed integer range.
fn u64_to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).wrap_err("token or size counter exceeds SQLite integer range")
}

/// Convert a non-negative `SQLite` integer to `u64`.
fn i64_to_u64(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

/// Convert a boolean into `SQLite`'s integer representation.
const fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

/// Convert five `SQLite` counters into usage totals.
fn usage_from_i64(
    input: i64,
    cached_input: i64,
    output: i64,
    reasoning_output: i64,
    total: i64,
) -> Option<UsageTotals> {
    Some(UsageTotals {
        input: i64_to_u64(input)?,
        cached_input: i64_to_u64(cached_input)?,
        output: i64_to_u64(output)?,
        reasoning_output: i64_to_u64(reasoning_output)?,
        total: i64_to_u64(total)?,
    })
}

/// Convert nullable previous-total counters into parser raw usage.
fn raw_usage_from_options(
    input: Option<i64>,
    cached_input: Option<i64>,
    output: Option<i64>,
    reasoning_output: Option<i64>,
    total: Option<i64>,
) -> std::result::Result<Option<RawUsage>, ()> {
    match (input, cached_input, output, reasoning_output, total) {
        (None, None, None, None, None) => Ok(None),
        (Some(input), Some(cached_input), Some(output), Some(reasoning_output), Some(total)) => {
            Ok(Some(RawUsage {
                input: i64_to_u64(input).ok_or(())?,
                cached_input: i64_to_u64(cached_input).ok_or(())?,
                output: i64_to_u64(output).ok_or(())?,
                reasoning_output: i64_to_u64(reasoning_output).ok_or(())?,
                total: i64_to_u64(total).ok_or(())?,
            }))
        }
        _ => Err(()),
    }
}
