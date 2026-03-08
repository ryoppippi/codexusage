//! CLI orchestration and report building.

use crate::pricing::{Pricing, PricingCatalog, PricingLoadOptions, load_pricing_catalog};
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use clap::{Parser, Subcommand, ValueEnum};
use eyre::{Result, WrapErr, eyre};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Environment variable used to override the default Codex home directory.
const DEFAULT_CODEX_HOME_ENV: &str = "CODEX_HOME";
/// Model used when legacy logs do not expose model metadata.
const DEFAULT_FALLBACK_MODEL: &str = "gpt-5";
/// Number of tokens in one million-token pricing unit.
const MILLION: f64 = 1_000_000.0;

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

/// CLI-free options passed into report generation.
#[derive(Clone, Debug)]
pub struct ReportOptions {
    /// Inclusive lower bound.
    pub since: Option<String>,
    /// Inclusive upper bound.
    pub until: Option<String>,
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
    /// Session directories to scan.
    pub session_dirs: Vec<PathBuf>,
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

/// Build the requested report from the provided session directories.
///
/// # Errors
///
/// Returns an error when date filters are invalid, pricing setup fails, session files cannot be
/// read, or an event timestamp is malformed.
pub fn build_report(kind: ReportKind, options: &ReportOptions) -> Result<ReportOutput> {
    let timezone = parse_timezone(&options.timezone)?;
    let since = options
        .since
        .as_deref()
        .map(normalize_filter_date)
        .transpose()?;
    let until = options
        .until
        .as_deref()
        .map(normalize_filter_date)
        .transpose()?;
    let session_dirs = resolve_session_dirs(&options.session_dirs);
    let pricing = load_pricing_catalog(&PricingLoadOptions {
        offline: options.offline,
        force_refresh: options.refresh_pricing,
    })?;
    let mut builder = ReportBuilder::new(kind, timezone, since, until);
    let missing_directories = scan_session_dirs(&session_dirs, &mut builder)?;
    builder.finish(&pricing, missing_directories)
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
    let kind = cli.command.unwrap_or(Command::Daily).into();
    let options = ReportOptions {
        since: cli.since,
        until: cli.until,
        timezone: cli.timezone.unwrap_or_else(default_timezone_name),
        locale: cli.locale,
        number_format: cli.number_format,
        json: cli.json,
        offline: cli.offline,
        refresh_pricing: cli.refresh_pricing,
        session_dirs: cli.session_dir,
    };
    let output = build_report(kind, &options)?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "{}",
            render_report(&output, &options.locale, options.number_format)
        );
    }
    Ok(())
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
    /// Override the session directory. May be repeated.
    #[arg(long, global = true)]
    session_dir: Vec<PathBuf>,
    /// Report to execute.
    #[command(subcommand)]
    command: Option<Command>,
}

/// CLI subcommands.
#[derive(Clone, Copy, Debug, Subcommand)]
enum Command {
    /// Group usage by day.
    Daily,
    /// Group usage by month.
    Monthly,
    /// Group usage by session.
    Session,
}

impl From<Command> for ReportKind {
    fn from(value: Command) -> Self {
        match value {
            Command::Daily => Self::Daily,
            Command::Monthly => Self::Monthly,
            Command::Session => Self::Session,
        }
    }
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

    /// Return whether this usage bucket contains any billable activity.
    fn has_usage(&self) -> bool {
        self.input > 0
            || self.cached_input > 0
            || self.output > 0
            || self.reasoning_output > 0
            || self.total > 0
    }
}

