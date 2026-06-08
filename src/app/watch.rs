//! Watch-mode runtime and incremental refresh helpers.

use super::codex_limits::{
    CodexLimitStatus, codex_limit_status_for_watch_start, fetch_codex_limits,
};
use super::model::{
    BurnRateHistoryPoint, BurnRateSnapshot, ReportKind, ScannerParallelism, Totals,
    UsagePresentation, UsageTotals, WatchOptions, WatchSnapshot,
};
use super::render::render_watch_screen_with_limits;
use super::report::{
    GroupSummary, ProjectFilter, SessionScanTarget, calculate_summary_cost,
    collect_missing_session_dirs, collect_session_scan_targets, ensure_model_breakdown,
    parse_timezone, push_usage_into_breakdown, remove_usage_from_breakdown,
    resolve_scan_worker_count, resolve_session_dirs, resolve_session_target_across_roots,
    session_file_id, to_sorted_models,
};
use super::scan_index::{
    IndexedScanRequest, ScanIndexConfig, update_selected_session_targets_in_index,
};
#[cfg(test)]
use super::scan_runtime::NoopScanBatchRunner;
use super::scan_runtime::{CliScanBatchRunner, ScanBatchRunner, ScanBehavior, ScanObserver};
use super::session_files::{SessionFileFormat, session_file_format};
#[cfg(test)]
use super::session_log::TokenUsageEvent;
#[cfg(test)]
use super::session_log::scan_session_file_with;
use super::session_log::{SessionParseCheckpoint, scan_session_file_from_checkpoint_with_observer};
use crate::pricing::{
    CacheDecision, PricingCatalog, PricingLoadOptions, decide_cache_action, default_cache_path,
    load_pricing_catalog,
};
use chrono::{DateTime, LocalResult, NaiveDate, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use eyre::{Result, WrapErr, eyre};
use notify::{
    Config as NotifyConfig, Event as NotifyEvent, EventKind as NotifyEventKind, PollWatcher,
    RecommendedWatcher, RecursiveMode, Watcher,
    event::{DataChange, ModifyKind},
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How often watch mode should rediscover new or renamed session files.
const WATCH_DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);
/// Polling cadence used when native filesystem notifications are unavailable.
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Sliding window used by the compact cost burn-rate graph.
const WATCH_GRAPH_WINDOW: Duration = Duration::from_secs(30 * 60);
/// Minimum time between Codex account limit refresh attempts.
const WATCH_LIMITS_REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 60);

