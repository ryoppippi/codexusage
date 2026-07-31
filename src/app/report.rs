//! Report preparation, aggregation, and session target selection.

use super::model::{
    CachedInputCostMode, DEFAULT_CODEX_HOME_ENV, DailyRow, MILLION, ModelBreakdown, MonthlyRow,
    ReportKind, ReportOptions, ReportOutput, ScannerParallelism, SessionRow, Totals,
    UsagePresentation, UsageTotals, explicit_usage,
};
use super::scan_index::{
    IndexedScanRequest, ScanIndexConfig, scan_selected_session_targets_with_index,
};
use super::scan_runtime::{CliScanBatchRunner, ScanBehavior, ScanObserver};
use super::session_files::{
    SessionFileFormat, existing_session_path, plain_session_path, session_file_format,
    should_skip_format,
};
use super::session_lineage::{SessionDescriptor, read_session_descriptor};
#[cfg(test)]
use super::session_log::scan_session_file_from_checkpoint;
use super::session_log::{
    SessionParseCheckpoint, TokenUsageEvent, scan_session_file_from_checkpoint_with_format,
    scan_session_file_with_callback_and_observer_with_format,
};
use crate::pricing::{Pricing, PricingCatalog, PricingLoadOptions, load_pricing_catalog};
use chrono::{DateTime, Datelike, Days, NaiveDate, Utc};
use chrono_tz::Tz;
use eyre::{Result, WrapErr, eyre};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Multiplier applied to automatic report-scan worker selection.
const AUTO_WORKER_MULTIPLIER: usize = 3;

/// Compact calendar-month grouping key in `YYYYMM` form.
type MonthKey = i32;

/// Shared report inputs resolved once before scanning begins.
pub(in crate::app) struct PreparedReport {
    /// Timezone used for grouping and date filters.
    timezone: Tz,
    /// Inclusive lower bound after normalization.
    since: Option<NaiveDate>,
    /// Inclusive upper bound after normalization.
    until: Option<NaiveDate>,
    /// Session roots selected for this run.
    session_dirs: Vec<PathBuf>,
    /// Optional project working-directory filter.
    project_filter: Option<ProjectFilter>,
    /// Pricing catalog loaded for the final render.
    pricing: PricingCatalog,
    /// Usage and cost presentation behavior.
    presentation: UsagePresentation,
}

/// Component-aware filter for sessions recorded under one project directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::app) struct ProjectFilter {
    /// Normalized absolute project root.
    pub(in crate::app) root: PathBuf,
}

impl ProjectFilter {
    /// Build an optional filter from an optional CLI or API path.
    pub(in crate::app) fn from_path_option(path: Option<&Path>) -> Result<Option<Self>> {
        path.map(Self::from_path).transpose()
    }

    /// Build a filter from a CLI or API path.
    pub(in crate::app) fn from_path(path: &Path) -> Result<Self> {
        Ok(Self {
            root: normalize_absolute_path(path)?,
        })
    }

    /// Return whether a logged working directory belongs to this project tree.
    pub(in crate::app) fn matches_logged_cwd(&self, cwd: &Path) -> Result<bool> {
        Ok(normalize_absolute_path(cwd)?.starts_with(&self.root))
    }
}

/// Resolve shared report inputs before scanning.
pub(in crate::app) fn prepare_report(
    kind: ReportKind,
    options: &ReportOptions,
) -> Result<PreparedReport> {
    let timezone = parse_timezone(&options.timezone)?;
    let (since, until) = resolve_report_date_filters(
        kind,
        options.last_days,
        options.since.as_deref(),
        options.until.as_deref(),
        timezone,
        Utc::now(),
    )?;
    Ok(PreparedReport {
        timezone,
        since,
        until,
        session_dirs: resolve_session_dirs(&options.session_dirs),
        project_filter: ProjectFilter::from_path_option(options.project_dir.as_deref())?,
        pricing: load_pricing_catalog(&PricingLoadOptions {
            offline: options.offline,
            force_refresh: options.refresh_pricing,
        })?,
        presentation: UsagePresentation::new(
            options.cached_input_cost_mode,
            options.cache_read_mode,
        ),
    })
}

/// Build the requested report from the provided session directories.
///
/// # Errors
///
/// Returns an error when date filters are invalid, pricing setup fails, session files cannot be
/// read, or an event timestamp is malformed.
pub fn build_report(kind: ReportKind, options: &ReportOptions) -> Result<ReportOutput> {
    let prepared = prepare_report(kind, options)?;
    build_report_from_prepared_targets(
        kind,
        &prepared,
        options.parallelism,
        |selected_files, parallelism, kind, timezone, since, until| {
            scan_selected_session_targets(selected_files, parallelism, kind, timezone, since, until)
        },
    )
}

/// Build the requested report with CLI-only scan behavior enabled.
pub(in crate::app) fn build_report_for_cli(
    kind: ReportKind,
    options: &ReportOptions,
    scan_behavior: ScanBehavior,
    scan_index: &ScanIndexConfig,
) -> Result<ReportOutput> {
    let prepared = prepare_report(kind, options)?;
    let scan_runner = CliScanBatchRunner::new(scan_behavior);
    build_report_from_prepared_targets(
        kind,
        &prepared,
        options.parallelism,
        |selected_files, parallelism, kind, timezone, since, until| {
            scan_selected_session_targets_with_index(&IndexedScanRequest {
                selected_files,
                parallelism,
                kind,
                timezone,
                since,
                until,
                scan_runner: &scan_runner,
                config: scan_index,
            })
        },
    )
}

/// Build the requested report with an explicit persistent scan-index path.
///
/// # Errors
///
/// Returns an error when normal report preparation or session parsing fails. Scan-index read and
/// write failures are warnings and fall back to normal scanning.
pub fn build_report_with_scan_index(
    kind: ReportKind,
    options: &ReportOptions,
    scan_index_path: &Path,
) -> Result<ReportOutput> {
    use super::scan_runtime::NoopScanBatchRunner;

    let prepared = prepare_report(kind, options)?;
    let scan_runner = NoopScanBatchRunner;
    let scan_index = ScanIndexConfig::with_path(scan_index_path.to_path_buf());
    build_report_from_prepared_targets(
        kind,
        &prepared,
        options.parallelism,
        |selected_files, parallelism, kind, timezone, since, until| {
            scan_selected_session_targets_with_index(&IndexedScanRequest {
                selected_files,
                parallelism,
                kind,
                timezone,
                since,
                until,
                scan_runner: &scan_runner,
                config: &scan_index,
            })
        },
    )
}