/// Event ready for aggregation.
#[derive(Clone, Debug)]
struct TokenUsageEvent {
    /// Unique session key including source-root identity.
    session_key: String,
    /// Session identifier.
    session_id: String,
    /// Timestamp as parsed UTC datetime.
    timestamp_utc: DateTime<Utc>,
    /// Model name.
    model: String,
    /// Whether model fallback was used.
    is_fallback_model: bool,
    /// Token totals.
    usage: UsageTotals,
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
    fn observe(&mut self, event: &TokenUsageEvent) {
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
                    .entry(event.session_key.clone())
                    .or_insert_with(|| {
                        SessionSummary::new(event.timestamp_utc, event.session_id.clone())
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
        missing_directories: Vec<String>,
    ) -> Result<ReportOutput> {
        match self.kind {
            ReportKind::Daily => {
                let mut rows = Vec::with_capacity(self.daily.len());
                let mut keys = self.daily.keys().cloned().collect::<Vec<_>>();
                keys.sort_unstable();
                let mut totals = Totals::default();
                for key in keys {
                    let summary = self
                        .daily
                        .get(&key)
                        .ok_or_else(|| eyre!("missing daily summary for key {key}"))?;
                    let models = to_sorted_models(&summary.models, pricing);
                    let cost = calculate_summary_cost(&summary.models, pricing);
                    push_totals(&mut totals, &summary.totals, cost);
                    rows.push(DailyRow {
                        date: key,
                        input_tokens: summary.totals.input,
                        cached_input_tokens: summary.totals.cached_input,
                        output_tokens: summary.totals.output,
                        reasoning_output_tokens: summary.totals.reasoning_output,
                        total_tokens: summary.totals.total,
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
            ReportKind::Monthly => {
                let mut rows = Vec::with_capacity(self.monthly.len());
                let mut keys = self.monthly.keys().cloned().collect::<Vec<_>>();
                keys.sort_unstable();
                let mut totals = Totals::default();
                for key in keys {
                    let summary = self
                        .monthly
                        .get(&key)
                        .ok_or_else(|| eyre!("missing monthly summary for key {key}"))?;
                    let models = to_sorted_models(&summary.models, pricing);
                    let cost = calculate_summary_cost(&summary.models, pricing);
                    push_totals(&mut totals, &summary.totals, cost);
                    rows.push(MonthlyRow {
                        month: key,
                        input_tokens: summary.totals.input,
                        cached_input_tokens: summary.totals.cached_input,
                        output_tokens: summary.totals.output,
                        reasoning_output_tokens: summary.totals.reasoning_output,
                        total_tokens: summary.totals.total,
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
            ReportKind::Session => {
                let mut rows = Vec::with_capacity(self.session.len());
                let mut entries = self.session.into_iter().collect::<Vec<_>>();
                sort_session_entries(&mut entries);
                let mut totals = Totals::default();
                for (_session_key, summary) in entries {
                    let cost = calculate_summary_cost(&summary.models, pricing);
                    push_totals(&mut totals, &summary.totals, cost);
                    let (directory, session_file) = split_session_id(&summary.display_session_id);
                    rows.push(SessionRow {
                        session_id: summary.display_session_id,
                        directory,
                        session_file,
                        last_activity: summary
                            .last_activity
                            .with_timezone(&self.timezone)
                            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        input_tokens: summary.totals.input,
                        cached_input_tokens: summary.totals.cached_input,
                        output_tokens: summary.totals.output,
                        reasoning_output_tokens: summary.totals.reasoning_output,
                        total_tokens: summary.totals.total,
                        cost_usd: cost,
                        models: to_sorted_models(&summary.models, pricing),
                    });
                }
                Ok(ReportOutput::Session {
                    rows,
                    totals,
                    missing_directories,
                })
            }
        }
    }
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
fn push_event_into_summary(summary: &mut GroupSummary, event: &TokenUsageEvent) {
    summary.totals.add(&event.usage);
    let breakdown = summary.models.entry(event.model.clone()).or_default();
    push_usage_into_breakdown(breakdown, &event.usage, event.is_fallback_model);
    if event.is_fallback_model {
        breakdown.is_fallback = true;
    }
}

/// Push event data into a session summary.
fn push_event_into_session_summary(summary: &mut SessionSummary, event: &TokenUsageEvent) {
    summary.totals.add(&event.usage);
    let breakdown = summary.models.entry(event.model.clone()).or_default();
    push_usage_into_breakdown(breakdown, &event.usage, event.is_fallback_model);
    if event.is_fallback_model {
        breakdown.is_fallback = true;
    }
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

/// Convert a hash map into a stable `BTreeMap`.
fn to_sorted_models(
    models: &HashMap<String, ModelBreakdown>,
    pricing: &PricingCatalog,
) -> BTreeMap<String, ModelBreakdown> {
    models
        .iter()
        .map(|(model, usage)| {
            let mut breakdown = usage.clone();
            let resolved_pricing = pricing.resolve(model);
            breakdown.cost_usd =
                calculate_cost_from_usage(&explicit_usage(&breakdown), &resolved_pricing);
            breakdown.fallback_cost_usd =
                calculate_cost_from_usage(&breakdown.fallback_usage, &resolved_pricing);
            (model.clone(), breakdown)
        })
        .collect()
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
) -> f64 {
    models
        .iter()
        .map(|(model, usage)| calculate_cost(usage, &pricing.resolve(model)))
        .sum()
}

/// Split a session identifier into directory and leaf name.
fn split_session_id(session_id: &str) -> (String, String) {
    match session_id.rsplit_once('/') {
        Some((directory, session_file)) => (directory.to_string(), session_file.to_string()),
        None => (String::new(), session_id.to_string()),
    }
}

/// Render a report as a text table.
fn render_report(report: &ReportOutput, locale: &str, number_format: NumberFormat) -> String {
    let mut output = match report {
        ReportOutput::Daily { rows, totals, .. } => {
            render_daily_report(rows, totals, locale, number_format)
        }
        ReportOutput::Monthly { rows, totals, .. } => {
            render_monthly_report(rows, totals, locale, number_format)
        }
        ReportOutput::Session { rows, totals, .. } => {
            render_session_report(rows, totals, locale, number_format)
        }
    };

    let missing_directories = match report {
        ReportOutput::Daily {
            missing_directories,
            ..
        }
        | ReportOutput::Monthly {
            missing_directories,
            ..
        }
        | ReportOutput::Session {
            missing_directories,
            ..
        } => missing_directories,
    };
    if !missing_directories.is_empty() {
        let mut warning = String::from("Warning: missing session directories\n");
        for directory in missing_directories {
            let _ = writeln!(&mut warning, "- {directory}");
        }
        warning.push('\n');
        warning.push_str(&output);
        output = warning;
    }

    output
}

/// Render the daily report body.
fn render_daily_report(
    rows: &[DailyRow],
    totals: &Totals,
    locale: &str,
    number_format: NumberFormat,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    render_usage_table(
        "Daily",
        render_config,
        locale,
        &[
            "Date",
            "Model",
            "Input",
            "Cache",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
        ],
        daily_display_rows(rows),
        totals,
    )
}

/// Render the monthly report body.
fn render_monthly_report(
    rows: &[MonthlyRow],
    totals: &Totals,
    locale: &str,
    number_format: NumberFormat,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    render_usage_table(
        "Monthly",
        render_config,
        locale,
        &[
            "Month",
            "Model",
            "Input",
            "Cache",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
        ],
        monthly_display_rows(rows),
        totals,
    )
}

/// Render the session report body.
fn render_session_report(
    rows: &[SessionRow],
    totals: &Totals,
    locale: &str,
    number_format: NumberFormat,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    render_usage_table(
        "Session",
        render_config,
        locale,
        &[
            "Directory",
            "Session",
            "Model",
            "Input",
            "Cache",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
            "Last Activity",
        ],
        session_display_rows(rows),
        totals,
    )
}

/// One rendered table row.
#[derive(Clone, Debug)]
struct DisplayRow {
    /// Cells in table order.
    cells: Vec<String>,
    /// Visual styling for the row.
    kind: DisplayRowKind,
}

/// Styling group for one display row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DisplayRowKind {
    /// Insert a blank line before the next row.
    Spacer,
    /// Group subtotal row.
    Subtotal,
    /// Per-model child row.
    Detail,
    /// Final grand total row.
    GrandTotal,
}

/// Terminal color mode for table rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TableStyle {
    /// Do not emit ANSI escapes.
    Plain,
    /// Emit 256-color ANSI escapes.
    Ansi256,
}

/// Border style for the rendered table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BorderStyle {
    /// ASCII-safe borders.
    Ascii,
    /// Unicode box-drawing borders.
    Unicode,
}

/// Rendering controls for the human-readable table.
#[derive(Clone, Copy, Debug)]
struct TableRenderConfig {
    /// ANSI styling mode.
    style: TableStyle,
    /// Border glyph mode.
    borders: BorderStyle,
    /// Numeric display mode.
    number_format: NumberFormat,
}

/// Detect the best table style for the current stdout stream.
///
/// This auto-detect behavior is intentional: the user explicitly chose richer ANSI output on
/// capable terminals and plain text fallback elsewhere.
fn detect_table_style() -> TableStyle {
    detect_table_style_for(
        std::io::stdout().is_terminal(),
        env::var("TERM").ok().as_deref(),
        env::var("COLORTERM").ok().as_deref(),
        env::var_os("NO_COLOR").is_some(),
    )
}

/// Detect the best border style for the current stdout stream.
///
/// This auto-detect behavior is intentional: the user explicitly chose Unicode box-drawing on
/// UTF-8 terminals and ASCII fallback elsewhere.
fn detect_border_style() -> BorderStyle {
    detect_border_style_for(
        std::io::stdout().is_terminal(),
        env::var("LC_ALL").ok().as_deref(),
        env::var("LC_CTYPE").ok().as_deref(),
        env::var("LANG").ok().as_deref(),
    )
}

/// Decide whether Unicode box-drawing is safe for the current environment.
fn detect_border_style_for(
    stdout_is_terminal: bool,
    lc_all: Option<&str>,
    lc_ctype: Option<&str>,
    lang: Option<&str>,
) -> BorderStyle {
    if !stdout_is_terminal {
        return BorderStyle::Ascii;
    }

    let locale = lc_all
        .filter(|value| !value.is_empty())
        .or(lc_ctype.filter(|value| !value.is_empty()))
        .or(lang.filter(|value| !value.is_empty()))
        .unwrap_or_default()
        .to_ascii_lowercase();

    if locale.contains("utf-8") || locale.contains("utf8") {
        BorderStyle::Unicode
    } else {
        BorderStyle::Ascii
    }
}

/// Decide whether styled output should be enabled.
fn detect_table_style_for(
    stdout_is_terminal: bool,
    term: Option<&str>,
    colorterm: Option<&str>,
    no_color: bool,
) -> TableStyle {
    if no_color || !stdout_is_terminal {
        return TableStyle::Plain;
    }

    let term = term.unwrap_or_default();
    if term.is_empty() || term == "dumb" {
        return TableStyle::Plain;
    }

    let colorterm = colorterm.unwrap_or_default();
    if term.contains("256color")
        || colorterm.eq_ignore_ascii_case("truecolor")
        || colorterm.eq_ignore_ascii_case("24bit")
    {
        TableStyle::Ansi256
    } else {
        TableStyle::Plain
    }
}

/// Build display rows for a daily report.
fn daily_display_rows(rows: &[DailyRow]) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            display_rows.push(DisplayRow {
                cells: Vec::new(),
                kind: DisplayRowKind::Spacer,
            });
        }
        display_rows.push(DisplayRow {
            cells: vec![
                row.date.clone(),
                "TOTAL".to_string(),
                row.input_tokens.to_string(),
                row.cached_input_tokens.to_string(),
                row.output_tokens.to_string(),
                row.reasoning_output_tokens.to_string(),
                row.total_tokens.to_string(),
                format_currency(row.cost_usd),
            ],
            kind: DisplayRowKind::Subtotal,
        });
        append_model_display_rows(&mut display_rows, 1, false, &row.models);
    }
    display_rows
}

/// Build display rows for a monthly report.
fn monthly_display_rows(rows: &[MonthlyRow]) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            display_rows.push(DisplayRow {
                cells: Vec::new(),
                kind: DisplayRowKind::Spacer,
            });
        }
        display_rows.push(DisplayRow {
            cells: vec![
                row.month.clone(),
                "TOTAL".to_string(),
                row.input_tokens.to_string(),
                row.cached_input_tokens.to_string(),
                row.output_tokens.to_string(),
                row.reasoning_output_tokens.to_string(),
                row.total_tokens.to_string(),
                format_currency(row.cost_usd),
            ],
            kind: DisplayRowKind::Subtotal,
        });
        append_model_display_rows(&mut display_rows, 1, false, &row.models);
    }
    display_rows
}

/// Build display rows for a session report.
fn session_display_rows(rows: &[SessionRow]) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            display_rows.push(DisplayRow {
                cells: Vec::new(),
                kind: DisplayRowKind::Spacer,
            });
        }
        display_rows.push(DisplayRow {
            cells: vec![
                if row.directory.is_empty() {
                    "-".to_string()
                } else {
                    row.directory.clone()
                },
                row.session_file.clone(),
                "TOTAL".to_string(),
                row.input_tokens.to_string(),
                row.cached_input_tokens.to_string(),
                row.output_tokens.to_string(),
                row.reasoning_output_tokens.to_string(),
                row.total_tokens.to_string(),
                format_currency(row.cost_usd),
                row.last_activity.clone(),
            ],
            kind: DisplayRowKind::Subtotal,
        });
        append_model_display_rows(&mut display_rows, 2, true, &row.models);
    }
    display_rows
}