#[cfg(test)]
pub(in crate::app) fn build_watch_snapshot_at(
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
pub(in crate::app) fn build_watch_snapshot_with_pricing_at(
    options: &WatchOptions,
    now_utc: DateTime<Utc>,
    pricing: &PricingCatalog,
) -> Result<WatchSnapshot> {
    let timezone = parse_timezone(&options.timezone)?;
    let session_dirs = resolve_session_dirs(&options.session_dirs);
    let project_filter = ProjectFilter::from_path_option(options.project_dir.as_deref())?;
    let (missing_directories, selected_files) =
        collect_session_scan_targets(&session_dirs, project_filter.as_ref())?;
    let builder = scan_watch_targets(&selected_files, options.parallelism, timezone, now_utc)?;
    Ok(builder.finish(
        pricing,
        UsagePresentation::new(options.cached_input_cost_mode, options.cache_read_mode),
        missing_directories,
        options.show_model_burn_rate,
    ))
}

/// Validate global flags that conflict with the watch contract.
pub(in crate::app) fn validate_watch_flags(
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
pub(in crate::app) fn cli_scan_behavior(
    show_progress: bool,
    debug_simulate_slow_disk: bool,
) -> ScanBehavior {
    ScanBehavior::cli(show_progress, debug_simulate_slow_disk)
}

/// Build the scan behavior used by CLI-driven scans.
#[cfg(not(debug_assertions))]
pub(in crate::app) fn cli_scan_behavior(show_progress: bool) -> ScanBehavior {
    ScanBehavior::cli(show_progress)
}

/// Pending filesystem changes observed by the watch backend.
#[derive(Default)]
pub(in crate::app) struct WatchChangeSet {
    /// Stable session identifiers that need to be refreshed and how aggressively to refresh them.
    pub(in crate::app) dirty_sessions: HashMap<String, WatchDirtyKind>,
    /// Whether the full session tree should be rediscovered.
    pub(in crate::app) discovery_due: bool,
}

/// Scan settings shared by one watch refresh.
#[derive(Clone, Copy)]
struct WatchScanContext<'a, R>
where
    R: ScanBatchRunner,
{
    /// Scanner worker configuration.
    parallelism: ScannerParallelism,
    /// Runtime scan observer provider.
    scan_runner: &'a R,
    /// Persistent scan-index configuration.
    scan_index: &'a ScanIndexConfig,
}

/// Refresh policy selected for one dirty watched session.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::app) enum WatchDirtyKind {
    /// The file only grew, so the parser checkpoint can resume from the prior EOF.
    AppendOnly,
    /// The file may have been rewritten, renamed, created, or deleted; rebuild it from scratch.
    FullRebuild,
}

impl WatchChangeSet {
    /// Mark the provided session identifiers dirty, keeping the most conservative refresh mode.
    pub(in crate::app) fn mark_dirty_sessions(
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
pub(in crate::app) struct WatchEventSource {
    /// Active watcher backend.
    watcher: ActiveWatchWatcher,
    /// Cross-thread notification channel fed by `notify`.
    receiver: mpsc::Receiver<notify::Result<NotifyEvent>>,
    /// Roots currently registered with the watcher.
    pub(in crate::app) watched_roots: HashSet<PathBuf>,
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
    pub(in crate::app) fn new_polling(session_dirs: &[PathBuf]) -> Result<Self> {
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
    pub(in crate::app) fn sync_session_dirs(&mut self, session_dirs: &[PathBuf]) -> Result<bool> {
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
                let path_dirty_kind =
                    if session_file_format(path).is_some_and(SessionFileFormat::is_compressed) {
                        WatchDirtyKind::FullRebuild
                    } else {
                        dirty_kind
                    };
                changes.mark_dirty_sessions(session_ids, path_dirty_kind);
            } else if path_is_under_roots(session_dirs, path) {
                changes.discovery_due = true;
            }
        }
    }
}

/// Map a filesystem event to the safest incremental refresh mode.
pub(in crate::app) fn watch_dirty_kind(event_kind: NotifyEventKind) -> WatchDirtyKind {
    match event_kind {
        NotifyEventKind::Modify(ModifyKind::Data(DataChange::Size)) => WatchDirtyKind::AppendOnly,
        _ => WatchDirtyKind::FullRebuild,
    }
}

/// Return whether the path falls under one of the configured session roots.
pub(in crate::app) fn path_is_under_roots(session_dirs: &[PathBuf], path: &Path) -> bool {
    session_dirs.iter().any(|root| path.starts_with(root))
}

/// Resolve every stable session identifier that a watcher event path can map to.
pub(in crate::app) fn watch_event_session_ids(
    session_dirs: &[PathBuf],
    path: &Path,
) -> Vec<String> {
    if session_file_format(path).is_none() {
        return Vec::new();
    }

    session_dirs
        .iter()
        .filter(|root| path.starts_with(root))
        .map(|root| session_file_id(root, path))
        .collect()
}

/// Run the live watch loop until interrupted.
pub(in crate::app) fn run_watch_loop(
    options: &WatchOptions,
    scan_index: &ScanIndexConfig,
) -> Result<()> {
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
    let project_filter = ProjectFilter::from_path_option(options.project_dir.as_deref())?;
    let (startup_scan_runner, refresh_scan_runner) = watch_scan_runners(options);
    let mut watch_events = WatchEventSource::new(&session_dirs)?;
    let mut runtime = WatchRuntimeState::load_with_scan_runner(
        &session_dirs,
        project_filter.as_ref(),
        timezone,
        Utc::now(),
        &watch_scan_context(options.parallelism, &startup_scan_runner, scan_index),
    )?;
    let mut last_pricing_refresh_attempt_at = startup_refresh_attempted.then_some(now);
    let (mut codex_limit_status, mut last_codex_limit_refresh_attempt_at) =
        initial_codex_limit_watch_state(options.offline);
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
        refresh_watch_pricing_if_due(
            &mut pricing,
            &pricing_cache_path,
            now,
            options.offline,
            &mut last_pricing_refresh_attempt_at,
        )?;
        refresh_codex_limits_if_due(
            &mut codex_limit_status,
            &mut last_codex_limit_refresh_attempt_at,
            now,
            options.offline,
        );
        let mut changes = watch_events.drain_changes(&session_dirs);
        changes.discovery_due |= discovered_new_root;
        runtime.refresh_with_scan_runner(
            &session_dirs,
            project_filter.as_ref(),
            snapshot_now,
            changes,
            &watch_scan_context(options.parallelism, &refresh_scan_runner, scan_index),
        )?;
        let snapshot = runtime.snapshot(
            &pricing,
            UsagePresentation::new(options.cached_input_cost_mode, options.cache_read_mode),
            snapshot_now,
            options.show_model_burn_rate,
        )?;
        write_watch_snapshot(
            &mut stdout,
            &snapshot,
            options,
            &codex_limit_status,
            snapshot_now.timestamp(),
        )?;
        runtime.flush_pending_scan_index_updates(&watch_scan_context(
            options.parallelism,
            &refresh_scan_runner,
            scan_index,
        ));
        thread::sleep(remaining_watch_sleep(
            options.interval,
            loop_started_at.elapsed(),
        ));
    }
}

/// Build startup and steady-state scan runners for watch mode.
#[cfg(debug_assertions)]
fn watch_scan_runners(options: &WatchOptions) -> (CliScanBatchRunner, CliScanBatchRunner) {
    (
        CliScanBatchRunner::new(cli_scan_behavior(true, options.debug.simulate_slow_disk)),
        CliScanBatchRunner::new(cli_scan_behavior(false, options.debug.simulate_slow_disk)),
    )
}

/// Build startup and steady-state scan runners for watch mode.
#[cfg(not(debug_assertions))]
fn watch_scan_runners(_options: &WatchOptions) -> (CliScanBatchRunner, CliScanBatchRunner) {
    (
        CliScanBatchRunner::new(cli_scan_behavior(true)),
        CliScanBatchRunner::new(cli_scan_behavior(false)),
    )
}

/// Build one watch scan context.
fn watch_scan_context<'a, R>(
    parallelism: ScannerParallelism,
    scan_runner: &'a R,
    scan_index: &'a ScanIndexConfig,
) -> WatchScanContext<'a, R>
where
    R: ScanBatchRunner,
{
    WatchScanContext {
        parallelism,
        scan_runner,
        scan_index,
    }
}