/// Finish one report from prepared shared inputs and a selected-target scan strategy.
pub(in crate::app) fn build_report_from_prepared_targets<S>(
    kind: ReportKind,
    prepared: &PreparedReport,
    parallelism: ScannerParallelism,
    scan_targets: S,
) -> Result<ReportOutput>
where
    S: FnOnce(
        &[SessionScanTarget],
        ScannerParallelism,
        ReportKind,
        Tz,
        Option<NaiveDate>,
        Option<NaiveDate>,
    ) -> Result<ReportBuilder>,
{
    let mut builder = ReportBuilder::new(kind, prepared.timezone, prepared.since, prepared.until);
    let (missing_directories, selected_files) =
        collect_session_scan_targets(&prepared.session_dirs, prepared.project_filter.as_ref())?;
    let scanned = scan_targets(
        &selected_files,
        parallelism,
        kind,
        prepared.timezone,
        prepared.since,
        prepared.until,
    )?;
    builder.merge(scanned);
    if kind == ReportKind::Session {
        let attributions = build_session_attributions(&selected_files)?;
        builder.fold_session_summaries(&attributions);
    }
    Ok(builder.finish(
        &prepared.pricing,
        prepared.presentation,
        missing_directories,
    ))
}

/// Parse a timezone name.
pub(in crate::app) fn parse_timezone(candidate: &str) -> Result<Tz> {
    candidate
        .parse::<Tz>()
        .wrap_err_with(|| format!("invalid timezone {candidate}"))
}

/// Determine the default timezone name from the local system configuration.
pub(in crate::app) fn default_timezone_name() -> String {
    std::env::var("TZ")
        .ok()
        .as_deref()
        .and_then(normalize_timezone_name)
        .or_else(|| {
            fs::read_to_string("/etc/timezone")
                .ok()
                .and_then(|contents| timezone_from_etc_timezone_contents(&contents))
        })
        .or_else(|| {
            fs::read_link("/etc/localtime")
                .ok()
                .and_then(|target| timezone_from_localtime_target(&target))
        })
        .unwrap_or_else(|| "UTC".to_string())
}

/// Normalize a timezone string from configuration sources.
pub(in crate::app) fn normalize_timezone_name(candidate: &str) -> Option<String> {
    let normalized = candidate.trim().trim_start_matches(':');
    if normalized.is_empty() || normalized.parse::<Tz>().is_err() {
        return None;
    }

    Some(normalized.to_string())
}

/// Extract a timezone name from `/etc/timezone` contents.
pub(in crate::app) fn timezone_from_etc_timezone_contents(contents: &str) -> Option<String> {
    contents.lines().find_map(normalize_timezone_name)
}

/// Extract a timezone name from an `/etc/localtime` symlink target.
pub(in crate::app) fn timezone_from_localtime_target(target: &Path) -> Option<String> {
    target
        .to_string_lossy()
        .rsplit_once("zoneinfo/")
        .and_then(|(_, timezone)| normalize_timezone_name(timezone))
}

/// Normalize a filter date.
pub(in crate::app) fn normalize_filter_date(value: &str) -> Result<NaiveDate> {
    let compact = value.trim().replace('-', "");
    if compact.len() != 8 || !compact.chars().all(|character| character.is_ascii_digit()) {
        return Err(eyre!(
            "invalid date format {value}; expected YYYYMMDD or YYYY-MM-DD"
        ));
    }
    NaiveDate::parse_from_str(&compact, "%Y%m%d")
        .wrap_err_with(|| format!("failed to parse date {value}"))
}

/// Convert one local date into a compact sortable month key.
fn month_key(date: NaiveDate) -> MonthKey {
    date.year() * 100 + i32::try_from(date.month()).unwrap_or_default()
}

/// Format one daily report key for public output.
fn format_day_key(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// Format one monthly report key for public output.
fn format_month_key(key: MonthKey) -> String {
    let year = key.div_euclid(100);
    let month = key.rem_euclid(100);
    format!("{year:04}-{month:02}")
}

/// Resolve the effective date filters for the requested report.
pub(in crate::app) fn resolve_report_date_filters(
    kind: ReportKind,
    last_days: Option<NonZeroUsize>,
    since: Option<&str>,
    until: Option<&str>,
    timezone: Tz,
    now_utc: DateTime<Utc>,
) -> Result<(Option<NaiveDate>, Option<NaiveDate>)> {
    if last_days.is_some() && kind != ReportKind::Daily {
        return Err(eyre!("last_days is only supported for the daily report"));
    }
    if last_days.is_some() && (since.is_some() || until.is_some()) {
        return Err(eyre!(
            "last_days cannot be combined with explicit since/until filters"
        ));
    }

    match last_days {
        Some(last_days) => {
            let until = now_utc.with_timezone(&timezone).date_naive();
            let offset_days = u64::try_from(last_days.get() - 1)
                .wrap_err("failed to convert last_days window width")?;
            let since = until
                .checked_sub_days(Days::new(offset_days))
                .ok_or_else(|| eyre!("last_days window underflowed the supported date range"))?;
            Ok((Some(since), Some(until)))
        }
        None => Ok((
            since.map(normalize_filter_date).transpose()?,
            until.map(normalize_filter_date).transpose()?,
        )),
    }
}

/// Resolve session directories from CLI options or environment defaults.
pub(in crate::app) fn resolve_session_dirs(session_dirs: &[PathBuf]) -> Vec<PathBuf> {
    if !session_dirs.is_empty() {
        return session_dirs.to_vec();
    }

    let codex_home = std::env::var_os(DEFAULT_CODEX_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"));
    vec![codex_home.join("sessions")]
}

/// Resolve CLI roots, optionally adding Pi's session store to the Codex default.
pub(in crate::app) fn resolve_cli_session_dirs(
    session_dirs: &[PathBuf],
    include_pi: bool,
) -> Vec<PathBuf> {
    if !session_dirs.is_empty() {
        return session_dirs.to_vec();
    }

    let mut resolved = resolve_session_dirs(session_dirs);
    if include_pi {
        let pi_sessions = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map_or_else(default_pi_session_dir, expand_pi_path);
        if pi_sessions.is_dir() && !resolved.contains(&pi_sessions) {
            resolved.push(pi_sessions);
        }
    }
    resolved
}

/// Resolve Pi's default session directory from its configurable agent directory.
fn default_pi_session_dir() -> PathBuf {
    let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(expand_pi_path)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pi").join("agent"))
        })
        .unwrap_or_else(|| PathBuf::from(".pi").join("agent"));
    agent_dir.join("sessions")
}

/// Expand Pi's documented `~` and `~/...` directory forms.
fn expand_pi_path(path: PathBuf) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path;
    };
    if value == "~" {
        return std::env::var_os("HOME").map_or(path, PathBuf::from);
    }
    let Some(relative) = value.strip_prefix("~/") else {
        return path;
    };
    std::env::var_os("HOME").map_or(path.clone(), |home| PathBuf::from(home).join(relative))
}

/// Convert a path into an absolute lexical form without touching the filesystem.
pub(in crate::app) fn normalize_absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .wrap_err_with(|| {
                format!("failed to resolve current directory for {}", path.display())
            })?
            .join(path)
    };

    Ok(normalize_path_components(&absolute))
}

/// Convert a session file path into a stable cache key without filesystem canonicalization.
#[cfg(test)]
pub(in crate::app) fn session_target_path_key(path: &Path) -> Result<String> {
    session_target_path_key_for_format(
        path,
        session_file_format(path).unwrap_or(SessionFileFormat::Plain),
    )
}