/// Append child rows for every model in a group.
fn append_model_display_rows(
    display_rows: &mut Vec<DisplayRow>,
    columns_before_model: usize,
    include_last_activity_column: bool,
    models: &BTreeMap<String, ModelBreakdown>,
) {
    for (model, breakdown) in models {
        let explicit_usage = explicit_usage(breakdown);
        if explicit_usage.has_usage() {
            display_rows.push(model_display_row(
                columns_before_model,
                include_last_activity_column,
                model,
                &explicit_usage,
                breakdown.cost_usd,
            ));
        }

        if breakdown.fallback_usage.has_usage() {
            display_rows.push(model_display_row(
                columns_before_model,
                include_last_activity_column,
                &format!("{model} (fallback)"),
                &breakdown.fallback_usage,
                breakdown.fallback_cost_usd,
            ));
        }
    }
}

/// Build one child row for a model breakdown.
fn model_display_row(
    columns_before_model: usize,
    include_last_activity_column: bool,
    model_label: &str,
    usage: &UsageTotals,
    cost_usd: f64,
) -> DisplayRow {
    let mut cells = vec![String::new(); columns_before_model];
    cells.push(format!("  {model_label}"));
    cells.extend_from_slice(&[
        usage.input.to_string(),
        usage.cached_input.to_string(),
        usage.output.to_string(),
        usage.reasoning_output.to_string(),
        usage.total.to_string(),
        format_currency(cost_usd),
    ]);
    if include_last_activity_column {
        cells.push(String::new());
    }

    DisplayRow {
        cells,
        kind: DisplayRowKind::Detail,
    }
}

