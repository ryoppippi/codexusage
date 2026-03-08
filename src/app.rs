//! CLI orchestration and report building.

use crate::pricing::{Pricing, PricingCatalog, PricingLoadOptions, load_pricing_catalog};
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use clap::{Parser, Subcommand, ValueEnum};
use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Human-readable table rendering helpers.
#[path = "app/render.rs"]
mod render;

use render::{explicit_usage, render_report};

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
fn push_event_into_summary(summary: &mut GroupSummary, event: &TokenUsageEvent<'_, '_>) {
    summary.totals.add(&event.usage);
    let breakdown = ensure_model_breakdown(&mut summary.models, event.model);
    push_usage_into_breakdown(breakdown, &event.usage, event.is_fallback_model);
    if event.is_fallback_model {
        breakdown.is_fallback = true;
    }
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
        if !line_might_affect_usage(trimmed) {
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
            builder.observe(&event);
        }
    }
    Ok(())
}

/// Return whether one JSONL line might affect usage aggregation.
fn line_might_affect_usage(line: &str) -> bool {
    // Fail open for escaped JSON strings because relevant event types may be unicode-escaped.
    line.contains("\\u") || line.contains("turn_context") || line.contains("token_count")
}

/// Derive the stable session identifier from a JSONL path.
fn session_file_id(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .trim_end_matches(".jsonl")
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