/// Convert a session file path with known format into a stable logical cache key.
fn session_target_path_key_for_format(
    path: &Path,
    file_format: SessionFileFormat,
) -> Result<String> {
    let plain_path;
    let logical_path = if file_format.is_compressed() {
        plain_path = plain_session_path(path);
        plain_path.as_path()
    } else {
        path
    };

    Ok(normalize_absolute_path(logical_path)?
        .to_string_lossy()
        .into_owned())
}

/// Collapse `.` and `..` components from an already absolute path.
pub(in crate::app) fn normalize_path_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let _ = normalized.pop();
            }
            std::path::Component::Normal(segment) => normalized.push(segment),
        }
    }

    normalized
}

/// Convert `SystemTime` into nanoseconds since Unix epoch.
fn system_time_to_nanos(value: SystemTime) -> Option<i64> {
    let nanos = value.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    i64::try_from(nanos).ok()
}

/// Return the Unix device identifier for metadata.
#[cfg(unix)]
fn metadata_dev(metadata: &fs::Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    i64::try_from(metadata.dev()).ok()
}

/// Return the Unix device identifier for metadata.
#[cfg(not(unix))]
fn metadata_dev(_metadata: &fs::Metadata) -> Option<i64> {
    None
}

/// Return the Unix inode identifier for metadata.
#[cfg(unix)]
fn metadata_ino(metadata: &fs::Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    i64::try_from(metadata.ino()).ok()
}

/// Return the Unix inode identifier for metadata.
#[cfg(not(unix))]
fn metadata_ino(_metadata: &fs::Metadata) -> Option<i64> {
    None
}

/// Return Unix ctime in nanoseconds since epoch for metadata.
#[cfg(unix)]
fn metadata_ctime_ns(metadata: &fs::Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    let seconds = i128::from(metadata.ctime());
    let nanos = i128::from(metadata.ctime_nsec());
    i64::try_from(seconds.saturating_mul(1_000_000_000) + nanos).ok()
}

/// Return Unix ctime in nanoseconds since epoch for metadata.
#[cfg(not(unix))]
fn metadata_ctime_ns(_metadata: &fs::Metadata) -> Option<i64> {
    None
}

/// Recursively collect JSONL files.
#[cfg(test)]
pub(in crate::app) fn collect_session_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_session_files(&path, files)?;
        } else if file_type.is_file() {
            let Some(file_format) = session_file_format(&path) else {
                continue;
            };
            if !should_skip_format(&path, file_format) {
                files.push(path);
            }
        }
    }
    Ok(())
}

/// Scanner state shared across files.
#[derive(Clone, Debug, Default)]
pub(in crate::app) struct GroupSummary {
    /// Totals for all models in the group.
    pub(in crate::app) totals: UsageTotals,
    /// Per-model breakdowns.
    pub(in crate::app) models: HashMap<String, ModelBreakdown>,
}

/// One session summary.
#[derive(Clone, Debug)]
pub(in crate::app) struct SessionSummary {
    /// Session identifier shown in output.
    pub(in crate::app) display_session_id: String,
    /// Totals for all models in the session.
    pub(in crate::app) totals: UsageTotals,
    /// Per-model breakdowns.
    pub(in crate::app) models: HashMap<String, ModelBreakdown>,
    /// Most recent activity.
    pub(in crate::app) last_activity: DateTime<Utc>,
}

impl SessionSummary {
    /// Create a summary starting with the given event time.
    pub(in crate::app) fn new(last_activity: DateTime<Utc>, display_session_id: String) -> Self {
        Self {
            display_session_id,
            totals: UsageTotals::default(),
            models: HashMap::default(),
            last_activity,
        }
    }
}

/// Candidate session file discovered during directory traversal.
#[derive(Clone, Debug)]
pub(in crate::app) struct SessionScanTarget {
    /// Stable session identifier derived from the relative path.
    ///
    /// This is intentionally global across `--session-dir` roots. When the same identifier
    /// appears more than once, the longer file is treated as the newer copy of that session.
    pub(in crate::app) session_id: String,
    /// Path to the selected JSONL file.
    pub(in crate::app) path: PathBuf,
    /// Physical file representation.
    pub(in crate::app) file_format: SessionFileFormat,
    /// File length used to detect newer copies across roots.
    pub(in crate::app) bytes: u64,
    /// Modification time used as a deterministic tie-breaker.
    pub(in crate::app) modified: Option<SystemTime>,
    /// Metadata stamp captured during session discovery.
    pub(in crate::app) metadata: SessionFileMetadata,
    /// No-filesystem path key used by the persistent scan index.
    pub(in crate::app) path_key: String,
}

impl SessionScanTarget {
    /// Decide whether this candidate should replace an existing one.
    pub(in crate::app) fn is_preferred_over(&self, existing: &Self) -> bool {
        if (self.file_format.is_compressed() || existing.file_format.is_compressed())
            && self.modified != existing.modified
        {
            return self.modified > existing.modified;
        }
        self.bytes > existing.bytes
            || (self.bytes == existing.bytes
                && (self.modified > existing.modified
                    || (self.modified == existing.modified && self.path > existing.path)))
    }

    /// Return this target with a fresh metadata stamp from the selected path.
    pub(in crate::app) fn refresh_metadata(&self) -> Result<Self> {
        let metadata = fs::metadata(&self.path)
            .wrap_err_with(|| format!("failed to refresh session file {}", self.path.display()))?;
        if !metadata.is_file() {
            return Err(eyre!(
                "selected session path is no longer a file: {}",
                self.path.display()
            ));
        }
        let modified = metadata.modified().ok();
        Ok(Self {
            bytes: metadata.len(),
            modified,
            metadata: SessionFileMetadata::from_metadata(&metadata, modified),
            file_format: session_file_format(&self.path).unwrap_or(self.file_format),
            ..self.clone()
        })
    }
}

/// File metadata captured while discovering a session target.
#[derive(Clone, Debug, Default)]
pub(in crate::app) struct SessionFileMetadata {
    /// Modification time in nanoseconds since Unix epoch.
    pub(in crate::app) mtime_ns: Option<i64>,
    /// Unix device identifier.
    pub(in crate::app) dev: Option<i64>,
    /// Unix inode identifier.
    pub(in crate::app) ino: Option<i64>,
    /// Unix ctime in nanoseconds since Unix epoch.
    pub(in crate::app) ctime_ns: Option<i64>,
}

impl SessionFileMetadata {
    /// Build metadata fields from `std::fs::Metadata`.
    fn from_metadata(metadata: &fs::Metadata, modified: Option<SystemTime>) -> Self {
        Self {
            mtime_ns: modified.and_then(system_time_to_nanos),
            dev: metadata_dev(metadata),
            ino: metadata_ino(metadata),
            ctime_ns: metadata_ctime_ns(metadata),
        }
    }
}

