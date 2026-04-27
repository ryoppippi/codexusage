//! CLI orchestration and report building.

use crate::pricing::{
    CacheDecision, Pricing, PricingCatalog, PricingLoadOptions, decide_cache_action,
    default_cache_path, load_pricing_catalog,
};
use chrono::{DateTime, Days, LocalResult, NaiveDate, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use clap::{Args, Parser, Subcommand, ValueEnum};
use eyre::{Result, WrapErr, eyre};
use notify::{
    Config as NotifyConfig, Event as NotifyEvent, EventKind as NotifyEventKind, PollWatcher,
    RecommendedWatcher, RecursiveMode, Watcher,
    event::{DataChange, ModifyKind},
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, IsTerminal, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Human-readable table rendering helpers.
#[path = "app/render.rs"]
mod render;

/// CLI-only scan progress and debug runtime helpers.
#[path = "app/scan_runtime.rs"]
mod scan_runtime;

use render::{explicit_usage, render_report, render_watch_screen};
#[cfg(test)]
use scan_runtime::NoopScanBatchRunner;
use scan_runtime::{CliScanBatchRunner, ScanBatchRunner, ScanBehavior, ScanObserver};

#[cfg(test)]
use render::{
    BorderStyle, TableElement, TableRenderConfig, TableRuleKind, TableStyle,
    detect_border_style_for, detect_table_style_for, format_currency, format_data_row, format_u64,
    format_u64_with, paint, table_rule, write_table_row,
};

/// Environment variable used to override the default Codex home directory.
const DEFAULT_CODEX_HOME_ENV: &str = "CODEX_HOME";
/// Model used when legacy logs do not expose model metadata.
const DEFAULT_FALLBACK_MODEL: &str = "gpt-5";
/// Number of tokens in one million-token pricing unit.
const MILLION: f64 = 1_000_000.0;

/// Cached input token pricing mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CachedInputCostMode {
    /// Use the cached-input price from the pricing catalog.
    #[default]
    Priced,
    /// Treat cached input tokens as free.
    Free,
}

/// Cache-read token reporting mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CacheReadMode {
    /// Include cache-read input tokens in the reported token counters.
    #[default]
    Include,
    /// Exclude cache-read input tokens from input and total counters.
    Exclude,
}

/// Supported report kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportKind {
    /// Group usage by calendar day.
    Daily,
    /// Group usage by calendar month.
    Monthly,
    /// Group usage by session file.
    Session,
}

/// Numeric table display mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum NumberFormat {
    /// Shorten token counts using integer K/M/B/T suffixes.
    #[default]
    Short,
    /// Show full token counts with separators.
    Full,
}

/// Scanner worker configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScannerParallelism {
    /// Use the host's available parallelism.
    Auto,
    /// Use an explicit worker count.
    Fixed(NonZeroUsize),
}

/// CLI-free options passed into report generation.
#[derive(Clone, Debug)]
pub struct ReportOptions {
    /// Inclusive lower bound.
    pub since: Option<String>,
    /// Inclusive upper bound.
    pub until: Option<String>,
    /// Trailing calendar days ending today in the selected timezone. Daily report only.
    pub last_days: Option<NonZeroUsize>,
    /// IANA timezone name.
    pub timezone: String,
    /// Output locale hint.
    pub locale: String,
    /// Human-readable number formatting mode.
    pub number_format: NumberFormat,
    /// Emit JSON instead of table output.
    pub json: bool,
    /// Disable network pricing refreshes.
    pub offline: bool,
    /// Force pricing refresh even when cache is fresh.
    pub refresh_pricing: bool,
    /// Cached input token pricing mode.
    pub cached_input_cost_mode: CachedInputCostMode,
    /// Cache-read token reporting mode.
    pub cache_read_mode: CacheReadMode,
    /// Session directories to scan.
    pub session_dirs: Vec<PathBuf>,
    /// Scanner worker configuration.
    pub parallelism: ScannerParallelism,
}

/// Aggregated token usage for a single model.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ModelBreakdown {
    /// Total input tokens.
    pub input_tokens: u64,
    /// Total cached input tokens.
    pub cached_input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Total reasoning tokens.
    pub reasoning_output_tokens: u64,
    /// Total billable tokens.
    pub total_tokens: u64,
    /// Precomputed cost in USD for text rendering.
    #[serde(skip_serializing)]
    pub cost_usd: f64,
    /// Fallback-only usage kept for human-readable rendering.
    #[serde(skip_serializing)]
    pub fallback_usage: UsageTotals,
    /// Fallback-only cost kept for human-readable rendering.
    #[serde(skip_serializing)]
    pub fallback_cost_usd: f64,
    /// Whether fallback model inference was used.
    #[serde(skip_serializing_if = "is_false")]
    pub is_fallback: bool,
}

/// Daily row shape.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DailyRow {
    /// Calendar day in the requested timezone.
    pub date: String,
    /// Total input tokens.
    pub input_tokens: u64,
    /// Total cached input tokens.
    pub cached_input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Total reasoning output tokens.
    pub reasoning_output_tokens: u64,
    /// Total billable tokens.
    pub total_tokens: u64,
    /// Cost in USD.
    pub cost_usd: f64,
    /// Per-model breakdown.
    pub models: BTreeMap<String, ModelBreakdown>,
}

/// Monthly row shape.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MonthlyRow {
    /// Calendar month in the requested timezone.
    pub month: String,
    /// Total input tokens.
    pub input_tokens: u64,
    /// Total cached input tokens.
    pub cached_input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Total reasoning output tokens.
    pub reasoning_output_tokens: u64,
    /// Total billable tokens.
    pub total_tokens: u64,
    /// Cost in USD.
    pub cost_usd: f64,
    /// Per-model breakdown.
    pub models: BTreeMap<String, ModelBreakdown>,
}

/// Session row shape.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionRow {
    /// Relative session identifier.
    pub session_id: String,
    /// Relative directory.
    pub directory: String,
    /// Session file name without extension.
    pub session_file: String,
    /// Last activity timestamp in RFC 3339.
    pub last_activity: String,
    /// Total input tokens.
    pub input_tokens: u64,
    /// Total cached input tokens.
    pub cached_input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Total reasoning output tokens.
    pub reasoning_output_tokens: u64,
    /// Total billable tokens.
    pub total_tokens: u64,
    /// Cost in USD.
    pub cost_usd: f64,
    /// Per-model breakdown.
    pub models: BTreeMap<String, ModelBreakdown>,
}

/// Grand totals emitted with every report.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Totals {
    /// Total input tokens.
    pub input_tokens: u64,
    /// Total cached input tokens.
    pub cached_input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Total reasoning output tokens.
    pub reasoning_output_tokens: u64,
    /// Total billable tokens.
    pub total_tokens: u64,
    /// Cost in USD.
    pub cost_usd: f64,
}

/// Watch-only CLI-free options.
#[derive(Clone, Debug)]
struct WatchOptions {
    /// IANA timezone name.
    timezone: String,
    /// Output locale hint.
    locale: String,
    /// Human-readable number formatting mode.
    number_format: NumberFormat,
    /// Disable network pricing refreshes.
    offline: bool,
    /// Force pricing refresh even when cache is fresh.
    refresh_pricing: bool,
    /// Cached input token pricing mode.
    cached_input_cost_mode: CachedInputCostMode,
    /// Cache-read token reporting mode.
    cache_read_mode: CacheReadMode,
    /// Session directories to scan.
    session_dirs: Vec<PathBuf>,
    /// Scanner worker configuration.
    parallelism: ScannerParallelism,
    /// Refresh interval for the live screen.
    interval: Duration,
    /// Show per-model detail rows in the watch table.
    show_model_burn_rate: bool,
    /// Debug-only runtime options forwarded from the CLI.
    #[cfg(debug_assertions)]
    debug: DebugRuntimeOptions,
}

/// CLI-only cache-cost flag group.
#[derive(Args, Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CacheCostCliOptions {
    /// Treat cache-read input tokens as free in cost calculations.
    #[arg(long = "no-cache-cost", global = true)]
    no_cache_cost: bool,
    /// Exclude cache-read input tokens from reported usage and cost calculations.
    #[arg(long = "exclude-cache-read", global = true)]
    exclude_cache_read: bool,
}

impl CacheCostCliOptions {
    /// Convert the parsed CLI flags into the internal pricing mode.
    fn cached_input_cost_mode(self) -> CachedInputCostMode {
        if self.no_cache_cost || self.exclude_cache_read {
            CachedInputCostMode::Free
        } else {
            CachedInputCostMode::Priced
        }
    }

    /// Convert the parsed CLI flags into the cache-read reporting mode.
    fn cache_read_mode(self) -> CacheReadMode {
        if self.exclude_cache_read {
            CacheReadMode::Exclude
        } else {
            CacheReadMode::Include
        }
    }
}

/// Debug-only runtime options for development builds.
#[cfg(debug_assertions)]
#[derive(Args, Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DebugRuntimeOptions {
    /// Simulate variable disk latency before opening each parsed file.
    #[arg(long = "debug-simulate-slow-disk", global = true)]
    simulate_slow_disk: bool,
}

/// Rolling burn-rate metrics for watch mode.
#[derive(Clone, Debug, PartialEq)]
struct BurnRateSnapshot {
    /// Exact rolling window duration.
    window_duration: Duration,
    /// Effective rolling window width after current-day clamping.
    window_minutes: u64,
    /// Input tokens per hour.
    input_tokens_per_hour: u64,
    /// Cached input tokens per hour.
    cached_input_tokens_per_hour: u64,
    /// Output tokens per hour.
    output_tokens_per_hour: u64,
    /// Reasoning output tokens per hour.
    reasoning_output_tokens_per_hour: u64,
    /// Total billable tokens per hour.
    total_tokens_per_hour: u64,
    /// Cost in USD per hour.
    cost_usd_per_hour: f64,
}

/// One rendered watch snapshot.
#[derive(Clone, Debug, PartialEq)]
struct WatchSnapshot {
    /// Current day in the selected timezone.
    date: String,
    /// Current-day cumulative totals.
    totals: Totals,
    /// Rolling burn-rate summary.
    burn_rate: BurnRateSnapshot,
    /// Per-model rolling burn-window detail.
    per_model: BTreeMap<String, ModelBreakdown>,
    /// Missing directories encountered during scan.
    missing_directories: Vec<String>,
    /// Last refresh time in the selected timezone.
    updated_time: String,
}

/// How often watch mode should rediscover new or renamed session files.
const WATCH_DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);
/// Polling cadence used when native filesystem notifications are unavailable.
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Result of a report command.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReportOutput {
    /// Daily report output.
    Daily {
        /// Rows in report order.
        rows: Vec<DailyRow>,
        /// Grand totals.
        totals: Totals,
        /// Missing directories encountered during scan.
        missing_directories: Vec<String>,
    },
    /// Monthly report output.
    Monthly {
        /// Rows in report order.
        rows: Vec<MonthlyRow>,
        /// Grand totals.
        totals: Totals,
        /// Missing directories encountered during scan.
        missing_directories: Vec<String>,
    },
    /// Session report output.
    Session {
        /// Rows in report order.
        rows: Vec<SessionRow>,
        /// Grand totals.
        totals: Totals,
        /// Missing directories encountered during scan.
        missing_directories: Vec<String>,
    },
}

/// Report inputs resolved once before the scan starts.
struct PreparedReport {
    /// Timezone used for grouping and date filters.
    timezone: Tz,
    /// Inclusive lower bound after normalization.
    since: Option<NaiveDate>,
    /// Inclusive upper bound after normalization.
    until: Option<NaiveDate>,
    /// Session roots selected for this run.
    session_dirs: Vec<PathBuf>,
    /// Pricing catalog loaded for the final render.
    pricing: PricingCatalog,
    /// Usage and cost presentation behavior.
    presentation: UsagePresentation,
}

/// Usage and cost presentation behavior shared by reports and watch snapshots.
#[derive(Clone, Copy, Debug)]
struct UsagePresentation {
    /// Cached input token pricing behavior.
    cached_input_cost_mode: CachedInputCostMode,
    /// Cache-read token reporting behavior.
    cache_read_mode: CacheReadMode,
}

impl UsagePresentation {
    /// Create presentation behavior from the CLI-free option modes.
    const fn new(
        cached_input_cost_mode: CachedInputCostMode,
        cache_read_mode: CacheReadMode,
    ) -> Self {
        Self {
            cached_input_cost_mode,
            cache_read_mode,
        }
    }
}

/// Resolve shared report inputs before scanning.
fn prepare_report(kind: ReportKind, options: &ReportOptions) -> Result<PreparedReport> {
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
fn build_report_for_cli(
    kind: ReportKind,
    options: &ReportOptions,
    scan_behavior: ScanBehavior,
) -> Result<ReportOutput> {
    let prepared = prepare_report(kind, options)?;
    let scan_runner = CliScanBatchRunner::new(scan_behavior);
    build_report_from_prepared_targets(
        kind,
        &prepared,
        options.parallelism,
        |selected_files, parallelism, kind, timezone, since, until| {
            scan_runner.run_batch(selected_files.len(), |observer| {
                scan_selected_session_targets_with_observer(
                    selected_files,
                    parallelism,
                    kind,
                    timezone,
                    since,
                    until,
                    observer,
                )
            })
        },
    )
}

/// Finish one report from prepared shared inputs and a selected-target scan strategy.
fn build_report_from_prepared_targets<S>(
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
        collect_session_scan_targets(&prepared.session_dirs)?;
    let scanned = scan_targets(
        &selected_files,
        parallelism,
        kind,
        prepared.timezone,
        prepared.since,
        prepared.until,
    )?;
    builder.merge(scanned);
    builder.finish(
        &prepared.pricing,
        prepared.presentation,
        missing_directories,
    )
}

/// Build one watch snapshot at a fixed logical time.
#[cfg(test)]
fn build_watch_snapshot_at(
    options: &WatchOptions,
    now_utc: DateTime<Utc>,
) -> Result<WatchSnapshot> {
    let pricing = load_pricing_catalog(&PricingLoadOptions {
        offline: options.offline,
        force_refresh: options.refresh_pricing,
    })?;
    build_watch_snapshot_with_pricing_at(options, now_utc, &pricing)
}

/// Build one watch snapshot with an already loaded pricing catalog.
#[cfg(test)]
fn build_watch_snapshot_with_pricing_at(
    options: &WatchOptions,
    now_utc: DateTime<Utc>,
    pricing: &PricingCatalog,
) -> Result<WatchSnapshot> {
    let timezone = parse_timezone(&options.timezone)?;
    let session_dirs = resolve_session_dirs(&options.session_dirs);
    let (missing_directories, selected_files) = collect_session_scan_targets(&session_dirs)?;
    let builder = scan_watch_targets(&selected_files, options.parallelism, timezone, now_utc)?;
    Ok(builder.finish(
        pricing,
        UsagePresentation::new(options.cached_input_cost_mode, options.cache_read_mode),
        missing_directories,
        options.show_model_burn_rate,
    ))
}

/// Validate global flags that conflict with the watch contract.
fn validate_watch_flags(
    json: bool,
    since: Option<&str>,
    until: Option<&str>,
    last_days: Option<NonZeroUsize>,
) -> Result<()> {
    if json {
        return Err(eyre!("watch mode does not support --json"));
    }
    if since.is_some() || until.is_some() || last_days.is_some() {
        return Err(eyre!(
            "watch mode only supports the current day and cannot be combined with --since, --until, or --last-days"
        ));
    }

    Ok(())
}

/// Build the scan behavior used by CLI-driven scans.
#[cfg(debug_assertions)]
fn cli_scan_behavior(show_progress: bool, debug_simulate_slow_disk: bool) -> ScanBehavior {
    ScanBehavior::cli(show_progress, debug_simulate_slow_disk)
}

/// Build the scan behavior used by CLI-driven scans.
#[cfg(not(debug_assertions))]
fn cli_scan_behavior(show_progress: bool) -> ScanBehavior {
    ScanBehavior::cli(show_progress)
}

/// Pending filesystem changes observed by the watch backend.
#[derive(Default)]
struct WatchChangeSet {
    /// Stable session identifiers that need to be refreshed and how aggressively to refresh them.
    dirty_sessions: HashMap<String, WatchDirtyKind>,
    /// Whether the full session tree should be rediscovered.
    discovery_due: bool,
}

/// Refresh policy selected for one dirty watched session.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum WatchDirtyKind {
    /// The file only grew, so the parser checkpoint can resume from the prior EOF.
    AppendOnly,
    /// The file may have been rewritten, renamed, created, or deleted; rebuild it from scratch.
    FullRebuild,
}

impl WatchChangeSet {
    /// Mark the provided session identifiers dirty, keeping the most conservative refresh mode.
    fn mark_dirty_sessions(
        &mut self,
        session_ids: impl IntoIterator<Item = String>,
        dirty_kind: WatchDirtyKind,
    ) {
        for session_id in session_ids {
            self.dirty_sessions
                .entry(session_id)
                .and_modify(|existing| *existing = (*existing).max(dirty_kind))
                .or_insert(dirty_kind);
        }
    }
}

/// Concrete filesystem watcher kept alive for the duration of watch mode.
enum ActiveWatchWatcher {
    /// Native watcher provided by `notify`.
    Recommended(RecommendedWatcher),
    /// Polling fallback when native notifications are unavailable.
    Poll(PollWatcher),
}

impl ActiveWatchWatcher {
    /// Register one root for recursive monitoring.
    fn watch(&mut self, path: &Path) -> notify::Result<()> {
        match self {
            Self::Recommended(watcher) => watcher.watch(path, RecursiveMode::Recursive),
            Self::Poll(watcher) => watcher.watch(path, RecursiveMode::Recursive),
        }
    }

    /// Remove one root from recursive monitoring.
    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        match self {
            Self::Recommended(watcher) => watcher.unwatch(path),
            Self::Poll(watcher) => watcher.unwatch(path),
        }
    }
}

/// Filesystem event source that marks dirty sessions between watch refreshes.
struct WatchEventSource {
    /// Active watcher backend.
    watcher: ActiveWatchWatcher,
    /// Cross-thread notification channel fed by `notify`.
    receiver: mpsc::Receiver<notify::Result<NotifyEvent>>,
    /// Roots currently registered with the watcher.
    watched_roots: HashSet<PathBuf>,
}

impl WatchEventSource {
    /// Build the watcher backend and register all existing session roots.
    fn new(session_dirs: &[PathBuf]) -> Result<Self> {
        if let Some(source) = Self::new_recommended(session_dirs) {
            return Ok(source);
        }

        Self::new_polling(session_dirs)
    }

    /// Try to build a native watcher and register all existing session roots.
    fn new_recommended(session_dirs: &[PathBuf]) -> Option<Self> {
        let (sender, receiver) = mpsc::channel();
        let watcher = match RecommendedWatcher::new(
            move |event| {
                let _ = sender.send(event);
            },
            NotifyConfig::default(),
        ) {
            Ok(watcher) => watcher,
            Err(_error) => return None,
        };

        let mut source = Self {
            watcher: ActiveWatchWatcher::Recommended(watcher),
            receiver,
            watched_roots: HashSet::new(),
        };
        match source.sync_session_dirs(session_dirs) {
            Ok(_discovered_new_root) => Some(source),
            Err(_error) => None,
        }
    }

    /// Build the polling fallback watcher and register all existing session roots.
    fn new_polling(session_dirs: &[PathBuf]) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let watcher = PollWatcher::new(
            move |event| {
                let _ = sender.send(event);
            },
            NotifyConfig::default().with_poll_interval(WATCH_POLL_INTERVAL),
        )?;
        let mut source = Self {
            watcher: ActiveWatchWatcher::Poll(watcher),
            receiver,
            watched_roots: HashSet::new(),
        };
        let _ = source.sync_session_dirs(session_dirs)?;
        Ok(source)
    }

    /// Keep the registered watcher roots aligned with the configured session directories.
    fn sync_session_dirs(&mut self, session_dirs: &[PathBuf]) -> Result<bool> {
        let mut desired_roots = HashSet::new();
        let mut discovered_new_root = false;
        for directory in session_dirs {
            match fs::metadata(directory) {
                Ok(metadata) if metadata.is_dir() => {
                    desired_roots.insert(directory.clone());
                    if self.watched_roots.insert(directory.clone()) {
                        discovered_new_root = true;
                        self.watcher.watch(directory).wrap_err_with(|| {
                            format!("failed to watch session directory {}", directory.display())
                        })?;
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).wrap_err_with(|| {
                        format!("failed to access session directory {}", directory.display())
                    });
                }
            }
        }

        let stale_roots = self
            .watched_roots
            .iter()
            .filter(|path| !desired_roots.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        for stale_root in stale_roots {
            let _ = self.watcher.unwatch(&stale_root);
            self.watched_roots.remove(&stale_root);
        }

        Ok(discovered_new_root)
    }

    /// Drain all pending watcher notifications into the current refresh batch.
    fn drain_changes(&mut self, session_dirs: &[PathBuf]) -> WatchChangeSet {
        let mut changes = WatchChangeSet::default();
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                Ok(event) => Self::apply_event(session_dirs, &event, &mut changes),
                Err(_error) => changes.discovery_due = true,
            }
        }
        changes
    }

    /// Translate one `notify` event into dirty session identifiers.
    fn apply_event(session_dirs: &[PathBuf], event: &NotifyEvent, changes: &mut WatchChangeSet) {
        if matches!(event.kind, NotifyEventKind::Access(_)) {
            return;
        }

        let dirty_kind = watch_dirty_kind(event.kind);
        if event.paths.len() > 1 {
            changes.discovery_due = true;
        }

        for path in &event.paths {
            let session_ids = watch_event_session_ids(session_dirs, path);
            if !session_ids.is_empty() {
                changes.mark_dirty_sessions(session_ids, dirty_kind);
            } else if path_is_under_roots(session_dirs, path) {
                changes.discovery_due = true;
            }
        }
    }
}

/// Map a filesystem event to the safest incremental refresh mode.
fn watch_dirty_kind(event_kind: NotifyEventKind) -> WatchDirtyKind {
    match event_kind {
        NotifyEventKind::Modify(ModifyKind::Data(DataChange::Size)) => WatchDirtyKind::AppendOnly,
        _ => WatchDirtyKind::FullRebuild,
    }
}

/// Return whether the path falls under one of the configured session roots.
fn path_is_under_roots(session_dirs: &[PathBuf], path: &Path) -> bool {
    session_dirs.iter().any(|root| path.starts_with(root))
}

/// Resolve every stable session identifier that a watcher event path can map to.
fn watch_event_session_ids(session_dirs: &[PathBuf], path: &Path) -> Vec<String> {
    if !path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
    {
        return Vec::new();
    }

    session_dirs
        .iter()
        .filter(|root| path.starts_with(root))
        .map(|root| session_file_id(root, path))
        .collect()
}