/// Refresh watch pricing data when the cache policy says it is due.
fn refresh_watch_pricing_if_due(
    pricing: &mut PricingCatalog,
    pricing_cache_path: &Path,
    now: SystemTime,
    offline: bool,
    last_refresh_attempt_at: &mut Option<SystemTime>,
) -> Result<()> {
    if watch_pricing_refresh_due(pricing_cache_path, now, offline, *last_refresh_attempt_at)? {
        *pricing = load_pricing_catalog(&PricingLoadOptions {
            offline,
            force_refresh: false,
        })?;
        *last_refresh_attempt_at = Some(now);
    }
    Ok(())
}

/// Build the initial watch-mode Codex limit state.
fn initial_codex_limit_watch_state(offline: bool) -> (CodexLimitStatus, Option<SystemTime>) {
    (
        codex_limit_status_for_watch_start(offline, fetch_codex_limits),
        (!offline).then_some(SystemTime::now()),
    )
}

/// Refresh Codex limit status when the watch refresh interval has elapsed.
fn refresh_codex_limits_if_due(
    codex_limit_status: &mut CodexLimitStatus,
    last_refresh_attempt_at: &mut Option<SystemTime>,
    now: SystemTime,
    offline: bool,
) {
    if watch_codex_limits_refresh_due(now, offline, *last_refresh_attempt_at) {
        *codex_limit_status = fetch_codex_limits();
        *last_refresh_attempt_at = Some(now);
    }
}

/// Render and flush one watch snapshot to terminal output.
fn write_watch_snapshot(
    output: &mut impl Write,
    snapshot: &WatchSnapshot,
    options: &WatchOptions,
    codex_limit_status: &CodexLimitStatus,
    now_epoch_seconds: i64,
) -> Result<()> {
    output.write_all(b"\x1b[2J\x1b[H")?;
    output.write_all(
        render_watch_screen_with_limits(
            snapshot,
            &options.locale,
            options.number_format,
            options.show_model_burn_rate,
            options.cache_read_mode,
            codex_limit_status,
            now_epoch_seconds,
        )
        .as_bytes(),
    )?;
    output.flush()?;
    Ok(())
}

/// Decide whether watch mode should refresh Codex account limits before redraw.
pub(in crate::app) fn watch_codex_limits_refresh_due(
    now: SystemTime,
    offline: bool,
    last_refresh_attempt_at: Option<SystemTime>,
) -> bool {
    !offline
        && last_refresh_attempt_at.is_none_or(|attempted_at| {
            now.duration_since(attempted_at)
                .is_ok_and(|elapsed| elapsed >= WATCH_LIMITS_REFRESH_INTERVAL)
        })
}

/// Decide whether the terminal should receive ANSI clear-screen sequences.
pub(in crate::app) fn supports_watch_screen_clear(term: Option<&str>) -> bool {
    supports_watch_screen_clear_with_platform(term, cfg!(windows), windows_stdout_supports_ansi())
}