/// Builder for the requested report kind.
pub(in crate::app) struct ReportBuilder {
    /// Report flavor.
    kind: ReportKind,
    /// Grouping timezone.
    timezone: Tz,
    /// Inclusive lower bound.
    since: Option<NaiveDate>,
    /// Inclusive upper bound.
    until: Option<NaiveDate>,
    /// Daily summaries.
    daily: HashMap<NaiveDate, GroupSummary>,
    /// Monthly summaries.
    monthly: HashMap<MonthKey, GroupSummary>,
    /// Session summaries.
    session: HashMap<String, SessionSummary>,
}

impl ReportBuilder {
    /// Create a new builder.
    pub(in crate::app) fn new(
        kind: ReportKind,
        timezone: Tz,
        since: Option<NaiveDate>,
        until: Option<NaiveDate>,
    ) -> Self {
        Self {
            kind,
            timezone,
            since,
            until,
            daily: HashMap::default(),
            monthly: HashMap::default(),
            session: HashMap::default(),
        }
    }

    /// Observe one event.
    pub(in crate::app) fn observe(&mut self, event: &TokenUsageEvent<'_, '_>) {
        let local = event.timestamp_utc.with_timezone(&self.timezone);
        let date = local.date_naive();
        if self.since.is_some_and(|since| date < since)
            || self.until.is_some_and(|until| date > until)
        {
            return;
        }

        match self.kind {
            ReportKind::Daily => {
                let summary = self.daily.entry(date).or_default();
                push_event_into_summary(summary, event);
            }
            ReportKind::Monthly => {
                let key = month_key(date);
                let summary = self.monthly.entry(key).or_default();
                push_event_into_summary(summary, event);
            }
            ReportKind::Session => {
                let summary = self
                    .session
                    .entry(event.session_key.to_string())
                    .or_insert_with(|| {
                        SessionSummary::new(event.timestamp_utc, event.session_id.to_string())
                    });
                if event.timestamp_utc > summary.last_activity {
                    summary.last_activity = event.timestamp_utc;
                }
                push_event_into_session_summary(summary, event);
            }
        }
    }

    /// Finish the report.
    pub(in crate::app) fn finish(
        self,
        pricing: &PricingCatalog,
        presentation: UsagePresentation,
        missing_directories: Vec<String>,
    ) -> ReportOutput {
        let context = ReportFinishContext {
            pricing,
            presentation,
        };
        match self.kind {
            ReportKind::Daily => self.finish_daily(context, missing_directories),
            ReportKind::Monthly => self.finish_monthly(context, missing_directories),
            ReportKind::Session => self.finish_session(context, missing_directories),
        }
    }

    /// Finish a daily report.
    fn finish_daily(
        self,
        context: ReportFinishContext<'_>,
        missing_directories: Vec<String>,
    ) -> ReportOutput {
        let mut rows = Vec::with_capacity(self.daily.len());
        let mut entries = self.daily.into_iter().collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(key, _summary)| *key);
        let mut totals = Totals::default();
        for (key, summary) in entries {
            let (visible_totals, cost, models) =
                context.finish_summary(&summary.totals, &summary.models);
            push_totals(&mut totals, &visible_totals, cost);
            rows.push(DailyRow {
                date: format_day_key(key),
                input_tokens: visible_totals.input,
                cached_input_tokens: visible_totals.cached_input,
                output_tokens: visible_totals.output,
                reasoning_output_tokens: visible_totals.reasoning_output,
                total_tokens: visible_totals.total,
                cost_usd: cost,
                models,
            });
        }
        ReportOutput::Daily {
            rows,
            totals,
            missing_directories,
        }
    }

    /// Finish a monthly report.
    fn finish_monthly(
        self,
        context: ReportFinishContext<'_>,
        missing_directories: Vec<String>,
    ) -> ReportOutput {
        let mut rows = Vec::with_capacity(self.monthly.len());
        let mut entries = self.monthly.into_iter().collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(key, _summary)| *key);
        let mut totals = Totals::default();
        for (key, summary) in entries {
            let (visible_totals, cost, models) =
                context.finish_summary(&summary.totals, &summary.models);
            push_totals(&mut totals, &visible_totals, cost);
            rows.push(MonthlyRow {
                month: format_month_key(key),
                input_tokens: visible_totals.input,
                cached_input_tokens: visible_totals.cached_input,
                output_tokens: visible_totals.output,
                reasoning_output_tokens: visible_totals.reasoning_output,
                total_tokens: visible_totals.total,
                cost_usd: cost,
                models,
            });
        }
        ReportOutput::Monthly {
            rows,
            totals,
            missing_directories,
        }
    }

    /// Finish a session report.
    fn finish_session(
        self,
        context: ReportFinishContext<'_>,
        missing_directories: Vec<String>,
    ) -> ReportOutput {
        let mut rows = Vec::with_capacity(self.session.len());
        let mut entries = self.session.into_iter().collect::<Vec<_>>();
        sort_session_entries(&mut entries);
        let mut totals = Totals::default();
        for (_session_key, summary) in entries {
            let (visible_totals, cost, models) =
                context.finish_summary(&summary.totals, &summary.models);
            push_totals(&mut totals, &visible_totals, cost);
            let (directory, session_file) = split_session_id(&summary.display_session_id);
            rows.push(SessionRow {
                session_id: summary.display_session_id,
                directory,
                session_file,
                last_activity: summary
                    .last_activity
                    .with_timezone(&self.timezone)
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                input_tokens: visible_totals.input,
                cached_input_tokens: visible_totals.cached_input,
                output_tokens: visible_totals.output,
                reasoning_output_tokens: visible_totals.reasoning_output,
                total_tokens: visible_totals.total,
                cost_usd: cost,
                models,
            });
        }
        ReportOutput::Session {
            rows,
            totals,
            missing_directories,
        }
    }

    /// Merge another builder with matching report settings into this one.
    pub(in crate::app) fn merge(&mut self, other: Self) {
        debug_assert_eq!(
            self.kind, other.kind,
            "parallel scan chunks must preserve report kind",
        );
        debug_assert_eq!(
            self.timezone, other.timezone,
            "parallel scan chunks must preserve report timezone",
        );
        debug_assert_eq!(
            self.since, other.since,
            "parallel scan chunks must preserve lower date bound",
        );
        debug_assert_eq!(
            self.until, other.until,
            "parallel scan chunks must preserve upper date bound",
        );

        merge_group_summaries(&mut self.daily, other.daily);
        merge_group_summaries(&mut self.monthly, other.monthly);
        merge_session_summaries(&mut self.session, other.session);
    }

    /// Fold spawned-agent summaries into their resolved root sessions.
    fn fold_session_summaries(&mut self, attributions: &HashMap<String, String>) {
        if attributions.is_empty() {
            return;
        }
        let mut folded = HashMap::with_capacity(self.session.len());
        for (session_key, mut summary) in std::mem::take(&mut self.session) {
            let attributed_key = attributions
                .get(&session_key)
                .cloned()
                .unwrap_or(session_key);
            summary.display_session_id.clone_from(&attributed_key);
            if let Some(existing) = folded.get_mut(&attributed_key) {
                merge_session_summary(existing, summary);
            } else {
                folded.insert(attributed_key, summary);
            }
        }
        self.session = folded;
    }

    /// Merge one cached file-local day summary into this report.
    pub(in crate::app) fn merge_file_day_summary(
        &mut self,
        session_key: &str,
        session_id: &str,
        local_day: NaiveDate,
        summary: GroupSummary,
        last_activity: DateTime<Utc>,
    ) {
        if self.since.is_some_and(|since| local_day < since)
            || self.until.is_some_and(|until| local_day > until)
        {
            return;
        }

        match self.kind {
            ReportKind::Daily => {
                let target = self.daily.entry(local_day).or_default();
                merge_group_summary(target, summary);
            }
            ReportKind::Monthly => {
                let key = month_key(local_day);
                let target = self.monthly.entry(key).or_default();
                merge_group_summary(target, summary);
            }
            ReportKind::Session => {
                let target = self
                    .session
                    .entry(session_key.to_string())
                    .or_insert_with(|| SessionSummary::new(last_activity, session_id.to_string()));
                target.last_activity = target.last_activity.max(last_activity);
                target.totals.add(&summary.totals);
                merge_model_breakdowns(&mut target.models, summary.models);
            }
        }
    }
}