/// Return the explicit portion of a mixed model breakdown.
fn explicit_usage(breakdown: &ModelBreakdown) -> UsageTotals {
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

/// Render a rectangular table with grouped rows and a totals row.
fn render_usage_table(
    title: &str,
    render_config: TableRenderConfig,
    _locale: &str,
    headers: &[&str],
    rows: Vec<DisplayRow>,
    totals: &Totals,
) -> String {
    let mut all_rows = rows;
    let grand_total_row = grand_total_row(headers, totals);
    let widths = column_widths(
        headers,
        &all_rows,
        &grand_total_row,
        render_config.number_format,
    );
    all_rows.push(grand_total_row);

    let mut output = String::new();
    write_table_title(&mut output, render_config.style, title);
    let _ = writeln!(&mut output);
    write_table_header(&mut output, render_config, headers, &widths);
    for row in all_rows {
        if row.kind == DisplayRowKind::Spacer {
            write_table_rule(
                &mut output,
                render_config.style,
                table_rule_element(TableRuleKind::GroupSeparator),
                &table_rule(
                    TableRuleKind::GroupSeparator,
                    render_config.borders,
                    &widths,
                ),
            );
            continue;
        }
        write_table_row(
            &mut output,
            render_config,
            headers,
            &widths,
            &row.cells,
            row_table_element(row.kind),
        );
    }
    write_table_rule(
        &mut output,
        render_config.style,
        table_rule_element(TableRuleKind::Bottom),
        &table_rule(TableRuleKind::Bottom, render_config.borders, &widths),
    );
    output
}

/// Build the final grand total row for a table.
fn grand_total_row(headers: &[&str], totals: &Totals) -> DisplayRow {
    let cells = if headers.first() == Some(&"Directory") {
        vec![
            String::new(),
            String::new(),
            "GRAND TOTAL".to_string(),
            totals.input_tokens.to_string(),
            totals.cached_input_tokens.to_string(),
            totals.output_tokens.to_string(),
            totals.reasoning_output_tokens.to_string(),
            totals.total_tokens.to_string(),
            format_currency(totals.cost_usd),
            String::new(),
        ]
    } else {
        vec![
            String::new(),
            "GRAND TOTAL".to_string(),
            totals.input_tokens.to_string(),
            totals.cached_input_tokens.to_string(),
            totals.output_tokens.to_string(),
            totals.reasoning_output_tokens.to_string(),
            totals.total_tokens.to_string(),
            format_currency(totals.cost_usd),
        ]
    };

    DisplayRow {
        cells,
        kind: DisplayRowKind::GrandTotal,
    }
}

/// Compute the display width of every column.
fn column_widths(
    headers: &[&str],
    rows: &[DisplayRow],
    grand_total_row: &DisplayRow,
    number_format: NumberFormat,
) -> Vec<usize> {
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in rows.iter().chain(std::iter::once(grand_total_row)) {
        for (index, cell) in row.cells.iter().enumerate() {
            widths[index] =
                widths[index].max(format_table_cell(headers, index, cell, number_format).len());
        }
    }
    widths
}

/// Write the table title.
fn write_table_title(output: &mut String, style: TableStyle, title: &str) {
    let _ = writeln!(
        output,
        "{}",
        paint(
            style,
            TableElement::Title,
            &format!("{title} Codex Usage Report")
        )
    );
}

/// Write the table header and separator.
fn write_table_header(
    output: &mut String,
    render_config: TableRenderConfig,
    headers: &[&str],
    widths: &[usize],
) {
    write_table_rule(
        output,
        render_config.style,
        table_rule_element(TableRuleKind::Top),
        &table_rule(TableRuleKind::Top, render_config.borders, widths),
    );
    let header_cells = headers
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    write_table_row(
        output,
        render_config,
        headers,
        widths,
        &header_cells,
        TableElement::Header,
    );
    write_table_rule(
        output,
        render_config.style,
        table_rule_element(TableRuleKind::HeaderSeparator),
        &table_rule(
            TableRuleKind::HeaderSeparator,
            render_config.borders,
            widths,
        ),
    );
}

/// Format one data row with aligned cells.
#[cfg(test)]
fn format_data_row(
    headers: &[&str],
    borders: BorderStyle,
    widths: &[usize],
    cells: &[String],
) -> String {
    let theme = border_theme(borders);
    let body = format_aligned_cells(headers, widths, cells, NumberFormat::Full)
        .join(&theme.vertical.to_string());
    format!("{}{}{}", theme.vertical, body, theme.vertical)
}

/// Render one row with border segments styled independently from the cell text.
fn write_table_row(
    output: &mut String,
    render_config: TableRenderConfig,
    headers: &[&str],
    widths: &[usize],
    cells: &[String],
    cell_element: TableElement,
) {
    let theme = border_theme(render_config.borders);
    let border = paint(
        render_config.style,
        TableElement::Border,
        &theme.vertical.to_string(),
    );
    let separator = paint(
        render_config.style,
        TableElement::Border,
        &theme.vertical.to_string(),
    );
    let styled_cells = format_aligned_cells(headers, widths, cells, render_config.number_format)
        .into_iter()
        .map(|cell| paint(render_config.style, cell_element, &cell))
        .collect::<Vec<_>>();
    let _ = writeln!(
        output,
        "{}{}{}",
        border,
        styled_cells.join(&separator),
        border
    );
}

/// Align row cells before borders or colors are applied.
fn format_aligned_cells(
    headers: &[&str],
    widths: &[usize],
    cells: &[String],
    number_format: NumberFormat,
) -> Vec<String> {
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let display = format_table_cell(headers, index, cell, number_format);
            let formatted = if index <= text_column_limit(headers)
                || headers[index] == "Directory"
                || headers[index] == "Session"
                || headers[index] == "Model"
                || headers[index] == "Last Activity"
            {
                format!("{display:width$}", width = widths[index])
            } else {
                format!("{display:>width$}", width = widths[index])
            };
            format!(" {formatted} ")
        })
        .collect()
}