/// Run the live watch loop until interrupted.
fn run_watch_loop(options: &WatchOptions) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(eyre!("watch mode requires a terminal stdout"));
    }

    let pricing_cache_path = default_cache_path();
    let now = SystemTime::now();
    let startup_refresh_attempted = if options.offline {
        false
    } else if options.refresh_pricing {
        true
    } else {
        decide_cache_action(
            &pricing_cache_path,
            now,
            Duration::from_secs(24 * 60 * 60),
            options.offline,
            false,
        )? == CacheDecision::Refresh
    };
    let mut pricing = load_pricing_catalog(&PricingLoadOptions {
        offline: options.offline,
        force_refresh: options.refresh_pricing,
    })?;
    let timezone = parse_timezone(&options.timezone)?;
    let session_dirs = resolve_session_dirs(&options.session_dirs);
    #[cfg(debug_assertions)]
    let startup_scan_behavior = cli_scan_behavior(true, options.debug.simulate_slow_disk);
    #[cfg(not(debug_assertions))]
    let startup_scan_behavior = cli_scan_behavior(true);
    #[cfg(debug_assertions)]
    let refresh_scan_behavior = cli_scan_behavior(false, options.debug.simulate_slow_disk);
    #[cfg(not(debug_assertions))]
    let refresh_scan_behavior = cli_scan_behavior(false);
    let startup_scan_runner = CliScanBatchRunner::new(startup_scan_behavior);
    let refresh_scan_runner = CliScanBatchRunner::new(refresh_scan_behavior);
    let mut watch_events = WatchEventSource::new(&session_dirs)?;
    let mut runtime = WatchRuntimeState::load_with_scan_runner(
        &session_dirs,
        options.parallelism,
        timezone,
        Utc::now(),
        &startup_scan_runner,
    )?;
    let mut last_pricing_refresh_attempt_at = startup_refresh_attempted.then_some(now);
    let should_clear = supports_watch_screen_clear(std::env::var("TERM").ok().as_deref());
    if !should_clear {
        return Err(eyre!(
            "watch mode requires a terminal with ANSI screen-clearing support"
        ));
    }
    let mut stdout = std::io::stdout().lock();
    loop {
        let loop_started_at = Instant::now();
        let now = SystemTime::now();
        let snapshot_now = Utc::now();
        let discovered_new_root = watch_events.sync_session_dirs(&session_dirs)?;
        if watch_pricing_refresh_due(
            &pricing_cache_path,
            now,
            options.offline,
            last_pricing_refresh_attempt_at,
        )? {
            pricing = load_pricing_catalog(&PricingLoadOptions {
                offline: options.offline,
                force_refresh: false,
            })?;
            last_pricing_refresh_attempt_at = Some(now);
        }
        let mut changes = watch_events.drain_changes(&session_dirs);
        changes.discovery_due |= discovered_new_root;
        runtime.refresh_with_scan_runner(
            &session_dirs,
            options.parallelism,
            timezone,
            snapshot_now,
            changes,
            &refresh_scan_runner,
        )?;
        let snapshot = runtime.snapshot(
            &pricing,
            UsagePresentation::new(options.cached_input_cost_mode, options.cache_read_mode),
            snapshot_now,
            options.show_model_burn_rate,
        )?;
        write_watch_snapshot(&mut stdout, &snapshot, options)?;
        thread::sleep(remaining_watch_sleep(
            options.interval,
            loop_started_at.elapsed(),
        ));
    }
}

/// Render and flush one watch snapshot to terminal output.
fn write_watch_snapshot(
    output: &mut impl Write,
    snapshot: &WatchSnapshot,
    options: &WatchOptions,
) -> Result<()> {
    output.write_all(b"\x1b[2J\x1b[H")?;
    output.write_all(
        render_watch_screen(
            snapshot,
            &options.locale,
            options.number_format,
            options.show_model_burn_rate,
            options.cache_read_mode,
        )
        .as_bytes(),
    )?;
    output.flush()?;
    Ok(())
}

/// Decide whether the terminal should receive ANSI clear-screen sequences.
fn supports_watch_screen_clear(term: Option<&str>) -> bool {
    supports_watch_screen_clear_with_platform(term, cfg!(windows), windows_stdout_supports_ansi())
}

/// Decide whether the terminal should receive ANSI clear-screen sequences.
fn supports_watch_screen_clear_with_platform(
    term: Option<&str>,
    is_windows: bool,
    windows_stdout_supports_ansi: bool,
) -> bool {
    if term == Some("dumb") {
        return false;
    }

    if !is_windows {
        return true;
    }

    term.is_some() || windows_stdout_supports_ansi
}

/// Check whether the active Windows stdout console supports ANSI clear-screen sequences.
#[cfg(windows)]
fn windows_stdout_supports_ansi() -> bool {
    type Bool = i32;
    type Dword = u32;
    type Handle = *mut std::ffi::c_void;

    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: Dword = 0x0004;
    const STD_OUTPUT_HANDLE: Dword = (-11_i32) as Dword;

    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: Dword) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut Dword) -> Bool;
        fn SetConsoleMode(handle: Handle, mode: Dword) -> Bool;
    }

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle as isize == -1 {
            return false;
        }

        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return true;
        }

        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

/// Non-Windows platforms rely on TERM and TTY detection instead of console probing.
#[cfg(not(windows))]
fn windows_stdout_supports_ansi() -> bool {
    false
}

/// Decide whether watch mode should refresh pricing before the next redraw.
fn watch_pricing_refresh_due(
    cache_path: &Path,
    now: SystemTime,
    offline: bool,
    last_refresh_attempt_at: Option<SystemTime>,
) -> Result<bool> {
    let decision = decide_cache_action(
        cache_path,
        now,
        Duration::from_secs(24 * 60 * 60),
        offline,
        false,
    )?;
    if decision != CacheDecision::Refresh {
        return Ok(false);
    }

    Ok(last_refresh_attempt_at.is_none_or(|attempted_at| {
        now.duration_since(attempted_at)
            .is_ok_and(|elapsed| elapsed >= Duration::from_secs(5 * 60))
    }))
}

/// Run the CLI.
///
/// # Errors
///
/// Returns an error when report generation fails or JSON output cannot be serialized.
pub fn run<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = OsString>,
{
    let cli = Cli::parse_from(args);
    #[cfg(debug_assertions)]
    let debug = cli.debug;
    let Cli {
        json,
        since,
        until,
        last_days,
        timezone,
        locale,
        number_format,
        offline,
        refresh_pricing,
        cache_cost,
        session_dir,
        threads,
        command,
        ..
    } = cli;
    let timezone = timezone.unwrap_or_else(default_timezone_name);
    let parallelism = threads.map_or(ScannerParallelism::Auto, ScannerParallelism::Fixed);
    let cached_input_cost_mode = cache_cost.cached_input_cost_mode();
    let cache_read_mode = cache_cost.cache_read_mode();

    match command {
        Some(Command::Watch {
            interval,
            per_model_burn_rate,
        }) => {
            validate_watch_flags(json, since.as_deref(), until.as_deref(), last_days)?;
            run_watch_loop(&WatchOptions {
                timezone,
                locale,
                number_format,
                offline,
                refresh_pricing,
                cached_input_cost_mode,
                cache_read_mode,
                session_dirs: session_dir,
                parallelism,
                interval,
                show_model_burn_rate: per_model_burn_rate,
                #[cfg(debug_assertions)]
                debug,
            })
        }
        command => {
            let kind = command
                .as_ref()
                .map_or(ReportKind::Daily, ReportKind::from_command);
            let options = ReportOptions {
                since,
                until,
                last_days,
                timezone,
                locale,
                number_format,
                json,
                offline,
                refresh_pricing,
                cached_input_cost_mode,
                cache_read_mode,
                session_dirs: session_dir,
                parallelism,
            };
            #[cfg(debug_assertions)]
            let scan_behavior = cli_scan_behavior(true, debug.simulate_slow_disk);
            #[cfg(not(debug_assertions))]
            let scan_behavior = cli_scan_behavior(true);
            let output = build_report_for_cli(kind, &options, scan_behavior)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "{}",
                    render_report(
                        &output,
                        &options.locale,
                        options.number_format,
                        options.cache_read_mode,
                    )
                );
            }
            Ok(())
        }
    }
}

/// Command-line interface for the binary.
#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Analyze Codex session usage with a fast Rust scanner"
)]
struct Cli {
    /// Output structured JSON.
    #[arg(long, short = 'j', global = true)]
    json: bool,
    /// Inclusive start date in YYYY-MM-DD or YYYYMMDD form.
    #[arg(long, short = 's', global = true)]
    since: Option<String>,
    /// Inclusive end date in YYYY-MM-DD or YYYYMMDD form.
    #[arg(long, short = 'u', global = true)]
    until: Option<String>,
    /// Show the last N calendar days ending today in the selected timezone.
    #[arg(
        long,
        short = 'L',
        global = true,
        value_name = "N",
        value_parser = clap::value_parser!(NonZeroUsize),
        conflicts_with_all = ["since", "until"]
    )]
    last_days: Option<NonZeroUsize>,
    /// IANA timezone used for grouping. Defaults to the system timezone.
    #[arg(long, short = 'z', global = true)]
    timezone: Option<String>,
    /// Locale hint reserved for display formatting.
    #[arg(long, short = 'l', default_value = "en-US", global = true)]
    locale: String,
    /// Table number formatting mode.
    #[arg(long, value_enum, default_value_t = NumberFormat::Short, global = true)]
    number_format: NumberFormat,
    /// Disable network pricing refreshes.
    #[arg(long, short = 'O', global = true)]
    offline: bool,
    /// Force pricing refresh even if the cache is fresh.
    #[arg(long, global = true)]
    refresh_pricing: bool,
    /// Cache-cost pricing options.
    #[command(flatten)]
    cache_cost: CacheCostCliOptions,
    /// Override the session directory. May be repeated.
    #[arg(long, global = true)]
    session_dir: Vec<PathBuf>,
    /// Scanner worker count. Use `1` for single-threaded profiling runs.
    #[arg(long, global = true, value_name = "N", value_parser = clap::value_parser!(NonZeroUsize))]
    threads: Option<NonZeroUsize>,
    /// Debug-only runtime options for development builds.
    #[cfg(debug_assertions)]
    #[command(flatten)]
    debug: DebugRuntimeOptions,
    /// Report to execute.
    #[command(subcommand)]
    command: Option<Command>,
}

/// CLI subcommands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Subcommand)]
enum Command {
    /// Group usage by day.
    #[default]
    Daily,
    /// Group usage by month.
    Monthly,
    /// Group usage by session.
    Session,
    /// Continuously monitor current-day usage and a rolling burn rate.
    Watch {
        /// Refresh interval in seconds.
        #[arg(long, value_name = "SECONDS", default_value = "5", value_parser = parse_interval_seconds)]
        interval: Duration,
        /// Show per-model detail rows for every watch metric.
        #[arg(long)]
        per_model_burn_rate: bool,
    },
}

impl ReportKind {
    /// Convert a report-bearing command into its output kind.
    fn from_command(value: &Command) -> Self {
        match value {
            Command::Daily => Self::Daily,
            Command::Monthly => Self::Monthly,
            Command::Session => Self::Session,
            Command::Watch { .. } => unreachable!("watch mode does not map to ReportKind"),
        }
    }
}

/// Parse a positive refresh interval in seconds.
fn parse_interval_seconds(value: &str) -> std::result::Result<Duration, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| format!("invalid interval {value}; expected a positive integer"))?;
    if seconds == 0 {
        return Err("interval must be greater than 0 seconds".to_string());
    }

    Ok(Duration::from_secs(seconds))
}

/// Whether a bool is false.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if passes field values by reference"
)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// Parse a timezone name.
fn parse_timezone(candidate: &str) -> Result<Tz> {
    candidate
        .parse::<Tz>()
        .wrap_err_with(|| format!("invalid timezone {candidate}"))
}

/// Determine the default timezone name from the local system configuration.
fn default_timezone_name() -> String {
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
fn normalize_timezone_name(candidate: &str) -> Option<String> {
    let normalized = candidate.trim().trim_start_matches(':');
    if normalized.is_empty() || normalized.parse::<Tz>().is_err() {
        return None;
    }

    Some(normalized.to_string())
}

/// Extract a timezone name from `/etc/timezone` contents.
fn timezone_from_etc_timezone_contents(contents: &str) -> Option<String> {
    contents.lines().find_map(normalize_timezone_name)
}

/// Extract a timezone name from an `/etc/localtime` symlink target.
fn timezone_from_localtime_target(target: &Path) -> Option<String> {
    target
        .to_string_lossy()
        .rsplit_once("zoneinfo/")
        .and_then(|(_, timezone)| normalize_timezone_name(timezone))
}

/// Normalize a filter date.
fn normalize_filter_date(value: &str) -> Result<NaiveDate> {
    let compact = value.trim().replace('-', "");
    if compact.len() != 8 || !compact.chars().all(|character| character.is_ascii_digit()) {
        return Err(eyre!(
            "invalid date format {value}; expected YYYYMMDD or YYYY-MM-DD"
        ));
    }
    NaiveDate::parse_from_str(&compact, "%Y%m%d")
        .wrap_err_with(|| format!("failed to parse date {value}"))
}

/// Resolve the effective date filters for the requested report.
fn resolve_report_date_filters(
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
fn resolve_session_dirs(session_dirs: &[PathBuf]) -> Vec<PathBuf> {
    if !session_dirs.is_empty() {
        return session_dirs.to_vec();
    }

    let codex_home = std::env::var_os(DEFAULT_CODEX_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"));
    vec![codex_home.join("sessions")]
}

/// Recursively collect JSONL files.
#[cfg(test)]
fn collect_session_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_session_files(&path, files)?;
        } else if file_type.is_file()
            && path
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            files.push(path);
        }
    }
    Ok(())
}

/// Scanner state shared across files.
#[derive(Clone, Copy, Debug, Default)]
struct RawUsage {
    /// Input tokens.
    input: u64,
    /// Cached input tokens.
    cached_input: u64,
    /// Output tokens.
    output: u64,
    /// Reasoning tokens.
    reasoning_output: u64,
    /// Total tokens.
    total: u64,
}

impl RawUsage {
    /// Convert raw usage into normalized totals.
    fn into_usage_totals(self) -> UsageTotals {
        UsageTotals {
            input: self.input,
            cached_input: self.cached_input.min(self.input),
            output: self.output,
            reasoning_output: self.reasoning_output,
            total: if self.total > 0 {
                self.total
            } else {
                self.input + self.output
            },
        }
    }

    /// Return the billable total, deriving it when legacy payloads omit `total_tokens`.
    fn billable_total(self) -> u64 {
        if self.total > 0 {
            self.total
        } else {
            self.input.saturating_add(self.output)
        }
    }

    /// Advance cumulative totals with one delta usage payload.
    fn advance(self, delta: RawUsage) -> Self {
        Self {
            input: self.input.saturating_add(delta.input),
            cached_input: self.cached_input.saturating_add(delta.cached_input),
            output: self.output.saturating_add(delta.output),
            reasoning_output: self.reasoning_output.saturating_add(delta.reasoning_output),
            total: self.total.saturating_add(delta.billable_total()),
        }
    }
}

impl UsagePayload {
    /// Convert a deserialized usage payload into raw usage counters.
    fn into_raw_usage(self) -> RawUsage {
        RawUsage {
            input: self.input_tokens,
            cached_input: self
                .cached_input_tokens
                .or(self.cache_read_input_tokens)
                .unwrap_or(0),
            output: self.output_tokens,
            reasoning_output: self.reasoning_output_tokens,
            total: self.total_tokens,
        }
    }
}

/// Internal usage accumulator.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageTotals {
    /// Total input tokens.
    pub input: u64,
    /// Total cached input tokens.
    pub cached_input: u64,
    /// Total output tokens.
    pub output: u64,
    /// Total reasoning output tokens.
    pub reasoning_output: u64,
    /// Total billable tokens.
    pub total: u64,
}

impl UsageTotals {
    /// Add one event.
    fn add(&mut self, other: &UsageTotals) {
        self.input += other.input;
        self.cached_input += other.cached_input;
        self.output += other.output;
        self.reasoning_output += other.reasoning_output;
        self.total += other.total;
    }

    /// Remove one event.
    fn subtract(&mut self, other: &UsageTotals) {
        self.input = self.input.saturating_sub(other.input);
        self.cached_input = self.cached_input.saturating_sub(other.cached_input);
        self.output = self.output.saturating_sub(other.output);
        self.reasoning_output = self.reasoning_output.saturating_sub(other.reasoning_output);
        self.total = self.total.saturating_sub(other.total);
    }

    /// Return whether this usage bucket contains any billable activity.
    fn has_usage(&self) -> bool {
        self.input > 0
            || self.cached_input > 0
            || self.output > 0
            || self.reasoning_output > 0
            || self.total > 0
    }

    /// Return usage counters with the selected cache-read reporting mode applied.
    fn with_cache_read_mode(&self, cache_read_mode: CacheReadMode) -> Self {
        match cache_read_mode {
            CacheReadMode::Include => self.clone(),
            CacheReadMode::Exclude => {
                let cached_input = self.cached_input.min(self.input);
                Self {
                    input: self.input.saturating_sub(cached_input),
                    cached_input: 0,
                    output: self.output,
                    reasoning_output: self.reasoning_output,
                    total: self.total.saturating_sub(cached_input),
                }
            }
        }
    }
}

/// Event ready for aggregation.
#[derive(Clone, Debug)]
struct TokenUsageEvent<'session, 'model> {
    /// Unique session key including source-root identity.
    session_key: &'session str,
    /// Session identifier.
    session_id: &'session str,
    /// Timestamp as parsed UTC datetime.
    timestamp_utc: DateTime<Utc>,
    /// Model name.
    model: &'model str,
    /// Whether model fallback was used.
    is_fallback_model: bool,
    /// Token totals.
    usage: UsageTotals,
}

/// One parsed JSONL entry.
#[derive(Deserialize)]
struct SessionLogEntry<'a> {
    /// Entry kind.
    #[serde(
        rename = "type",
        borrow,
        default,
        deserialize_with = "deserialize_optional_cow_lossy"
    )]
    entry_type: Option<Cow<'a, str>>,
    /// Event timestamp.
    #[serde(borrow, default, deserialize_with = "deserialize_optional_cow_lossy")]
    timestamp: Option<Cow<'a, str>>,
    /// Entry payload.
    #[serde(
        borrow,
        default,
        deserialize_with = "deserialize_optional_object_lossy"
    )]
    payload: Option<EntryPayload<'a>>,
}

/// Payload fields used by turn-context and token-count events.
#[derive(Default, Deserialize)]
struct EntryPayload<'a> {
    /// Payload kind.
    #[serde(
        rename = "type",
        borrow,
        default,
        deserialize_with = "deserialize_optional_cow_lossy"
    )]
    payload_type: Option<Cow<'a, str>>,
    /// Nested event info object.
    #[serde(
        borrow,
        default,
        deserialize_with = "deserialize_optional_object_lossy"
    )]
    info: Option<EntryInfo<'a>>,
    /// Inline usage delta.
    #[serde(default, deserialize_with = "deserialize_optional_object_lossy")]
    last_token_usage: Option<UsagePayload>,
    /// Inline cumulative usage.
    #[serde(default, deserialize_with = "deserialize_optional_object_lossy")]
    total_token_usage: Option<UsagePayload>,
    /// Model lookup fields.
    #[serde(flatten, borrow)]
    model_fields: ModelFields<'a>,
}

/// Nested info object inside token-count events.
#[derive(Default, Deserialize)]
struct EntryInfo<'a> {
    /// Usage delta.
    #[serde(default, deserialize_with = "deserialize_optional_object_lossy")]
    last_token_usage: Option<UsagePayload>,
    /// Cumulative usage.
    #[serde(default, deserialize_with = "deserialize_optional_object_lossy")]
    total_token_usage: Option<UsagePayload>,
    /// Model lookup fields.
    #[serde(flatten, borrow)]
    model_fields: ModelFields<'a>,
}

/// Common model lookup fields reused across payload shapes.
#[derive(Default, Deserialize)]
struct ModelFields<'a> {
    /// Primary model field.
    #[serde(borrow, default, deserialize_with = "deserialize_optional_cow_lossy")]
    model: Option<Cow<'a, str>>,
    /// Alternate model field.
    #[serde(
        rename = "model_name",
        borrow,
        default,
        deserialize_with = "deserialize_optional_cow_lossy"
    )]
    model_name: Option<Cow<'a, str>>,
    /// Nested metadata lookup.
    #[serde(
        borrow,
        default,
        deserialize_with = "deserialize_optional_object_lossy"
    )]
    metadata: Option<ModelMetadata<'a>>,
}

/// Nested metadata container.
#[derive(Deserialize)]
struct ModelMetadata<'a> {
    /// Model name from metadata.
    #[serde(borrow, default, deserialize_with = "deserialize_optional_cow_lossy")]
    model: Option<Cow<'a, str>>,
}

/// Deserialize an optional string field while ignoring invalid scalar shapes.
fn deserialize_optional_cow_lossy<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Cow<'de, str>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalCowVisitor;

    impl<'de> serde::de::Visitor<'de> for OptionalCowVisitor {
        type Value = Option<Cow<'de, str>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an optional string")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_optional_cow_lossy(deserializer)
        }

        fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(Cow::Borrowed(value)))
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(Cow::Owned(value.to_string())))
        }

        fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(Cow::Owned(value)))
        }

        fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_u64<E>(self, _value: u64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(OptionalCowVisitor)
}

/// Deserialize a token counter while treating invalid field types as zero.
fn deserialize_u64_lossy<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct LossyU64Visitor;

    impl<'de> serde::de::Visitor<'de> for LossyU64Visitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an integer token count")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_u64_lossy(deserializer)
        }

        fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value)
        }

        fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_borrowed_str<E>(self, _value: &'de str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_str<E>(self, _value: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_string<E>(self, _value: String) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(0)
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {}
            Ok(0)
        }
    }

    deserializer.deserialize_any(LossyU64Visitor)
}

/// Deserialize an optional token counter while ignoring invalid field types.
fn deserialize_optional_u64_lossy<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalU64Visitor;

    impl<'de> serde::de::Visitor<'de> for OptionalU64Visitor {
        type Value = Option<u64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an optional integer token count")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_optional_u64_lossy(deserializer)
        }

        fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(value))
        }

        fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_borrowed_str<E>(self, _value: &'de str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_str<E>(self, _value: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_string<E>(self, _value: String) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(OptionalU64Visitor)
}

/// Deserialize an optional object while ignoring wrong-type values.
fn deserialize_optional_object_lossy<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    struct OptionalObjectVisitor<T>(PhantomData<T>);

    impl<'de, T> serde::de::Visitor<'de> for OptionalObjectVisitor<T>
    where
        T: serde::Deserialize<'de>,
    {
        type Value = Option<T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an optional object")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_optional_object_lossy(deserializer)
        }

        fn visit_map<A>(self, map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let value = T::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
            Ok(Some(value))
        }

        fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_u64<E>(self, _value: u64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_borrowed_str<E>(self, _value: &'de str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_str<E>(self, _value: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_string<E>(self, _value: String) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(OptionalObjectVisitor::<T>(PhantomData))
}

/// Usage payload read directly from JSON.
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror the Codex JSON payload shape verbatim"
)]
#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct UsagePayload {
    /// Input tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    input_tokens: u64,
    /// Cached input tokens.
    #[serde(default, deserialize_with = "deserialize_optional_u64_lossy")]
    cached_input_tokens: Option<u64>,
    /// Legacy cached input token field.
    #[serde(default, deserialize_with = "deserialize_optional_u64_lossy")]
    cache_read_input_tokens: Option<u64>,
    /// Output tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    output_tokens: u64,
    /// Reasoning output tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    reasoning_output_tokens: u64,
    /// Total tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    total_tokens: u64,
}

/// One per-group summary.
#[derive(Clone, Debug, Default)]
struct GroupSummary {
    /// Totals for all models in the group.
    totals: UsageTotals,
    /// Per-model breakdowns.
    models: HashMap<String, ModelBreakdown>,
}

/// One session summary.
#[derive(Clone, Debug)]
struct SessionSummary {
    /// Session identifier shown in output.
    display_session_id: String,
    /// Totals for all models in the session.
    totals: UsageTotals,
    /// Per-model breakdowns.
    models: HashMap<String, ModelBreakdown>,
    /// Most recent activity.
    last_activity: DateTime<Utc>,
}