/// Shared report finalization inputs.
#[derive(Clone, Copy)]
pub(in crate::app) struct ReportFinishContext<'a> {
    /// Pricing catalog used to compute costs.
    pricing: &'a PricingCatalog,
    /// Usage and cost presentation behavior.
    presentation: UsagePresentation,
}

impl ReportFinishContext<'_> {
    /// Finish one group summary into visible totals, cost, and model rows.
    fn finish_summary(
        self,
        totals: &UsageTotals,
        models: &HashMap<String, ModelBreakdown>,
    ) -> (UsageTotals, f64, BTreeMap<String, ModelBreakdown>) {
        let visible_totals = totals.with_cache_read_mode(self.presentation.cache_read_mode);
        let cost = calculate_summary_cost(models, self.pricing, self.presentation);
        let sorted_models = to_sorted_models(models, self.pricing, self.presentation);
        (visible_totals, cost, sorted_models)
    }
}

/// Resolve the preferred file for one stable session identifier across all session roots.
pub(in crate::app) fn resolve_session_target_across_roots(
    session_dirs: &[PathBuf],
    project_filter: Option<&ProjectFilter>,
    session_id: &str,
) -> Result<Option<SessionScanTarget>> {
    let mut selected = None;
    for directory in session_dirs {
        let logical_path = session_file_path(directory, session_id);
        let Some(path) = existing_session_path(&logical_path)? else {
            continue;
        };
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                if !session_matches_project_filter(&path, project_filter)? {
                    continue;
                }
                let modified = metadata.modified().ok();
                let file_format = session_file_format(&path).unwrap_or(SessionFileFormat::Plain);
                let candidate = SessionScanTarget {
                    session_id: session_id.to_string(),
                    path_key: session_target_path_key_for_format(&path, file_format)?,
                    file_format,
                    path,
                    bytes: metadata.len(),
                    modified,
                    metadata: SessionFileMetadata::from_metadata(&metadata, modified),
                };
                if selected
                    .as_ref()
                    .is_none_or(|existing| candidate.is_preferred_over(existing))
                {
                    selected = Some(candidate);
                }
            }
            Ok(_) => {}
            Err(error) => {
                return Err(error).wrap_err_with(|| {
                    format!("failed to refresh session target {}", path.display())
                });
            }
        }
    }
    Ok(selected)
}

/// Build the JSONL path for one stable session identifier under a specific root.
pub(in crate::app) fn session_file_path(root: &Path, session_id: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    if let Some((parent, file_name)) = session_id.rsplit_once('/') {
        for component in parent.split('/') {
            path.push(component);
        }
        path.push(format!("{file_name}.jsonl"));
        return path;
    }

    for component in session_id.split('/') {
        path.push(component);
    }
    path.set_file_name(format!("{session_id}.jsonl"));
    path
}

/// Collect only missing root directories without recursively walking the session tree.
pub(in crate::app) fn collect_missing_session_dirs(
    session_dirs: &[PathBuf],
) -> Result<Vec<String>> {
    let mut missing_directories = Vec::new();
    for directory in session_dirs {
        match fs::metadata(directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => missing_directories.push(directory.display().to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_directories.push(directory.display().to_string());
            }
            Err(error) => {
                return Err(error).wrap_err_with(|| {
                    format!("failed to access session directory {}", directory.display())
                });
            }
        }
    }
    Ok(missing_directories)
}

/// Sort session entries deterministically for stable CLI output.
pub(in crate::app) fn sort_session_entries(entries: &mut [(String, SessionSummary)]) {
    entries.sort_by(|(left_key, left_summary), (right_key, right_summary)| {
        left_summary
            .last_activity
            .cmp(&right_summary.last_activity)
            .then_with(|| {
                left_summary
                    .display_session_id
                    .cmp(&right_summary.display_session_id)
            })
            .then_with(|| left_key.cmp(right_key))
    });
}

/// Push event data into a grouped summary.
pub(in crate::app) fn push_event_into_summary(
    summary: &mut GroupSummary,
    event: &TokenUsageEvent<'_, '_>,
) {
    summary.totals.add(&event.usage);
    let breakdown = ensure_model_breakdown(&mut summary.models, event.model);
    push_usage_into_breakdown(breakdown, &event.usage, event.is_fallback_model);
    if event.is_fallback_model {
        breakdown.is_fallback = true;
    }
}

/// Push event data into a session summary.
pub(in crate::app) fn push_event_into_session_summary(
    summary: &mut SessionSummary,
    event: &TokenUsageEvent<'_, '_>,
) {
    summary.totals.add(&event.usage);
    let breakdown = ensure_model_breakdown(&mut summary.models, event.model);
    push_usage_into_breakdown(breakdown, &event.usage, event.is_fallback_model);
    if event.is_fallback_model {
        breakdown.is_fallback = true;
    }
}

/// Fetch or create one model breakdown without allocating on every lookup.
pub(in crate::app) fn ensure_model_breakdown<'a>(
    models: &'a mut HashMap<String, ModelBreakdown>,
    model: &str,
) -> &'a mut ModelBreakdown {
    if models.contains_key(model) {
        return models
            .get_mut(model)
            .expect("contains_key true implies get_mut succeeds");
    }

    models.entry(model.to_string()).or_default()
}

/// Add usage into a public model breakdown.
pub(in crate::app) fn push_usage_into_breakdown(
    target: &mut ModelBreakdown,
    usage: &UsageTotals,
    is_fallback_model: bool,
) {
    target.input_tokens += usage.input;
    target.cached_input_tokens += usage.cached_input;
    target.output_tokens += usage.output;
    target.reasoning_output_tokens += usage.reasoning_output;
    target.total_tokens += usage.total;
    if is_fallback_model {
        target.fallback_usage.add(usage);
    }
}