/// Format one table cell according to its column semantics.
fn format_table_cell(
    headers: &[&str],
    index: usize,
    cell: &str,
    number_format: NumberFormat,
) -> String {
    if is_token_column(headers[index]) {
        cell.parse::<u64>().map_or_else(
            |_| cell.to_string(),
            |value| format_u64_with(value, number_format),
        )
    } else {
        cell.to_string()
    }
}

/// Return whether the table column contains token counts.
fn is_token_column(header: &str) -> bool {
    matches!(header, "Input" | "Cache" | "Output" | "Reasoning" | "Total")
}

/// One row-rule kind for the table frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TableRuleKind {
    /// Top border.
    Top,
    /// Header separator.
    HeaderSeparator,
    /// Group separator between report sections.
    GroupSeparator,
    /// Bottom border.
    Bottom,
}

/// Static characters used to draw a table.
#[derive(Clone, Copy, Debug)]
struct BorderTheme {
    /// Horizontal line segment.
    horizontal: char,
    /// Outer vertical border.
    vertical: char,
    /// Left corner/junction.
    left: char,
    /// Inner junction.
    middle: char,
    /// Right corner/junction.
    right: char,
}

/// Return the concrete drawing theme for one border style.
fn border_theme(style: BorderStyle) -> BorderTheme {
    match style {
        BorderStyle::Ascii => BorderTheme {
            horizontal: '-',
            vertical: '|',
            left: '+',
            middle: '+',
            right: '+',
        },
        BorderStyle::Unicode => BorderTheme {
            horizontal: '─',
            vertical: '│',
            left: '┌',
            middle: '┬',
            right: '┐',
        },
    }
}

