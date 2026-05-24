//! CLI-only scan progress and debug runtime helpers.

use eyre::Result;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(debug_assertions)]
use std::sync::atomic::AtomicU64;
#[cfg(debug_assertions)]
use std::time::{SystemTime, UNIX_EPOCH};

/// Delay before interactive progress becomes visible.
pub(super) const PROGRESS_RENDER_DELAY: Duration = Duration::from_secs(5);
/// Polling cadence for the background progress renderer.
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Minimum simulated delay for debug slow-disk mode.
#[cfg(test)]
pub(super) const DEBUG_SLOW_DISK_MIN_DELAY: Duration = Duration::from_millis(250);
/// Maximum simulated delay for debug slow-disk mode.
#[cfg(test)]
pub(super) const DEBUG_SLOW_DISK_MAX_DELAY: Duration = Duration::from_millis(1_250);
/// Minimum simulated delay in milliseconds.
#[cfg(debug_assertions)]
const DEBUG_SLOW_DISK_MIN_MILLIS: u64 = 250;
/// Maximum simulated delay in milliseconds.
#[cfg(debug_assertions)]
const DEBUG_SLOW_DISK_MAX_MILLIS: u64 = 1_250;
/// Fixed increment used by `SplitMix64`.
#[cfg(debug_assertions)]
const SPLITMIX64_INCREMENT: u64 = 0x9e37_79b9_7f4a_7c15;
/// Default non-zero seed used when the runtime clock collapses to zero.
#[cfg(debug_assertions)]
const DEFAULT_SLOW_DISK_SEED: u64 = 0xa5a5_5a5a_c3c3_3c3c;

/// Batch-level scan behavior selected by the CLI.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ScanBehavior {
    /// Whether this scan batch should render interactive progress.
    show_progress: bool,
    /// Whether debug builds should sleep before opening each parsed file.
    #[cfg(debug_assertions)]
    simulate_slow_disk: bool,
}

impl ScanBehavior {
    /// Return the CLI-selected scan behavior.
    #[cfg(debug_assertions)]
    pub(super) const fn cli(show_progress: bool, simulate_slow_disk: bool) -> Self {
        Self {
            show_progress,
            simulate_slow_disk,
        }
    }

    /// Return the CLI-selected scan behavior.
    #[cfg(not(debug_assertions))]
    pub(super) const fn cli(show_progress: bool) -> Self {
        Self { show_progress }
    }
}

/// Batch-scoped scan runtime used by CLI and test entrypoints.
pub(super) trait ScanBatchRunner {
    /// Observer type shared across all files in one batch.
    type Observer: ScanObserver;

    /// Run one scan batch with access to its cloneable observer.
    fn run_batch<T, F>(&self, total_files: usize, scan: F) -> Result<T>
    where
        F: FnOnce(&Self::Observer) -> Result<T>;
}

/// Hook interface called by the parser around file-level work.
pub(super) trait ScanObserver: Clone + Send + Sync {
    /// Run immediately before opening one parsed file.
    fn before_file_open(&self) {}

    /// Run immediately after one parsed file finishes successfully.
    fn on_file_complete(&self) {}
}

/// No-op observer used by tests and non-interactive scan paths.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct NoopScanObserver;

impl ScanObserver for NoopScanObserver {}

/// No-op batch runner used when no CLI runtime hooks are active.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct NoopScanBatchRunner;

impl ScanBatchRunner for NoopScanBatchRunner {
    type Observer = NoopScanObserver;

    fn run_batch<T, F>(&self, _total_files: usize, scan: F) -> Result<T>
    where
        F: FnOnce(&Self::Observer) -> Result<T>,
    {
        scan(&NoopScanObserver)
    }
}

/// Shared observer used by one CLI-driven scan batch.
#[derive(Clone)]
pub(super) struct BatchScanObserver {
    /// Shared progress state for the background renderer.
    progress: Option<Arc<ProgressState>>,
    /// Shared debug slow-disk simulator.
    #[cfg(debug_assertions)]
    slow_disk: Option<Arc<SlowDiskSimulator>>,
}