/// Remove usage from a public model breakdown.
pub(in crate::app) fn remove_usage_from_breakdown(
    target: &mut ModelBreakdown,
    usage: &UsageTotals,
    is_fallback_model: bool,
) {
    target.input_tokens = target.input_tokens.saturating_sub(usage.input);
    target.cached_input_tokens = target
        .cached_input_tokens
        .saturating_sub(usage.cached_input);
    target.output_tokens = target.output_tokens.saturating_sub(usage.output);
    target.reasoning_output_tokens = target
        .reasoning_output_tokens
        .saturating_sub(usage.reasoning_output);
    target.total_tokens = target.total_tokens.saturating_sub(usage.total);
    if is_fallback_model {
        target.fallback_usage.subtract(usage);
    }
}

/// Convert a hash map into a stable `BTreeMap`.
pub(in crate::app) fn to_sorted_models(
    models: &HashMap<String, ModelBreakdown>,
    pricing: &PricingCatalog,
    presentation: UsagePresentation,
) -> BTreeMap<String, ModelBreakdown> {
    models
        .iter()
        .map(|(model, usage)| {
            let visible_usage =
                breakdown_usage(usage).with_cache_read_mode(presentation.cache_read_mode);
            let visible_fallback_usage = usage
                .fallback_usage
                .with_cache_read_mode(presentation.cache_read_mode);
            let mut breakdown = ModelBreakdown {
                input_tokens: visible_usage.input,
                cached_input_tokens: visible_usage.cached_input,
                output_tokens: visible_usage.output,
                reasoning_output_tokens: visible_usage.reasoning_output,
                total_tokens: visible_usage.total,
                fallback_usage: visible_fallback_usage,
                is_fallback: usage.is_fallback,
                ..ModelBreakdown::default()
            };
            let resolved_pricing = pricing.resolve(model);
            breakdown.cost_usd = calculate_cost_from_usage(
                &explicit_usage(&breakdown),
                &resolved_pricing,
                presentation.cached_input_cost_mode,
            );
            breakdown.fallback_cost_usd = calculate_cost_from_usage(
                &breakdown.fallback_usage,
                &resolved_pricing,
                presentation.cached_input_cost_mode,
            );
            (model.clone(), breakdown)
        })
        .collect()
}

/// Convert one model breakdown into internal usage totals.
pub(in crate::app) fn breakdown_usage(breakdown: &ModelBreakdown) -> UsageTotals {
    UsageTotals {
        input: breakdown.input_tokens,
        cached_input: breakdown.cached_input_tokens,
        output: breakdown.output_tokens,
        reasoning_output: breakdown.reasoning_output_tokens,
        total: breakdown.total_tokens,
    }
}

/// Add one row into grand totals.
pub(in crate::app) fn push_totals(totals: &mut Totals, usage: &UsageTotals, cost: f64) {
    totals.input_tokens += usage.input;
    totals.cached_input_tokens += usage.cached_input;
    totals.output_tokens += usage.output;
    totals.reasoning_output_tokens += usage.reasoning_output;
    totals.total_tokens += usage.total;
    totals.cost_usd += cost;
}

/// Calculate the total cost for one summary.
pub(in crate::app) fn calculate_summary_cost(
    models: &HashMap<String, ModelBreakdown>,
    pricing: &PricingCatalog,
    presentation: UsagePresentation,
) -> f64 {
    models
        .iter()
        .map(|(model, usage)| {
            calculate_cost_from_usage(
                &breakdown_usage(usage).with_cache_read_mode(presentation.cache_read_mode),
                &pricing.resolve(model),
                presentation.cached_input_cost_mode,
            )
        })
        .sum()
}

/// Split a session identifier into directory and leaf name.
pub(in crate::app) fn split_session_id(session_id: &str) -> (String, String) {
    match session_id.rsplit_once('/') {
        Some((directory, session_file)) => (directory.to_string(), session_file.to_string()),
        None => (String::new(), session_id.to_string()),
    }
}

/// Return whether one session file passes the optional project directory filter.
pub(in crate::app) fn session_matches_project_filter(
    file: &Path,
    project_filter: Option<&ProjectFilter>,
) -> Result<bool> {
    let Some(project_filter) = project_filter else {
        return Ok(true);
    };
    let Some(cwd) = read_session_descriptor(file)?.and_then(|descriptor| descriptor.cwd) else {
        return Ok(false);
    };

    project_filter.matches_logged_cwd(&cwd)
}

/// Resolve each spawned rollout to the highest available non-cyclic ancestor session.
fn build_session_attributions(
    selected_files: &[SessionScanTarget],
) -> Result<HashMap<String, String>> {
    let mut descriptors = HashMap::<String, SessionDescriptor>::new();
    let mut sessions_by_thread = HashMap::<String, String>::new();
    for target in selected_files {
        let Some(descriptor) = read_session_descriptor(&target.path)? else {
            continue;
        };
        if let Some(thread_id) = descriptor.thread_id.as_ref() {
            sessions_by_thread.insert(thread_id.clone(), target.session_id.clone());
        }
        descriptors.insert(target.session_id.clone(), descriptor);
    }

    let mut attributions = HashMap::new();
    for session_id in descriptors.keys() {
        let root = resolve_root_session_id(session_id, &descriptors, &sessions_by_thread);
        if root != *session_id {
            attributions.insert(session_id.clone(), root);
        }
    }
    Ok(attributions)
}

/// Follow parent thread identifiers while every ancestor remains selected and acyclic.
fn resolve_root_session_id(
    session_id: &str,
    descriptors: &HashMap<String, SessionDescriptor>,
    sessions_by_thread: &HashMap<String, String>,
) -> String {
    let mut current = session_id;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current) {
            return session_id.to_string();
        }
        let Some(parent_thread_id) = descriptors
            .get(current)
            .and_then(|descriptor| descriptor.parent_thread_id.as_deref())
        else {
            return current.to_string();
        };
        let Some(parent_session_id) = sessions_by_thread.get(parent_thread_id) else {
            return current.to_string();
        };
        current = parent_session_id;
    }
}

/// Discover selected session files and collect missing roots.
pub(in crate::app) fn collect_session_scan_targets(
    session_dirs: &[PathBuf],
    project_filter: Option<&ProjectFilter>,
) -> Result<(Vec<String>, Vec<SessionScanTarget>)> {
    let mut missing_directories = Vec::new();
    let mut selected_files = HashMap::new();
    for directory in session_dirs {
        match fs::metadata(directory) {
            Ok(metadata) if metadata.is_dir() => {
                scan_session_tree(directory, directory, project_filter, &mut selected_files)?;
            }
            Ok(_) => missing_directories.push(directory.display().to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_directories.push(directory.display().to_string());
            }
            Err(error) => {
                return Err(error).wrap_err_with(|| {
                    format!("failed to access session directory {}", directory.display())
                });
            }
        }
    }
    let mut targets = selected_files.into_values().collect::<Vec<_>>();
    targets.sort_unstable_by(|left, right| left.session_id.cmp(&right.session_id));
    Ok((missing_directories, targets))
}