/// Return the concrete drawing theme for one border rule.
fn rule_theme(kind: TableRuleKind, style: BorderStyle) -> BorderTheme {
    match (style, kind) {
        (BorderStyle::Ascii, _) => border_theme(style),
        (BorderStyle::Unicode, TableRuleKind::Top) => BorderTheme {
            horizontal: '─',
            vertical: '│',
            left: '┌',
            middle: '┬',
            right: '┐',
        },
        (BorderStyle::Unicode, TableRuleKind::HeaderSeparator | TableRuleKind::GroupSeparator) => {
            BorderTheme {
                horizontal: '─',
                vertical: '│',
                left: '├',
                middle: '┼',
                right: '┤',
            }
        }
        (BorderStyle::Unicode, TableRuleKind::Bottom) => BorderTheme {
            horizontal: '─',
            vertical: '│',
            left: '└',
            middle: '┴',
            right: '┘',
        },
    }
}

/// Build one table border rule line.
fn table_rule(kind: TableRuleKind, borders: BorderStyle, widths: &[usize]) -> String {
    let theme = rule_theme(kind, borders);
    let segments = widths
        .iter()
        .map(|width| theme.horizontal.to_string().repeat(width + 2))
        .collect::<Vec<_>>();
    format!(
        "{}{}{}",
        theme.left,
        segments.join(&theme.middle.to_string()),
        theme.right
    )
}

/// Write one border rule with styling.
fn write_table_rule(output: &mut String, style: TableStyle, element: TableElement, line: &str) {
    let _ = writeln!(output, "{}", paint(style, element, line));
}

/// How many leading columns should left-align in this table.
fn text_column_limit(headers: &[&str]) -> usize {
    if headers.first() == Some(&"Directory") {
        2
    } else {
        1
    }
}

/// One styleable table element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TableElement {
    /// Report title.
    Title,
    /// Header row.
    Header,
    /// Separator row.
    Border,
    /// Group subtotal row.
    Subtotal,
    /// Model detail row.
    Detail,
    /// Grand total row.
    GrandTotal,
}

/// Map one display row kind to its table element style.
fn row_table_element(kind: DisplayRowKind) -> TableElement {
    match kind {
        DisplayRowKind::Subtotal => TableElement::Subtotal,
        DisplayRowKind::Spacer | DisplayRowKind::Detail => TableElement::Detail,
        DisplayRowKind::GrandTotal => TableElement::GrandTotal,
    }
}

/// Map one rule kind to the border styling bucket.
fn table_rule_element(kind: TableRuleKind) -> TableElement {
    match kind {
        TableRuleKind::Top
        | TableRuleKind::HeaderSeparator
        | TableRuleKind::GroupSeparator
        | TableRuleKind::Bottom => TableElement::Border,
    }
}

/// Apply ANSI styling when enabled.
fn paint(style: TableStyle, element: TableElement, text: &str) -> String {
    match style {
        TableStyle::Plain => text.to_string(),
        TableStyle::Ansi256 => {
            let sequence = match element {
                TableElement::Title => "\u{1b}[1;38;5;81m",
                TableElement::Header => "\u{1b}[1;38;5;45m",
                TableElement::Border => "\u{1b}[38;5;24m",
                TableElement::Subtotal => "\u{1b}[1;38;5;117m",
                TableElement::Detail => "\u{1b}[38;5;153m",
                TableElement::GrandTotal => "\u{1b}[1;38;5;39m",
            };
            format!("{sequence}{text}\u{1b}[0m")
        }
    }
}

/// Format an integer with group separators.
fn format_u64(value: u64) -> String {
    let raw = value.to_string();
    let mut output = String::new();
    let chars = raw.chars().rev().collect::<Vec<_>>();
    for (index, character) in chars.iter().enumerate() {
        if index > 0 && index % 3 == 0 {
            output.push(',');
        }
        output.push(*character);
    }
    output.chars().rev().collect()
}

/// Format an integer according to the selected display mode.
fn format_u64_with(value: u64, number_format: NumberFormat) -> String {
    match number_format {
        NumberFormat::Full => format_u64(value),
        NumberFormat::Short => format_u64_short(value),
    }
}