/// Decide whether the terminal should receive ANSI clear-screen sequences.
pub(in crate::app) fn supports_watch_screen_clear_with_platform(
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
pub(in crate::app) fn watch_pricing_refresh_due(
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

/// Shared metadata needed to render one watch snapshot.
pub(in crate::app) struct WatchSnapshotContext {
    /// Grouping timezone.
    timezone: Tz,
    /// Current day in the selected timezone.
    current_day: NaiveDate,
    /// Inclusive UTC upper bound for the rolling window.
    now_utc: DateTime<Utc>,
    /// Effective rolling window duration after clamping to the current day.
    window_duration: Duration,
}

/// Aggregated inputs used to finish one watch snapshot.
struct WatchSnapshotParts<'a> {
    /// Current-day cumulative usage.
    current_day_summary: &'a GroupSummary,
    /// Bounded rolling-window usage.
    burn_window_summary: &'a GroupSummary,
    /// Cost burn-rate samples for the compact graph.
    burn_history: Vec<BurnRateHistoryPoint>,
    /// Missing directories encountered during scan.
    missing_directories: Vec<String>,
    /// Whether to include per-model burn-rate columns.
    include_per_model: bool,
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
        parts: WatchSnapshotParts<'_>,
        pricing: &PricingCatalog,
        presentation: UsagePresentation,
    ) -> WatchSnapshot {
        let current_day_cost =
            calculate_summary_cost(&parts.current_day_summary.models, pricing, presentation);
        let burn_cost =
            calculate_summary_cost(&parts.burn_window_summary.models, pricing, presentation);
        let current_day_totals = parts
            .current_day_summary
            .totals
            .with_cache_read_mode(presentation.cache_read_mode);
        let burn_window_totals = parts
            .burn_window_summary
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
        let per_model = if parts.include_per_model {
            to_sorted_models(&parts.burn_window_summary.models, pricing, presentation)
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
            burn_history: parts.burn_history,
            per_model,
            missing_directories: parts.missing_directories,
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
pub(in crate::app) struct WatchBuilder {
    /// Shared snapshot rendering context.
    snapshot_context: WatchSnapshotContext,
    /// Inclusive UTC lower bound for the rolling window.
    window_start_utc: DateTime<Utc>,
    /// Earliest retained event needed for current-day totals and graph history.
    history_start_utc: DateTime<Utc>,
    /// Current-day cumulative usage.
    current_day_summary: GroupSummary,
    /// Bounded rolling-window usage.
    burn_window_summary: GroupSummary,
    /// Visible events retained for graph sample aggregation.
    history_events: Vec<OwnedWatchEvent>,
}

#[cfg(test)]
impl WatchBuilder {
    /// Create a new builder.
    pub(in crate::app) fn new(timezone: Tz, now_utc: DateTime<Utc>) -> Result<Self> {
        Ok(Self {
            snapshot_context: WatchSnapshotContext::new(timezone, now_utc)?,
            window_start_utc: watch_window_start_utc(timezone, now_utc)?,
            history_start_utc: watch_history_start_utc(timezone, now_utc)?,
            current_day_summary: GroupSummary::default(),
            burn_window_summary: GroupSummary::default(),
            history_events: Vec::new(),
        })
    }

    /// Observe one event.
    fn observe(&mut self, event: &TokenUsageEvent<'_, '_>) {
        if event.timestamp_utc > self.snapshot_context.now_utc {
            return;
        }
        if event.timestamp_utc < self.history_start_utc {
            return;
        }

        let owned_event = OwnedWatchEvent {
            timestamp_utc: event.timestamp_utc,
            model: event.model.to_string(),
            is_fallback_model: event.is_fallback_model,
            usage: event.usage.clone(),
        };
        self.history_events.push(owned_event.clone());

        let local = event
            .timestamp_utc
            .with_timezone(&self.snapshot_context.timezone);
        if local.date_naive() != self.snapshot_context.current_day {
            return;
        }

        push_owned_watch_event_into_summary(&mut self.current_day_summary, &owned_event);
        if event.timestamp_utc >= self.window_start_utc {
            push_owned_watch_event_into_summary(&mut self.burn_window_summary, &owned_event);
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
            self.history_start_utc, other.history_start_utc,
            "parallel scan chunks must preserve watch history start",
        );
        debug_assert_eq!(
            self.snapshot_context.now_utc, other.snapshot_context.now_utc,
            "parallel scan chunks must preserve watch now",
        );

        super::report::merge_group_summary(
            &mut self.current_day_summary,
            other.current_day_summary,
        );
        super::report::merge_group_summary(
            &mut self.burn_window_summary,
            other.burn_window_summary,
        );
        self.history_events.extend(other.history_events);
    }

    /// Finish the snapshot.
    pub(in crate::app) fn finish(
        self,
        pricing: &PricingCatalog,
        presentation: UsagePresentation,
        missing_directories: Vec<String>,
        include_per_model: bool,
    ) -> WatchSnapshot {
        let history_event_refs = self.history_events.iter().collect::<Vec<_>>();
        let burn_history = build_burn_history(
            &history_event_refs,
            &self.snapshot_context,
            pricing,
            presentation,
        );
        self.snapshot_context.finish(
            WatchSnapshotParts {
                current_day_summary: &self.current_day_summary,
                burn_window_summary: &self.burn_window_summary,
                burn_history,
                missing_directories,
                include_per_model,
            },
            pricing,
            presentation,
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

/// Resolve the earliest event needed for watch-mode current-day totals and graph history.
fn watch_history_start_utc(timezone: Tz, now_utc: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let current_day = now_utc.with_timezone(&timezone).date_naive();
    let day_start_utc = resolve_local_midnight_utc(timezone, current_day)?;
    let graph_history_start = now_utc
        .checked_sub_signed(TimeDelta::hours(8) + TimeDelta::minutes(30))
        .ok_or_else(|| eyre!("watch graph history underflowed the supported timestamp range"))?;
    Ok(day_start_utc.min(graph_history_start))
}

/// Build fixed 30-minute cost burn-rate samples for the compact watch graph.
fn build_burn_history(
    events: &[&OwnedWatchEvent],
    context: &WatchSnapshotContext,
    pricing: &PricingCatalog,
    presentation: UsagePresentation,
) -> Vec<BurnRateHistoryPoint> {
    let Some(mut sample_end) = context.now_utc.checked_sub_signed(TimeDelta::hours(8)) else {
        return Vec::new();
    };
    let mut points = Vec::with_capacity(33);
    while sample_end <= context.now_utc {
        let Some(window_start) = sample_end.checked_sub_signed(TimeDelta::minutes(30)) else {
            break;
        };
        let mut summary = GroupSummary::default();
        for event in events {
            if event.timestamp_utc >= window_start && event.timestamp_utc <= sample_end {
                push_owned_watch_event_into_summary(&mut summary, event);
            }
        }
        let cost = calculate_summary_cost(&summary.models, pricing, presentation);
        points.push(BurnRateHistoryPoint {
            end_time: sample_end
                .with_timezone(&context.timezone)
                .format("%H:%M")
                .to_string(),
            cost_usd_per_hour: scale_cost_per_hour(cost, WATCH_GRAPH_WINDOW),
        });
        let Some(next_sample_end) = sample_end.checked_add_signed(TimeDelta::minutes(15)) else {
            break;
        };
        sample_end = next_sample_end;
    }
    points
}

/// Return whether one cached event contributes to the current local day.
fn event_is_current_day(event: &OwnedWatchEvent, timezone: Tz, current_day: NaiveDate) -> bool {
    event.timestamp_utc.with_timezone(&timezone).date_naive() == current_day
}

/// One owned usage event cached for watch-mode refreshes.
#[derive(Clone, Debug)]
pub(in crate::app) struct OwnedWatchEvent {
    /// Event timestamp in UTC.
    pub(in crate::app) timestamp_utc: DateTime<Utc>,
    /// Resolved model name.
    pub(in crate::app) model: String,
    /// Whether the model was inferred from fallback metadata.
    pub(in crate::app) is_fallback_model: bool,
    /// Normalized usage delta.
    pub(in crate::app) usage: UsageTotals,
}

/// Cached watch contribution for one selected session file.
#[derive(Clone, Debug)]
pub(in crate::app) struct CachedWatchFile {
    /// Metadata used to detect whether the file changed since the previous refresh.
    pub(in crate::app) target: SessionScanTarget,
    /// Retained events already parsed from the file.
    pub(in crate::app) cached_events: Vec<OwnedWatchEvent>,
    /// Incremental parser state at the end of the file.
    pub(in crate::app) parser_checkpoint: SessionParseCheckpoint,
    /// Number of cached events that are visible at the current watch timestamp.
    pub(in crate::app) visible_end: usize,
    /// Index of the first event still inside the rolling burn window.
    pub(in crate::app) burn_start: usize,
}

impl CachedWatchFile {
    /// Build one cached file from a full file scan.
    fn from_full_scan(
        target: SessionScanTarget,
        parser_checkpoint: SessionParseCheckpoint,
        mut cached_events: Vec<OwnedWatchEvent>,
        window_start_utc: DateTime<Utc>,
        now_utc: DateTime<Utc>,
    ) -> Self {
        cached_events.sort_unstable_by_key(|event| event.timestamp_utc);
        let mut cached = Self {
            target,
            cached_events,
            parser_checkpoint,
            visible_end: 0,
            burn_start: 0,
        };
        cached.recompute_bounds(window_start_utc, now_utc);
        cached
    }

    /// Return the current-day events visible at the current watch timestamp.
    pub(in crate::app) fn visible_events(&self) -> &[OwnedWatchEvent] {
        &self.cached_events[..self.visible_end]
    }

    /// Return only the visible events that still fall inside the rolling watch window.
    pub(in crate::app) fn burn_events(&self) -> &[OwnedWatchEvent] {
        &self.cached_events[self.burn_start..self.visible_end]
    }

    /// Recompute visibility and burn-window bounds for the provided logical timestamp.
    pub(in crate::app) fn recompute_bounds(
        &mut self,
        window_start_utc: DateTime<Utc>,
        now_utc: DateTime<Utc>,
    ) {
        self.visible_end = self
            .cached_events
            .partition_point(|event| event.timestamp_utc <= now_utc);
        self.burn_start = self.cached_events[..self.visible_end]
            .partition_point(|event| event.timestamp_utc < window_start_utc);
    }
}

/// Cached watch-mode state reused between refreshes.
pub(in crate::app) struct WatchRuntimeState {
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
    /// Watch-parsed files whose report scan-index rows should be refreshed after rendering.
    pending_index_updates: Vec<SessionScanTarget>,
}

impl WatchRuntimeState {
    /// Load the initial runtime state.
    #[cfg(test)]
    pub(in crate::app) fn load(
        session_dirs: &[PathBuf],
        project_filter: Option<&ProjectFilter>,
        parallelism: ScannerParallelism,
        timezone: Tz,
        now_utc: DateTime<Utc>,
    ) -> Result<Self> {
        Self::load_with_scan_runner(
            session_dirs,
            project_filter,
            timezone,
            now_utc,
            &WatchScanContext {
                parallelism,
                scan_runner: &NoopScanBatchRunner,
                scan_index: &ScanIndexConfig::disabled(),
            },
        )
    }

    /// Load the initial runtime state while using the persistent scan index.
    #[cfg(test)]
    pub(in crate::app) fn load_with_scan_index(
        session_dirs: &[PathBuf],
        project_filter: Option<&ProjectFilter>,
        parallelism: ScannerParallelism,
        timezone: Tz,
        now_utc: DateTime<Utc>,
        scan_index: &ScanIndexConfig,
    ) -> Result<Self> {
        Self::load_with_scan_runner(
            session_dirs,
            project_filter,
            timezone,
            now_utc,
            &WatchScanContext {
                parallelism,
                scan_runner: &NoopScanBatchRunner,
                scan_index,
            },
        )
    }

    /// Load the initial runtime state with the provided batch runner.
    fn load_with_scan_runner<R>(
        session_dirs: &[PathBuf],
        project_filter: Option<&ProjectFilter>,
        timezone: Tz,
        now_utc: DateTime<Utc>,
        scan: &WatchScanContext<'_, R>,
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
            pending_index_updates: Vec::new(),
        };
        state.refresh_with_scan_runner(
            session_dirs,
            project_filter,
            now_utc,
            WatchChangeSet {
                dirty_sessions: HashMap::new(),
                discovery_due: true,
            },
            scan,
        )?;
        Ok(state)
    }

    /// Refresh cached file data for the current watch tick.
    #[cfg(test)]
    pub(in crate::app) fn refresh(
        &mut self,
        session_dirs: &[PathBuf],
        project_filter: Option<&ProjectFilter>,
        parallelism: ScannerParallelism,
        now_utc: DateTime<Utc>,
        changes: WatchChangeSet,
    ) -> Result<()> {
        self.refresh_with_scan_runner(
            session_dirs,
            project_filter,
            now_utc,
            changes,
            &WatchScanContext {
                parallelism,
                scan_runner: &NoopScanBatchRunner,
                scan_index: &ScanIndexConfig::disabled(),
            },
        )
    }

    /// Refresh cached file data while using the persistent scan index.
    #[cfg(test)]
    pub(in crate::app) fn refresh_with_scan_index(
        &mut self,
        session_dirs: &[PathBuf],
        project_filter: Option<&ProjectFilter>,
        parallelism: ScannerParallelism,
        now_utc: DateTime<Utc>,
        changes: WatchChangeSet,
        scan_index: &ScanIndexConfig,
    ) -> Result<()> {
        self.refresh_with_scan_runner(
            session_dirs,
            project_filter,
            now_utc,
            changes,
            &WatchScanContext {
                parallelism,
                scan_runner: &NoopScanBatchRunner,
                scan_index,
            },
        )
    }

    /// Flush queued scan-index updates using the persistent scan index.
    #[cfg(test)]
    pub(in crate::app) fn flush_pending_scan_index_updates_with_scan_index(
        &mut self,
        parallelism: ScannerParallelism,
        scan_index: &ScanIndexConfig,
    ) {
        self.flush_pending_scan_index_updates(&WatchScanContext {
            parallelism,
            scan_runner: &NoopScanBatchRunner,
            scan_index,
        });
    }

    /// Refresh cached file data for the current watch tick with the provided batch runner.
    fn refresh_with_scan_runner<R>(
        &mut self,
        session_dirs: &[PathBuf],
        project_filter: Option<&ProjectFilter>,
        now_utc: DateTime<Utc>,
        mut changes: WatchChangeSet,
        scan: &WatchScanContext<'_, R>,
    ) -> Result<()>
    where
        R: ScanBatchRunner,
    {
        let current_day = now_utc.with_timezone(&self.timezone).date_naive();
        if current_day != self.current_day {
            self.current_day = current_day;
            self.selected_targets.clear();
            self.cached_files.clear();
            self.current_day_summary = GroupSummary::default();
            self.burn_window_summary = GroupSummary::default();
            self.last_snapshot_utc = None;
            self.next_discovery_at = None;
            self.pending_index_updates.clear();
            changes.discovery_due = true;
        }

        self.advance_to(now_utc)?;

        let refresh_started_at = Instant::now();
        let discovery_due = changes.discovery_due
            || self
                .next_discovery_at
                .is_none_or(|deadline| refresh_started_at >= deadline);
        if discovery_due {
            let (missing_directories, selected_files) =
                collect_session_scan_targets(session_dirs, project_filter)?;
            self.missing_directories = missing_directories;
            self.next_discovery_at = Some(refresh_started_at + WATCH_DISCOVERY_INTERVAL);
            self.refresh_discovered_targets(
                self.timezone,
                now_utc,
                selected_files,
                &changes.dirty_sessions,
                scan,
            )?;
        } else {
            self.missing_directories = collect_missing_session_dirs(session_dirs)?;
            for (session_id, dirty_kind) in changes.dirty_sessions {
                let resolved =
                    resolve_session_target_across_roots(session_dirs, project_filter, &session_id)?;
                self.refresh_dirty_session(
                    session_id,
                    resolved,
                    dirty_kind,
                    self.timezone,
                    now_utc,
                    scan,
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

            for event in &cached_file.cached_events[previous_visible_end..cached_file.visible_end] {
                if event_is_current_day(event, self.timezone, self.current_day) {
                    push_owned_watch_event_into_summary(&mut self.current_day_summary, event);
                }
                if event.timestamp_utc >= window_start_utc {
                    push_owned_watch_event_into_summary(&mut self.burn_window_summary, event);
                }
            }
            for event in &cached_file.cached_events[previous_burn_start..cached_file.burn_start] {
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
                if event_is_current_day(event, self.timezone, self.current_day) {
                    push_owned_watch_event_into_summary(&mut self.current_day_summary, event);
                }
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
        timezone: Tz,
        now_utc: DateTime<Utc>,
        selected_files: Vec<SessionScanTarget>,
        dirty_sessions: &HashMap<String, WatchDirtyKind>,
        scan: &WatchScanContext<'_, R>,
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
                Some(_) | None
                    if watch_target_may_have_relevant_events(&target, timezone, now_utc)? =>
                {
                    full_rebuild_targets.push(target);
                }
                Some(_) | None => {}
            }
        }

        for cached_file in scan
            .scan_runner
            .run_batch(full_rebuild_targets.len(), |observer| {
                load_cached_watch_files_with_observer(
                    &full_rebuild_targets,
                    scan.parallelism,
                    timezone,
                    now_utc,
                    observer,
                )
            })?
        {
            if scan.scan_index.enabled {
                self.pending_index_updates.push(cached_file.target.clone());
            }
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
        scan: &WatchScanContext<'_, R>,
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
                scan.scan_runner.run_batch(1, |observer| {
                    append_cached_watch_file_with_observer(
                        &mut cached,
                        target,
                        timezone,
                        window_start_utc,
                        now_utc,
                        observer,
                    )
                })?;
                add_cached_watch_file_to_runtime(self, &cached);
                self.selected_targets
                    .insert(session_id, cached.target.clone());
                if scan.scan_index.enabled {
                    self.pending_index_updates.push(cached.target.clone());
                }
                self.cached_files
                    .insert(cached.target.session_id.clone(), cached);
            }
            (_existing, Some(target), _) => {
                let cached = scan.scan_runner.run_batch(1, |observer| {
                    build_cached_watch_file_with_observer(&target, timezone, now_utc, observer)
                })?;
                add_cached_watch_file_to_runtime(self, &cached);
                self.selected_targets.insert(session_id, target);
                if scan.scan_index.enabled {
                    self.pending_index_updates.push(cached.target.clone());
                }
                self.cached_files
                    .insert(cached.target.session_id.clone(), cached);
            }
            (_existing, None, _) => {
                self.selected_targets.remove(&session_id);
            }
        }
        Ok(())
    }

    /// Flush queued scan-index writes without letting cache failures stop watch mode.
    fn flush_pending_scan_index_updates<R>(&mut self, scan: &WatchScanContext<'_, R>)
    where
        R: ScanBatchRunner,
    {
        if !scan.scan_index.enabled || self.pending_index_updates.is_empty() {
            self.pending_index_updates.clear();
            return;
        }

        let mut by_session = HashMap::new();
        for target in self.pending_index_updates.drain(..) {
            by_session.insert(target.session_id.clone(), target);
        }
        let targets = by_session.into_values().collect::<Vec<_>>();
        update_scan_index_for_watch_targets(
            &targets,
            scan.parallelism,
            self.timezone,
            self.current_day,
            scan.scan_runner,
            scan.scan_index,
        );
    }

    /// Build one watch snapshot from the cached aggregate summaries.
    pub(in crate::app) fn snapshot(
        &self,
        pricing: &PricingCatalog,
        presentation: UsagePresentation,
        now_utc: DateTime<Utc>,
        include_per_model: bool,
    ) -> Result<WatchSnapshot> {
        let snapshot_context = WatchSnapshotContext::new(self.timezone, now_utc)?;
        let history_events = self
            .cached_files
            .values()
            .flat_map(CachedWatchFile::visible_events)
            .collect::<Vec<_>>();
        let burn_history =
            build_burn_history(&history_events, &snapshot_context, pricing, presentation);
        Ok(snapshot_context.finish(
            WatchSnapshotParts {
                current_day_summary: &self.current_day_summary,
                burn_window_summary: &self.burn_window_summary,
                burn_history,
                missing_directories: self.missing_directories.clone(),
                include_per_model,
            },
            pricing,
            presentation,
        ))
    }
}

/// Return whether two selected watch targets represent the same unchanged file.
fn same_watch_target(left: &SessionScanTarget, right: &SessionScanTarget) -> bool {
    left.path == right.path && left.bytes == right.bytes && left.modified == right.modified
}

/// Return whether a selected file can contain events needed by the current watch view.
fn watch_target_may_have_relevant_events(
    target: &SessionScanTarget,
    timezone: Tz,
    now_utc: DateTime<Utc>,
) -> Result<bool> {
    let Some(mtime_ns) = target.metadata.mtime_ns else {
        return Ok(true);
    };
    let history_start_ns = watch_history_start_utc(timezone, now_utc)?
        .timestamp_nanos_opt()
        .ok_or_else(|| eyre!("watch history start is outside the supported timestamp range"))?;
    Ok(mtime_ns >= history_start_ns)
}

/// Update the persistent report scan index for files that watch mode is already parsing.
fn update_scan_index_for_watch_targets<R>(
    targets: &[SessionScanTarget],
    parallelism: ScannerParallelism,
    timezone: Tz,
    current_day: NaiveDate,
    scan_runner: &R,
    scan_index: &ScanIndexConfig,
) where
    R: ScanBatchRunner,
{
    if targets.is_empty() || !scan_index.enabled {
        return;
    }

    if let Err(error) = update_selected_session_targets_in_index(&IndexedScanRequest {
        selected_files: targets,
        parallelism,
        kind: ReportKind::Daily,
        timezone,
        since: Some(current_day),
        until: Some(current_day),
        scan_runner,
        config: scan_index,
    }) {
        eprintln!("Warning: failed to update scan index from watch refresh: {error:#}");
    }
}

/// Parse all retained graph/current-day events from one session file for watch-mode caching.
fn scan_watch_file_delta_with_observer<O>(
    file: &Path,
    session_id: &str,
    history_start_utc: DateTime<Utc>,
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
            if event.timestamp_utc < history_start_utc {
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
    now_utc: DateTime<Utc>,
    observer: &O,
) -> Result<CachedWatchFile>
where
    O: ScanObserver,
{
    let (checkpoint, cached_events) = scan_watch_file_delta_with_observer(
        &target.path,
        &target.session_id,
        watch_history_start_utc(timezone, now_utc)?,
        &SessionParseCheckpoint::default(),
        observer,
    )?;
    observer.on_file_complete();
    Ok(CachedWatchFile::from_full_scan(
        target.clone(),
        checkpoint,
        cached_events,
        watch_window_start_utc(timezone, now_utc)?,
        now_utc,
    ))
}

/// Refresh one cached watch file by parsing only the appended suffix.
fn append_cached_watch_file_with_observer<O>(
    cached: &mut CachedWatchFile,
    target: SessionScanTarget,
    timezone: Tz,
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
        watch_history_start_utc(timezone, now_utc)?,
        &cached.parser_checkpoint,
        observer,
    )?;
    observer.on_file_complete();
    cached.target = target;
    cached.parser_checkpoint = checkpoint;
    cached.cached_events.extend(appended_events);
    cached
        .cached_events
        .sort_unstable_by_key(|event| event.timestamp_utc);
    cached.recompute_bounds(window_start_utc, now_utc);
    Ok(())
}

/// Add one cached file's visible contributions into the runtime aggregates.
fn add_cached_watch_file_to_runtime(runtime: &mut WatchRuntimeState, cached: &CachedWatchFile) {
    for event in cached.visible_events() {
        if event_is_current_day(event, runtime.timezone, runtime.current_day) {
            push_owned_watch_event_into_summary(&mut runtime.current_day_summary, event);
        }
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
        if event_is_current_day(event, runtime.timezone, runtime.current_day) {
            remove_owned_watch_event_from_summary(&mut runtime.current_day_summary, event);
        }
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
        return scan_watch_file_chunk_with_observer(selected_files, timezone, now_utc, observer);
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
                    scan_watch_file_chunk_with_observer(chunk, timezone, now_utc, &observer)
                })
            })
            .collect::<Vec<_>>();

        let mut cached_files =
            scan_watch_file_chunk_with_observer(first_chunk, timezone, now_utc, observer)?;
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
    now_utc: DateTime<Utc>,
    observer: &O,
) -> Result<Vec<CachedWatchFile>>
where
    O: ScanObserver,
{
    selected_files
        .iter()
        .map(|target| build_cached_watch_file_with_observer(target, timezone, now_utc, observer))
        .collect()
}

/// Return the displayed rolling-window size in whole minutes.
pub(in crate::app) fn display_window_minutes(window_duration: Duration) -> u64 {
    let seconds = window_duration.as_secs();
    if seconds == 0 {
        return 0;
    }

    seconds.div_ceil(60)
}

/// Compute how long watch mode should sleep after one refresh cycle.
pub(in crate::app) fn remaining_watch_sleep(interval: Duration, elapsed: Duration) -> Duration {
    interval.saturating_sub(elapsed)
}

/// Resolve local midnight for one day into UTC.
pub(in crate::app) fn resolve_local_midnight_utc(
    timezone: Tz,
    day: NaiveDate,
) -> Result<DateTime<Utc>> {
    let midnight = day
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| eyre!("failed to build midnight timestamp for {day}"))?;
    let local = first_valid_local_timestamp(timezone, midnight)
        .ok_or_else(|| eyre!("failed to resolve the start of {day} in timezone {timezone}"))?;

    Ok(local.with_timezone(&Utc))
}

/// Resolve the earliest valid local timestamp at or after the provided naive datetime.
pub(in crate::app) fn first_valid_local_timestamp(
    timezone: Tz,
    start: chrono::NaiveDateTime,
) -> Option<DateTime<Tz>> {
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
pub(in crate::app) fn scale_usage_per_hour(value: u64, window_duration: Duration) -> u64 {
    if value == 0 || window_duration.is_zero() {
        return 0;
    }

    let nanos = window_duration.as_nanos();
    let numerator = u128::from(value) * 3_600_000_000_000 + (nanos / 2);
    let scaled = numerator / nanos;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Convert one cost total into a per-hour burn rate.
pub(in crate::app) fn scale_cost_per_hour(value: f64, window_duration: Duration) -> f64 {
    if value == 0.0 || window_duration.is_zero() {
        return 0.0;
    }

    (value * 3600.0) / window_duration.as_secs_f64()
}

/// Push one cached watch event into a grouped summary.
pub(in crate::app) fn push_owned_watch_event_into_summary(
    summary: &mut GroupSummary,
    event: &OwnedWatchEvent,
) {
    summary.totals.add(&event.usage);
    let breakdown = ensure_model_breakdown(&mut summary.models, &event.model);
    push_usage_into_breakdown(breakdown, &event.usage, event.is_fallback_model);
    if event.is_fallback_model {
        breakdown.is_fallback = true;
    }
}

/// Remove one cached watch event from a grouped summary.
pub(in crate::app) fn remove_owned_watch_event_from_summary(
    summary: &mut GroupSummary,
    event: &OwnedWatchEvent,
) {
    summary.totals.subtract(&event.usage);
    let breakdown = ensure_model_breakdown(&mut summary.models, &event.model);
    remove_usage_from_breakdown(breakdown, &event.usage, event.is_fallback_model);
}

#[cfg(test)]
pub(in crate::app) fn scan_watch_targets(
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
#[cfg(test)]
pub(in crate::app) fn scan_watch_chunk(
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
