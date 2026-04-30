//! Shared app data models and presentation options.

use clap::ValueEnum;
use serde::Serialize;
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

/// Environment variable that overrides the default Codex home directory.
pub(in crate::app) const DEFAULT_CODEX_HOME_ENV: &str = "CODEX_HOME";
/// Model used when legacy logs do not expose model metadata.
pub(in crate::app) const DEFAULT_FALLBACK_MODEL: &str = "gpt-5";
/// Number of tokens in one million-token pricing unit.
pub(in crate::app) const MILLION: f64 = 1_000_000.0;

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
    /// Project directory used to filter sessions by logged working directory.
    pub project_dir: Option<PathBuf>,
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
pub(in crate::app) struct WatchOptions {
    /// IANA timezone name.
    pub(in crate::app) timezone: String,
    /// Output locale hint.
    pub(in crate::app) locale: String,
    /// Human-readable number formatting mode.
    pub(in crate::app) number_format: NumberFormat,
    /// Disable network pricing refreshes.
    pub(in crate::app) offline: bool,
    /// Force pricing refresh even when cache is fresh.
    pub(in crate::app) refresh_pricing: bool,
    /// Cached input token pricing mode.
    pub(in crate::app) cached_input_cost_mode: CachedInputCostMode,
    /// Cache-read token reporting mode.
    pub(in crate::app) cache_read_mode: CacheReadMode,
    /// Session directories to scan.
    pub(in crate::app) session_dirs: Vec<PathBuf>,
    /// Project directory used to filter sessions by logged working directory.
    pub(in crate::app) project_dir: Option<PathBuf>,
    /// Scanner worker configuration.
    pub(in crate::app) parallelism: ScannerParallelism,
    /// Refresh interval for the live screen.
    pub(in crate::app) interval: Duration,
    /// Show per-model detail rows in the watch table.
    pub(in crate::app) show_model_burn_rate: bool,
    /// Debug-only runtime options forwarded from the CLI.
    #[cfg(debug_assertions)]
    pub(in crate::app) debug: DebugRuntimeOptions,
}

/// Debug-only runtime behavior selected by the CLI and consumed by app runtimes.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::app) struct DebugRuntimeOptions {
    /// Simulate variable disk latency before opening each parsed file.
    pub(in crate::app) simulate_slow_disk: bool,
}

/// Rolling usage rate computed for one watch snapshot.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) struct BurnRateSnapshot {
    /// Exact rolling window duration.
    pub(in crate::app) window_duration: Duration,
    /// Effective rolling window width after current-day clamping.
    pub(in crate::app) window_minutes: u64,
    /// Input tokens per hour.
    pub(in crate::app) input_tokens_per_hour: u64,
    /// Cached input tokens per hour.
    pub(in crate::app) cached_input_tokens_per_hour: u64,
    /// Output tokens per hour.
    pub(in crate::app) output_tokens_per_hour: u64,
    /// Reasoning output tokens per hour.
    pub(in crate::app) reasoning_output_tokens_per_hour: u64,
    /// Total billable tokens per hour.
    pub(in crate::app) total_tokens_per_hour: u64,
    /// Cost in USD per hour.
    pub(in crate::app) cost_usd_per_hour: f64,
}

/// One rendered watch snapshot.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) struct WatchSnapshot {
    /// Current day in the selected timezone.
    pub(in crate::app) date: String,
    /// Current-day cumulative totals.
    pub(in crate::app) totals: Totals,
    /// Rolling burn-rate summary.
    pub(in crate::app) burn_rate: BurnRateSnapshot,
    /// Per-model rolling burn-window detail.
    pub(in crate::app) per_model: BTreeMap<String, ModelBreakdown>,
    /// Missing directories encountered during scan.
    pub(in crate::app) missing_directories: Vec<String>,
    /// Last refresh time in the selected timezone.
    pub(in crate::app) updated_time: String,
}

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

/// Usage and cost presentation behavior shared by reports and watch snapshots.
#[derive(Clone, Copy, Debug)]
pub(in crate::app) struct UsagePresentation {
    /// Cached input token pricing behavior.
    pub(in crate::app) cached_input_cost_mode: CachedInputCostMode,
    /// Cache-read token reporting behavior.
    pub(in crate::app) cache_read_mode: CacheReadMode,
}

impl UsagePresentation {
    /// Create presentation behavior from the CLI-free option modes.
    pub(in crate::app) const fn new(
        cached_input_cost_mode: CachedInputCostMode,
        cache_read_mode: CacheReadMode,
    ) -> Self {
        Self {
            cached_input_cost_mode,
            cache_read_mode,
        }
    }
}

/// Whether a bool is false.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if passes field values by reference"
)]
pub(in crate::app) fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, PartialEq)]
/// Internal usage accumulator.
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
    pub(in crate::app) fn add(&mut self, other: &UsageTotals) {
        self.input += other.input;
        self.cached_input += other.cached_input;
        self.output += other.output;
        self.reasoning_output += other.reasoning_output;
        self.total += other.total;
    }

    /// Remove one event.
    pub(in crate::app) fn subtract(&mut self, other: &UsageTotals) {
        self.input = self.input.saturating_sub(other.input);
        self.cached_input = self.cached_input.saturating_sub(other.cached_input);
        self.output = self.output.saturating_sub(other.output);
        self.reasoning_output = self.reasoning_output.saturating_sub(other.reasoning_output);
        self.total = self.total.saturating_sub(other.total);
    }

    /// Return whether this usage bucket contains any billable activity.
    pub(in crate::app) fn has_usage(&self) -> bool {
        self.input > 0
            || self.cached_input > 0
            || self.output > 0
            || self.reasoning_output > 0
            || self.total > 0
    }

    /// Return usage counters with the selected cache-read reporting mode applied.
    pub(in crate::app) fn with_cache_read_mode(&self, cache_read_mode: CacheReadMode) -> Self {
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

/// Return the non-fallback portion of a mixed model breakdown.
pub(in crate::app) fn explicit_usage(breakdown: &ModelBreakdown) -> UsageTotals {
    UsageTotals {
        input: breakdown
            .input_tokens
            .saturating_sub(breakdown.fallback_usage.input),
        cached_input: breakdown
            .cached_input_tokens
            .saturating_sub(breakdown.fallback_usage.cached_input),
        output: breakdown
            .output_tokens
            .saturating_sub(breakdown.fallback_usage.output),
        reasoning_output: breakdown
            .reasoning_output_tokens
            .saturating_sub(breakdown.fallback_usage.reasoning_output),
        total: breakdown
            .total_tokens
            .saturating_sub(breakdown.fallback_usage.total),
    }
}
