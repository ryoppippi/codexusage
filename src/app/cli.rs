//! CLI parsing and command dispatch.

#[cfg(debug_assertions)]
use super::model::DebugRuntimeOptions;
use super::model::{
    CacheReadMode, CachedInputCostMode, NumberFormat, ReportKind, ReportOptions,
    ScannerParallelism, WatchOptions,
};
use super::render::render_report;
use super::report::{build_report_for_cli, default_timezone_name, resolve_cli_session_dirs};
use super::scan_index::ScanIndexConfig;
use super::watch::{cli_scan_behavior, run_watch_loop, validate_watch_flags};
use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr};
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

/// CLI-only cache-cost flag group.
#[derive(Args, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::app) struct CacheCostCliOptions {
    /// Treat cache-read input tokens as free in cost calculations.
    #[arg(long = "no-cache-cost", global = true)]
    pub(in crate::app) no_cache_cost: bool,
    /// Exclude cache-read input tokens from reported usage and cost calculations.
    #[arg(long = "exclude-cache-read", global = true)]
    pub(in crate::app) exclude_cache_read: bool,
}

impl CacheCostCliOptions {
    /// Convert the parsed CLI flags into the internal pricing mode.
    pub(in crate::app) fn cached_input_cost_mode(self) -> CachedInputCostMode {
        if self.no_cache_cost || self.exclude_cache_read {
            CachedInputCostMode::Free
        } else {
            CachedInputCostMode::Priced
        }
    }

    /// Convert the parsed CLI flags into the cache-read reporting mode.
    pub(in crate::app) fn cache_read_mode(self) -> CacheReadMode {
        if self.exclude_cache_read {
            CacheReadMode::Exclude
        } else {
            CacheReadMode::Include
        }
    }
}

/// CLI-only project-filter flag group.
#[derive(Args, Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::app) struct ProjectCliOptions {
    /// Only include sessions whose logged working directory is under this project path.
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        conflicts_with = "current_dir"
    )]
    pub(in crate::app) project_dir: Option<PathBuf>,
    /// Only include sessions whose logged working directory is under the current directory.
    #[arg(long, global = true, conflicts_with = "project_dir")]
    pub(in crate::app) current_dir: bool,
}

impl ProjectCliOptions {
    /// Resolve project-filter flags into one optional project path.
    pub(in crate::app) fn resolve_project_dir(self) -> Result<Option<PathBuf>> {
        if self.current_dir {
            return std::env::current_dir()
                .map(Some)
                .wrap_err("failed to resolve current directory for --current-dir");
        }

        Ok(self.project_dir)
    }
}

/// CLI-only scan-index flag group.
#[derive(Args, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::app) struct ScanIndexCliOptions {
    /// Disable the persistent scan index for report commands.
    #[arg(long, global = true)]
    pub(in crate::app) no_scan_index: bool,
}

impl ScanIndexCliOptions {
    /// Convert parsed scan-index flags into report scan-index configuration.
    pub(in crate::app) const fn config(self) -> ScanIndexConfig {
        if self.no_scan_index {
            ScanIndexConfig::disabled()
        } else {
            ScanIndexConfig::enabled()
        }
    }
}

/// Debug-only runtime options parsed by the CLI in development builds.
#[cfg(debug_assertions)]
#[derive(Args, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::app) struct DebugRuntimeCliOptions {
    /// Simulate variable disk latency before opening each parsed file.
    #[arg(long = "debug-simulate-slow-disk", global = true)]
    pub(in crate::app) simulate_slow_disk: bool,
}

#[cfg(debug_assertions)]
impl From<DebugRuntimeCliOptions> for DebugRuntimeOptions {
    fn from(options: DebugRuntimeCliOptions) -> Self {
        Self {
            simulate_slow_disk: options.simulate_slow_disk,
        }
    }
}