/// Format an integer using 3 significant digits and K/M/B/T suffixes.
fn format_u64_short(value: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1_000, "K"),
        (1_000_000, "M"),
        (1_000_000_000, "B"),
        (1_000_000_000_000, "T"),
    ];

    if value < UNITS[0].0 {
        return value.to_string();
    }

    let mut unit_index = UNITS
        .iter()
        .enumerate()
        .rfind(|(_index, (divisor, _suffix))| value >= *divisor)
        .map_or(0, |(index, _unit)| index);

    loop {
        let (divisor, suffix) = UNITS[unit_index];
        let whole = value / divisor;
        let decimals: u32 = if whole >= 100 {
            0
        } else if whole >= 10 {
            1
        } else {
            2
        };
        let multiplier = 10_u128.pow(decimals);
        let rounded_units =
            ((u128::from(value) * multiplier) + (u128::from(divisor) / 2)) / u128::from(divisor);

        if rounded_units >= 1_000 * multiplier && unit_index + 1 < UNITS.len() {
            unit_index += 1;
            continue;
        }

        return format_short_with_suffix(rounded_units, decimals, suffix);
    }
}

/// Format a rounded abbreviated value and trim redundant trailing zeros.
fn format_short_with_suffix(value: u128, decimals: u32, suffix: &str) -> String {
    if decimals == 0 {
        return format!("{value}{suffix}");
    }

    let divisor = 10_u128.pow(decimals);
    let integer = value / divisor;
    let fractional = value % divisor;
    let fractional_width = usize::try_from(decimals).expect("decimal width fits usize");
    let mut number = format!("{integer}.{fractional:0fractional_width$}");
    while number.ends_with('0') {
        number.pop();
    }
    if number.ends_with('.') {
        number.pop();
    }
    format!("{number}{suffix}")
}

/// Format USD with two decimal places.
fn format_currency(value: f64) -> String {
    format!("${value:.2}")
}

/// Scan the provided directories and feed all events into the report builder.
///
/// Duplicate relative session identifiers across roots intentionally collapse to one selected
/// file. The user-defined contract is that the session identifier itself is globally unique, and
/// a longer duplicate file represents a newer version of the same session.
fn scan_session_dirs(session_dirs: &[PathBuf], builder: &mut ReportBuilder) -> Result<Vec<String>> {
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
    for session_id in session_ids {
        let target = selected_files
            .remove(&session_id)
            .ok_or_else(|| eyre!("missing session target for key {session_id}"))?;
        scan_session_file(&target.path, &target.session_id, builder)?;
    }
    Ok(missing_directories)
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
    let session_id = session_file_id(root, &path);
    let candidate = SessionScanTarget {
        session_id: session_id.clone(),
        path,
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
    };
    let should_replace = selected_files
        .get(&session_id)
        .is_none_or(|existing| candidate.is_preferred_over(existing));
    if should_replace {
        selected_files.insert(session_id, candidate);
    }
    Ok(())
}

/// Scan one JSONL session file.
fn scan_session_file(file: &Path, session_id: &str, builder: &mut ReportBuilder) -> Result<()> {
    let session_key = session_id.to_string();
    let reader = BufReader::new(File::open(file)?);
    let mut line = String::new();
    let mut previous_totals: Option<RawUsage> = None;
    let mut current_model: Option<String> = None;
    let mut current_model_is_fallback = false;
    let mut reader = reader;
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(event) = parse_token_usage_event(
            &entry,
            &session_key,
            session_id,
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )? {
            builder.observe(&event);
        }
    }
    Ok(())
}

/// Derive the stable session identifier from a JSONL path.
fn session_file_id(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .trim_end_matches(".jsonl")
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Parse one JSONL entry into a token-usage event when applicable.
fn parse_token_usage_event(
    entry: &Value,
    session_key: &str,
    session_id: &str,
    previous_totals: &mut Option<RawUsage>,
    current_model: &mut Option<String>,
    current_model_is_fallback: &mut bool,
) -> Result<Option<TokenUsageEvent>> {
    let Some(entry_type) = entry.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };
    if entry_type == "turn_context" {
        if let Some(model) = entry.get("payload").and_then(extract_model) {
            *current_model = Some(model);
            *current_model_is_fallback = false;
        }
        return Ok(None);
    }
    if entry_type != "event_msg" {
        return Ok(None);
    }
    let Some(payload) = entry.get("payload") else {
        return Ok(None);
    };
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return Ok(None);
    }
    let Some(timestamp) = entry.get("timestamp").and_then(Value::as_str) else {
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
        session_key: session_key.to_string(),
        session_id: session_id.to_string(),
        timestamp_utc,
        model,
        is_fallback_model,
        usage,
    }))
}