/// Scan one JSONL session file into a report builder.
#[cfg(test)]
pub(in crate::app) fn scan_session_file(
    file: &Path,
    session_id: &str,
    builder: &mut ReportBuilder,
) -> Result<()> {
    let _ = scan_session_file_from_checkpoint(
        file,
        session_id,
        &SessionParseCheckpoint::default(),
        |event| builder.observe(event),
    )?;
    Ok(())
}

/// Scan one selected session target into a report builder.
fn scan_session_target_file(target: &SessionScanTarget, builder: &mut ReportBuilder) -> Result<()> {
    let _ = scan_session_file_from_checkpoint_with_format(
        &target.path,
        &target.session_id,
        target.file_format,
        &SessionParseCheckpoint::default(),
        |event| builder.observe(event),
    )?;
    Ok(())
}

/// Scan one selected session target into a report builder with an explicit observer.
fn scan_session_target_file_with_observer<O>(
    target: &SessionScanTarget,
    builder: &mut ReportBuilder,
    observer: &O,
) -> Result<()>
where
    O: ScanObserver,
{
    scan_session_file_with_callback_and_observer_with_format(
        &target.path,
        &target.session_id,
        target.file_format,
        observer,
        |event| {
            builder.observe(event);
        },
    )
}

/// Scan all selected targets, optionally across multiple worker threads.
pub(in crate::app) fn scan_selected_session_targets(
    selected_files: &[SessionScanTarget],
    parallelism: ScannerParallelism,
    kind: ReportKind,
    timezone: Tz,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
) -> Result<ReportBuilder> {
    scan_selected_session_targets_with(
        selected_files,
        parallelism,
        || ReportBuilder::new(kind, timezone, since, until),
        scan_session_target_file,
    )
}

/// Scan all selected targets, optionally across multiple worker threads.
pub(in crate::app) fn scan_selected_session_targets_with_observer<O>(
    selected_files: &[SessionScanTarget],
    parallelism: ScannerParallelism,
    kind: ReportKind,
    timezone: Tz,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
    observer: &O,
) -> Result<ReportBuilder>
where
    O: ScanObserver,
{
    scan_selected_session_targets_with(
        selected_files,
        parallelism,
        || ReportBuilder::new(kind, timezone, since, until),
        |target, builder| scan_session_target_file_with_observer(target, builder, observer),
    )
}

/// Scan selected session targets using the provided file strategy.
pub(in crate::app) fn scan_selected_session_targets_with<F, B>(
    selected_files: &[SessionScanTarget],
    parallelism: ScannerParallelism,
    empty_builder: B,
    scan_file: F,
) -> Result<ReportBuilder>
where
    F: Fn(&SessionScanTarget, &mut ReportBuilder) -> Result<()> + Copy + Send + Sync,
    B: Fn() -> ReportBuilder + Copy + Send + Sync,
{
    if selected_files.is_empty() {
        return Ok(empty_builder());
    }

    let worker_count = resolve_scan_worker_count(parallelism, selected_files.len());
    if worker_count == 1 {
        return scan_selected_session_slice_with(selected_files, empty_builder, scan_file);
    }

    let chunks = balanced_scan_chunks(selected_files, worker_count);
    thread::scope(|scope| -> Result<ReportBuilder> {
        let mut chunks = chunks.iter();
        let first_chunk = chunks
            .next()
            .ok_or_else(|| eyre!("missing initial scan chunk"))?;
        let handles = chunks
            .map(|chunk| {
                scope.spawn(move || {
                    scan_selected_session_index_chunk_with(
                        selected_files,
                        chunk,
                        empty_builder,
                        scan_file,
                    )
                })
            })
            .collect::<Vec<_>>();

        let mut merged = scan_selected_session_index_chunk_with(
            selected_files,
            first_chunk,
            empty_builder,
            scan_file,
        )?;
        for handle in handles {
            let partial = handle
                .join()
                .map_err(|_| eyre!("session scan worker panicked"))??;
            merged.merge(partial);
        }
        Ok(merged)
    })
}

/// Scan one borrowed slice of selected targets using the provided file strategy.
fn scan_selected_session_slice_with<F, B>(
    selected_files: &[SessionScanTarget],
    empty_builder: B,
    mut scan_file: F,
) -> Result<ReportBuilder>
where
    F: FnMut(&SessionScanTarget, &mut ReportBuilder) -> Result<()>,
    B: FnOnce() -> ReportBuilder,
{
    let mut builder = empty_builder();
    for target in selected_files {
        scan_file(target, &mut builder)?;
    }
    Ok(builder)
}

/// Scan one worker chunk of target indexes using the provided file strategy.
fn scan_selected_session_index_chunk_with<F, B>(
    selected_files: &[SessionScanTarget],
    target_indexes: &[usize],
    empty_builder: B,
    mut scan_file: F,
) -> Result<ReportBuilder>
where
    F: FnMut(&SessionScanTarget, &mut ReportBuilder) -> Result<()>,
    B: FnOnce() -> ReportBuilder,
{
    let mut builder = empty_builder();
    for &target_index in target_indexes {
        let target = selected_files.get(target_index).ok_or_else(|| {
            eyre!(
                "scan chunk index {target_index} out of range for {} selected files",
                selected_files.len()
            )
        })?;
        scan_file(target, &mut builder)?;
    }
    Ok(builder)
}

/// Resolve the effective worker count for the current workload.
pub(in crate::app) fn resolve_scan_worker_count(
    parallelism: ScannerParallelism,
    selected_files: usize,
) -> usize {
    if selected_files <= 1 {
        return selected_files.max(1);
    }

    let configured = match parallelism {
        ScannerParallelism::Auto => thread::available_parallelism().map_or(1, |threads| {
            threads.get().saturating_mul(AUTO_WORKER_MULTIPLIER)
        }),
        ScannerParallelism::Fixed(threads) => threads.get(),
    };

    configured.clamp(1, selected_files)
}

/// Split selected session targets into worker chunks balanced by byte size.
pub(in crate::app) fn balanced_scan_chunks(
    selected_files: &[SessionScanTarget],
    worker_count: usize,
) -> Vec<Vec<usize>> {
    if selected_files.is_empty() {
        return Vec::new();
    }

    let worker_count = worker_count.clamp(1, selected_files.len());
    let mut chunks = (0..worker_count)
        .map(|_| WeightedScanChunk::default())
        .collect::<Vec<_>>();
    let mut ordered = selected_files.iter().enumerate().collect::<Vec<_>>();
    ordered.sort_unstable_by(|(_, left), (_, right)| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.session_id.cmp(&right.session_id))
            .then_with(|| left.path.cmp(&right.path))
    });

    for (target_index, target) in ordered {
        let chunk = chunks
            .iter_mut()
            .min_by_key(|chunk| (chunk.bytes, chunk.targets.len()))
            .expect("worker count is at least one");
        chunk.bytes = chunk.bytes.saturating_add(target.bytes);
        chunk.targets.push(target_index);
    }

    chunks
        .into_iter()
        .map(|chunk| chunk.targets)
        .filter(|chunk| !chunk.is_empty())
        .collect()
}