/// Run the CLI from an argument iterator.
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
    let debug = DebugRuntimeOptions::from(cli.debug);
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
        project,
        session_dir,
        no_pi,
        threads,
        scan_index,
        command,
        ..
    } = cli;
    let timezone = timezone.unwrap_or_else(default_timezone_name);
    let project_dir = project.resolve_project_dir()?;
    let parallelism = threads.map_or(ScannerParallelism::Auto, ScannerParallelism::Fixed);
    let session_dir = resolve_cli_session_dirs(&session_dir, !no_pi);
    let cached_input_cost_mode = cache_cost.cached_input_cost_mode();
    let cache_read_mode = cache_cost.cache_read_mode();

    match command {
        Some(Command::Watch {
            interval,
            per_model_burn_rate,
        }) => {
            validate_watch_flags(json, since.as_deref(), until.as_deref(), last_days)?;
            run_watch_loop(
                &WatchOptions {
                    timezone,
                    locale,
                    number_format,
                    offline,
                    refresh_pricing,
                    cached_input_cost_mode,
                    cache_read_mode,
                    session_dirs: session_dir,
                    project_dir,
                    parallelism,
                    interval,
                    show_model_burn_rate: per_model_burn_rate,
                    #[cfg(debug_assertions)]
                    debug,
                },
                &scan_index.config(),
            )
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
                project_dir,
                parallelism,
            };
            #[cfg(debug_assertions)]
            let scan_behavior = cli_scan_behavior(true, debug.simulate_slow_disk);
            #[cfg(not(debug_assertions))]
            let scan_behavior = cli_scan_behavior(true);
            let output = build_report_for_cli(kind, &options, scan_behavior, &scan_index.config())?;
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
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent global CLI switches map naturally to booleans"
)]
#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Analyze Codex session usage with a fast Rust scanner"
)]
pub(in crate::app) struct Cli {
    /// Output structured JSON.
    #[arg(long, short = 'j', global = true)]
    pub(in crate::app) json: bool,
    /// Inclusive start date in YYYY-MM-DD or YYYYMMDD form.
    #[arg(long, short = 's', global = true)]
    pub(in crate::app) since: Option<String>,
    /// Inclusive end date in YYYY-MM-DD or YYYYMMDD form.
    #[arg(long, short = 'u', global = true)]
    pub(in crate::app) until: Option<String>,
    /// Show the last N calendar days ending today in the selected timezone.
    #[arg(
        long,
        short = 'L',
        global = true,
        value_name = "N",
        value_parser = clap::value_parser!(NonZeroUsize),
        conflicts_with_all = ["since", "until"]
    )]
    pub(in crate::app) last_days: Option<NonZeroUsize>,
    /// IANA timezone used for grouping. Defaults to the system timezone.
    #[arg(long, short = 'z', global = true)]
    pub(in crate::app) timezone: Option<String>,
    /// Locale hint reserved for display formatting.
    #[arg(long, short = 'l', default_value = "en-US", global = true)]
    pub(in crate::app) locale: String,
    /// Table number formatting mode.
    #[arg(long, value_enum, default_value_t = NumberFormat::Short, global = true)]
    pub(in crate::app) number_format: NumberFormat,
    /// Disable network pricing refreshes.
    #[arg(long, short = 'O', global = true)]
    pub(in crate::app) offline: bool,
    /// Force pricing refresh even if the cache is fresh.
    #[arg(long, global = true)]
    pub(in crate::app) refresh_pricing: bool,
    /// Cache-cost pricing options.
    #[command(flatten)]
    pub(in crate::app) cache_cost: CacheCostCliOptions,
    /// Project directory filtering options.
    #[command(flatten)]
    pub(in crate::app) project: ProjectCliOptions,
    /// Override the session directory. May be repeated.
    #[arg(long, global = true)]
    pub(in crate::app) session_dir: Vec<PathBuf>,
    /// Do not automatically ingest Pi sessions using the openai-codex provider.
    #[arg(long, global = true)]
    pub(in crate::app) no_pi: bool,
    /// Scanner worker count. Use `1` for single-threaded profiling runs.
    #[arg(long, global = true, value_name = "N", value_parser = clap::value_parser!(NonZeroUsize))]
    pub(in crate::app) threads: Option<NonZeroUsize>,
    /// Persistent scan-index options.
    #[command(flatten)]
    pub(in crate::app) scan_index: ScanIndexCliOptions,
    /// Debug-only runtime options for development builds.
    #[cfg(debug_assertions)]
    #[command(flatten)]
    pub(in crate::app) debug: DebugRuntimeCliOptions,
    /// Report to execute.
    #[command(subcommand)]
    pub(in crate::app) command: Option<Command>,
}

/// CLI subcommands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Subcommand)]
pub(in crate::app) enum Command {
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
    pub(in crate::app) fn from_command(value: &Command) -> Self {
        match value {
            Command::Daily => Self::Daily,
            Command::Monthly => Self::Monthly,
            Command::Session => Self::Session,
            Command::Watch { .. } => unreachable!("watch mode does not map to ReportKind"),
        }
    }
}

/// Parse a positive refresh interval in seconds.
pub(in crate::app) fn parse_interval_seconds(value: &str) -> std::result::Result<Duration, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| format!("invalid interval {value}; expected a positive integer"))?;
    if seconds == 0 {
        return Err("interval must be greater than 0 seconds".to_string());
    }

    Ok(Duration::from_secs(seconds))
}