impl SessionSummary {
    /// Create a summary starting with the given event time.
    fn new(last_activity: DateTime<Utc>, display_session_id: String) -> Self {
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
struct SessionScanTarget {
    /// Stable session identifier derived from the relative path.
    ///
    /// This is intentionally global across `--session-dir` roots. When the same identifier
    /// appears more than once, the longer file is treated as the newer copy of that session.
    session_id: String,
    /// Path to the selected JSONL file.
    path: PathBuf,
    /// File length used to detect newer copies across roots.
    bytes: u64,
    /// Modification time used as a deterministic tie-breaker.
    modified: Option<SystemTime>,
}

impl SessionScanTarget {
    /// Decide whether this candidate should replace an existing one.
    fn is_preferred_over(&self, existing: &Self) -> bool {
        self.bytes > existing.bytes
            || (self.bytes == existing.bytes
                && (self.modified > existing.modified
                    || (self.modified == existing.modified && self.path > existing.path)))
    }
}

/// Builder for the requested report kind.
struct ReportBuilder {
    /// Report flavor.
    kind: ReportKind,
    /// Grouping timezone.
    timezone: Tz,
    /// Inclusive lower bound.
    since: Option<NaiveDate>,
    /// Inclusive upper bound.
    until: Option<NaiveDate>,
    /// Daily summaries.
    daily: HashMap<String, GroupSummary>,
    /// Monthly summaries.
    monthly: HashMap<String, GroupSummary>,
    /// Session summaries.
    session: HashMap<String, SessionSummary>,
}

impl ReportBuilder {
    /// Create a new builder.
    fn new(
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
    fn observe(&mut self, event: &TokenUsageEvent<'_, '_>) {
        let local = event.timestamp_utc.with_timezone(&self.timezone);
        let date = local.date_naive();
        if self.since.is_some_and(|since| date < since)
            || self.until.is_some_and(|until| date > until)
        {
            return;
        }

        match self.kind {
            ReportKind::Daily => {
                let key = date.format("%Y-%m-%d").to_string();
                let summary = self.daily.entry(key).or_default();
                push_event_into_summary(summary, event);
            }
            ReportKind::Monthly => {
                let key = local.format("%Y-%m").to_string();
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
    fn finish(
        self,
        pricing: &PricingCatalog,
        presentation: UsagePresentation,
        missing_directories: Vec<String>,
    ) -> Result<ReportOutput> {
        let context = ReportFinishContext {
            pricing,
            presentation,
        };
        match self.kind {
            ReportKind::Daily => self.finish_daily(context, missing_directories),
            ReportKind::Monthly => self.finish_monthly(context, missing_directories),
            ReportKind::Session => Ok(self.finish_session(context, missing_directories)),
        }
    }

    /// Finish a daily report.
    fn finish_daily(
        self,
        context: ReportFinishContext<'_>,
        missing_directories: Vec<String>,
    ) -> Result<ReportOutput> {
        let mut rows = Vec::with_capacity(self.daily.len());
        let mut keys = self.daily.keys().cloned().collect::<Vec<_>>();
        keys.sort_unstable();
        let mut totals = Totals::default();
        for key in keys {
            let summary = self
                .daily
                .get(&key)
                .ok_or_else(|| eyre!("missing daily summary for key {key}"))?;
            let (visible_totals, cost, models) =
                context.finish_summary(&summary.totals, &summary.models);
            push_totals(&mut totals, &visible_totals, cost);
            rows.push(DailyRow {
                date: key,
                input_tokens: visible_totals.input,
                cached_input_tokens: visible_totals.cached_input,
                output_tokens: visible_totals.output,
                reasoning_output_tokens: visible_totals.reasoning_output,
                total_tokens: visible_totals.total,
                cost_usd: cost,
                models,
            });
        }
        Ok(ReportOutput::Daily {
            rows,
            totals,
            missing_directories,
        })
    }

    /// Finish a monthly report.
    fn finish_monthly(
        self,
        context: ReportFinishContext<'_>,
        missing_directories: Vec<String>,
    ) -> Result<ReportOutput> {
        let mut rows = Vec::with_capacity(self.monthly.len());
        let mut keys = self.monthly.keys().cloned().collect::<Vec<_>>();
        keys.sort_unstable();
        let mut totals = Totals::default();
        for key in keys {
            let summary = self
                .monthly
                .get(&key)
                .ok_or_else(|| eyre!("missing monthly summary for key {key}"))?;
            let (visible_totals, cost, models) =
                context.finish_summary(&summary.totals, &summary.models);
            push_totals(&mut totals, &visible_totals, cost);
            rows.push(MonthlyRow {
                month: key,
                input_tokens: visible_totals.input,
                cached_input_tokens: visible_totals.cached_input,
                output_tokens: visible_totals.output,
                reasoning_output_tokens: visible_totals.reasoning_output,
                total_tokens: visible_totals.total,
                cost_usd: cost,
                models,
            });
        }
        Ok(ReportOutput::Monthly {
            rows,
            totals,
            missing_directories,
        })
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
    fn merge(&mut self, other: Self) {
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
}

/// Shared report finalization inputs.
#[derive(Clone, Copy)]
struct ReportFinishContext<'a> {
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

/// Shared metadata needed to render one watch snapshot.
struct WatchSnapshotContext {
    /// Grouping timezone.
    timezone: Tz,
    /// Current day in the selected timezone.
    current_day: NaiveDate,
    /// Inclusive UTC upper bound for the rolling window.
    now_utc: DateTime<Utc>,
    /// Effective rolling window duration after clamping to the current day.
    window_duration: Duration,
}

impl WatchSnapshotContext {
    /// Build the snapshot context for one logical watch timestamp.
    fn new(timezone: Tz, now_utc: DateTime<Utc>) -> Result<Self> {
        let current_day = now_utc.with_timezone(&timezone).date_naive();
        let window_duration = now_utc
            .signed_duration_since(watch_window_start_utc(timezone, now_utc)?)
            .to_std()
            .unwrap_or_default();

        Ok(Self {
            timezone,
            current_day,
            now_utc,
            window_duration,
        })
    }

    /// Build one rendered snapshot from already aggregated summaries.
    fn finish(
        &self,
        current_day_summary: &GroupSummary,
        burn_window_summary: &GroupSummary,
        pricing: &PricingCatalog,
        presentation: UsagePresentation,
        missing_directories: Vec<String>,
        include_per_model: bool,
    ) -> WatchSnapshot {
        let current_day_cost =
            calculate_summary_cost(&current_day_summary.models, pricing, presentation);
        let burn_cost = calculate_summary_cost(&burn_window_summary.models, pricing, presentation);
        let current_day_totals = current_day_summary
            .totals
            .with_cache_read_mode(presentation.cache_read_mode);
        let burn_window_totals = burn_window_summary
            .totals
            .with_cache_read_mode(presentation.cache_read_mode);
        let totals = Totals {
            input_tokens: current_day_totals.input,
            cached_input_tokens: current_day_totals.cached_input,
            output_tokens: current_day_totals.output,
            reasoning_output_tokens: current_day_totals.reasoning_output,
            total_tokens: current_day_totals.total,
            cost_usd: current_day_cost,
        };
        let per_model = if include_per_model {
            to_sorted_models(&burn_window_summary.models, pricing, presentation)
        } else {
            BTreeMap::new()
        };

        WatchSnapshot {
            date: self.current_day.format("%Y-%m-%d").to_string(),
            totals,
            burn_rate: BurnRateSnapshot {
                window_duration: self.window_duration,
                window_minutes: display_window_minutes(self.window_duration),
                input_tokens_per_hour: scale_usage_per_hour(
                    burn_window_totals.input,
                    self.window_duration,
                ),
                cached_input_tokens_per_hour: scale_usage_per_hour(
                    burn_window_totals.cached_input,
                    self.window_duration,
                ),
                output_tokens_per_hour: scale_usage_per_hour(
                    burn_window_totals.output,
                    self.window_duration,
                ),
                reasoning_output_tokens_per_hour: scale_usage_per_hour(
                    burn_window_totals.reasoning_output,
                    self.window_duration,
                ),
                total_tokens_per_hour: scale_usage_per_hour(
                    burn_window_totals.total,
                    self.window_duration,
                ),
                cost_usd_per_hour: scale_cost_per_hour(burn_cost, self.window_duration),
            },
            per_model,
            missing_directories,
            updated_time: self
                .now_utc
                .with_timezone(&self.timezone)
                .format("%H:%M:%S")
                .to_string(),
        }
    }
}

/// Builder for one live watch snapshot.
#[cfg(test)]
struct WatchBuilder {
    /// Shared snapshot rendering context.
    snapshot_context: WatchSnapshotContext,
    /// Inclusive UTC lower bound for the rolling window.
    window_start_utc: DateTime<Utc>,
    /// Current-day cumulative usage.
    current_day_summary: GroupSummary,
    /// Bounded rolling-window usage.
    burn_window_summary: GroupSummary,
}

#[cfg(test)]
impl WatchBuilder {
    /// Create a new builder.
    fn new(timezone: Tz, now_utc: DateTime<Utc>) -> Result<Self> {
        Ok(Self {
            snapshot_context: WatchSnapshotContext::new(timezone, now_utc)?,
            window_start_utc: watch_window_start_utc(timezone, now_utc)?,
            current_day_summary: GroupSummary::default(),
            burn_window_summary: GroupSummary::default(),
        })
    }

    /// Observe one event.
    fn observe(&mut self, event: &TokenUsageEvent<'_, '_>) {
        if event.timestamp_utc > self.snapshot_context.now_utc {
            return;
        }

        let local = event
            .timestamp_utc
            .with_timezone(&self.snapshot_context.timezone);
        if local.date_naive() != self.snapshot_context.current_day {
            return;
        }

        push_event_into_summary(&mut self.current_day_summary, event);
        if event.timestamp_utc >= self.window_start_utc {
            push_event_into_summary(&mut self.burn_window_summary, event);
        }
    }

    /// Merge another watch builder into this one.
    fn merge(&mut self, other: Self) {
        debug_assert_eq!(
            self.snapshot_context.timezone, other.snapshot_context.timezone,
            "parallel scan chunks must preserve watch timezone",
        );
        debug_assert_eq!(
            self.snapshot_context.current_day, other.snapshot_context.current_day,
            "parallel scan chunks must preserve watch day",
        );
        debug_assert_eq!(
            self.window_start_utc, other.window_start_utc,
            "parallel scan chunks must preserve watch window start",
        );
        debug_assert_eq!(
            self.snapshot_context.now_utc, other.snapshot_context.now_utc,
            "parallel scan chunks must preserve watch now",
        );

        merge_group_summary(&mut self.current_day_summary, other.current_day_summary);
        merge_group_summary(&mut self.burn_window_summary, other.burn_window_summary);
    }

    /// Finish the snapshot.
    fn finish(
        self,
        pricing: &PricingCatalog,
        presentation: UsagePresentation,
        missing_directories: Vec<String>,
        include_per_model: bool,
    ) -> WatchSnapshot {
        self.snapshot_context.finish(
            &self.current_day_summary,
            &self.burn_window_summary,
            pricing,
            presentation,
            missing_directories,
            include_per_model,
        )
    }
}

/// Resolve the UTC start of the rolling watch window for one logical timestamp.
fn watch_window_start_utc(timezone: Tz, now_utc: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let current_day = now_utc.with_timezone(&timezone).date_naive();
    let day_start_utc = resolve_local_midnight_utc(timezone, current_day)?;
    let hour_ago = now_utc
        .checked_sub_signed(TimeDelta::hours(1))
        .ok_or_else(|| eyre!("watch window underflowed the supported timestamp range"))?;
    Ok(hour_ago.max(day_start_utc))
}

/// One owned usage event cached for watch-mode refreshes.
#[derive(Clone, Debug)]
struct OwnedWatchEvent {
    /// Event timestamp in UTC.
    timestamp_utc: DateTime<Utc>,
    /// Resolved model name.
    model: String,
    /// Whether the model was inferred from fallback metadata.
    is_fallback_model: bool,
    /// Normalized usage delta.
    usage: UsageTotals,
}

/// Incremental parser checkpoint reused when a watch file grows by appending new lines.
#[derive(Clone, Debug, Default)]
struct SessionParseCheckpoint {
    /// Byte offset of the next unread position in the file.
    offset: u64,
    /// Previous cumulative usage totals needed to normalize future events.
    previous_totals: Option<RawUsage>,
    /// Last resolved model name.
    current_model: Option<String>,
    /// Whether the remembered model came from fallback inference.
    current_model_is_fallback: bool,
}

/// Cached current-day contribution for one selected session file.
#[derive(Clone, Debug)]
struct CachedWatchFile {
    /// Metadata used to detect whether the file changed since the previous refresh.
    target: SessionScanTarget,
    /// Current-day events already parsed from the file.
    current_day_events: Vec<OwnedWatchEvent>,
    /// Incremental parser state at the end of the file.
    parser_checkpoint: SessionParseCheckpoint,
    /// Number of cached current-day events that are visible at the current watch timestamp.
    visible_end: usize,
    /// Index of the first event still inside the rolling burn window.
    burn_start: usize,
}

impl CachedWatchFile {
    /// Build one cached file from a full file scan.
    fn from_full_scan(
        target: SessionScanTarget,
        parser_checkpoint: SessionParseCheckpoint,
        mut current_day_events: Vec<OwnedWatchEvent>,
        window_start_utc: DateTime<Utc>,
        now_utc: DateTime<Utc>,
    ) -> Self {
        current_day_events.sort_unstable_by_key(|event| event.timestamp_utc);
        let mut cached = Self {
            target,
            current_day_events,
            parser_checkpoint,
            visible_end: 0,
            burn_start: 0,
        };
        cached.recompute_bounds(window_start_utc, now_utc);
        cached
    }

    /// Return the current-day events visible at the current watch timestamp.
    fn visible_events(&self) -> &[OwnedWatchEvent] {
        &self.current_day_events[..self.visible_end]
    }

    /// Return only the visible events that still fall inside the rolling watch window.
    fn burn_events(&self) -> &[OwnedWatchEvent] {
        &self.current_day_events[self.burn_start..self.visible_end]
    }

    /// Recompute visibility and burn-window bounds for the provided logical timestamp.
    fn recompute_bounds(&mut self, window_start_utc: DateTime<Utc>, now_utc: DateTime<Utc>) {
        self.visible_end = self
            .current_day_events
            .partition_point(|event| event.timestamp_utc <= now_utc);
        self.burn_start = self.current_day_events[..self.visible_end]
            .partition_point(|event| event.timestamp_utc < window_start_utc);
    }
}

/// Cached watch-mode state reused between refreshes.
struct WatchRuntimeState {
    /// Grouping timezone.
    timezone: Tz,
    /// Current day covered by the cached events.
    current_day: NaiveDate,
    /// Missing session roots from the last discovery pass.
    missing_directories: Vec<String>,
    /// Selected targets currently tracked by watch mode.
    selected_targets: HashMap<String, SessionScanTarget>,
    /// Current-day events keyed by stable session identifier.
    cached_files: HashMap<String, CachedWatchFile>,
    /// Current-day totals visible at `last_snapshot_utc`.
    current_day_summary: GroupSummary,
    /// Rolling burn-window totals visible at `last_snapshot_utc`.
    burn_window_summary: GroupSummary,
    /// Logical timestamp used to build the cached summaries.
    last_snapshot_utc: Option<DateTime<Utc>>,
    /// Next time watch mode should rediscover the session tree.
    next_discovery_at: Option<Instant>,
}

impl WatchRuntimeState {
    /// Load the initial runtime state.
    #[cfg(test)]
    fn load(
        session_dirs: &[PathBuf],
        parallelism: ScannerParallelism,
        timezone: Tz,
        now_utc: DateTime<Utc>,
    ) -> Result<Self> {
        Self::load_with_scan_runner(
            session_dirs,
            parallelism,
            timezone,
            now_utc,
            &NoopScanBatchRunner,
        )
    }

    /// Load the initial runtime state with the provided batch runner.
    fn load_with_scan_runner<R>(
        session_dirs: &[PathBuf],
        parallelism: ScannerParallelism,
        timezone: Tz,
        now_utc: DateTime<Utc>,
        scan_runner: &R,
    ) -> Result<Self>
    where
        R: ScanBatchRunner,
    {
        let mut state = Self {
            timezone,
            current_day: now_utc.with_timezone(&timezone).date_naive(),
            missing_directories: Vec::new(),
            selected_targets: HashMap::new(),
            cached_files: HashMap::new(),
            current_day_summary: GroupSummary::default(),
            burn_window_summary: GroupSummary::default(),
            last_snapshot_utc: None,
            next_discovery_at: None,
        };
        state.refresh_with_scan_runner(
            session_dirs,
            parallelism,
            timezone,
            now_utc,
            WatchChangeSet {
                dirty_sessions: HashMap::new(),
                discovery_due: true,
            },
            scan_runner,
        )?;
        Ok(state)
    }

    /// Refresh cached file data for the current watch tick.
    #[cfg(test)]
    fn refresh(
        &mut self,
        session_dirs: &[PathBuf],
        parallelism: ScannerParallelism,
        timezone: Tz,
        now_utc: DateTime<Utc>,
        changes: WatchChangeSet,
    ) -> Result<()> {
        self.refresh_with_scan_runner(
            session_dirs,
            parallelism,
            timezone,
            now_utc,
            changes,
            &NoopScanBatchRunner,
        )
    }

    /// Refresh cached file data for the current watch tick with the provided batch runner.
    fn refresh_with_scan_runner<R>(
        &mut self,
        session_dirs: &[PathBuf],
        parallelism: ScannerParallelism,
        timezone: Tz,
        now_utc: DateTime<Utc>,
        mut changes: WatchChangeSet,
        scan_runner: &R,
    ) -> Result<()>
    where
        R: ScanBatchRunner,
    {
        let current_day = now_utc.with_timezone(&timezone).date_naive();
        if current_day != self.current_day {
            self.current_day = current_day;
            self.selected_targets.clear();
            self.cached_files.clear();
            self.current_day_summary = GroupSummary::default();
            self.burn_window_summary = GroupSummary::default();
            self.last_snapshot_utc = None;
            self.next_discovery_at = None;
            changes.discovery_due = true;
        }

        self.advance_to(now_utc)?;

        let refresh_started_at = Instant::now();
        let discovery_due = changes.discovery_due
            || self
                .next_discovery_at
                .is_none_or(|deadline| refresh_started_at >= deadline);
        if discovery_due {
            let (missing_directories, selected_files) = collect_session_scan_targets(session_dirs)?;
            self.missing_directories = missing_directories;
            self.next_discovery_at = Some(refresh_started_at + WATCH_DISCOVERY_INTERVAL);
            self.refresh_discovered_targets(
                parallelism,
                timezone,
                now_utc,
                selected_files,
                &changes.dirty_sessions,
                scan_runner,
            )?;
        } else {
            self.missing_directories = collect_missing_session_dirs(session_dirs)?;
            for (session_id, dirty_kind) in changes.dirty_sessions {
                let resolved = resolve_session_target_across_roots(session_dirs, &session_id)?;
                self.refresh_dirty_session(
                    session_id,
                    resolved,
                    dirty_kind,
                    timezone,
                    now_utc,
                    scan_runner,
                )?;
            }
        }

        Ok(())
    }

    /// Advance cached file bounds and aggregate totals to the provided logical timestamp.
    fn advance_to(&mut self, now_utc: DateTime<Utc>) -> Result<()> {
        let Some(previous_now_utc) = self.last_snapshot_utc else {
            self.rebuild_summaries_at(now_utc)?;
            return Ok(());
        };
        if now_utc < previous_now_utc {
            self.rebuild_summaries_at(now_utc)?;
            return Ok(());
        }

        let window_start_utc = watch_window_start_utc(self.timezone, now_utc)?;
        for cached_file in self.cached_files.values_mut() {
            let previous_visible_end = cached_file.visible_end;
            let previous_burn_start = cached_file.burn_start;
            cached_file.recompute_bounds(window_start_utc, now_utc);

            for event in
                &cached_file.current_day_events[previous_visible_end..cached_file.visible_end]
            {
                push_owned_watch_event_into_summary(&mut self.current_day_summary, event);
                if event.timestamp_utc >= window_start_utc {
                    push_owned_watch_event_into_summary(&mut self.burn_window_summary, event);
                }
            }
            for event in
                &cached_file.current_day_events[previous_burn_start..cached_file.burn_start]
            {
                remove_owned_watch_event_from_summary(&mut self.burn_window_summary, event);
            }
        }
        self.last_snapshot_utc = Some(now_utc);
        Ok(())
    }

    /// Rebuild aggregate summaries from the cached per-file event lists.
    fn rebuild_summaries_at(&mut self, now_utc: DateTime<Utc>) -> Result<()> {
        let window_start_utc = watch_window_start_utc(self.timezone, now_utc)?;
        self.current_day_summary = GroupSummary::default();
        self.burn_window_summary = GroupSummary::default();
        for cached_file in self.cached_files.values_mut() {
            cached_file.recompute_bounds(window_start_utc, now_utc);
            for event in cached_file.visible_events() {
                push_owned_watch_event_into_summary(&mut self.current_day_summary, event);
            }
            for event in cached_file.burn_events() {
                push_owned_watch_event_into_summary(&mut self.burn_window_summary, event);
            }
        }
        self.last_snapshot_utc = Some(now_utc);
        Ok(())
    }

    /// Refresh the tracked set of selected files after a full discovery pass.
    fn refresh_discovered_targets<R>(
        &mut self,
        parallelism: ScannerParallelism,
        timezone: Tz,
        now_utc: DateTime<Utc>,
        selected_files: Vec<SessionScanTarget>,
        dirty_sessions: &HashMap<String, WatchDirtyKind>,
        scan_runner: &R,
    ) -> Result<()>
    where
        R: ScanBatchRunner,
    {
        let mut selected_targets = HashMap::with_capacity(selected_files.len());
        let mut next_cached_files = HashMap::with_capacity(selected_files.len());
        let mut full_rebuild_targets = Vec::new();

        for target in selected_files {
            let session_id = target.session_id.clone();
            selected_targets.insert(session_id.clone(), target.clone());
            let is_dirty = dirty_sessions.contains_key(&session_id);
            match self.cached_files.remove(&session_id) {
                Some(cached) if !is_dirty && same_watch_target(&cached.target, &target) => {
                    next_cached_files.insert(session_id, cached);
                }
                Some(_) | None => {
                    full_rebuild_targets.push(target);
                }
            }
        }

        for cached_file in scan_runner.run_batch(full_rebuild_targets.len(), |observer| {
            load_cached_watch_files_with_observer(
                &full_rebuild_targets,
                parallelism,
                timezone,
                self.current_day,
                now_utc,
                observer,
            )
        })? {
            next_cached_files.insert(cached_file.target.session_id.clone(), cached_file);
        }

        self.selected_targets = selected_targets;
        self.cached_files = next_cached_files;
        self.rebuild_summaries_at(now_utc)
    }

    /// Refresh one dirty session selected by the watcher.
    fn refresh_dirty_session<R>(
        &mut self,
        session_id: String,
        target: Option<SessionScanTarget>,
        dirty_kind: WatchDirtyKind,
        timezone: Tz,
        now_utc: DateTime<Utc>,
        scan_runner: &R,
    ) -> Result<()>
    where
        R: ScanBatchRunner,
    {
        let window_start_utc = watch_window_start_utc(timezone, now_utc)?;
        let existing = self.cached_files.remove(&session_id);
        if let Some(cached) = existing.as_ref() {
            remove_cached_watch_file_from_runtime(self, cached);
        }

        match (existing, target, dirty_kind) {
            (Some(mut cached), Some(target), WatchDirtyKind::AppendOnly)
                if cached.target.path == target.path
                    && target.bytes > cached.target.bytes
                    && cached.parser_checkpoint.offset == cached.target.bytes =>
            {
                scan_runner.run_batch(1, |observer| {
                    append_cached_watch_file_with_observer(
                        &mut cached,
                        target,
                        timezone,
                        self.current_day,
                        window_start_utc,
                        now_utc,
                        observer,
                    )
                })?;
                add_cached_watch_file_to_runtime(self, &cached);
                self.selected_targets
                    .insert(session_id, cached.target.clone());
                self.cached_files
                    .insert(cached.target.session_id.clone(), cached);
            }
            (_existing, Some(target), _) => {
                let cached = scan_runner.run_batch(1, |observer| {
                    build_cached_watch_file_with_observer(
                        &target,
                        timezone,
                        self.current_day,
                        now_utc,
                        observer,
                    )
                })?;
                add_cached_watch_file_to_runtime(self, &cached);
                self.selected_targets.insert(session_id, target);
                self.cached_files
                    .insert(cached.target.session_id.clone(), cached);
            }
            (_existing, None, _) => {
                self.selected_targets.remove(&session_id);
            }
        }
        Ok(())
    }

    /// Build one watch snapshot from the cached aggregate summaries.
    fn snapshot(
        &self,
        pricing: &PricingCatalog,
        presentation: UsagePresentation,
        now_utc: DateTime<Utc>,
        include_per_model: bool,
    ) -> Result<WatchSnapshot> {
        let snapshot_context = WatchSnapshotContext::new(self.timezone, now_utc)?;
        Ok(snapshot_context.finish(
            &self.current_day_summary,
            &self.burn_window_summary,
            pricing,
            presentation,
            self.missing_directories.clone(),
            include_per_model,
        ))
    }
}

/// Return whether two selected watch targets represent the same unchanged file.
fn same_watch_target(left: &SessionScanTarget, right: &SessionScanTarget) -> bool {
    left.path == right.path && left.bytes == right.bytes && left.modified == right.modified
}

/// Parse all current-day events from one session file for watch-mode caching.
fn scan_watch_file_delta_with_observer<O>(
    file: &Path,
    session_id: &str,
    timezone: Tz,
    current_day: NaiveDate,
    checkpoint: &SessionParseCheckpoint,
    observer: &O,
) -> Result<(SessionParseCheckpoint, Vec<OwnedWatchEvent>)>
where
    O: ScanObserver,
{
    let mut events = Vec::new();
    let checkpoint = scan_session_file_from_checkpoint_with_observer(
        file,
        session_id,
        checkpoint,
        observer,
        |event| {
            if event.timestamp_utc.with_timezone(&timezone).date_naive() != current_day {
                return;
            }
            events.push(OwnedWatchEvent {
                timestamp_utc: event.timestamp_utc,
                model: event.model.to_string(),
                is_fallback_model: event.is_fallback_model,
                usage: event.usage.clone(),
            });
        },
    )?;
    Ok((checkpoint, events))
}

/// Build one cached watch file from the current file contents.
fn build_cached_watch_file_with_observer<O>(
    target: &SessionScanTarget,
    timezone: Tz,
    current_day: NaiveDate,
    now_utc: DateTime<Utc>,
    observer: &O,
) -> Result<CachedWatchFile>
where
    O: ScanObserver,
{
    let (checkpoint, current_day_events) = scan_watch_file_delta_with_observer(
        &target.path,
        &target.session_id,
        timezone,
        current_day,
        &SessionParseCheckpoint::default(),
        observer,
    )?;
    observer.on_file_complete();
    Ok(CachedWatchFile::from_full_scan(
        target.clone(),
        checkpoint,
        current_day_events,
        watch_window_start_utc(timezone, now_utc)?,
        now_utc,
    ))
}

/// Refresh one cached watch file by parsing only the appended suffix.
fn append_cached_watch_file_with_observer<O>(
    cached: &mut CachedWatchFile,
    target: SessionScanTarget,
    timezone: Tz,
    current_day: NaiveDate,
    window_start_utc: DateTime<Utc>,
    now_utc: DateTime<Utc>,
    observer: &O,
) -> Result<()>
where
    O: ScanObserver,
{
    let (checkpoint, appended_events) = scan_watch_file_delta_with_observer(
        &target.path,
        &target.session_id,
        timezone,
        current_day,
        &cached.parser_checkpoint,
        observer,
    )?;
    observer.on_file_complete();
    cached.target = target;
    cached.parser_checkpoint = checkpoint;
    cached.current_day_events.extend(appended_events);
    cached
        .current_day_events
        .sort_unstable_by_key(|event| event.timestamp_utc);
    cached.recompute_bounds(window_start_utc, now_utc);
    Ok(())
}

/// Add one cached file's visible contributions into the runtime aggregates.
fn add_cached_watch_file_to_runtime(runtime: &mut WatchRuntimeState, cached: &CachedWatchFile) {
    for event in cached.visible_events() {
        push_owned_watch_event_into_summary(&mut runtime.current_day_summary, event);
    }
    for event in cached.burn_events() {
        push_owned_watch_event_into_summary(&mut runtime.burn_window_summary, event);
    }
}

/// Remove one cached file's visible contributions from the runtime aggregates.
fn remove_cached_watch_file_from_runtime(
    runtime: &mut WatchRuntimeState,
    cached: &CachedWatchFile,
) {
    for event in cached.visible_events() {
        remove_owned_watch_event_from_summary(&mut runtime.current_day_summary, event);
    }
    for event in cached.burn_events() {
        remove_owned_watch_event_from_summary(&mut runtime.burn_window_summary, event);
    }
}

/// Load current-day watch data for the provided changed targets.
fn load_cached_watch_files_with_observer<O>(
    selected_files: &[SessionScanTarget],
    parallelism: ScannerParallelism,
    timezone: Tz,
    current_day: NaiveDate,
    now_utc: DateTime<Utc>,
    observer: &O,
) -> Result<Vec<CachedWatchFile>>
where
    O: ScanObserver,
{
    if selected_files.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = resolve_scan_worker_count(parallelism, selected_files.len());
    if worker_count == 1 {
        return scan_watch_file_chunk_with_observer(
            selected_files,
            timezone,
            current_day,
            now_utc,
            observer,
        );
    }

    let chunk_size = selected_files.len().div_ceil(worker_count);
    thread::scope(|scope| -> Result<Vec<CachedWatchFile>> {
        let mut chunks = selected_files.chunks(chunk_size);
        let first_chunk = chunks
            .next()
            .ok_or_else(|| eyre!("missing initial watch file chunk"))?;
        let handles = chunks
            .map(|chunk| {
                let observer = observer.clone();
                scope.spawn(move || {
                    scan_watch_file_chunk_with_observer(
                        chunk,
                        timezone,
                        current_day,
                        now_utc,
                        &observer,
                    )
                })
            })
            .collect::<Vec<_>>();

        let mut cached_files = scan_watch_file_chunk_with_observer(
            first_chunk,
            timezone,
            current_day,
            now_utc,
            observer,
        )?;
        for handle in handles {
            cached_files.extend(
                handle
                    .join()
                    .map_err(|_| eyre!("watch file worker panicked"))??,
            );
        }
        Ok(cached_files)
    })
}

/// Parse one worker chunk of changed watch files.
fn scan_watch_file_chunk_with_observer<O>(
    selected_files: &[SessionScanTarget],
    timezone: Tz,
    current_day: NaiveDate,
    now_utc: DateTime<Utc>,
    observer: &O,
) -> Result<Vec<CachedWatchFile>>
where
    O: ScanObserver,
{
    selected_files
        .iter()
        .map(|target| {
            build_cached_watch_file_with_observer(target, timezone, current_day, now_utc, observer)
        })
        .collect()
}

/// Resolve the preferred file for one stable session identifier across all session roots.
fn resolve_session_target_across_roots(
    session_dirs: &[PathBuf],
    session_id: &str,
) -> Result<Option<SessionScanTarget>> {
    let mut selected = None;
    for directory in session_dirs {
        let path = session_file_path(directory, session_id);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                let candidate = SessionScanTarget {
                    session_id: session_id.to_string(),
                    path,
                    bytes: metadata.len(),
                    modified: metadata.modified().ok(),
                };
                if selected
                    .as_ref()
                    .is_none_or(|existing| candidate.is_preferred_over(existing))
                {
                    selected = Some(candidate);
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
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
fn session_file_path(root: &Path, session_id: &str) -> PathBuf {
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
fn collect_missing_session_dirs(session_dirs: &[PathBuf]) -> Result<Vec<String>> {
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

/// Convert the effective window duration into a human-readable minute count.
fn display_window_minutes(window_duration: Duration) -> u64 {
    let seconds = window_duration.as_secs();
    if seconds == 0 {
        return 0;
    }

    seconds.div_ceil(60)
}

/// Compute how long watch mode should sleep after one refresh cycle.
fn remaining_watch_sleep(interval: Duration, elapsed: Duration) -> Duration {
    interval.saturating_sub(elapsed)
}

/// Resolve local midnight for one day into UTC.
fn resolve_local_midnight_utc(timezone: Tz, day: NaiveDate) -> Result<DateTime<Utc>> {
    let midnight = day
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| eyre!("failed to build midnight timestamp for {day}"))?;
    let local = first_valid_local_timestamp(timezone, midnight)
        .ok_or_else(|| eyre!("failed to resolve the start of {day} in timezone {timezone}"))?;

    Ok(local.with_timezone(&Utc))
}

/// Resolve the earliest valid local timestamp at or after the provided naive datetime.
fn first_valid_local_timestamp(timezone: Tz, start: chrono::NaiveDateTime) -> Option<DateTime<Tz>> {
    for minute_offset in 0_i64..=(24_i64 * 60) {
        let candidate = start.checked_add_signed(TimeDelta::minutes(minute_offset))?;
        match timezone.from_local_datetime(&candidate) {
            LocalResult::Single(timestamp) => return Some(timestamp),
            LocalResult::Ambiguous(first, second) => return Some(first.min(second)),
            LocalResult::None => {}
        }
    }

    None
}

/// Convert one usage total into a per-hour burn rate.
fn scale_usage_per_hour(value: u64, window_duration: Duration) -> u64 {
    if value == 0 || window_duration.is_zero() {
        return 0;
    }

    let nanos = window_duration.as_nanos();
    let numerator = u128::from(value) * 3_600_000_000_000 + (nanos / 2);
    let scaled = numerator / nanos;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Convert one cost total into a per-hour burn rate.
fn scale_cost_per_hour(value: f64, window_duration: Duration) -> f64 {
    if value == 0.0 || window_duration.is_zero() {
        return 0.0;
    }

    (value * 3600.0) / window_duration.as_secs_f64()
}

/// Sort session entries deterministically for stable CLI output.
fn sort_session_entries(entries: &mut [(String, SessionSummary)]) {
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
fn push_event_into_summary(summary: &mut GroupSummary, event: &TokenUsageEvent<'_, '_>) {
    summary.totals.add(&event.usage);
    let breakdown = ensure_model_breakdown(&mut summary.models, event.model);
    push_usage_into_breakdown(breakdown, &event.usage, event.is_fallback_model);
    if event.is_fallback_model {
        breakdown.is_fallback = true;
    }
}

/// Push one cached watch event into a grouped summary.
fn push_owned_watch_event_into_summary(summary: &mut GroupSummary, event: &OwnedWatchEvent) {
    summary.totals.add(&event.usage);
    let breakdown = ensure_model_breakdown(&mut summary.models, &event.model);
    push_usage_into_breakdown(breakdown, &event.usage, event.is_fallback_model);
    if event.is_fallback_model {
        breakdown.is_fallback = true;
    }
}

/// Remove one cached watch event from a grouped summary.
fn remove_owned_watch_event_from_summary(summary: &mut GroupSummary, event: &OwnedWatchEvent) {
    summary.totals.subtract(&event.usage);
    let breakdown = ensure_model_breakdown(&mut summary.models, &event.model);
    remove_usage_from_breakdown(breakdown, &event.usage, event.is_fallback_model);
}

/// Push event data into a session summary.
fn push_event_into_session_summary(summary: &mut SessionSummary, event: &TokenUsageEvent<'_, '_>) {
    summary.totals.add(&event.usage);
    let breakdown = ensure_model_breakdown(&mut summary.models, event.model);
    push_usage_into_breakdown(breakdown, &event.usage, event.is_fallback_model);
    if event.is_fallback_model {
        breakdown.is_fallback = true;
    }
}

/// Fetch or create one model breakdown without allocating on every lookup.
fn ensure_model_breakdown<'a>(
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
fn push_usage_into_breakdown(
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
fn remove_usage_from_breakdown(
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
fn to_sorted_models(
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
fn breakdown_usage(breakdown: &ModelBreakdown) -> UsageTotals {
    UsageTotals {
        input: breakdown.input_tokens,
        cached_input: breakdown.cached_input_tokens,
        output: breakdown.output_tokens,
        reasoning_output: breakdown.reasoning_output_tokens,
        total: breakdown.total_tokens,
    }
}

/// Add one row into grand totals.
fn push_totals(totals: &mut Totals, usage: &UsageTotals, cost: f64) {
    totals.input_tokens += usage.input;
    totals.cached_input_tokens += usage.cached_input;
    totals.output_tokens += usage.output;
    totals.reasoning_output_tokens += usage.reasoning_output;
    totals.total_tokens += usage.total;
    totals.cost_usd += cost;
}

/// Calculate the total cost for one summary.
fn calculate_summary_cost(
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
fn split_session_id(session_id: &str) -> (String, String) {
    match session_id.rsplit_once('/') {
        Some((directory, session_file)) => (directory.to_string(), session_file.to_string()),
        None => (String::new(), session_id.to_string()),
    }
}

/// Discover selected session files and collect missing roots.
fn collect_session_scan_targets(
    session_dirs: &[PathBuf],
) -> Result<(Vec<String>, Vec<SessionScanTarget>)> {
    let mut missing_directories = Vec::new();
    let mut selected_files = HashMap::new();
    for directory in session_dirs {
        match fs::metadata(directory) {
            Ok(metadata) if metadata.is_dir() => {
                scan_session_tree(directory, directory, &mut selected_files)?;
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
    let mut session_ids = selected_files.keys().cloned().collect::<Vec<_>>();
    session_ids.sort_unstable();
    let targets = session_ids
        .into_iter()
        .map(|session_id| {
            selected_files
                .remove(&session_id)
                .ok_or_else(|| eyre!("missing session target for key {session_id}"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((missing_directories, targets))
}

/// Scan all selected targets, optionally across multiple worker threads.
fn scan_selected_session_targets(
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
        |chunk| scan_selected_session_chunk(chunk, kind, timezone, since, until),
    )
}

/// Scan all selected targets, optionally across multiple worker threads.
fn scan_selected_session_targets_with_observer<O>(
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
        |chunk| {
            scan_selected_session_chunk_with_observer(chunk, kind, timezone, since, until, observer)
        },
    )
}

/// Scan selected session targets using the provided chunk strategy.
fn scan_selected_session_targets_with<F, B>(
    selected_files: &[SessionScanTarget],
    parallelism: ScannerParallelism,
    empty_builder: B,
    scan_chunk: F,
) -> Result<ReportBuilder>
where
    F: Fn(&[SessionScanTarget]) -> Result<ReportBuilder> + Copy + Send + Sync,
    B: FnOnce() -> ReportBuilder,
{
    if selected_files.is_empty() {
        return Ok(empty_builder());
    }

    let worker_count = resolve_scan_worker_count(parallelism, selected_files.len());
    if worker_count == 1 {
        return scan_chunk(selected_files);
    }

    let chunk_size = selected_files.len().div_ceil(worker_count);
    thread::scope(|scope| -> Result<ReportBuilder> {
        let mut chunks = selected_files.chunks(chunk_size);
        let first_chunk = chunks
            .next()
            .ok_or_else(|| eyre!("missing initial scan chunk"))?;
        let handles = chunks
            .map(|chunk| scope.spawn(move || scan_chunk(chunk)))
            .collect::<Vec<_>>();

        let mut merged = scan_chunk(first_chunk)?;
        for handle in handles {
            let partial = handle
                .join()
                .map_err(|_| eyre!("session scan worker panicked"))??;
            merged.merge(partial);
        }
        Ok(merged)
    })
}

/// Scan all selected targets for watch mode, optionally across worker threads.
#[cfg(test)]
fn scan_watch_targets(
    selected_files: &[SessionScanTarget],
    parallelism: ScannerParallelism,
    timezone: Tz,
    now_utc: DateTime<Utc>,
) -> Result<WatchBuilder> {
    if selected_files.is_empty() {
        return WatchBuilder::new(timezone, now_utc);
    }

    let worker_count = resolve_scan_worker_count(parallelism, selected_files.len());
    if worker_count == 1 {
        return scan_watch_chunk(selected_files, timezone, now_utc);
    }

    let chunk_size = selected_files.len().div_ceil(worker_count);
    thread::scope(|scope| -> Result<WatchBuilder> {
        let mut chunks = selected_files.chunks(chunk_size);
        let first_chunk = chunks
            .next()
            .ok_or_else(|| eyre!("missing initial watch scan chunk"))?;
        let handles = chunks
            .map(|chunk| scope.spawn(move || scan_watch_chunk(chunk, timezone, now_utc)))
            .collect::<Vec<_>>();

        let mut merged = scan_watch_chunk(first_chunk, timezone, now_utc)?;
        for handle in handles {
            let partial = handle
                .join()
                .map_err(|_| eyre!("watch scan worker panicked"))??;
            merged.merge(partial);
        }
        Ok(merged)
    })
}

/// Scan one worker chunk of selected session files.
fn scan_selected_session_chunk(
    selected_files: &[SessionScanTarget],
    kind: ReportKind,
    timezone: Tz,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
) -> Result<ReportBuilder> {
    scan_selected_session_chunk_with(
        selected_files,
        kind,
        timezone,
        since,
        until,
        |target, builder| scan_session_file(&target.path, &target.session_id, builder),
    )
}

/// Scan one worker chunk of selected session files.
fn scan_selected_session_chunk_with_observer<O>(
    selected_files: &[SessionScanTarget],
    kind: ReportKind,
    timezone: Tz,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
    observer: &O,
) -> Result<ReportBuilder>
where
    O: ScanObserver,
{
    scan_selected_session_chunk_with(
        selected_files,
        kind,
        timezone,
        since,
        until,
        |target, builder| {
            scan_session_file_with_observer(&target.path, &target.session_id, builder, observer)
        },
    )
}

/// Scan one worker chunk of selected session files using the provided file strategy.
fn scan_selected_session_chunk_with<F>(
    selected_files: &[SessionScanTarget],
    kind: ReportKind,
    timezone: Tz,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
    mut scan_file: F,
) -> Result<ReportBuilder>
where
    F: FnMut(&SessionScanTarget, &mut ReportBuilder) -> Result<()>,
{
    let mut builder = ReportBuilder::new(kind, timezone, since, until);
    for target in selected_files {
        scan_file(target, &mut builder)?;
    }
    Ok(builder)
}

/// Scan one worker chunk of selected session files for watch mode.
#[cfg(test)]
fn scan_watch_chunk(
    selected_files: &[SessionScanTarget],
    timezone: Tz,
    now_utc: DateTime<Utc>,
) -> Result<WatchBuilder> {
    let mut builder = WatchBuilder::new(timezone, now_utc)?;
    for target in selected_files {
        scan_session_file_with(&target.path, &target.session_id, |event| {
            builder.observe(event);
        })?;
    }
    Ok(builder)
}

/// Resolve the effective worker count for the current workload.
fn resolve_scan_worker_count(parallelism: ScannerParallelism, selected_files: usize) -> usize {
    if selected_files <= 1 {
        return selected_files.max(1);
    }

    let configured = match parallelism {
        ScannerParallelism::Auto => thread::available_parallelism().map_or(1, NonZeroUsize::get),
        ScannerParallelism::Fixed(threads) => threads.get(),
    };

    configured.clamp(1, selected_files)
}

/// Discover the best session file for each session identifier.
fn scan_session_tree(
    root: &Path,
    directory: &Path,
    selected_files: &mut HashMap<String, SessionScanTarget>,
) -> Result<()> {
    let mut entries =
        fs::read_dir(directory)?.collect::<std::result::Result<Vec<_>, std::io::Error>>()?;
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            scan_session_tree(root, &path, selected_files)?;
        } else if file_type.is_file()
            && path
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            register_session_target(root, &entry, selected_files)?;
        }
    }

    Ok(())
}

/// Register one JSONL file as a session scan candidate.
fn register_session_target(
    root: &Path,
    entry: &std::fs::DirEntry,
    selected_files: &mut HashMap<String, SessionScanTarget>,
) -> Result<()> {
    let path = entry.path();
    let metadata = entry.metadata()?;
    let modified = metadata.modified().ok();
    let session_id = session_file_id(root, &path);
    let candidate = SessionScanTarget {
        session_id: session_id.clone(),
        path,
        bytes: metadata.len(),
        modified,
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
fn merge_group_summaries(
    target: &mut HashMap<String, GroupSummary>,
    source: HashMap<String, GroupSummary>,
) {
    for (key, source_summary) in source {
        let summary = target.entry(key).or_default();
        merge_group_summary(summary, source_summary);
    }
}

/// Merge one grouped summary into another.
fn merge_group_summary(target: &mut GroupSummary, source: GroupSummary) {
    target.totals.add(&source.totals);
    merge_model_breakdowns(&mut target.models, source.models);
}

/// Merge session summaries by stable session key.
fn merge_session_summaries(
    target: &mut HashMap<String, SessionSummary>,
    source: HashMap<String, SessionSummary>,
) {
    for (session_key, source_summary) in source {
        if let Some(existing) = target.get_mut(&session_key) {
            existing.totals.add(&source_summary.totals);
            existing.last_activity = existing.last_activity.max(source_summary.last_activity);
            merge_model_breakdowns(&mut existing.models, source_summary.models);
            continue;
        }

        target.insert(session_key, source_summary);
    }
}

/// Merge per-model usage without allocating intermediate report rows.
fn merge_model_breakdowns(
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

/// Scan one JSONL session file.
fn scan_session_file(file: &Path, session_id: &str, builder: &mut ReportBuilder) -> Result<()> {
    let _ = scan_session_file_from_checkpoint(
        file,
        session_id,
        &SessionParseCheckpoint::default(),
        |event| builder.observe(event),
    )?;
    Ok(())
}

/// Scan one JSONL session file and feed every parsed event into the provided callback.
#[cfg(test)]
fn scan_session_file_with(
    file: &Path,
    session_id: &str,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<()> {
    let _ = scan_session_file_from_checkpoint(
        file,
        session_id,
        &SessionParseCheckpoint::default(),
        |event| {
            on_event(event);
        },
    )?;
    Ok(())
}

/// Scan one JSONL session file into a report builder with an explicit observer.
fn scan_session_file_with_observer<O>(
    file: &Path,
    session_id: &str,
    builder: &mut ReportBuilder,
    observer: &O,
) -> Result<()>
where
    O: ScanObserver,
{
    scan_session_file_with_callback_and_observer(file, session_id, observer, |event| {
        builder.observe(event);
    })
}

/// Scan one JSONL session file and feed every parsed event into the provided callback.
fn scan_session_file_with_callback_and_observer<O>(
    file: &Path,
    session_id: &str,
    observer: &O,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<()>
where
    O: ScanObserver,
{
    let _ = scan_session_file_from_checkpoint_with_observer(
        file,
        session_id,
        &SessionParseCheckpoint::default(),
        observer,
        |event| on_event(event),
    )?;
    observer.on_file_complete();
    Ok(())
}

/// Scan one JSONL session file from a stored parser checkpoint.
fn scan_session_file_from_checkpoint(
    file: &Path,
    session_id: &str,
    checkpoint: &SessionParseCheckpoint,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint> {
    scan_session_file_from_checkpoint_inner(
        file,
        session_id,
        checkpoint,
        || {},
        |event| {
            on_event(event);
        },
    )
}

/// Scan one JSONL session file from a stored parser checkpoint.
fn scan_session_file_from_checkpoint_with_observer<O>(
    file: &Path,
    session_id: &str,
    checkpoint: &SessionParseCheckpoint,
    observer: &O,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint>
where
    O: ScanObserver,
{
    scan_session_file_from_checkpoint_inner(
        file,
        session_id,
        checkpoint,
        || observer.before_file_open(),
        |event| on_event(event),
    )
}

/// Scan one JSONL session file from a stored parser checkpoint using shared parse mechanics.
fn scan_session_file_from_checkpoint_inner(
    file: &Path,
    session_id: &str,
    checkpoint: &SessionParseCheckpoint,
    before_file_open: impl FnOnce(),
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint> {
    before_file_open();

    let mut file = File::open(file)?;
    file.seek(SeekFrom::Start(checkpoint.offset))?;
    let reader = BufReader::new(file);
    let mut line = String::new();
    let mut previous_totals = checkpoint.previous_totals;
    let mut current_model = checkpoint.current_model.clone();
    let mut current_model_is_fallback = checkpoint.current_model_is_fallback;
    let mut reader = reader;
    let mut offset = checkpoint.offset;
    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        let next_offset = offset.saturating_add(u64::try_from(bytes_read).unwrap_or(u64::MAX));
        let trimmed = line.trim();
        if trimmed.is_empty() {
            offset = next_offset;
            continue;
        }
        if !line.ends_with('\n') && serde_json::from_str::<SessionLogEntry<'_>>(trimmed).is_err() {
            offset = line_start_offset;
            break;
        }
        if !line_might_affect_usage(trimmed) {
            offset = next_offset;
            continue;
        }
        if let Some(event) = parse_token_usage_line(
            trimmed,
            session_id,
            session_id,
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )? {
            on_event(&event);
        }
        offset = next_offset;
    }
    Ok(SessionParseCheckpoint {
        offset,
        previous_totals,
        current_model,
        current_model_is_fallback,
    })
}

/// Return whether one JSONL line might affect usage aggregation.
fn line_might_affect_usage(line: &str) -> bool {
    // Fail open for escaped JSON strings because relevant event types may be unicode-escaped.
    line.contains("\\u") || line.contains("turn_context") || line.contains("token_count")
}

/// Derive the stable session identifier from a JSONL path.
fn session_file_id(root: &Path, file: &Path) -> String {
    let mut relative = file.strip_prefix(root).unwrap_or(file).to_path_buf();
    if relative.extension() == Some(std::ffi::OsStr::new("jsonl")) {
        relative.set_extension("");
    }
    relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Parse one JSONL line into a token-usage event when applicable.
fn parse_token_usage_line<'session, 'model>(
    line: &str,
    session_key: &'session str,
    session_id: &'session str,
    previous_totals: &mut Option<RawUsage>,
    current_model: &'model mut Option<String>,
    current_model_is_fallback: &mut bool,
) -> Result<Option<TokenUsageEvent<'session, 'model>>> {
    let Ok(entry) = serde_json::from_str::<SessionLogEntry<'_>>(line) else {
        return Ok(None);
    };
    let Some(entry_type) = entry.entry_type.as_deref() else {
        return Ok(None);
    };
    if entry_type == "turn_context" {
        if let Some(model) = entry.payload.as_ref().and_then(extract_payload_model) {
            remember_model(current_model, model);
            *current_model_is_fallback = false;
        }
        return Ok(None);
    }
    if entry_type != "event_msg" {
        return Ok(None);
    }
    let Some(payload) = entry.payload.as_ref() else {
        return Ok(None);
    };
    if payload.payload_type.as_deref() != Some("token_count") {
        return Ok(None);
    }
    let Some(timestamp) = entry.timestamp.as_deref() else {
        return Ok(None);
    };
    let Some(usage) = extract_event_usage(payload, previous_totals) else {
        return Ok(None);
    };
    let (model, is_fallback_model) =
        resolve_event_model(payload, current_model, current_model_is_fallback);
    let timestamp_utc = DateTime::parse_from_rfc3339(timestamp)
        .wrap_err_with(|| format!("invalid timestamp {timestamp}"))?
        .with_timezone(&Utc);
    Ok(Some(TokenUsageEvent {
        session_key,
        session_id,
        timestamp_utc,
        model,
        is_fallback_model,
        usage,
    }))
}

/// Extract normalized usage from one token-count payload.
fn extract_event_usage(
    payload: &EntryPayload<'_>,
    previous_totals: &mut Option<RawUsage>,
) -> Option<UsageTotals> {
    let (last_usage, total_usage) = if payload.info.is_some() {
        (
            info_usage(payload, UsageKind::Last),
            info_usage(payload, UsageKind::Total),
        )
    } else {
        (
            payload
                .last_token_usage
                .as_ref()
                .copied()
                .map(UsagePayload::into_raw_usage),
            payload
                .total_token_usage
                .as_ref()
                .copied()
                .map(UsagePayload::into_raw_usage),
        )
    };
    let mut raw_usage = last_usage;
    if raw_usage.is_none()
        && let Some(total_usage) = total_usage
    {
        raw_usage = Some(subtract_usage(total_usage, *previous_totals));
    }
    if let Some(total_usage) = total_usage {
        *previous_totals = Some(total_usage);
    } else if let Some(last_usage) = last_usage {
        *previous_totals = Some(previous_totals.unwrap_or_default().advance(last_usage));
    }
    let usage = raw_usage?.into_usage_totals();
    if usage.input == 0
        && usage.cached_input == 0
        && usage.output == 0
        && usage.reasoning_output == 0
    {
        return None;
    }
    Some(usage)
}

/// Resolve the model for one token-count payload and keep parser state in sync.
fn resolve_event_model<'a>(
    payload: &EntryPayload<'_>,
    current_model: &'a mut Option<String>,
    current_model_is_fallback: &mut bool,
) -> (&'a str, bool) {
    if let Some(model) = extract_payload_model(payload) {
        remember_model(current_model, model);
        *current_model_is_fallback = false;
    }
    if current_model.is_none() {
        remember_model(current_model, DEFAULT_FALLBACK_MODEL);
        *current_model_is_fallback = true;
    }

    (
        current_model
            .as_deref()
            .expect("resolved event model should always be present"),
        *current_model_is_fallback,
    )
}

/// Extract a model name from a payload shape.
fn extract_payload_model<'a>(payload: &'a EntryPayload<'a>) -> Option<&'a str> {
    let info_fields = payload.info.as_ref().map(|info| &info.model_fields);
    [
        info_fields.and_then(|fields| fields.model.as_deref()),
        info_fields.and_then(|fields| fields.model_name.as_deref()),
        payload.model_fields.model.as_deref(),
        payload.model_fields.model_name.as_deref(),
        info_fields.and_then(metadata_model),
        metadata_model(&payload.model_fields),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|model| !model.is_empty())
}

/// Extract the metadata model from one model lookup container.
fn metadata_model<'a>(fields: &'a ModelFields<'a>) -> Option<&'a str> {
    fields
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.model.as_deref())
}

/// Remember the last resolved model while reusing existing allocation capacity.
fn remember_model(current_model: &mut Option<String>, model: &str) {
    match current_model {
        Some(current) => {
            current.clear();
            current.push_str(model);
        }
        None => *current_model = Some(model.to_string()),
    }
}

/// Usage field selectors reused across payload levels.
#[derive(Clone, Copy)]
enum UsageKind {
    /// Event delta usage.
    Last,
    /// Cumulative usage.
    Total,
}

/// Extract one usage payload from the nested `info` object when present.
fn info_usage(payload: &EntryPayload<'_>, usage_kind: UsageKind) -> Option<RawUsage> {
    let info = payload.info.as_ref()?;
    match usage_kind {
        UsageKind::Last => info
            .last_token_usage
            .as_ref()
            .copied()
            .map(UsagePayload::into_raw_usage),
        UsageKind::Total => info
            .total_token_usage
            .as_ref()
            .copied()
            .map(UsagePayload::into_raw_usage),
    }
}

/// Convert cumulative totals into a delta.
fn subtract_usage(current: RawUsage, previous: Option<RawUsage>) -> RawUsage {
    let previous = previous.unwrap_or_default();
    RawUsage {
        input: current.input.saturating_sub(previous.input),
        cached_input: current.cached_input.saturating_sub(previous.cached_input),
        output: current.output.saturating_sub(previous.output),
        reasoning_output: current
            .reasoning_output
            .saturating_sub(previous.reasoning_output),
        total: current.total.saturating_sub(previous.total),
    }
}

/// Price one usage entry.
#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    reason = "Codex token counters are orders of magnitude below f64 precision limits"
)]
fn calculate_cost(
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
fn calculate_cost_from_usage(
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

#[cfg(test)]
#[allow(clippy::missing_docs_in_private_items)]
mod tests {
    use super::*;
    use filetime::FileTime;
    use serde_json::json;
    use std::time::Duration;
    use tempfile::TempDir;

    #[derive(Debug, Deserialize, PartialEq)]
    struct LossyStringField<'a> {
        #[serde(borrow, deserialize_with = "deserialize_optional_cow_lossy")]
        value: Option<Cow<'a, str>>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct LossyU64Field {
        #[serde(deserialize_with = "deserialize_u64_lossy")]
        value: u64,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct LossyOptionalU64Field {
        #[serde(deserialize_with = "deserialize_optional_u64_lossy")]
        value: Option<u64>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct ObjectPayload {
        name: String,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct LossyObjectField {
        #[serde(deserialize_with = "deserialize_optional_object_lossy")]
        value: Option<ObjectPayload>,
    }

    fn strip_ansi_sequences(value: &str) -> String {
        let mut output = String::with_capacity(value.len());
        let mut characters = value.chars();
        while let Some(character) = characters.next() {
            if character == '\u{1b}' && matches!(characters.next(), Some('[')) {
                for next in characters.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            output.push(character);
        }
        output
    }

    fn watch_snapshot_with_models(models: BTreeMap<String, ModelBreakdown>) -> WatchSnapshot {
        WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 60,
                cached_input_tokens: 15,
                output_tokens: 12,
                reasoning_output_tokens: 3,
                total_tokens: 72,
                cost_usd: 1.80,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(30 * 60),
                window_minutes: 30,
                input_tokens_per_hour: 120,
                cached_input_tokens_per_hour: 30,
                output_tokens_per_hour: 24,
                reasoning_output_tokens_per_hour: 6,
                total_tokens_per_hour: 144,
                cost_usd_per_hour: 3.60,
            },
            per_model: models,
            missing_directories: Vec::new(),
            updated_time: "00:30:00".to_string(),
        }
    }

    #[test]
    fn normalize_filter_date_accepts_supported_formats() {
        assert_eq!(
            normalize_filter_date("2025-09-11").expect("date"),
            NaiveDate::from_ymd_opt(2025, 9, 11).expect("naive date")
        );
        assert_eq!(
            normalize_filter_date("20250912").expect("date"),
            NaiveDate::from_ymd_opt(2025, 9, 12).expect("naive date")
        );
        assert!(normalize_filter_date("2025/09/12").is_err());
    }

    #[test]
    fn parse_timezone_rejects_invalid_names() {
        assert_eq!(
            parse_timezone("Europe/Warsaw").expect("timezone"),
            chrono_tz::Europe::Warsaw
        );
        assert!(parse_timezone("Not/A_Timezone").is_err());
    }

    #[test]
    fn normalize_timezone_name_accepts_valid_sources() {
        assert_eq!(
            normalize_timezone_name(":Europe/Warsaw").as_deref(),
            Some("Europe/Warsaw")
        );
        assert!(normalize_timezone_name("Not/A_Timezone").is_none());
    }

    #[test]
    fn timezone_source_helpers_extract_valid_names() {
        assert_eq!(
            timezone_from_etc_timezone_contents("Europe/Warsaw\n").as_deref(),
            Some("Europe/Warsaw")
        );
        assert_eq!(
            timezone_from_localtime_target(Path::new("/usr/share/zoneinfo/Europe/Warsaw"))
                .as_deref(),
            Some("Europe/Warsaw")
        );
        assert_eq!(
            timezone_from_localtime_target(Path::new("../usr/share/zoneinfo/America/New_York"))
                .as_deref(),
            Some("America/New_York")
        );
    }

    #[test]
    fn resolve_session_dirs_prefers_explicit_paths() {
        let explicit = vec![PathBuf::from("/tmp/custom-sessions")];
        assert_eq!(resolve_session_dirs(&explicit), explicit);
    }

    #[test]
    fn extract_model_checks_nested_metadata() {
        let payload =
            serde_json::from_str::<EntryPayload<'_>>(r#"{"info":{"metadata":{"model":"gpt-5"}}}"#)
                .expect("payload");
        assert_eq!(extract_payload_model(&payload), Some("gpt-5"));
    }

    #[test]
    fn normalize_usage_reads_cache_alias() {
        let usage = serde_json::from_str::<UsagePayload>(
            r#"{"input_tokens":100,"cache_read_input_tokens":25,"output_tokens":20,"reasoning_output_tokens":5}"#,
        )
        .expect("usage")
        .into_raw_usage();
        assert_eq!(usage.input, 100);
        assert_eq!(usage.cached_input, 25);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.reasoning_output, 5);
        assert_eq!(usage.total, 0);
    }

    #[test]
    fn normalize_usage_prefers_primary_cached_input_field_when_both_are_present() {
        let usage = serde_json::from_str::<UsagePayload>(
            r#"{"input_tokens":100,"cached_input_tokens":10,"cache_read_input_tokens":25,"output_tokens":20,"total_tokens":120}"#,
        )
        .expect("usage")
        .into_raw_usage();

        assert_eq!(usage.cached_input, 10);
        assert_eq!(usage.total, 120);
    }

    #[test]
    fn lossy_string_deserializer_ignores_non_string_shapes() {
        assert_eq!(
            serde_json::from_str::<LossyStringField<'_>>(r#"{"value":"gpt-5"}"#)
                .expect("string")
                .value
                .as_deref(),
            Some("gpt-5")
        );

        for input in [
            r#"{"value":null}"#,
            r#"{"value":true}"#,
            r#"{"value":-1}"#,
            r#"{"value":7}"#,
            r#"{"value":1.5}"#,
            r#"{"value":["gpt-5"]}"#,
            r#"{"value":{"model":"gpt-5"}}"#,
        ] {
            let field = serde_json::from_str::<LossyStringField<'_>>(input).expect("field");
            assert_eq!(field.value, None);
        }
    }

    #[test]
    fn lossy_u64_deserializers_ignore_non_integer_shapes() {
        assert_eq!(
            serde_json::from_value::<LossyU64Field>(json!({"value": 7}))
                .expect("integer")
                .value,
            7
        );
        assert_eq!(
            serde_json::from_value::<LossyOptionalU64Field>(json!({"value": 7}))
                .expect("optional integer")
                .value,
            Some(7)
        );

        for value in [
            json!(null),
            json!(true),
            json!(-1),
            json!(1.5),
            json!("7"),
            json!(["7"]),
            json!({"value": 7}),
        ] {
            let field = serde_json::from_value::<LossyU64Field>(json!({"value": value}))
                .expect("lossy u64 field");
            assert_eq!(field.value, 0);

            let optional = serde_json::from_value::<LossyOptionalU64Field>(json!({"value": value}))
                .expect("lossy optional u64 field");
            assert_eq!(optional.value, None);
        }
    }

    #[test]
    fn lossy_object_deserializer_ignores_non_object_shapes() {
        assert_eq!(
            serde_json::from_value::<LossyObjectField>(json!({"value": {"name": "codex"}}))
                .expect("object")
                .value,
            Some(ObjectPayload {
                name: "codex".to_string(),
            })
        );

        for value in [
            json!(null),
            json!(true),
            json!(-1),
            json!(7),
            json!(1.5),
            json!("codex"),
            json!(["codex"]),
        ] {
            let field =
                serde_json::from_value::<LossyObjectField>(json!({"value": value})).expect("field");
            assert_eq!(field.value, None);
        }
    }

    #[test]
    fn subtract_usage_saturates_negative_deltas() {
        let delta = subtract_usage(
            RawUsage {
                input: 5,
                cached_input: 2,
                output: 3,
                reasoning_output: 1,
                total: 8,
            },
            Some(RawUsage {
                input: 10,
                cached_input: 4,
                output: 4,
                reasoning_output: 2,
                total: 14,
            }),
        );
        assert_eq!(delta.input, 0);
        assert_eq!(delta.cached_input, 0);
        assert_eq!(delta.output, 0);
        assert_eq!(delta.reasoning_output, 0);
        assert_eq!(delta.total, 0);
    }

    #[test]
    fn split_session_id_handles_nested_paths() {
        let (directory, session) = split_session_id("team/project/session-1");
        assert_eq!(directory, "team/project");
        assert_eq!(session, "session-1");
    }

    #[test]
    fn session_file_round_trip_preserves_dotted_leaf_names() {
        let root = Path::new("/logs");
        let session_id = "team/session.v2";

        let rebuilt = session_file_path(root, session_id);

        assert_eq!(rebuilt, PathBuf::from("/logs/team/session.v2.jsonl"));
        assert_eq!(session_file_id(root, &rebuilt), session_id);
    }

    #[test]
    fn sort_session_entries_breaks_timestamp_ties_by_session_id() {
        let later = DateTime::parse_from_rfc3339("2025-09-11T18:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let mut entries = vec![
            (
                "zeta/session".to_string(),
                SessionSummary::new(later, "zeta/session".to_string()),
            ),
            (
                "alpha/session".to_string(),
                SessionSummary::new(later, "alpha/session".to_string()),
            ),
        ];

        sort_session_entries(&mut entries);

        assert_eq!(entries[0].0, "alpha/session");
        assert_eq!(entries[1].0, "zeta/session");
    }

    #[test]
    fn mark_dirty_sessions_keeps_the_most_conservative_refresh_kind() {
        let mut changes = WatchChangeSet::default();
        changes.mark_dirty_sessions(vec!["session-a".to_string()], WatchDirtyKind::AppendOnly);
        changes.mark_dirty_sessions(
            vec!["session-a".to_string(), "session-b".to_string()],
            WatchDirtyKind::FullRebuild,
        );
        changes.mark_dirty_sessions(vec!["session-b".to_string()], WatchDirtyKind::AppendOnly);

        assert_eq!(
            changes.dirty_sessions.get("session-a"),
            Some(&WatchDirtyKind::FullRebuild)
        );
        assert_eq!(
            changes.dirty_sessions.get("session-b"),
            Some(&WatchDirtyKind::FullRebuild)
        );
    }

    #[test]
    fn validate_watch_flags_rejects_json_and_date_filters() {
        assert!(validate_watch_flags(false, None, None, None).is_ok());
        assert!(
            validate_watch_flags(true, None, None, None)
                .expect_err("json should fail")
                .to_string()
                .contains("--json")
        );
        assert!(
            validate_watch_flags(false, Some("2026-01-01"), None, NonZeroUsize::new(1),)
                .expect_err("date filters should fail")
                .to_string()
                .contains("--since")
        );
    }

    #[test]
    fn watch_helpers_classify_dirty_events_and_paths() {
        let session_root = PathBuf::from("/tmp/codexusage-tests/sessions");
        let session_path = session_root.join("team").join("session.jsonl");
        let txt_path = session_root.join("team").join("session.txt");

        assert_eq!(
            watch_dirty_kind(NotifyEventKind::Modify(ModifyKind::Data(DataChange::Size))),
            WatchDirtyKind::AppendOnly
        );
        assert_eq!(
            watch_dirty_kind(NotifyEventKind::Create(notify::event::CreateKind::Any)),
            WatchDirtyKind::FullRebuild
        );
        assert!(path_is_under_roots(
            std::slice::from_ref(&session_root),
            &session_path
        ));
        assert!(!path_is_under_roots(
            std::slice::from_ref(&session_root),
            Path::new("/tmp/elsewhere/session.jsonl")
        ));
        assert_eq!(
            watch_event_session_ids(std::slice::from_ref(&session_root), &session_path),
            vec!["team/session".to_string()]
        );
        assert!(watch_event_session_ids(&[session_root], &txt_path).is_empty());
    }

    #[test]
    fn parse_interval_and_report_kind_helpers_cover_cli_routing_paths() {
        assert_eq!(
            parse_interval_seconds("5").expect("interval"),
            Duration::from_secs(5)
        );
        assert!(
            parse_interval_seconds("0")
                .expect_err("zero should fail")
                .contains("greater than 0")
        );
        assert!(
            parse_interval_seconds("abc")
                .expect_err("non-integer should fail")
                .contains("positive integer")
        );
        assert_eq!(ReportKind::from_command(&Command::Daily), ReportKind::Daily);
        assert_eq!(
            ReportKind::from_command(&Command::Monthly),
            ReportKind::Monthly
        );
        assert_eq!(
            ReportKind::from_command(&Command::Session),
            ReportKind::Session
        );
        assert!(is_false(&false));
        assert!(!is_false(&true));
    }

    #[test]
    fn supports_watch_screen_clear_wrapper_matches_term_contract() {
        assert!(!supports_watch_screen_clear(Some("dumb")));
        assert!(supports_watch_screen_clear(Some("xterm-256color")));
    }

    #[test]
    fn run_succeeds_for_json_and_table_reports() {
        let temp = TempDir::new().expect("tempdir");
        let missing = temp.path().join("missing-sessions");

        run(vec![
            OsString::from("codexusage"),
            OsString::from("--json"),
            OsString::from("--offline"),
            OsString::from("--session-dir"),
            missing.clone().into_os_string(),
        ])
        .expect("json run");
        run(vec![
            OsString::from("codexusage"),
            OsString::from("--offline"),
            OsString::from("--session-dir"),
            missing.into_os_string(),
            OsString::from("session"),
        ])
        .expect("table run");
    }

    #[test]
    fn format_helpers_produce_human_readable_output() {
        assert_eq!(format_u64(1_234_567), "1,234,567");
        assert_eq!(format_currency(12.5), "$12.50");
    }

    #[test]
    fn short_number_format_uses_three_significant_digits() {
        assert_eq!(format_u64_with(999, NumberFormat::Short), "999");
        assert_eq!(format_u64_with(1_000, NumberFormat::Short), "1K");
        assert_eq!(format_u64_with(1_234, NumberFormat::Short), "1.23K");
        assert_eq!(format_u64_with(12_345, NumberFormat::Short), "12.3K");
        assert_eq!(format_u64_with(123_456, NumberFormat::Short), "123K");
        assert_eq!(format_u64_with(1_234_567, NumberFormat::Short), "1.23M");
        assert_eq!(format_u64_with(12_345_678, NumberFormat::Short), "12.3M");
        assert_eq!(format_u64_with(123_456_789, NumberFormat::Short), "123M");
        assert_eq!(format_u64_with(1_000_000_000, NumberFormat::Short), "1B");
        assert_eq!(format_u64_with(1_200_000_000, NumberFormat::Short), "1.2B");
        assert_eq!(
            format_u64_with(1_000_000_000_000, NumberFormat::Short),
            "1T"
        );
    }

    #[test]
    fn full_number_format_preserves_grouped_digits() {
        assert_eq!(format_u64_with(1_234_567, NumberFormat::Full), "1,234,567");
    }

    #[test]
    fn calculate_cost_applies_cached_input_pricing() {
        let usage = ModelBreakdown {
            input_tokens: 1_000,
            cached_input_tokens: 200,
            output_tokens: 500,
            reasoning_output_tokens: 0,
            total_tokens: 1_500,
            cost_usd: 0.0,
            is_fallback: false,
            ..ModelBreakdown::default()
        };
        let pricing = Pricing {
            input_cost_per_mtoken: 1.25,
            cached_input_cost_per_mtoken: 0.125,
            output_cost_per_mtoken: 10.0,
        };
        let cost = calculate_cost(&usage, &pricing, CachedInputCostMode::Priced);
        let expected = (800.0 / 1_000_000.0) * 1.25
            + (200.0 / 1_000_000.0) * 0.125
            + (500.0 / 1_000_000.0) * 10.0;
        assert!((cost - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn calculate_cost_can_treat_cached_input_as_free() {
        let usage = UsageTotals {
            input: 1_000,
            cached_input: 200,
            output: 500,
            reasoning_output: 0,
            total: 1_500,
        };
        let pricing = Pricing {
            input_cost_per_mtoken: 1.25,
            cached_input_cost_per_mtoken: 0.125,
            output_cost_per_mtoken: 10.0,
        };

        let cost = calculate_cost_from_usage(&usage, &pricing, CachedInputCostMode::Free);
        let expected = (800.0 / 1_000_000.0) * 1.25 + (500.0 / 1_000_000.0) * 10.0;
        assert!((cost - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn calculate_cost_clamps_cached_input_to_avoid_double_accounting() {
        let usage = UsageTotals {
            input: 100,
            cached_input: 150,
            output: 0,
            reasoning_output: 0,
            total: 100,
        };
        let pricing = Pricing {
            input_cost_per_mtoken: 10.0,
            cached_input_cost_per_mtoken: 1.0,
            output_cost_per_mtoken: 0.0,
        };

        let cost = calculate_cost_from_usage(&usage, &pricing, CachedInputCostMode::Priced);
        let expected = (100.0 / 1_000_000.0) * 1.0;
        assert!((cost - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn render_report_includes_totals_for_daily_and_session_views() {
        let daily = ReportOutput::Daily {
            rows: vec![DailyRow {
                date: "2025-09-11".to_string(),
                input_tokens: 100,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 150,
                cost_usd: 0.25,
                models: BTreeMap::from([
                    (
                        "gpt-5".to_string(),
                        ModelBreakdown {
                            input_tokens: 60,
                            cached_input_tokens: 5,
                            output_tokens: 25,
                            reasoning_output_tokens: 0,
                            total_tokens: 85,
                            cost_usd: 0.0,
                            is_fallback: false,
                            ..ModelBreakdown::default()
                        },
                    ),
                    (
                        "gpt-5-codex".to_string(),
                        ModelBreakdown {
                            input_tokens: 40,
                            cached_input_tokens: 5,
                            output_tokens: 25,
                            reasoning_output_tokens: 0,
                            total_tokens: 65,
                            cost_usd: 0.0,
                            is_fallback: false,
                            ..ModelBreakdown::default()
                        },
                    ),
                ]),
            }],
            totals: Totals {
                input_tokens: 100,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 150,
                cost_usd: 0.25,
            },
            missing_directories: Vec::new(),
        };
        let session = ReportOutput::Session {
            rows: vec![SessionRow {
                session_id: "team/session".to_string(),
                directory: "team".to_string(),
                session_file: "session".to_string(),
                last_activity: "2025-09-11T18:00:00.000Z".to_string(),
                input_tokens: 100,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 150,
                cost_usd: 0.25,
                models: BTreeMap::from([(
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 100,
                        cached_input_tokens: 10,
                        output_tokens: 50,
                        reasoning_output_tokens: 0,
                        total_tokens: 150,
                        cost_usd: 0.0,
                        is_fallback: false,
                        ..ModelBreakdown::default()
                    },
                )]),
            }],
            totals: Totals {
                input_tokens: 100,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 150,
                cost_usd: 0.25,
            },
            missing_directories: Vec::new(),
        };

        let daily_render =
            render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Include);
        let session_render = render_report(
            &session,
            "en-US",
            NumberFormat::Short,
            CacheReadMode::Include,
        );
        assert!(daily_render.contains("TOTAL"));
        assert!(daily_render.contains("2025-09-11"));
        assert!(daily_render.contains("Model"));
        assert!(daily_render.contains("gpt-5-codex"));
        assert!(session_render.contains("session"));
        assert!(session_render.contains("gpt-5"));
        assert!(session_render.contains("Last Activity"));
    }

    #[test]
    fn render_report_handles_monthly_rows() {
        let monthly = ReportOutput::Monthly {
            rows: vec![MonthlyRow {
                month: "2025-09".to_string(),
                input_tokens: 120,
                cached_input_tokens: 20,
                output_tokens: 80,
                reasoning_output_tokens: 10,
                total_tokens: 200,
                cost_usd: 0.75,
                models: BTreeMap::new(),
            }],
            totals: Totals {
                input_tokens: 120,
                cached_input_tokens: 20,
                output_tokens: 80,
                reasoning_output_tokens: 10,
                total_tokens: 200,
                cost_usd: 0.75,
            },
            missing_directories: Vec::new(),
        };

        let rendered = render_report(
            &monthly,
            "en-US",
            NumberFormat::Short,
            CacheReadMode::Include,
        );
        assert!(rendered.contains("Monthly Codex Usage Report"));
        assert!(rendered.contains("2025-09"));
    }

    #[test]
    fn render_report_omits_cache_column_when_cache_read_is_excluded() {
        let daily = ReportOutput::Daily {
            rows: vec![DailyRow {
                date: "2025-09-11".to_string(),
                input_tokens: 80,
                cached_input_tokens: 0,
                output_tokens: 20,
                reasoning_output_tokens: 0,
                total_tokens: 100,
                cost_usd: 0.25,
                models: BTreeMap::new(),
            }],
            totals: Totals {
                input_tokens: 80,
                cached_input_tokens: 0,
                output_tokens: 20,
                reasoning_output_tokens: 0,
                total_tokens: 100,
                cost_usd: 0.25,
            },
            missing_directories: Vec::new(),
        };

        let rendered = render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Exclude);

        assert!(!rendered.contains("Cache"));
        assert!(rendered.contains("Input"));
        assert!(rendered.contains("Output"));
    }

    #[test]
    fn render_report_groups_model_rows_under_daily_subtotal() {
        let daily = ReportOutput::Daily {
            rows: vec![DailyRow {
                date: "2025-09-11".to_string(),
                input_tokens: 120,
                cached_input_tokens: 20,
                output_tokens: 80,
                reasoning_output_tokens: 10,
                total_tokens: 200,
                cost_usd: 0.75,
                models: BTreeMap::from([
                    (
                        "gpt-5".to_string(),
                        ModelBreakdown {
                            input_tokens: 70,
                            cached_input_tokens: 10,
                            output_tokens: 40,
                            reasoning_output_tokens: 5,
                            total_tokens: 110,
                            cost_usd: 0.0,
                            is_fallback: false,
                            ..ModelBreakdown::default()
                        },
                    ),
                    (
                        "gpt-5-codex".to_string(),
                        ModelBreakdown {
                            input_tokens: 50,
                            cached_input_tokens: 10,
                            output_tokens: 40,
                            reasoning_output_tokens: 5,
                            total_tokens: 90,
                            cost_usd: 0.0,
                            is_fallback: false,
                            ..ModelBreakdown::default()
                        },
                    ),
                ]),
            }],
            totals: Totals {
                input_tokens: 120,
                cached_input_tokens: 20,
                output_tokens: 80,
                reasoning_output_tokens: 10,
                total_tokens: 200,
                cost_usd: 0.75,
            },
            missing_directories: Vec::new(),
        };

        let rendered = render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Include);
        let subtotal = rendered
            .lines()
            .find(|line| line.contains("2025-09-11") && line.contains("TOTAL"))
            .expect("subtotal row");
        let gpt5 = rendered
            .lines()
            .find(|line| line.contains("gpt-5") && !line.contains("TOTAL"))
            .expect("gpt-5 row");
        let codex = rendered
            .lines()
            .find(|line| line.contains("gpt-5-codex"))
            .expect("gpt-5-codex row");

        assert!(subtotal.contains("120"));
        assert!(!gpt5.contains("2025-09-11"));
        assert!(gpt5.contains("70"));
        assert!(codex.contains("50"));
    }

    #[test]
    fn render_report_keeps_last_activity_on_session_subtotal_only() {
        let session = ReportOutput::Session {
            rows: vec![SessionRow {
                session_id: "team/session".to_string(),
                directory: "team".to_string(),
                session_file: "session".to_string(),
                last_activity: "2025-09-11T18:00:00.000Z".to_string(),
                input_tokens: 100,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 150,
                cost_usd: 0.25,
                models: BTreeMap::from([(
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 100,
                        cached_input_tokens: 10,
                        output_tokens: 50,
                        reasoning_output_tokens: 0,
                        total_tokens: 150,
                        cost_usd: 0.0,
                        is_fallback: false,
                        ..ModelBreakdown::default()
                    },
                )]),
            }],
            totals: Totals {
                input_tokens: 100,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 150,
                cost_usd: 0.25,
            },
            missing_directories: Vec::new(),
        };

        let rendered = render_report(
            &session,
            "en-US",
            NumberFormat::Short,
            CacheReadMode::Include,
        );
        let subtotal = rendered
            .lines()
            .find(|line| line.contains("session") && line.contains("TOTAL"))
            .expect("subtotal row");
        let model_row = rendered
            .lines()
            .find(|line| line.contains("gpt-5"))
            .expect("model row");

        assert!(subtotal.contains("2025-09-11T18:00:00.000Z"));
        assert!(!model_row.contains("2025-09-11T18:00:00.000Z"));
    }

    #[test]
    fn render_report_splits_mixed_fallback_and_explicit_usage_for_same_model() {
        let report = ReportOutput::Daily {
            rows: vec![DailyRow {
                date: "2025-09-11".to_string(),
                input_tokens: 100,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 150,
                cost_usd: 0.25,
                models: BTreeMap::from([(
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 100,
                        cached_input_tokens: 10,
                        output_tokens: 50,
                        reasoning_output_tokens: 0,
                        total_tokens: 150,
                        cost_usd: 0.20,
                        fallback_usage: UsageTotals {
                            input: 20,
                            cached_input: 0,
                            output: 10,
                            reasoning_output: 0,
                            total: 30,
                        },
                        fallback_cost_usd: 0.05,
                        is_fallback: true,
                    },
                )]),
            }],
            totals: Totals {
                input_tokens: 100,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 150,
                cost_usd: 0.25,
            },
            missing_directories: Vec::new(),
        };

        let rendered = render_report(
            &report,
            "en-US",
            NumberFormat::Short,
            CacheReadMode::Include,
        );
        let explicit_row = rendered
            .lines()
            .find(|line| line.contains("  gpt-5") && !line.contains("(fallback)"))
            .expect("explicit row");
        let fallback_row = rendered
            .lines()
            .find(|line| line.contains("  gpt-5 (fallback)"))
            .expect("fallback row");

        assert!(explicit_row.contains("80"));
        assert!(explicit_row.contains("$0.20"));
        assert!(fallback_row.contains("20"));
        assert!(fallback_row.contains("$0.05"));
    }

    #[test]
    fn detect_table_style_requires_tty_and_256_color_support() {
        assert_eq!(
            detect_table_style_for(true, Some("xterm-256color"), None, false),
            TableStyle::Ansi256
        );
        assert_eq!(
            detect_table_style_for(true, Some("xterm-256color"), None, true),
            TableStyle::Plain
        );
        assert_eq!(
            detect_table_style_for(false, Some("xterm-256color"), None, false),
            TableStyle::Plain
        );
        assert_eq!(
            detect_table_style_for(true, Some("xterm"), Some("truecolor"), false),
            TableStyle::Ansi256
        );
    }

    #[test]
    fn detect_border_style_requires_tty_and_utf8_locale() {
        assert_eq!(
            detect_border_style_for(true, Some("en_US.UTF-8"), None, None),
            BorderStyle::Unicode
        );
        assert_eq!(
            detect_border_style_for(true, None, Some("pl_PL.utf8"), None),
            BorderStyle::Unicode
        );
        assert_eq!(
            detect_border_style_for(true, Some("C"), None, None),
            BorderStyle::Ascii
        );
        assert_eq!(
            detect_border_style_for(false, Some("en_US.UTF-8"), None, None),
            BorderStyle::Ascii
        );
    }

    #[test]
    fn unicode_border_helpers_emit_box_drawing_characters() {
        let headers = ["Date", "Model"];
        let widths = vec![10, 8];
        let row = format_data_row(
            &headers,
            BorderStyle::Unicode,
            &widths,
            &["2025-09-11".to_string(), "TOTAL".to_string()],
        );

        assert_eq!(
            table_rule(TableRuleKind::Top, BorderStyle::Unicode, &widths),
            "┌────────────┬──────────┐"
        );
        assert!(row.starts_with('│'));
        assert!(row.contains(" │ "));
        assert!(row.ends_with('│'));
    }

    #[test]
    fn ascii_border_helpers_preserve_ascii_fallback() {
        let headers = ["Date", "Model"];
        let widths = vec![10, 8];
        let row = format_data_row(
            &headers,
            BorderStyle::Ascii,
            &widths,
            &["2025-09-11".to_string(), "TOTAL".to_string()],
        );

        assert_eq!(
            table_rule(TableRuleKind::Top, BorderStyle::Ascii, &widths),
            "+------------+----------+"
        );
        assert!(row.starts_with('|'));
        assert!(row.contains(" | "));
        assert!(row.ends_with('|'));
    }

    #[test]
    fn paint_only_emits_ansi_sequences_when_enabled() {
        let plain = paint(TableStyle::Plain, TableElement::Header, "Header");
        let styled = paint(TableStyle::Ansi256, TableElement::Header, "Header");

        assert_eq!(plain, "Header");
        assert!(styled.starts_with("\u{1b}["));
        assert!(styled.ends_with("\u{1b}[0m"));
    }

    #[test]
    fn styled_rows_keep_border_color_separate_from_row_color() {
        let mut output = String::new();
        let headers = ["Date", "Model"];
        let widths = vec![10, 8];
        let cells = vec!["2025-09-11".to_string(), "TOTAL".to_string()];

        write_table_row(
            &mut output,
            TableRenderConfig {
                style: TableStyle::Ansi256,
                borders: BorderStyle::Unicode,
                number_format: NumberFormat::Short,
            },
            &headers,
            &widths,
            &cells,
            TableElement::Subtotal,
        );

        assert!(output.contains("\u{1b}[38;5;24m│\u{1b}[0m"));
        assert!(output.contains("\u{1b}[1;38;5;117m 2025-09-11 \u{1b}[0m"));
        assert!(!output.contains("\u{1b}[1;38;5;117m│"));
    }

    #[test]
    fn render_report_surfaces_missing_directories() {
        let daily = ReportOutput::Daily {
            rows: Vec::new(),
            totals: Totals::default(),
            missing_directories: vec!["/tmp/missing-a".to_string(), "/tmp/missing-b".to_string()],
        };

        let rendered = render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Include);
        assert!(rendered.contains("Warning: missing session directories"));
        assert!(rendered.contains("/tmp/missing-a"));
        assert!(rendered.contains("/tmp/missing-b"));
    }

    #[test]
    fn render_report_shortens_token_columns_but_not_cost_by_default() {
        let daily = ReportOutput::Daily {
            rows: vec![DailyRow {
                date: "2025-09-11".to_string(),
                input_tokens: 100_000,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 100_050,
                cost_usd: 1234.5,
                models: BTreeMap::new(),
            }],
            totals: Totals {
                input_tokens: 100_000,
                cached_input_tokens: 10,
                output_tokens: 50,
                reasoning_output_tokens: 0,
                total_tokens: 100_050,
                cost_usd: 1234.5,
            },
            missing_directories: Vec::new(),
        };

        let short = render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Include);
        let full = render_report(&daily, "en-US", NumberFormat::Full, CacheReadMode::Include);

        assert!(short.contains("100K"));
        assert!(short.contains("$1234.50"));
        assert!(full.contains("100,000"));
        assert!(!full.contains("100K"));
    }

    #[test]
    fn full_number_format_keeps_table_frame_aligned_for_grouped_digits() {
        let daily = ReportOutput::Daily {
            rows: vec![DailyRow {
                date: "2025-09-11".to_string(),
                input_tokens: 1_000,
                cached_input_tokens: 2_000,
                output_tokens: 3_000,
                reasoning_output_tokens: 4_000,
                total_tokens: 5_000,
                cost_usd: 12.5,
                models: BTreeMap::new(),
            }],
            totals: Totals {
                input_tokens: 1_000,
                cached_input_tokens: 2_000,
                output_tokens: 3_000,
                reasoning_output_tokens: 4_000,
                total_tokens: 5_000,
                cost_usd: 12.5,
            },
            missing_directories: Vec::new(),
        };

        let rendered = render_report(&daily, "en-US", NumberFormat::Full, CacheReadMode::Include);
        let lines = rendered
            .lines()
            .map(strip_ansi_sequences)
            .collect::<Vec<_>>();
        let top = lines
            .iter()
            .find(|line| line.starts_with('+') || line.starts_with('┌'))
            .expect("top border");
        let subtotal = lines
            .iter()
            .find(|line| line.contains("2025-09-11") && line.contains("1,000"))
            .expect("subtotal row");

        assert_eq!(top.chars().count(), subtotal.chars().count());
    }

    #[test]
    fn collect_session_files_recurses_and_filters_extensions() {
        let temp = TempDir::new().expect("tempdir");
        let nested = temp.path().join("nested");
        fs::create_dir_all(&nested).expect("mkdir");
        fs::write(nested.join("a.jsonl"), "").expect("jsonl");
        fs::write(nested.join("b.txt"), "").expect("txt");

        let mut files = Vec::new();
        collect_session_files(temp.path(), &mut files).expect("collect");

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("a.jsonl"));
    }

    #[test]
    fn scan_session_file_skips_bad_json_and_errors_on_invalid_timestamp() {
        let temp = TempDir::new().expect("tempdir");
        let sessions = temp.path().join("sessions");
        let session_file = sessions.join("project").join("session.jsonl");
        fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
        fs::write(
            &session_file,
            concat!(
                "not-json\n",
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"bad-timestamp\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1,\"cached_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0,\"total_tokens\":2}}}}\n"
            ),
        )
        .expect("write session");

        let mut builder = ReportBuilder::new(ReportKind::Daily, chrono_tz::UTC, None, None);
        let error =
            scan_session_file(&session_file, "project/session", &mut builder).expect_err("error");
        assert!(error.to_string().contains("invalid timestamp"));
    }

    #[test]
    fn line_might_affect_usage_accepts_relevant_markers() {
        assert!(line_might_affect_usage(
            r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#
        ));
        assert!(line_might_affect_usage(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count"}}"#
        ));
        assert!(line_might_affect_usage(
            r#"{ "timestamp":"2026-01-01T00:00:00Z", "type":"event_msg", "payload":{"type":"token_count"} }"#
        ));
    }

    #[test]
    fn line_might_affect_usage_rejects_irrelevant_lines() {
        assert!(!line_might_affect_usage(r#"{"type":"response_item"}"#));
        assert!(!line_might_affect_usage(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"agent_reasoning"}}"#
        ));
        assert!(!line_might_affect_usage("not-json"));
    }

    #[test]
    fn line_might_affect_usage_fails_open_for_escaped_json_strings() {
        assert!(line_might_affect_usage(
            r#"{"type":"turn\u005fcontext","payload":{"model":"gpt-5"}}"#
        ));
        assert!(line_might_affect_usage(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token\u005fcount"}}"#
        ));
    }

    #[test]
    fn scan_session_file_advances_cumulative_state_after_last_usage() {
        let temp = TempDir::new().expect("tempdir");
        let sessions = temp.path().join("sessions");
        let session_file = sessions.join("session.jsonl");
        fs::create_dir_all(&sessions).expect("mkdir");
        fs::write(
            &session_file,
            [
                r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#,
                r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110}}}}"#,
                r#"{"timestamp":"2026-01-01T00:01:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":30,"output_tokens":5,"total_tokens":35}}}}"#,
                r#"{"timestamp":"2026-01-01T00:02:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":150,"output_tokens":20,"total_tokens":170}}}}"#,
            ]
            .join("\n"),
        )
        .expect("write session");

        let mut builder = ReportBuilder::new(ReportKind::Session, chrono_tz::UTC, None, None);
        scan_session_file(&session_file, "session", &mut builder).expect("scan");

        let report = builder
            .finish(
                &PricingCatalog::default(),
                UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
                Vec::new(),
            )
            .expect("report");
        let ReportOutput::Session { rows, .. } = report else {
            panic!("expected session report");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 150);
        assert_eq!(rows[0].output_tokens, 20);
        assert_eq!(rows[0].total_tokens, 170);
    }

    #[test]
    fn parse_token_usage_line_tracks_turn_context_and_metadata_model() {
        let mut previous_totals = None;
        let mut current_model = None;
        let mut current_model_is_fallback = false;

        let turn_context = parse_token_usage_line(
            r#"{"type":"turn_context","payload":{"metadata":{"model":"gpt-5-mini"}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("turn context parse");
        assert!(turn_context.is_none());
        assert_eq!(current_model.as_deref(), Some("gpt-5-mini"));
        assert!(!current_model_is_fallback);

        let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":12,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1,"total_tokens":15}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

        assert_eq!(event.model, "gpt-5-mini");
        assert!(!event.is_fallback_model);
        assert_eq!(
            event.usage,
            UsageTotals {
                input: 12,
                cached_input: 2,
                output: 3,
                reasoning_output: 1,
                total: 15,
            }
        );
    }

    #[test]
    fn parse_token_usage_line_uses_fallback_model_until_explicit_model_arrives() {
        let mut previous_totals = None;
        let mut current_model = None;
        let mut current_model_is_fallback = false;

        let first_event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":8,"output_tokens":2,"total_tokens":10}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("first event parse")
        .expect("first token usage event");
        assert_eq!(first_event.model, DEFAULT_FALLBACK_MODEL);
        assert!(first_event.is_fallback_model);

        let second_event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:01:00Z","type":"event_msg","payload":{"type":"token_count","model":"gpt-5","info":{"last_token_usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("second event parse")
        .expect("second token usage event");
        assert_eq!(second_event.model, "gpt-5");
        assert!(!second_event.is_fallback_model);
    }

    #[test]
    fn parse_token_usage_line_does_not_fall_back_to_payload_usage_when_info_exists() {
        let mut previous_totals = None;
        let mut current_model = None;
        let mut current_model_is_fallback = false;

        let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","last_token_usage":{"input_tokens":8,"output_tokens":2,"total_tokens":10},"info":{"model":"gpt-5"}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse");

        assert!(event.is_none());
    }

    #[test]
    fn parse_token_usage_line_accepts_escaped_model_strings() {
        let mut previous_totals = None;
        let mut current_model = None;
        let mut current_model_is_fallback = false;

        let turn_context = parse_token_usage_line(
            r#"{"type":"turn_context","payload":{"metadata":{"model":"gpt\u002d5"}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("turn context parse");
        assert!(turn_context.is_none());

        let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

        assert_eq!(event.model, "gpt-5");
        assert!(!event.is_fallback_model);
    }

    #[test]
    fn parse_token_usage_line_prefers_explicit_model_over_nested_metadata() {
        let mut previous_totals = None;
        let mut current_model = None;
        let mut current_model_is_fallback = false;

        let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","model":"gpt-5","info":{"metadata":{"model":"gpt-5-mini"},"last_token_usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

        assert_eq!(event.model, "gpt-5");
    }

    #[test]
    fn parse_token_usage_line_ignores_invalid_optional_subfields() {
        let mut previous_totals = None;
        let mut current_model = None;
        let mut current_model_is_fallback = false;

        let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"metadata":"invalid","last_token_usage":"invalid","total_token_usage":{"input_tokens":9,"output_tokens":1,"total_tokens":10}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

        assert_eq!(event.usage.total, 10);
        assert_eq!(event.model, DEFAULT_FALLBACK_MODEL);
        assert!(event.is_fallback_model);
    }

    #[test]
    fn parse_token_usage_line_keeps_usage_when_one_counter_has_wrong_type() {
        let mut previous_totals = None;
        let mut current_model = None;
        let mut current_model_is_fallback = false;

        let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":2,"reasoning_output_tokens":"invalid","total_tokens":9}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

        assert_eq!(event.usage.input, 7);
        assert_eq!(event.usage.output, 2);
        assert_eq!(event.usage.reasoning_output, 0);
        assert_eq!(event.usage.total, 9);
    }

    #[test]
    fn parse_token_usage_line_keeps_usage_when_model_field_has_wrong_type() {
        let mut previous_totals = None;
        let mut current_model = Some("gpt-5".to_string());
        let mut current_model_is_fallback = false;

        let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","model":["invalid"],"info":{"last_token_usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

        assert_eq!(event.model, "gpt-5");
        assert!(!event.is_fallback_model);
        assert_eq!(event.usage.total, 5);
    }

    #[test]
    fn session_reports_prefer_longer_duplicate_files() {
        let first = TempDir::new().expect("first");
        let second = TempDir::new().expect("second");
        let first_sessions = first.path().join("sessions");
        let second_sessions = second.path().join("sessions");
        let first_file = first_sessions.join("project").join("session.jsonl");
        let second_file = second_sessions.join("project").join("session.jsonl");
        fs::create_dir_all(first_file.parent().expect("first parent")).expect("mkdir first");
        fs::create_dir_all(second_file.parent().expect("second parent")).expect("mkdir second");
        let short_payload = concat!(
            "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n"
        );
        let long_payload = concat!(
            "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n",
            "{\"timestamp\":\"2025-09-11T18:02:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":50,\"cached_input_tokens\":0,\"output_tokens\":5,\"reasoning_output_tokens\":0,\"total_tokens\":55}}}}\n"
        );
        fs::write(&first_file, short_payload).expect("write first");
        fs::write(&second_file, long_payload).expect("write second");

        let report = build_report(
            ReportKind::Session,
            &ReportOptions {
                since: None,
                until: None,
                last_days: None,
                timezone: "UTC".to_string(),
                locale: "en-US".to_string(),
                number_format: NumberFormat::Short,
                json: true,
                offline: true,
                refresh_pricing: false,
                cached_input_cost_mode: CachedInputCostMode::Priced,
                cache_read_mode: CacheReadMode::Include,
                session_dirs: vec![first_sessions, second_sessions],
                parallelism: ScannerParallelism::Auto,
            },
        )
        .expect("report");

        match report {
            ReportOutput::Session { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].session_id, "project/session");
                assert_eq!(rows[0].input_tokens, 150);
                assert_eq!(rows[0].output_tokens, 15);
                assert_eq!(rows[0].total_tokens, 165);
            }
            other => panic!("unexpected report: {other:?}"),
        }
    }

    #[test]
    fn build_watch_snapshot_tracks_current_day_totals_and_bounded_hourly_burn() {
        let temp = TempDir::new().expect("tempdir");
        let sessions = temp.path().join("sessions");
        let session_file = sessions.join("project").join("session.jsonl");
        fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
        fs::write(
            &session_file,
            [
                r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#,
                r#"{"timestamp":"2026-01-01T23:40:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10,"reasoning_output_tokens":4,"total_tokens":110}}}}"#,
                r#"{"timestamp":"2026-01-02T00:05:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":20,"cached_input_tokens":5,"output_tokens":3,"reasoning_output_tokens":1,"total_tokens":23}}}}"#,
                r#"{"timestamp":"2026-01-02T00:20:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":30,"cached_input_tokens":10,"output_tokens":7,"reasoning_output_tokens":2,"total_tokens":37}}}}"#,
            ]
            .join("\n"),
        )
        .expect("write session");

        let now = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let options = WatchOptions {
            timezone: "UTC".to_string(),
            locale: "en-US".to_string(),
            number_format: NumberFormat::Short,
            offline: true,
            refresh_pricing: false,
            cached_input_cost_mode: CachedInputCostMode::Priced,
            cache_read_mode: CacheReadMode::Include,
            session_dirs: vec![sessions],
            parallelism: ScannerParallelism::Auto,
            interval: Duration::from_secs(5),
            show_model_burn_rate: true,
            #[cfg(debug_assertions)]
            debug: DebugRuntimeOptions::default(),
        };
        let snapshot = build_watch_snapshot_at(&options, now).expect("watch snapshot");

        assert_eq!(snapshot.date, "2026-01-02");
        assert_eq!(snapshot.totals.input_tokens, 50);
        assert_eq!(snapshot.totals.cached_input_tokens, 15);
        assert_eq!(snapshot.totals.output_tokens, 10);
        assert_eq!(snapshot.totals.reasoning_output_tokens, 3);
        assert_eq!(snapshot.totals.total_tokens, 60);
        assert_eq!(snapshot.burn_rate.window_minutes, 30);
        assert_eq!(snapshot.burn_rate.input_tokens_per_hour, 100);
        assert_eq!(snapshot.burn_rate.cached_input_tokens_per_hour, 30);
        assert_eq!(snapshot.burn_rate.output_tokens_per_hour, 20);
        assert_eq!(snapshot.burn_rate.reasoning_output_tokens_per_hour, 6);
        assert_eq!(snapshot.burn_rate.total_tokens_per_hour, 120);
        assert_eq!(snapshot.updated_time, "00:30:00");
        let gpt5 = snapshot.per_model.get("gpt-5").expect("gpt-5 model");
        assert_eq!(gpt5.input_tokens, 50);
        assert_eq!(gpt5.cached_input_tokens, 15);
        assert_eq!(gpt5.total_tokens, 60);

        let mut free_cache_options = options.clone();
        free_cache_options.cached_input_cost_mode = CachedInputCostMode::Free;
        let free_cache_snapshot =
            build_watch_snapshot_at(&free_cache_options, now).expect("watch snapshot");
        assert_eq!(free_cache_snapshot.totals.input_tokens, 50);
        assert_eq!(free_cache_snapshot.totals.cached_input_tokens, 15);
        assert!(
            (free_cache_snapshot.totals.cost_usd
                - ((35.0 / 1_000_000.0) * 1.25 + (10.0 / 1_000_000.0) * 10.0))
                .abs()
                < f64::EPSILON
        );
        assert!(
            (free_cache_snapshot.burn_rate.cost_usd_per_hour
                - ((70.0 / 1_000_000.0) * 1.25 + (20.0 / 1_000_000.0) * 10.0))
                .abs()
                < f64::EPSILON
        );

        let mut exclude_cache_options = options.clone();
        exclude_cache_options.cache_read_mode = CacheReadMode::Exclude;
        let exclude_cache_snapshot =
            build_watch_snapshot_at(&exclude_cache_options, now).expect("watch snapshot");
        assert_eq!(exclude_cache_snapshot.totals.input_tokens, 35);
        assert_eq!(exclude_cache_snapshot.totals.cached_input_tokens, 0);
        assert_eq!(exclude_cache_snapshot.totals.total_tokens, 45);
        assert_eq!(exclude_cache_snapshot.burn_rate.input_tokens_per_hour, 70);
        assert_eq!(
            exclude_cache_snapshot
                .burn_rate
                .cached_input_tokens_per_hour,
            0
        );
        assert_eq!(exclude_cache_snapshot.burn_rate.total_tokens_per_hour, 90);
    }

    #[test]
    fn watch_runtime_refresh_reuses_unchanged_file_cache() {
        let temp = TempDir::new().expect("tempdir");
        let sessions = temp.path().join("sessions");
        let session_file = sessions.join("today").join("session.jsonl");
        fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
        let valid_payload = concat!(
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
        );
        fs::write(&session_file, valid_payload).expect("write valid file");
        let original_metadata = fs::metadata(&session_file).expect("metadata");
        let original_mtime = original_metadata.modified().expect("modified time");

        let now = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let timezone = chrono_tz::UTC;
        let mut runtime = WatchRuntimeState::load(
            std::slice::from_ref(&sessions),
            ScannerParallelism::Auto,
            timezone,
            now,
        )
        .expect("runtime");
        let initial_snapshot = runtime
            .snapshot(
                &PricingCatalog::default(),
                UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
                now,
                false,
            )
            .expect("initial snapshot");
        assert_eq!(initial_snapshot.totals.total_tokens, 23);

        let mut invalid_payload = String::from(
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n\
             {\"timestamp\":\"bad-timestamp\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}\n",
        );
        invalid_payload.push_str(&" ".repeat(valid_payload.len() - invalid_payload.len()));
        fs::write(&session_file, invalid_payload).expect("write invalid file");
        filetime::set_file_mtime(&session_file, FileTime::from_system_time(original_mtime))
            .expect("restore mtime");

        runtime
            .refresh(
                &[sessions],
                ScannerParallelism::Auto,
                timezone,
                now,
                WatchChangeSet::default(),
            )
            .expect("refresh");
        let refreshed_snapshot = runtime
            .snapshot(
                &PricingCatalog::default(),
                UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
                now,
                false,
            )
            .expect("refreshed snapshot");
        assert_eq!(refreshed_snapshot.totals.input_tokens, 20);
        assert_eq!(refreshed_snapshot.totals.output_tokens, 3);
        assert_eq!(refreshed_snapshot.totals.total_tokens, 23);
    }

    #[test]
    fn watch_runtime_snapshot_ignores_future_dated_events_until_time_advances() {
        let temp = TempDir::new().expect("tempdir");
        let sessions = temp.path().join("sessions");
        let session_file = sessions.join("today").join("session.jsonl");
        fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
        let payload = concat!(
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
            "{\"timestamp\":\"2026-01-02T00:45:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":5,\"output_tokens\":1,\"total_tokens\":6}}}}\n"
        );
        fs::write(&session_file, payload).expect("write session");

        let timezone = chrono_tz::UTC;
        let loaded_at = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let runtime = WatchRuntimeState::load(
            std::slice::from_ref(&sessions),
            ScannerParallelism::Auto,
            timezone,
            loaded_at,
        )
        .expect("runtime");

        let early_snapshot = runtime
            .snapshot(
                &PricingCatalog::default(),
                UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
                loaded_at,
                false,
            )
            .expect("early snapshot");
        assert_eq!(early_snapshot.totals.total_tokens, 23);
        assert_eq!(early_snapshot.burn_rate.total_tokens_per_hour, 46);

        let later_now = DateTime::parse_from_rfc3339("2026-01-02T00:50:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let mut runtime = runtime;
        runtime
            .refresh(
                std::slice::from_ref(&sessions),
                ScannerParallelism::Auto,
                timezone,
                later_now,
                WatchChangeSet::default(),
            )
            .expect("advance runtime");
        let later_snapshot = runtime
            .snapshot(
                &PricingCatalog::default(),
                UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
                later_now,
                false,
            )
            .expect("later snapshot");
        assert_eq!(later_snapshot.totals.total_tokens, 29);
        assert_eq!(later_snapshot.burn_rate.total_tokens_per_hour, 35);
    }

    #[test]
    fn watch_runtime_refreshes_dirty_session_after_append() {
        let temp = TempDir::new().expect("tempdir");
        let sessions = temp.path().join("sessions");
        let session_file = sessions.join("today").join("session.jsonl");
        fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
            ),
        )
        .expect("write session");

        let timezone = chrono_tz::UTC;
        let now = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let mut runtime = WatchRuntimeState::load(
            std::slice::from_ref(&sessions),
            ScannerParallelism::Auto,
            timezone,
            now,
        )
        .expect("runtime");

        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:20:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":5,\"output_tokens\":1,\"total_tokens\":6}}}}\n"
            ),
        )
        .expect("append session");

        runtime
            .refresh(
                std::slice::from_ref(&sessions),
                ScannerParallelism::Auto,
                timezone,
                now,
                WatchChangeSet {
                    dirty_sessions: HashMap::from([(
                        "today/session".to_string(),
                        WatchDirtyKind::AppendOnly,
                    )]),
                    discovery_due: false,
                },
            )
            .expect("refresh dirty session");
        let snapshot = runtime
            .snapshot(
                &PricingCatalog::default(),
                UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
                now,
                false,
            )
            .expect("snapshot");

        assert_eq!(snapshot.totals.total_tokens, 29);
        assert_eq!(snapshot.totals.input_tokens, 25);
        assert_eq!(snapshot.totals.output_tokens, 4);
    }

    #[test]
    fn watch_runtime_rebuilds_longer_rewritten_file_instead_of_appending() {
        let temp = TempDir::new().expect("tempdir");
        let sessions = temp.path().join("sessions");
        let session_file = sessions.join("today").join("session.jsonl");
        fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
            ),
        )
        .expect("write session");

        let timezone = chrono_tz::UTC;
        let now = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let mut runtime = WatchRuntimeState::load(
            std::slice::from_ref(&sessions),
            ScannerParallelism::Auto,
            timezone,
            now,
        )
        .expect("runtime");

        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":50,\"output_tokens\":10,\"total_tokens\":60}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:20:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}\n"
            ),
        )
        .expect("rewrite longer session");

        runtime
            .refresh(
                std::slice::from_ref(&sessions),
                ScannerParallelism::Auto,
                timezone,
                now,
                WatchChangeSet {
                    dirty_sessions: HashMap::from([(
                        "today/session".to_string(),
                        WatchDirtyKind::FullRebuild,
                    )]),
                    discovery_due: false,
                },
            )
            .expect("refresh rewritten session");
        let snapshot = runtime
            .snapshot(
                &PricingCatalog::default(),
                UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
                now,
                false,
            )
            .expect("snapshot");

        assert_eq!(snapshot.totals.total_tokens, 62);
        assert_eq!(snapshot.totals.input_tokens, 51);
        assert_eq!(snapshot.totals.output_tokens, 11);
    }

    #[test]
    fn resolve_session_target_across_roots_re_resolves_duplicates() {
        let first = TempDir::new().expect("first");
        let second = TempDir::new().expect("second");
        let first_sessions = first.path().join("sessions");
        let second_sessions = second.path().join("sessions");
        let selected_file = first_sessions.join("project").join("session.jsonl");
        let duplicate_file = second_sessions.join("project").join("session.jsonl");
        let removed_file = first_sessions.join("gone").join("session.jsonl");
        fs::create_dir_all(selected_file.parent().expect("selected parent")).expect("mkdir first");
        fs::create_dir_all(duplicate_file.parent().expect("duplicate parent"))
            .expect("mkdir second");
        fs::create_dir_all(removed_file.parent().expect("removed parent")).expect("mkdir gone");
        fs::write(&selected_file, "first version in preferred root").expect("write selected");
        fs::write(&duplicate_file, "backup").expect("write duplicate");
        fs::write(&removed_file, "gone").expect("write removed");

        let session_dirs = vec![first_sessions.clone(), second_sessions.clone()];
        let original = resolve_session_target_across_roots(&session_dirs, "project/session")
            .expect("resolve selected")
            .expect("selected target");
        assert_eq!(original.path, selected_file);

        fs::remove_file(&selected_file).expect("remove selected");
        fs::remove_file(&removed_file).expect("remove file");

        let refreshed = resolve_session_target_across_roots(&session_dirs, "project/session")
            .expect("refresh")
            .expect("fallback target");
        assert_eq!(refreshed.session_id, "project/session");
        assert_eq!(refreshed.path, duplicate_file);
        assert_eq!(refreshed.bytes, 6);
        assert!(
            resolve_session_target_across_roots(&session_dirs, "gone/session")
                .expect("missing session")
                .is_none()
        );
    }

    #[test]
    fn watch_event_session_ids_cover_all_matching_roots() {
        let roots = vec![PathBuf::from("/logs"), PathBuf::from("/logs/project")];
        let path = PathBuf::from("/logs/project/a.jsonl");
        let mut session_ids = watch_event_session_ids(&roots, &path);
        session_ids.sort();

        assert_eq!(session_ids, vec!["a".to_string(), "project/a".to_string()]);
    }

    #[test]
    fn watch_event_source_marks_newly_available_root_for_rediscovery() {
        let temp = TempDir::new().expect("tempdir");
        let late_root = temp.path().join("late-root");
        let mut source =
            WatchEventSource::new_polling(std::slice::from_ref(&late_root)).expect("watch source");

        assert!(!source.watched_roots.contains(&late_root));

        fs::create_dir_all(&late_root).expect("create late root");

        assert!(
            source
                .sync_session_dirs(std::slice::from_ref(&late_root))
                .expect("sync session dirs"),
            "late root should trigger immediate rediscovery",
        );
        assert!(source.watched_roots.contains(&late_root));
    }

    #[test]
    fn scan_session_file_from_checkpoint_reads_only_appended_suffix() {
        let temp = TempDir::new().expect("tempdir");
        let session_file = temp.path().join("session.jsonl");
        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
            ),
        )
        .expect("write session");

        let mut initial_events = Vec::new();
        let checkpoint = scan_session_file_from_checkpoint(
            &session_file,
            "project/session",
            &SessionParseCheckpoint::default(),
            |event| initial_events.push(event.usage.total),
        )
        .expect("initial scan");
        assert_eq!(initial_events, vec![23]);

        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:10:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":5,\"output_tokens\":1,\"total_tokens\":6}}}}\n"
            ),
        )
        .expect("append session");

        let mut appended_events = Vec::new();
        let updated = scan_session_file_from_checkpoint(
            &session_file,
            "project/session",
            &checkpoint,
            |event| appended_events.push((event.model.to_string(), event.usage.total)),
        )
        .expect("incremental scan");
        assert_eq!(appended_events, vec![("gpt-5".to_string(), 6)]);
        assert!(updated.offset > checkpoint.offset);
    }

    #[test]
    fn scan_session_file_from_checkpoint_retries_unterminated_jsonl_record() {
        let temp = TempDir::new().expect("tempdir");
        let session_file = temp.path().join("session.jsonl");
        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:10:00Z\",\"type\":\"event_msg\""
            ),
        )
        .expect("write partial session");

        let checkpoint = scan_session_file_from_checkpoint(
            &session_file,
            "project/session",
            &SessionParseCheckpoint::default(),
            |_| {},
        )
        .expect("scan with partial line");
        assert_eq!(
            checkpoint.offset,
            u64::try_from(
                concat!(
                    "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                    "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
                )
                .len()
            )
            .expect("offset fits")
        );

        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:10:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":5,\"output_tokens\":1,\"total_tokens\":6}}}}\n"
            ),
        )
        .expect("finish session");

        let mut appended_totals = Vec::new();
        let updated = scan_session_file_from_checkpoint(
            &session_file,
            "project/session",
            &checkpoint,
            |event| appended_totals.push(event.usage.total),
        )
        .expect("rescan completed line");

        assert_eq!(appended_totals, vec![6]);
        assert!(updated.offset > checkpoint.offset);
    }

    #[test]
    fn cached_watch_file_burn_events_skip_older_history() {
        let event = |timestamp: &str, total: u64| OwnedWatchEvent {
            timestamp_utc: DateTime::parse_from_rfc3339(timestamp)
                .expect("timestamp")
                .with_timezone(&Utc),
            model: "gpt-5".to_string(),
            is_fallback_model: false,
            usage: UsageTotals {
                input: total,
                cached_input: 0,
                output: 0,
                reasoning_output: 0,
                total,
            },
        };
        let cached = CachedWatchFile {
            target: SessionScanTarget {
                path: PathBuf::from("/tmp/session.jsonl"),
                session_id: "project/session".to_string(),
                bytes: 0,
                modified: None,
            },
            parser_checkpoint: SessionParseCheckpoint::default(),
            current_day_events: vec![
                event("2026-01-02T00:05:00Z", 5),
                event("2026-01-02T00:30:00Z", 7),
                event("2026-01-02T00:45:00Z", 11),
            ],
            visible_end: 0,
            burn_start: 0,
        };

        let window_start = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let window_end = DateTime::parse_from_rfc3339("2026-01-02T00:59:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let mut cached = cached;
        cached.recompute_bounds(window_start, window_end);
        let suffix = cached.burn_events();

        assert_eq!(suffix.len(), 2);
        assert_eq!(suffix[0].usage.total, 7);
        assert_eq!(suffix[1].usage.total, 11);
    }

    #[test]
    fn render_watch_screen_includes_totals_and_burn_rate_columns() {
        let rendered = render_watch_screen(
            &WatchSnapshot {
                date: "2026-01-02".to_string(),
                totals: Totals {
                    input_tokens: 50,
                    cached_input_tokens: 15,
                    output_tokens: 10,
                    reasoning_output_tokens: 3,
                    total_tokens: 60,
                    cost_usd: 1.25,
                },
                burn_rate: BurnRateSnapshot {
                    window_duration: Duration::from_secs(30 * 60),
                    window_minutes: 30,
                    input_tokens_per_hour: 100,
                    cached_input_tokens_per_hour: 30,
                    output_tokens_per_hour: 20,
                    reasoning_output_tokens_per_hour: 6,
                    total_tokens_per_hour: 120,
                    cost_usd_per_hour: 2.5,
                },
                per_model: BTreeMap::new(),
                missing_directories: vec!["/tmp/missing".to_string()],
                updated_time: "00:30:00".to_string(),
            },
            "en-US",
            NumberFormat::Short,
            false,
            CacheReadMode::Include,
        );

        assert!(rendered.contains("Current Day Codex Usage Watch"));
        assert!(rendered.contains("Burn Rate (/h)"));
        assert!(rendered.contains("2026-01-02"));
        assert!(rendered.contains("Updated"));
        assert!(rendered.contains("00:30:00"));
        assert!(rendered.contains("Input"));
        assert!(rendered.contains("$2.50"));
        assert!(rendered.contains("/tmp/missing"));
    }

    #[test]
    fn render_watch_screen_omits_cache_metric_when_cache_read_is_excluded() {
        let rendered = render_watch_screen(
            &WatchSnapshot {
                date: "2026-01-02".to_string(),
                totals: Totals {
                    input_tokens: 35,
                    cached_input_tokens: 0,
                    output_tokens: 10,
                    reasoning_output_tokens: 3,
                    total_tokens: 45,
                    cost_usd: 1.25,
                },
                burn_rate: BurnRateSnapshot {
                    window_duration: Duration::from_secs(30 * 60),
                    window_minutes: 30,
                    input_tokens_per_hour: 70,
                    cached_input_tokens_per_hour: 0,
                    output_tokens_per_hour: 20,
                    reasoning_output_tokens_per_hour: 6,
                    total_tokens_per_hour: 90,
                    cost_usd_per_hour: 2.5,
                },
                per_model: BTreeMap::new(),
                missing_directories: Vec::new(),
                updated_time: "00:30:00".to_string(),
            },
            "en-US",
            NumberFormat::Short,
            false,
            CacheReadMode::Exclude,
        );

        assert!(!rendered.contains("Cache"));
        assert!(rendered.contains("Input"));
        assert!(rendered.contains("Output"));
    }

    #[test]
    fn render_watch_screen_honors_number_format() {
        let snapshot = WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 100_000,
                cached_input_tokens: 2_000,
                output_tokens: 3_000,
                reasoning_output_tokens: 4_000,
                total_tokens: 107_000,
                cost_usd: 12.5,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(60 * 60),
                window_minutes: 60,
                input_tokens_per_hour: 100_000,
                cached_input_tokens_per_hour: 2_000,
                output_tokens_per_hour: 3_000,
                reasoning_output_tokens_per_hour: 4_000,
                total_tokens_per_hour: 107_000,
                cost_usd_per_hour: 12.5,
            },
            per_model: BTreeMap::new(),
            missing_directories: Vec::new(),
            updated_time: "00:30:00".to_string(),
        };

        let short = render_watch_screen(
            &snapshot,
            "en-US",
            NumberFormat::Short,
            false,
            CacheReadMode::Include,
        );
        let full = render_watch_screen(
            &snapshot,
            "en-US",
            NumberFormat::Full,
            false,
            CacheReadMode::Include,
        );

        assert!(short.contains("100K"));
        assert!(!short.contains("100,000"));
        assert!(full.contains("100,000"));
    }

    #[test]
    fn render_watch_screen_omits_per_model_columns_when_flag_is_disabled() {
        let rendered = render_watch_screen(
            &WatchSnapshot {
                date: "2026-01-02".to_string(),
                totals: Totals {
                    input_tokens: 50,
                    cached_input_tokens: 15,
                    output_tokens: 10,
                    reasoning_output_tokens: 3,
                    total_tokens: 60,
                    cost_usd: 1.25,
                },
                burn_rate: BurnRateSnapshot {
                    window_duration: Duration::from_secs(30 * 60),
                    window_minutes: 30,
                    input_tokens_per_hour: 100,
                    cached_input_tokens_per_hour: 30,
                    output_tokens_per_hour: 20,
                    reasoning_output_tokens_per_hour: 6,
                    total_tokens_per_hour: 120,
                    cost_usd_per_hour: 2.5,
                },
                per_model: BTreeMap::from([(
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 20,
                        cached_input_tokens: 5,
                        output_tokens: 4,
                        reasoning_output_tokens: 1,
                        total_tokens: 24,
                        cost_usd: 0.5,
                        fallback_usage: UsageTotals::default(),
                        fallback_cost_usd: 0.0,
                        is_fallback: false,
                    },
                )]),
                missing_directories: Vec::new(),
                updated_time: "00:30:00".to_string(),
            },
            "en-US",
            NumberFormat::Short,
            false,
            CacheReadMode::Include,
        );

        assert!(!rendered.contains("| gpt-5 "));
    }

    #[test]
    fn render_watch_screen_includes_per_model_columns_and_skips_zero_usage_models() {
        let rendered = render_watch_screen(
            &WatchSnapshot {
                date: "2026-01-02".to_string(),
                totals: Totals {
                    input_tokens: 50,
                    cached_input_tokens: 15,
                    output_tokens: 10,
                    reasoning_output_tokens: 3,
                    total_tokens: 60,
                    cost_usd: 1.25,
                },
                burn_rate: BurnRateSnapshot {
                    window_duration: Duration::from_secs(30 * 60),
                    window_minutes: 30,
                    input_tokens_per_hour: 100,
                    cached_input_tokens_per_hour: 30,
                    output_tokens_per_hour: 20,
                    reasoning_output_tokens_per_hour: 6,
                    total_tokens_per_hour: 120,
                    cost_usd_per_hour: 2.5,
                },
                per_model: BTreeMap::from([
                    (
                        "gpt-5".to_string(),
                        ModelBreakdown {
                            input_tokens: 20,
                            cached_input_tokens: 5,
                            output_tokens: 4,
                            reasoning_output_tokens: 1,
                            total_tokens: 24,
                            cost_usd: 0.5,
                            fallback_usage: UsageTotals::default(),
                            fallback_cost_usd: 0.0,
                            is_fallback: false,
                        },
                    ),
                    (
                        "gpt-5-codex".to_string(),
                        ModelBreakdown {
                            input_tokens: 30,
                            cached_input_tokens: 10,
                            output_tokens: 6,
                            reasoning_output_tokens: 2,
                            total_tokens: 36,
                            cost_usd: 0.75,
                            fallback_usage: UsageTotals::default(),
                            fallback_cost_usd: 0.0,
                            is_fallback: false,
                        },
                    ),
                    (
                        "zero-model".to_string(),
                        ModelBreakdown {
                            input_tokens: 0,
                            cached_input_tokens: 0,
                            output_tokens: 0,
                            reasoning_output_tokens: 0,
                            total_tokens: 0,
                            cost_usd: 0.0,
                            fallback_usage: UsageTotals::default(),
                            fallback_cost_usd: 0.0,
                            is_fallback: false,
                        },
                    ),
                ]),
                missing_directories: Vec::new(),
                updated_time: "00:30:00".to_string(),
            },
            "en-US",
            NumberFormat::Short,
            true,
            CacheReadMode::Include,
        );

        assert!(rendered.contains("gpt-5"));
        assert!(rendered.contains("gpt-5-codex"));
        assert!(!rendered.contains("zero-model"));
        assert!(rendered.contains("Burn Rate (/h)"));
        assert!(rendered.contains("$1.00"));
        assert!(rendered.contains("$1.50"));
    }

    #[test]
    fn supports_watch_screen_clear_rejects_dumb_term() {
        assert!(supports_watch_screen_clear_with_platform(
            Some("xterm-256color"),
            false,
            false
        ));
        assert!(!supports_watch_screen_clear_with_platform(
            Some("dumb"),
            false,
            false
        ));
        assert!(supports_watch_screen_clear_with_platform(
            None, false, false
        ));
    }

    #[test]
    fn supports_watch_screen_clear_accepts_windows_console_probe() {
        assert!(!supports_watch_screen_clear_with_platform(
            None, true, false
        ));
        assert!(supports_watch_screen_clear_with_platform(
            Some("xterm-256color"),
            true,
            false
        ));
        assert!(supports_watch_screen_clear_with_platform(None, true, true));
    }

    #[test]
    fn display_window_minutes_rounds_up_partial_minutes() {
        assert_eq!(display_window_minutes(Duration::from_secs(0)), 0);
        assert_eq!(display_window_minutes(Duration::from_secs(1)), 1);
        assert_eq!(display_window_minutes(Duration::from_secs(59)), 1);
        assert_eq!(display_window_minutes(Duration::from_secs(61)), 2);
    }

    #[test]
    fn remaining_watch_sleep_subtracts_elapsed_work() {
        assert_eq!(
            remaining_watch_sleep(Duration::from_secs(5), Duration::from_secs(2)),
            Duration::from_secs(3)
        );
        assert_eq!(
            remaining_watch_sleep(Duration::from_secs(5), Duration::from_secs(7)),
            Duration::from_secs(0)
        );
    }

    #[test]
    fn scale_usage_per_hour_preserves_partial_second_windows() {
        assert_eq!(scale_usage_per_hour(2, Duration::from_millis(500)), 14_400);
        assert_eq!(scale_usage_per_hour(1, Duration::from_millis(250)), 14_400);
    }

    #[test]
    fn render_watch_screen_places_burn_rate_column_last() {
        let rendered = render_watch_screen(
            &WatchSnapshot {
                date: "2026-01-02".to_string(),
                totals: Totals {
                    input_tokens: 10,
                    cached_input_tokens: 0,
                    output_tokens: 2,
                    reasoning_output_tokens: 1,
                    total_tokens: 12,
                    cost_usd: 0.25,
                },
                burn_rate: BurnRateSnapshot {
                    window_duration: Duration::from_secs(30 * 60),
                    window_minutes: 30,
                    input_tokens_per_hour: 20,
                    cached_input_tokens_per_hour: 0,
                    output_tokens_per_hour: 4,
                    reasoning_output_tokens_per_hour: 2,
                    total_tokens_per_hour: 24,
                    cost_usd_per_hour: 0.5,
                },
                per_model: BTreeMap::from([(
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 10,
                        cached_input_tokens: 0,
                        output_tokens: 2,
                        reasoning_output_tokens: 1,
                        total_tokens: 12,
                        cost_usd: 0.25,
                        fallback_usage: UsageTotals::default(),
                        fallback_cost_usd: 0.0,
                        is_fallback: false,
                    },
                )]),
                missing_directories: Vec::new(),
                updated_time: "00:30:00".to_string(),
            },
            "en-US",
            NumberFormat::Full,
            true,
            CacheReadMode::Include,
        );

        let header = rendered
            .lines()
            .find(|line| line.contains("Metric") && line.contains("Burn Rate (/h)"))
            .expect("header");
        assert!(header.contains("Metric"));
        assert!(header.contains("Today"));
        assert!(header.contains("gpt-5 /h"));
        assert!(header.ends_with("Burn Rate (/h) |"));
    }

    #[test]
    fn render_watch_screen_scales_each_model_burn_column_independently() {
        let rendered = render_watch_screen(
            &WatchSnapshot {
                date: "2026-01-02".to_string(),
                totals: Totals {
                    input_tokens: 2,
                    cached_input_tokens: 0,
                    output_tokens: 0,
                    reasoning_output_tokens: 0,
                    total_tokens: 2,
                    cost_usd: 0.02,
                },
                burn_rate: BurnRateSnapshot {
                    window_duration: Duration::from_secs(59 * 60),
                    window_minutes: 59,
                    input_tokens_per_hour: 2,
                    cached_input_tokens_per_hour: 0,
                    output_tokens_per_hour: 0,
                    reasoning_output_tokens_per_hour: 0,
                    total_tokens_per_hour: 2,
                    cost_usd_per_hour: 0.02,
                },
                per_model: BTreeMap::from([
                    (
                        "gpt-5".to_string(),
                        ModelBreakdown {
                            input_tokens: 1,
                            cached_input_tokens: 0,
                            output_tokens: 0,
                            reasoning_output_tokens: 0,
                            total_tokens: 1,
                            cost_usd: 0.01,
                            fallback_usage: UsageTotals::default(),
                            fallback_cost_usd: 0.0,
                            is_fallback: false,
                        },
                    ),
                    (
                        "gpt-5-codex".to_string(),
                        ModelBreakdown {
                            input_tokens: 1,
                            cached_input_tokens: 0,
                            output_tokens: 0,
                            reasoning_output_tokens: 0,
                            total_tokens: 1,
                            cost_usd: 0.01,
                            fallback_usage: UsageTotals::default(),
                            fallback_cost_usd: 0.0,
                            is_fallback: false,
                        },
                    ),
                ]),
                missing_directories: Vec::new(),
                updated_time: "00:30:00".to_string(),
            },
            "en-US",
            NumberFormat::Full,
            true,
            CacheReadMode::Include,
        );

        let input_row = rendered
            .lines()
            .find(|line| line.contains("| Input "))
            .expect("input row");
        let cells = input_row
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(cells, vec!["Input", "2", "1", "1", "2"]);
    }

    #[test]
    fn render_watch_screen_includes_fallback_model_columns() {
        let rendered = render_watch_screen(
            &WatchSnapshot {
                date: "2026-01-02".to_string(),
                totals: Totals {
                    input_tokens: 2,
                    cached_input_tokens: 0,
                    output_tokens: 0,
                    reasoning_output_tokens: 0,
                    total_tokens: 2,
                    cost_usd: 0.02,
                },
                burn_rate: BurnRateSnapshot {
                    window_duration: Duration::from_secs(30 * 60),
                    window_minutes: 30,
                    input_tokens_per_hour: 4,
                    cached_input_tokens_per_hour: 0,
                    output_tokens_per_hour: 0,
                    reasoning_output_tokens_per_hour: 0,
                    total_tokens_per_hour: 4,
                    cost_usd_per_hour: 0.04,
                },
                per_model: BTreeMap::from([(
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 2,
                        cached_input_tokens: 0,
                        output_tokens: 0,
                        reasoning_output_tokens: 0,
                        total_tokens: 2,
                        cost_usd: 0.01,
                        fallback_usage: UsageTotals {
                            input: 1,
                            cached_input: 0,
                            output: 0,
                            reasoning_output: 0,
                            total: 1,
                        },
                        fallback_cost_usd: 0.01,
                        is_fallback: true,
                    },
                )]),
                missing_directories: Vec::new(),
                updated_time: "00:30:00".to_string(),
            },
            "en-US",
            NumberFormat::Full,
            true,
            CacheReadMode::Include,
        );

        let header = rendered
            .lines()
            .find(|line| line.contains("Metric") && line.contains("Burn Rate (/h)"))
            .expect("header");
        assert!(header.contains("gpt-5 /h"));
        assert!(header.contains("gpt-5 (fallback) /h"));

        let input_row = rendered
            .lines()
            .find(|line| line.contains("| Input "))
            .expect("input row");
        let cells = input_row
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(cells, vec!["Input", "2", "2", "2", "4"]);
    }

    #[test]
    fn render_watch_screen_wraps_model_columns_into_stacked_tables() {
        let snapshot = watch_snapshot_with_models(BTreeMap::from([
            (
                "gpt-5".to_string(),
                ModelBreakdown {
                    input_tokens: 20,
                    cached_input_tokens: 5,
                    output_tokens: 4,
                    reasoning_output_tokens: 1,
                    total_tokens: 24,
                    cost_usd: 0.60,
                    fallback_usage: UsageTotals::default(),
                    fallback_cost_usd: 0.0,
                    is_fallback: false,
                },
            ),
            (
                "gpt-5-codex".to_string(),
                ModelBreakdown {
                    input_tokens: 20,
                    cached_input_tokens: 5,
                    output_tokens: 4,
                    reasoning_output_tokens: 1,
                    total_tokens: 24,
                    cost_usd: 0.60,
                    fallback_usage: UsageTotals::default(),
                    fallback_cost_usd: 0.0,
                    is_fallback: false,
                },
            ),
            (
                "gpt-4.1-mini".to_string(),
                ModelBreakdown {
                    input_tokens: 20,
                    cached_input_tokens: 5,
                    output_tokens: 4,
                    reasoning_output_tokens: 1,
                    total_tokens: 24,
                    cost_usd: 0.60,
                    fallback_usage: UsageTotals::default(),
                    fallback_cost_usd: 0.0,
                    is_fallback: false,
                },
            ),
        ]));

        let rendered = super::render::render_watch_screen_with_width(
            &snapshot,
            "en-US",
            NumberFormat::Full,
            true,
            CacheReadMode::Include,
            Some(40),
        );

        let header_lines = rendered
            .lines()
            .filter(|line| line.contains("Metric"))
            .collect::<Vec<_>>();
        assert_eq!(header_lines.len(), 3);
        assert_eq!(
            header_lines
                .iter()
                .filter(|line| line.contains("Today"))
                .count(),
            1
        );
        assert_eq!(
            header_lines
                .iter()
                .filter(|line| line.contains("Burn Rate (/h)"))
                .count(),
            1
        );
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.contains("| Input "))
                .count(),
            3
        );
        assert!(header_lines[0].contains("gpt-4.1-mini /h"));
        assert!(header_lines[1].contains("gpt-5 /h"));
        assert!(header_lines[2].contains("gpt-5-codex /h"));
    }

    #[test]
    fn render_watch_screen_keeps_single_table_when_width_is_sufficient() {
        let snapshot = watch_snapshot_with_models(BTreeMap::from([
            (
                "gpt-5".to_string(),
                ModelBreakdown {
                    input_tokens: 30,
                    cached_input_tokens: 8,
                    output_tokens: 5,
                    reasoning_output_tokens: 2,
                    total_tokens: 35,
                    cost_usd: 0.75,
                    fallback_usage: UsageTotals::default(),
                    fallback_cost_usd: 0.0,
                    is_fallback: false,
                },
            ),
            (
                "gpt-5-codex".to_string(),
                ModelBreakdown {
                    input_tokens: 30,
                    cached_input_tokens: 7,
                    output_tokens: 7,
                    reasoning_output_tokens: 1,
                    total_tokens: 37,
                    cost_usd: 1.05,
                    fallback_usage: UsageTotals::default(),
                    fallback_cost_usd: 0.0,
                    is_fallback: false,
                },
            ),
        ]));

        let rendered = super::render::render_watch_screen_with_width(
            &snapshot,
            "en-US",
            NumberFormat::Full,
            true,
            CacheReadMode::Include,
            Some(200),
        );

        let header_lines = rendered
            .lines()
            .filter(|line| line.contains("Metric"))
            .collect::<Vec<_>>();
        assert_eq!(header_lines.len(), 1);
        assert!(header_lines[0].contains("Today"));
        assert!(header_lines[0].contains("gpt-5 /h"));
        assert!(header_lines[0].contains("gpt-5-codex /h"));
        assert!(header_lines[0].contains("Burn Rate (/h)"));
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.contains("| Input "))
                .count(),
            1
        );
    }

    #[test]
    fn watch_pricing_refresh_due_uses_cache_age_and_retry_backoff() {
        let temp = TempDir::new().expect("tempdir");
        let cache_path = temp.path().join("pricing-cache.json");
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(100);
        let fresh_refreshed_at =
            i64::try_from(Duration::from_hours(77).as_secs()).expect("fresh timestamp fits");
        fs::write(
            &cache_path,
            format!("{{\"refreshed_at_epoch_seconds\":{fresh_refreshed_at},\"models\":{{}}}}"),
        )
        .expect("write fresh cache");

        assert!(!watch_pricing_refresh_due(&cache_path, now, false, None).expect("fresh decision"));

        let stale_refreshed_at =
            i64::try_from(Duration::from_hours(75).as_secs()).expect("stale timestamp fits");
        fs::write(
            &cache_path,
            format!("{{\"refreshed_at_epoch_seconds\":{stale_refreshed_at},\"models\":{{}}}}"),
        )
        .expect("write stale cache");

        assert!(watch_pricing_refresh_due(&cache_path, now, false, None).expect("stale decision"));
        assert!(
            !watch_pricing_refresh_due(
                &cache_path,
                now,
                false,
                Some(now - Duration::from_mins(4)),
            )
            .expect("backoff decision")
        );
    }

    #[test]
    fn resolve_local_midnight_utc_handles_midnight_dst_skip() {
        let day = NaiveDate::from_ymd_opt(2024, 4, 26).expect("day");
        let resolved = resolve_local_midnight_utc(chrono_tz::Africa::Cairo, day).expect("start");

        assert_eq!(
            resolved,
            DateTime::parse_from_rfc3339("2024-04-25T22:00:00Z")
                .expect("timestamp")
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn cli_accepts_watch_interval_flag() {
        let cli = Cli::try_parse_from(["codexusage", "watch", "--interval", "15"]).expect("cli");

        let Some(Command::Watch {
            interval,
            per_model_burn_rate,
        }) = cli.command
        else {
            panic!("expected watch command");
        };
        assert_eq!(interval, Duration::from_secs(15));
        assert!(!per_model_burn_rate);
    }

    #[test]
    fn cli_accepts_watch_per_model_burn_rate_flag() {
        let cli =
            Cli::try_parse_from(["codexusage", "watch", "--per-model-burn-rate"]).expect("cli");

        let Some(Command::Watch {
            interval,
            per_model_burn_rate,
        }) = cli.command
        else {
            panic!("expected watch command");
        };
        assert_eq!(interval, Duration::from_secs(5));
        assert!(per_model_burn_rate);
    }

    #[test]
    fn cli_accepts_no_cache_cost_as_global_flag() {
        let cli = Cli::try_parse_from(["codexusage", "--no-cache-cost", "daily"]).expect("cli");

        assert_eq!(
            cli.cache_cost.cached_input_cost_mode(),
            CachedInputCostMode::Free
        );
        assert_eq!(cli.command, Some(Command::Daily));

        let watch_cli =
            Cli::try_parse_from(["codexusage", "watch", "--no-cache-cost"]).expect("cli");

        assert_eq!(
            watch_cli.cache_cost.cached_input_cost_mode(),
            CachedInputCostMode::Free
        );
        assert!(matches!(watch_cli.command, Some(Command::Watch { .. })));
    }

    #[test]
    fn cli_accepts_exclude_cache_read_as_global_flag() {
        let cli =
            Cli::try_parse_from(["codexusage", "--exclude-cache-read", "daily"]).expect("cli");

        assert_eq!(
            cli.cache_cost.cached_input_cost_mode(),
            CachedInputCostMode::Free
        );
        assert_eq!(cli.cache_cost.cache_read_mode(), CacheReadMode::Exclude);
        assert_eq!(cli.command, Some(Command::Daily));

        let watch_cli =
            Cli::try_parse_from(["codexusage", "watch", "--exclude-cache-read"]).expect("cli");

        assert_eq!(
            watch_cli.cache_cost.cached_input_cost_mode(),
            CachedInputCostMode::Free
        );
        assert_eq!(
            watch_cli.cache_cost.cache_read_mode(),
            CacheReadMode::Exclude
        );
        assert!(matches!(watch_cli.command, Some(Command::Watch { .. })));
    }

    #[test]
    fn run_rejects_watch_json_output() {
        let error = run(["codexusage", "--json", "watch"]
            .into_iter()
            .map(OsString::from))
        .expect_err("watch json should fail");

        assert!(error.to_string().contains("watch"));
        assert!(error.to_string().contains("--json"));
    }

    #[test]
    fn run_rejects_watch_date_filters() {
        let error = run(["codexusage", "--since", "2026-01-01", "watch"]
            .into_iter()
            .map(OsString::from))
        .expect_err("watch with date filters should fail");

        let rendered = error.to_string();
        assert!(rendered.contains("watch"));
        assert!(rendered.contains("--since"));
    }

    #[test]
    fn cli_accepts_threads_flag() {
        let cli = Cli::try_parse_from(["codexusage", "--threads", "1", "daily"]).expect("cli");

        assert_eq!(cli.threads, NonZeroUsize::new(1));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cli_accepts_debug_simulate_slow_disk_flag() {
        let cli = Cli::try_parse_from(["codexusage", "--debug-simulate-slow-disk", "daily"])
            .expect("cli");

        assert!(cli.debug.simulate_slow_disk);
    }

    #[test]
    fn cli_accepts_daily_last_days_flag() {
        let cli = Cli::try_parse_from(["codexusage", "daily", "--last-days", "7"]).expect("cli");

        assert_eq!(cli.command, Some(Command::Daily));
        assert_eq!(cli.last_days, NonZeroUsize::new(7));
    }

    #[test]
    fn cli_accepts_daily_last_days_short_flag() {
        let cli = Cli::try_parse_from(["codexusage", "daily", "-L", "7"]).expect("cli");

        assert_eq!(cli.command, Some(Command::Daily));
        assert_eq!(cli.last_days, NonZeroUsize::new(7));
    }

    #[test]
    fn cli_accepts_last_days_for_implicit_daily() {
        let cli = Cli::try_parse_from(["codexusage", "--last-days", "7"]).expect("cli");

        assert_eq!(cli.command, None);
        assert_eq!(cli.last_days, NonZeroUsize::new(7));
    }

    #[test]
    fn cli_rejects_zero_last_days() {
        let error = Cli::try_parse_from(["codexusage", "daily", "--last-days", "0"])
            .expect_err("zero last_days should fail");

        let rendered = error.to_string();
        assert!(rendered.contains("--last-days"));
        assert!(rendered.contains('0'));
    }

    #[test]
    fn effective_filters_reject_last_days_with_since() {
        let timezone = "UTC".parse::<Tz>().expect("timezone");
        let now_utc = DateTime::parse_from_rfc3339("2025-09-11T12:00:00+00:00")
            .expect("timestamp")
            .with_timezone(&Utc);
        let error = resolve_report_date_filters(
            ReportKind::Daily,
            Some(NonZeroUsize::new(7).expect("non-zero")),
            Some("2025-09-10"),
            None,
            timezone,
            now_utc,
        )
        .expect_err("conflicting date filters should fail");

        let rendered = error.to_string();
        assert!(rendered.contains("last_days"));
        assert!(rendered.contains("since/until"));
    }

    #[test]
    fn effective_filters_reject_last_days_with_until() {
        let timezone = "UTC".parse::<Tz>().expect("timezone");
        let now_utc = DateTime::parse_from_rfc3339("2025-09-11T12:00:00+00:00")
            .expect("timestamp")
            .with_timezone(&Utc);
        let error = resolve_report_date_filters(
            ReportKind::Daily,
            Some(NonZeroUsize::new(7).expect("non-zero")),
            None,
            Some("2025-09-12"),
            timezone,
            now_utc,
        )
        .expect_err("conflicting date filters should fail");

        let rendered = error.to_string();
        assert!(rendered.contains("last_days"));
        assert!(rendered.contains("since/until"));
    }

    #[test]
    fn effective_last_days_window_uses_selected_timezone_today() {
        let now_utc = DateTime::parse_from_rfc3339("2025-09-11T22:30:00+00:00")
            .expect("timestamp")
            .with_timezone(&Utc);
        let timezone = "Europe/Warsaw".parse::<Tz>().expect("timezone");
        let (since, until) = resolve_report_date_filters(
            ReportKind::Daily,
            Some(NonZeroUsize::new(2).expect("non-zero")),
            None,
            None,
            timezone,
            now_utc,
        )
        .expect("filters");

        assert_eq!(
            since,
            Some(NaiveDate::from_ymd_opt(2025, 9, 11).expect("since"))
        );
        assert_eq!(
            until,
            Some(NaiveDate::from_ymd_opt(2025, 9, 12).expect("until"))
        );
    }

    #[test]
    fn cli_rejects_zero_threads() {
        let error = Cli::try_parse_from(["codexusage", "--threads", "0", "daily"])
            .expect_err("zero threads should fail");

        let rendered = error.to_string();
        assert!(rendered.contains("--threads"));
        assert!(rendered.contains('0'));
    }

    #[test]
    fn resolve_scan_worker_count_caps_explicit_threads_to_workload() {
        assert_eq!(
            resolve_scan_worker_count(
                ScannerParallelism::Fixed(NonZeroUsize::new(4).expect("non-zero")),
                2,
            ),
            2
        );
    }
}