/// One worker chunk with accumulated scheduling weight.
#[derive(Default)]
struct WeightedScanChunk {
    /// Selected target indexes assigned to this chunk.
    targets: Vec<usize>,
    /// Sum of target file sizes.
    bytes: u64,
}

/// Discover the best session file for each session identifier.
pub(in crate::app) fn scan_session_tree(
    root: &Path,
    directory: &Path,
    project_filter: Option<&ProjectFilter>,
    selected_files: &mut HashMap<String, SessionScanTarget>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            scan_session_tree(root, &path, project_filter, selected_files)?;
        } else if file_type.is_file() {
            let Some(file_format) = session_file_format(&path) else {
                continue;
            };
            if !should_skip_format(&path, file_format) {
                register_session_target(root, &entry, file_format, project_filter, selected_files)?;
            }
        }
    }

    Ok(())
}

/// Register one JSONL file as a session scan candidate.
pub(in crate::app) fn register_session_target(
    root: &Path,
    entry: &std::fs::DirEntry,
    file_format: SessionFileFormat,
    project_filter: Option<&ProjectFilter>,
    selected_files: &mut HashMap<String, SessionScanTarget>,
) -> Result<()> {
    let path = entry.path();
    if !session_matches_project_filter(&path, project_filter)? {
        return Ok(());
    }
    let metadata = entry.metadata()?;
    let modified = metadata.modified().ok();
    let session_id = session_file_id_for_format(root, &path, file_format);
    let candidate = SessionScanTarget {
        session_id: session_id.clone(),
        path_key: session_target_path_key_for_format(&path, file_format)?,
        file_format,
        path,
        bytes: metadata.len(),
        modified,
        metadata: SessionFileMetadata::from_metadata(&metadata, modified),
    };
    let should_replace = selected_files
        .get(&session_id)
        .is_none_or(|existing| candidate.is_preferred_over(existing));
    if should_replace {
        selected_files.insert(session_id, candidate);
    }
    Ok(())
}

/// Merge grouped report summaries by key.
pub(in crate::app) fn merge_group_summaries<K>(
    target: &mut HashMap<K, GroupSummary>,
    source: HashMap<K, GroupSummary>,
) where
    K: Eq + Hash,
{
    for (key, source_summary) in source {
        let summary = target.entry(key).or_default();
        merge_group_summary(summary, source_summary);
    }
}

/// Merge one grouped summary into another.
pub(in crate::app) fn merge_group_summary(target: &mut GroupSummary, source: GroupSummary) {
    target.totals.add(&source.totals);
    merge_model_breakdowns(&mut target.models, source.models);
}

/// Merge session summaries by stable session key.
pub(in crate::app) fn merge_session_summaries(
    target: &mut HashMap<String, SessionSummary>,
    source: HashMap<String, SessionSummary>,
) {
    for (session_key, source_summary) in source {
        if let Some(existing) = target.get_mut(&session_key) {
            merge_session_summary(existing, source_summary);
            continue;
        }

        target.insert(session_key, source_summary);
    }
}

/// Merge one physical session summary into an existing attributed session.
fn merge_session_summary(target: &mut SessionSummary, source: SessionSummary) {
    target.totals.add(&source.totals);
    target.last_activity = target.last_activity.max(source.last_activity);
    merge_model_breakdowns(&mut target.models, source.models);
}

/// Merge per-model usage without allocating intermediate report rows.
pub(in crate::app) fn merge_model_breakdowns(
    target: &mut HashMap<String, ModelBreakdown>,
    source: HashMap<String, ModelBreakdown>,
) {
    for (model, source_breakdown) in source {
        let breakdown = target.entry(model).or_default();
        breakdown.input_tokens += source_breakdown.input_tokens;
        breakdown.cached_input_tokens += source_breakdown.cached_input_tokens;
        breakdown.output_tokens += source_breakdown.output_tokens;
        breakdown.reasoning_output_tokens += source_breakdown.reasoning_output_tokens;
        breakdown.total_tokens += source_breakdown.total_tokens;
        breakdown
            .fallback_usage
            .add(&source_breakdown.fallback_usage);
        breakdown.is_fallback |= source_breakdown.is_fallback;
    }
}

/// Derive the stable session identifier from a JSONL path.
pub(in crate::app) fn session_file_id(root: &Path, file: &Path) -> String {
    session_file_id_for_format(
        root,
        file,
        session_file_format(file).unwrap_or(SessionFileFormat::Plain),
    )
}

/// Derive the stable session identifier from a path with known format.
fn session_file_id_for_format(root: &Path, file: &Path, file_format: SessionFileFormat) -> String {
    let plain_file;
    let logical_file = if file_format.is_compressed() {
        plain_file = plain_session_path(file);
        plain_file.as_path()
    } else {
        file
    };
    let mut relative = logical_file
        .strip_prefix(root)
        .unwrap_or(logical_file)
        .to_path_buf();
    if relative.extension() == Some(std::ffi::OsStr::new("jsonl")) {
        relative.set_extension("");
    }
    relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Price one usage entry.
#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    reason = "Codex token counters are orders of magnitude below f64 precision limits"
)]
pub(in crate::app) fn calculate_cost(
    usage: &ModelBreakdown,
    pricing: &Pricing,
    cached_input_cost_mode: CachedInputCostMode,
) -> f64 {
    calculate_cost_from_usage(
        &UsageTotals {
            input: usage.input_tokens,
            cached_input: usage.cached_input_tokens,
            output: usage.output_tokens,
            reasoning_output: usage.reasoning_output_tokens,
            total: usage.total_tokens,
        },
        pricing,
        cached_input_cost_mode,
    )
}

/// Price one usage total entry.
#[allow(
    clippy::cast_precision_loss,
    reason = "Codex token counters are orders of magnitude below f64 precision limits"
)]
pub(in crate::app) fn calculate_cost_from_usage(
    usage: &UsageTotals,
    pricing: &Pricing,
    cached_input_cost_mode: CachedInputCostMode,
) -> f64 {
    let cached_input = usage.cached_input.min(usage.input);
    let non_cached_input = usage.input.saturating_sub(cached_input);
    let input_cost = (non_cached_input as f64 / MILLION) * pricing.input_cost_per_mtoken;
    let cached_cost = match cached_input_cost_mode {
        CachedInputCostMode::Priced => {
            (cached_input as f64 / MILLION) * pricing.cached_input_cost_per_mtoken
        }
        CachedInputCostMode::Free => 0.0,
    };
    let output_cost = (usage.output as f64 / MILLION) * pricing.output_cost_per_mtoken;
    input_cost + cached_cost + output_cost
}