impl BatchScanObserver {
    /// Build one observer plus the optional progress guard that owns the renderer thread.
    fn new(
        total_files: usize,
        behavior: ScanBehavior,
        stderr_is_terminal: bool,
    ) -> (Self, Option<ProgressGuard>) {
        let progress = progress_enabled(behavior.show_progress, total_files, stderr_is_terminal)
            .then(|| Arc::new(ProgressState::new(total_files)));
        let progress_guard = progress
            .as_ref()
            .map(|state| ProgressGuard::spawn(Arc::clone(state)));

        (
            Self {
                progress,
                #[cfg(debug_assertions)]
                slow_disk: behavior
                    .simulate_slow_disk
                    .then(|| Arc::new(SlowDiskSimulator::new())),
            },
            progress_guard,
        )
    }
}

impl ScanObserver for BatchScanObserver {
    fn before_file_open(&self) {
        #[cfg(debug_assertions)]
        if let Some(simulator) = &self.slow_disk {
            simulator.sleep_once();
        }
    }

    fn on_file_complete(&self) {
        if let Some(progress) = &self.progress {
            progress.completed_files.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// CLI batch runner that owns progress rendering and optional slow-disk simulation.
#[derive(Clone, Copy, Debug)]
pub(super) struct CliScanBatchRunner {
    /// CLI-selected runtime behavior for this batch.
    behavior: ScanBehavior,
}

impl CliScanBatchRunner {
    /// Build one CLI batch runner from the selected runtime behavior.
    pub(super) const fn new(behavior: ScanBehavior) -> Self {
        Self { behavior }
    }
}

impl ScanBatchRunner for CliScanBatchRunner {
    type Observer = BatchScanObserver;

    fn run_batch<T, F>(&self, total_files: usize, scan: F) -> Result<T>
    where
        F: FnOnce(&Self::Observer) -> Result<T>,
    {
        let context = BatchScanContext::new(total_files, self.behavior);
        let observer = context.observer();
        scan(&observer)
    }
}

/// Owning context for one scan batch.
struct BatchScanContext {
    /// Shared observer cloned into worker threads.
    observer: BatchScanObserver,
    /// Background renderer kept alive for the batch lifetime.
    _progress_guard: Option<ProgressGuard>,
}

impl BatchScanContext {
    /// Build a scan context using the real stderr terminal state.
    pub(super) fn new(total_files: usize, behavior: ScanBehavior) -> Self {
        Self::new_with_terminal(total_files, behavior, std::io::stderr().is_terminal())
    }

    /// Build a scan context with an explicit stderr terminal flag for tests.
    fn new_with_terminal(
        total_files: usize,
        behavior: ScanBehavior,
        stderr_is_terminal: bool,
    ) -> Self {
        let (observer, progress_guard) =
            BatchScanObserver::new(total_files, behavior, stderr_is_terminal);
        Self {
            observer,
            _progress_guard: progress_guard,
        }
    }

    /// Return the cloneable observer shared by worker threads.
    pub(super) fn observer(&self) -> BatchScanObserver {
        self.observer.clone()
    }
}

/// Shared state read by the background progress thread.
struct ProgressState {
    /// Total number of files in this scan batch.
    total_files: usize,
    /// Number of files that have finished parsing successfully.
    completed_files: AtomicUsize,
    /// Whether the renderer thread should stop.
    stop_requested: AtomicBool,
}

impl ProgressState {
    /// Create fresh progress state for one scan batch.
    fn new(total_files: usize) -> Self {
        Self {
            total_files,
            completed_files: AtomicUsize::new(0),
            stop_requested: AtomicBool::new(false),
        }
    }
}

/// Join handle for the background progress renderer.
struct ProgressGuard {
    /// Shared progress state observed by the renderer thread.
    state: Arc<ProgressState>,
    /// Background thread writing the transient progress line.
    handle: Option<JoinHandle<()>>,
}

impl ProgressGuard {
    /// Spawn the background renderer thread for one scan batch.
    fn spawn(state: Arc<ProgressState>) -> Self {
        let worker_state = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("scan-progress".to_string())
            .spawn(move || run_progress_worker(&worker_state))
            .ok();
        Self { state, handle }
    }
}

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        self.state.stop_requested.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Render the transient progress line until the scan batch finishes.
fn run_progress_worker(state: &ProgressState) {
    let started_at = Instant::now();
    let mut displayed_line = None;

    loop {
        if state.stop_requested.load(Ordering::Relaxed) {
            break;
        }

        if let Some(line) = progress_frame(
            started_at.elapsed(),
            state.completed_files.load(Ordering::Relaxed),
            state.total_files,
        ) {
            let should_render = displayed_line.as_ref() != Some(&line);
            if should_render {
                write_transient_line(&line);
                displayed_line = Some(line);
            }
        }

        thread::sleep(PROGRESS_POLL_INTERVAL);
    }

    if let Some(line) = displayed_line {
        write_transient_line(&clear_progress_line(&line));
    }
}

/// Return whether a scan batch should render interactive progress.
fn progress_enabled(show_progress: bool, total_files: usize, stderr_is_terminal: bool) -> bool {
    show_progress && total_files > 0 && stderr_is_terminal
}

/// Render one progress frame when the scan batch is slow enough.
fn progress_frame(elapsed: Duration, completed: usize, total: usize) -> Option<String> {
    (elapsed >= PROGRESS_RENDER_DELAY).then(|| format_progress_line(completed, total))
}

/// Write one transient line to stderr and flush it immediately.
fn write_transient_line(line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(line.as_bytes());
    let _ = stderr.flush();
}

/// Format one transient progress line.
pub(super) fn format_progress_line(completed: usize, total: usize) -> String {
    format!("\rparsing {completed}/{total}...")
}

/// Return the bytes needed to clear one transient progress line without ANSI escapes.
pub(super) fn clear_progress_line(line: &str) -> String {
    let visible_width = line.strip_prefix('\r').map_or(line.len(), str::len);
    format!("\r{}\r", " ".repeat(visible_width))
}

/// Simulated debug-only disk latency source.
#[cfg(debug_assertions)]
struct SlowDiskSimulator {
    /// PRNG state shared across worker threads.
    state: AtomicU64,
}

#[cfg(debug_assertions)]
impl SlowDiskSimulator {
    /// Create one simulator seeded from process-local runtime data.
    fn new() -> Self {
        Self {
            state: AtomicU64::new(initial_slow_disk_seed()),
        }
    }

    /// Sleep once using the configured random delay range.
    fn sleep_once(&self) {
        thread::sleep(sample_slow_disk_delay(&self.state));
    }
}

/// Build a non-zero PRNG seed from runtime data.
#[cfg(debug_assertions)]
fn initial_slow_disk_seed() -> u64 {
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |duration| duration.as_nanos());
    let now_bytes = now_nanos.to_le_bytes();
    let mut low = [0_u8; 8];
    low.copy_from_slice(&now_bytes[..8]);
    let mut high = [0_u8; 8];
    high.copy_from_slice(&now_bytes[8..]);

    let seed = u64::from_le_bytes(low)
        ^ u64::from_le_bytes(high)
        ^ u64::from(std::process::id())
        ^ DEFAULT_SLOW_DISK_SEED;
    if seed == 0 {
        DEFAULT_SLOW_DISK_SEED
    } else {
        seed
    }
}

/// Sample one simulated slow-disk delay from an internal PRNG state.
#[cfg(debug_assertions)]
pub(super) fn sample_slow_disk_delay(state: &AtomicU64) -> Duration {
    let span = DEBUG_SLOW_DISK_MAX_MILLIS - DEBUG_SLOW_DISK_MIN_MILLIS;
    let offset = splitmix64_next(state) % (span + 1);
    Duration::from_millis(DEBUG_SLOW_DISK_MIN_MILLIS + offset)
}

/// Return one pseudorandom `u64` using `SplitMix64`.
#[cfg(debug_assertions)]
fn splitmix64_next(state: &AtomicU64) -> u64 {
    let mut value = state.fetch_add(SPLITMIX64_INCREMENT, Ordering::Relaxed);
    value = value.wrapping_add(SPLITMIX64_INCREMENT);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::{
        BatchScanContext, CliScanBatchRunner, PROGRESS_RENDER_DELAY, ScanBatchRunner, ScanBehavior,
        ScanObserver, clear_progress_line, format_progress_line, progress_frame,
    };
    #[cfg(debug_assertions)]
    use super::{DEBUG_SLOW_DISK_MAX_DELAY, DEBUG_SLOW_DISK_MIN_DELAY, sample_slow_disk_delay};
    #[cfg(debug_assertions)]
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    #[test]
    fn progress_render_delay_stays_at_five_seconds() {
        assert_eq!(PROGRESS_RENDER_DELAY, Duration::from_secs(5));
    }

    #[test]
    fn progress_frame_stays_hidden_before_threshold() {
        assert!(progress_frame(Duration::from_secs(4), 0, 12).is_none());
    }

    #[test]
    fn progress_frame_renders_at_threshold() {
        assert_eq!(
            progress_frame(PROGRESS_RENDER_DELAY, 3, 12).as_deref(),
            Some("\rparsing 3/12...")
        );
    }

    #[test]
    fn format_progress_line_uses_requested_shape() {
        assert_eq!(format_progress_line(12, 34), "\rparsing 12/34...");
    }

    #[test]
    fn clear_progress_line_erases_previous_content_without_ansi() {
        assert_eq!(
            clear_progress_line("\rparsing 12/34..."),
            "\r                \r"
        );
    }

    #[test]
    fn batch_scan_context_disables_progress_when_stderr_is_not_terminal() {
        #[cfg(debug_assertions)]
        let behavior = ScanBehavior::cli(true, false);
        #[cfg(not(debug_assertions))]
        let behavior = ScanBehavior::cli(true);

        let context = BatchScanContext::new_with_terminal(5, behavior, false);
        assert!(context.observer.progress.is_none());
    }

    #[test]
    fn batch_scan_context_tracks_completed_files_when_progress_is_enabled() {
        #[cfg(debug_assertions)]
        let behavior = ScanBehavior::cli(true, false);
        #[cfg(not(debug_assertions))]
        let behavior = ScanBehavior::cli(true);

        let context = BatchScanContext::new_with_terminal(2, behavior, true);
        let observer = context.observer();
        let progress = observer.progress.as_ref().expect("progress state");

        observer.on_file_complete();

        assert_eq!(progress.total_files, 2);
        assert_eq!(progress.completed_files.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cli_scan_batch_runner_passes_observer_to_scan() {
        #[cfg(debug_assertions)]
        let behavior = ScanBehavior::cli(false, false);
        #[cfg(not(debug_assertions))]
        let behavior = ScanBehavior::cli(false);

        let runner = CliScanBatchRunner::new(behavior);
        let result = runner
            .run_batch(1, |observer| {
                observer.on_file_complete();
                Ok(observer.progress.is_none())
            })
            .expect("scan batch");

        assert!(result);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn sampled_slow_disk_delay_stays_within_requested_range() {
        let state = AtomicU64::new(0x1234_5678_9abc_def0);
        let delay = sample_slow_disk_delay(&state);

        assert!(delay >= DEBUG_SLOW_DISK_MIN_DELAY);
        assert!(delay <= DEBUG_SLOW_DISK_MAX_DELAY);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn sampled_slow_disk_delay_varies_between_draws() {
        let state = AtomicU64::new(0xfeed_face_cafe_beef);
        let first = sample_slow_disk_delay(&state);
        let second = sample_slow_disk_delay(&state);

        assert_ne!(first, second);
    }
}