/// Extract normalized usage from one token-count payload.
fn extract_event_usage(
    payload: &Value,
    previous_totals: &mut Option<RawUsage>,
) -> Option<UsageTotals> {
    let info = payload.get("info").unwrap_or(payload);
    let last_usage = info.get("last_token_usage").and_then(normalize_usage);
    let total_usage = info.get("total_token_usage").and_then(normalize_usage);
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
fn resolve_event_model(
    payload: &Value,
    current_model: &mut Option<String>,
    current_model_is_fallback: &mut bool,
) -> (String, bool) {
    let info = payload.get("info").unwrap_or(payload);
    let extracted_model = extract_model(payload).or_else(|| extract_model(info));
    if let Some(model) = extracted_model.clone() {
        *current_model = Some(model);
        *current_model_is_fallback = false;
    }
    match extracted_model.or_else(|| current_model.clone()) {
        Some(model) if *current_model_is_fallback => (model, true),
        Some(model) => (model, false),
        None => {
            *current_model = Some(DEFAULT_FALLBACK_MODEL.to_string());
            *current_model_is_fallback = true;
            (DEFAULT_FALLBACK_MODEL.to_string(), true)
        }
    }
}

/// Extract a model name from a JSON value.
fn extract_model(value: &Value) -> Option<String> {
    let info = value.get("info").unwrap_or(value);
    let direct = [
        info.get("model"),
        info.get("model_name"),
        value.get("model"),
        value.get("model_name"),
    ];
    for candidate in direct {
        if let Some(model) = candidate.and_then(Value::as_str).map(str::trim)
            && !model.is_empty()
        {
            return Some(model.to_string());
        }
    }
    for parent in [info, value] {
        if let Some(model) = parent
            .get("metadata")
            .and_then(|metadata| metadata.get("model"))
            .and_then(Value::as_str)
            .map(str::trim)
            && !model.is_empty()
        {
            return Some(model.to_string());
        }
    }
    None
}

/// Normalize usage payloads into a single shape.
fn normalize_usage(value: &Value) -> Option<RawUsage> {
    let object = value.as_object()?;
    let input = object
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_input = object
        .get("cached_input_tokens")
        .or_else(|| object.get("cache_read_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = object
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_output = object
        .get("reasoning_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = object
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(RawUsage {
        input,
        cached_input,
        output,
        reasoning_output,
        total,
    })
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
#[allow(
    clippy::cast_precision_loss,
    reason = "Codex token counters are orders of magnitude below f64 precision limits"
)]
fn calculate_cost(usage: &ModelBreakdown, pricing: &Pricing) -> f64 {
    calculate_cost_from_usage(
        &UsageTotals {
            input: usage.input_tokens,
            cached_input: usage.cached_input_tokens,
            output: usage.output_tokens,
            reasoning_output: usage.reasoning_output_tokens,
            total: usage.total_tokens,
        },
        pricing,
    )
}

/// Price one usage total entry.
#[allow(
    clippy::cast_precision_loss,
    reason = "Codex token counters are orders of magnitude below f64 precision limits"
)]
fn calculate_cost_from_usage(usage: &UsageTotals, pricing: &Pricing) -> f64 {
    let cached_input = usage.cached_input.min(usage.input);
    let non_cached_input = usage.input.saturating_sub(cached_input);
    let input_cost = (non_cached_input as f64 / MILLION) * pricing.input_cost_per_mtoken;
    let cached_cost = (cached_input as f64 / MILLION) * pricing.cached_input_cost_per_mtoken;
    let output_cost = (usage.output as f64 / MILLION) * pricing.output_cost_per_mtoken;
    input_cost + cached_cost + output_cost
}

#[cfg(test)]
#[allow(clippy::missing_docs_in_private_items)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        let payload = serde_json::json!({
            "info": {
                "metadata": {
                    "model": "gpt-5"
                }
            }
        });
        assert_eq!(extract_model(&payload).as_deref(), Some("gpt-5"));
    }

    #[test]
    fn normalize_usage_reads_cache_alias() {
        let usage = normalize_usage(&serde_json::json!({
            "input_tokens": 100,
            "cache_read_input_tokens": 25,
            "output_tokens": 20,
            "reasoning_output_tokens": 5,
        }))
        .expect("usage");
        assert_eq!(usage.input, 100);
        assert_eq!(usage.cached_input, 25);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.reasoning_output, 5);
        assert_eq!(usage.total, 0);
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
        assert_eq!(format_u64_with(1_000_000_000_000, NumberFormat::Short), "1T");
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
        let cost = calculate_cost(&usage, &pricing);
        let expected = (800.0 / 1_000_000.0) * 1.25
            + (200.0 / 1_000_000.0) * 0.125
            + (500.0 / 1_000_000.0) * 10.0;
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

        let daily_render = render_report(&daily, "en-US", NumberFormat::Short);
        let session_render = render_report(&session, "en-US", NumberFormat::Short);
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

        let rendered = render_report(&monthly, "en-US", NumberFormat::Short);
        assert!(rendered.contains("Monthly Codex Usage Report"));
        assert!(rendered.contains("2025-09"));
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

        let rendered = render_report(&daily, "en-US", NumberFormat::Short);
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

        let rendered = render_report(&session, "en-US", NumberFormat::Short);
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

        let rendered = render_report(&report, "en-US", NumberFormat::Short);
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

        let rendered = render_report(&daily, "en-US", NumberFormat::Short);
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

        let short = render_report(&daily, "en-US", NumberFormat::Short);
        let full = render_report(&daily, "en-US", NumberFormat::Full);

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

        let rendered = render_report(&daily, "en-US", NumberFormat::Full);
        let lines = rendered.lines().collect::<Vec<_>>();
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
            .finish(&PricingCatalog::default(), Vec::new())
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
                timezone: "UTC".to_string(),
                locale: "en-US".to_string(),
                number_format: NumberFormat::Short,
                json: true,
                offline: true,
                refresh_pricing: false,
                session_dirs: vec![first_sessions, second_sessions],
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
}
