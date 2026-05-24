//! Per-file aggregate rows for the scan index.

use super::{u64_to_i64, usage_from_i64};
use crate::app::model::{ModelBreakdown, UsageTotals};
use crate::app::report::{
    GroupSummary, ReportBuilder, SessionScanTarget, breakdown_usage, push_usage_into_breakdown,
};
use crate::app::session_log::TokenUsageEvent;
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use eyre::{Result, WrapErr, eyre};
use rusqlite::{Connection, Transaction, params, params_from_iter};
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};

/// Aggregate grouping kind used by persisted local-day rows.
pub(super) const GROUP_KIND_DAY: &str = "day";

/// Aggregate rows for a single scanned file.
#[derive(Clone, Default)]
pub(super) struct FileAggregateSet {
    /// Local-day aggregates.
    days: BTreeMap<NaiveDate, FileDayAggregate>,
}

impl FileAggregateSet {
    /// Observe one usage event.
    pub(super) fn observe(&mut self, event: &TokenUsageEvent<'_, '_>, timezone: Tz) {
        let local_day = event.timestamp_utc.with_timezone(&timezone).date_naive();
        let day = self.days.entry(local_day).or_default();
        day.observe(event);
    }

    /// Merge another aggregate set into this set.
    pub(super) fn merge(&mut self, other: Self) {
        for (day, source) in other.days {
            self.days.entry(day).or_default().merge(source);
        }
    }

    /// Merge this file's aggregates into a report builder.
    pub(super) fn merge_into_report_builder(
        &self,
        builder: &mut ReportBuilder,
        target: &SessionScanTarget,
    ) {
        for (local_day, aggregate) in &self.days {
            let Some((summary, last_activity)) = aggregate.to_group_summary() else {
                continue;
            };
            builder.merge_file_day_summary(
                &target.session_id,
                &target.session_id,
                *local_day,
                summary,
                last_activity,
            );
        }
    }

    /// Insert one raw `SQLite` aggregate row, returning an error when the row is invalid.
    pub(super) fn insert_raw_row(&mut self, row: RawAggregateRow) -> Result<()> {
        let local_day = NaiveDate::parse_from_str(&row.group_key, "%Y-%m-%d")
            .wrap_err_with(|| format!("invalid scan-index date {}", row.group_key))?;
        let usage = usage_from_i64(
            row.input_tokens,
            row.cached_input_tokens,
            row.output_tokens,
            row.reasoning_output_tokens,
            row.total_tokens,
        )
        .ok_or_else(|| eyre!("invalid negative token counter in scan index"))?;
        let fallback_usage = usage_from_i64(
            row.fallback_input_tokens,
            row.fallback_cached_input_tokens,
            row.fallback_output_tokens,
            row.fallback_reasoning_output_tokens,
            row.fallback_total_tokens,
        )
        .ok_or_else(|| eyre!("invalid negative fallback token counter in scan index"))?;
        let is_fallback = fallback_usage.has_usage();
        let last_activity = datetime_from_timestamp_nanos(row.last_activity_nanos)?;
        let mut breakdown = ModelBreakdown {
            input_tokens: usage.input,
            cached_input_tokens: usage.cached_input,
            output_tokens: usage.output,
            reasoning_output_tokens: usage.reasoning_output,
            total_tokens: usage.total,
            fallback_usage,
            is_fallback,
            ..ModelBreakdown::default()
        };
        if breakdown.fallback_usage.has_usage() {
            breakdown.is_fallback = true;
        }
        self.days.entry(local_day).or_default().models.insert(
            row.model,
            FileModelAggregate {
                breakdown,
                last_activity,
            },
        );
        Ok(())
    }

    /// Return whether aggregate totals match stored file-level totals.
    pub(super) fn matches_expected_totals(
        &self,
        total: &UsageTotals,
        fallback_total: &UsageTotals,
    ) -> bool {
        self.total_usage() == *total && self.fallback_usage() == *fallback_total
    }

    /// Return full-file totals.
    pub(super) fn total_usage(&self) -> UsageTotals {
        let mut total = UsageTotals::default();
        for day in self.days.values() {
            total.add(&day.total_usage());
        }
        total
    }

    /// Return full-file fallback totals.
    pub(super) fn fallback_usage(&self) -> UsageTotals {
        let mut total = UsageTotals::default();
        for day in self.days.values() {
            total.add(&day.fallback_usage());
        }
        total
    }
}

/// One local day's aggregates for a file.
#[derive(Clone, Default)]
struct FileDayAggregate {
    /// Per-model aggregate rows.
    models: BTreeMap<String, FileModelAggregate>,
}

impl FileDayAggregate {
    /// Observe one usage event.
    fn observe(&mut self, event: &TokenUsageEvent<'_, '_>) {
        let model = self.models.entry(event.model.to_string()).or_default();
        push_usage_into_breakdown(&mut model.breakdown, &event.usage, event.is_fallback_model);
        if event.is_fallback_model {
            model.breakdown.is_fallback = true;
        }
        model.last_activity = model.last_activity.max(event.timestamp_utc);
    }

    /// Merge another day aggregate.
    fn merge(&mut self, source: Self) {
        for (model, source_model) in source.models {
            let target = self.models.entry(model).or_default();
            merge_model_aggregate(target, &source_model);
        }
    }

    /// Convert this day into a report group summary.
    fn to_group_summary(&self) -> Option<(GroupSummary, DateTime<Utc>)> {
        let mut summary = GroupSummary::default();
        let mut last_activity = None;
        for (model, aggregate) in &self.models {
            let usage = breakdown_usage(&aggregate.breakdown);
            summary.totals.add(&usage);
            summary
                .models
                .insert(model.clone(), aggregate.breakdown.clone());
            last_activity = Some(
                last_activity.map_or(aggregate.last_activity, |existing: DateTime<Utc>| {
                    existing.max(aggregate.last_activity)
                }),
            );
        }
        last_activity.map(|last_activity| (summary, last_activity))
    }

    /// Return this day's full usage totals.
    fn total_usage(&self) -> UsageTotals {
        let mut total = UsageTotals::default();
        for aggregate in self.models.values() {
            total.add(&breakdown_usage(&aggregate.breakdown));
        }
        total
    }

    /// Return this day's fallback usage totals.
    fn fallback_usage(&self) -> UsageTotals {
        let mut total = UsageTotals::default();
        for aggregate in self.models.values() {
            total.add(&aggregate.breakdown.fallback_usage);
        }
        total
    }
}

/// One per-model file aggregate.
#[derive(Clone)]
struct FileModelAggregate {
    /// Token breakdown.
    breakdown: ModelBreakdown,
    /// Latest event timestamp in this aggregate.
    last_activity: DateTime<Utc>,
}

impl Default for FileModelAggregate {
    fn default() -> Self {
        Self {
            breakdown: ModelBreakdown::default(),
            last_activity: DateTime::<Utc>::UNIX_EPOCH,
        }
    }
}

/// Merge one per-model aggregate into another.
fn merge_model_aggregate(target: &mut FileModelAggregate, source: &FileModelAggregate) {
    target.breakdown.input_tokens += source.breakdown.input_tokens;
    target.breakdown.cached_input_tokens += source.breakdown.cached_input_tokens;
    target.breakdown.output_tokens += source.breakdown.output_tokens;
    target.breakdown.reasoning_output_tokens += source.breakdown.reasoning_output_tokens;
    target.breakdown.total_tokens += source.breakdown.total_tokens;
    target
        .breakdown
        .fallback_usage
        .add(&source.breakdown.fallback_usage);
    target.breakdown.is_fallback |= source.breakdown.is_fallback;
    target.last_activity = target.last_activity.max(source.last_activity);
}

/// Raw aggregate row loaded from `SQLite`.
pub(super) struct RawAggregateRow {
    /// Local day in `%Y-%m-%d` form.
    group_key: String,
    /// Model name.
    model: String,
    /// Input tokens.
    input_tokens: i64,
    /// Cached input tokens.
    cached_input_tokens: i64,
    /// Output tokens.
    output_tokens: i64,
    /// Reasoning output tokens.
    reasoning_output_tokens: i64,
    /// Total tokens.
    total_tokens: i64,
    /// Fallback input tokens.
    fallback_input_tokens: i64,
    /// Fallback cached input tokens.
    fallback_cached_input_tokens: i64,
    /// Fallback output tokens.
    fallback_output_tokens: i64,
    /// Fallback reasoning output tokens.
    fallback_reasoning_output_tokens: i64,
    /// Fallback total tokens.
    fallback_total_tokens: i64,
    /// Latest event timestamp in nanoseconds since Unix epoch.
    last_activity_nanos: i64,
}

impl RawAggregateRow {
    /// Build one raw aggregate row from `SQLite` starting at a column offset.
    fn from_row_offset(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            group_key: row.get(offset)?,
            model: row.get(offset + 1)?,
            input_tokens: row.get(offset + 2)?,
            cached_input_tokens: row.get(offset + 3)?,
            output_tokens: row.get(offset + 4)?,
            reasoning_output_tokens: row.get(offset + 5)?,
            total_tokens: row.get(offset + 6)?,
            fallback_input_tokens: row.get(offset + 7)?,
            fallback_cached_input_tokens: row.get(offset + 8)?,
            fallback_output_tokens: row.get(offset + 9)?,
            fallback_reasoning_output_tokens: row.get(offset + 10)?,
            fallback_total_tokens: row.get(offset + 11)?,
            last_activity_nanos: row.get(offset + 12)?,
        })
    }
}

/// Aggregate rows bulk-loaded for one timezone.
pub(super) struct LoadedAggregateRows {
    /// Valid aggregate sets keyed by session and file generation.
    pub(super) rows: HashMap<(String, i64), FileAggregateSet>,
    /// Keys that contained malformed aggregate content.
    pub(super) invalid: HashSet<(String, i64)>,
}

/// Maximum number of selected keys bound into one dynamic `SQLite` query.
const QUERY_KEY_CHUNK_SIZE: usize = 900;

/// Load all aggregate rows for one timezone in a single `SQLite` scan.
pub(super) fn load_aggregate_rows(
    connection: &Connection,
    timezone: Tz,
) -> Result<LoadedAggregateRows> {
    let mut statement = connection.prepare_cached(
        "SELECT session_key, generation, group_key, model, input_tokens, cached_input_tokens, \
         output_tokens, reasoning_output_tokens, total_tokens, fallback_input_tokens, \
         fallback_cached_input_tokens, fallback_output_tokens, fallback_reasoning_output_tokens, \
         fallback_total_tokens, last_activity_nanos \
         FROM file_aggregates \
         WHERE timezone = ?1 AND group_kind = ?2",
    )?;
    let mut rows = statement.query(params![timezone.name(), GROUP_KIND_DAY])?;
    let mut loaded = LoadedAggregateRows {
        rows: HashMap::new(),
        invalid: HashSet::new(),
    };

    while let Some(row) = rows.next()? {
        let key = (row.get::<_, String>(0)?, row.get::<_, i64>(1)?);
        if loaded.invalid.contains(&key) {
            continue;
        }
        let raw = RawAggregateRow::from_row_offset(row, 2)?;
        if loaded
            .rows
            .entry(key.clone())
            .or_default()
            .insert_raw_row(raw)
            .is_err()
        {
            loaded.rows.remove(&key);
            loaded.invalid.insert(key);
        }
    }

    Ok(loaded)
}

/// Load aggregate rows for the selected session keys only.
pub(super) fn load_aggregate_rows_for_keys(
    connection: &Connection,
    timezone: Tz,
    session_keys: &[&str],
) -> Result<LoadedAggregateRows> {
    let mut loaded = LoadedAggregateRows {
        rows: HashMap::new(),
        invalid: HashSet::new(),
    };
    if session_keys.is_empty() {
        return Ok(loaded);
    }

    for chunk in session_keys.chunks(QUERY_KEY_CHUNK_SIZE) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "SELECT session_key, generation, group_key, model, input_tokens, cached_input_tokens, \
             output_tokens, reasoning_output_tokens, total_tokens, fallback_input_tokens, \
             fallback_cached_input_tokens, fallback_output_tokens, fallback_reasoning_output_tokens, \
             fallback_total_tokens, last_activity_nanos \
             FROM file_aggregates \
             WHERE timezone = ? AND group_kind = ? AND session_key IN ({placeholders})"
        );
        let mut statement = connection.prepare(&query)?;
        let params = std::iter::once(timezone.name())
            .chain(std::iter::once(GROUP_KIND_DAY))
            .chain(chunk.iter().copied());
        let mut rows = statement.query(params_from_iter(params))?;
        load_aggregate_query_rows(&mut rows, &mut loaded)?;
    }

    Ok(loaded)
}

/// Load aggregate rows from one prepared query into the aggregate snapshot.
fn load_aggregate_query_rows(
    rows: &mut rusqlite::Rows<'_>,
    loaded: &mut LoadedAggregateRows,
) -> Result<()> {
    while let Some(row) = rows.next()? {
        let key = (row.get::<_, String>(0)?, row.get::<_, i64>(1)?);
        if loaded.invalid.contains(&key) {
            continue;
        }
        let raw = RawAggregateRow::from_row_offset(row, 2)?;
        if loaded
            .rows
            .entry(key.clone())
            .or_default()
            .insert_raw_row(raw)
            .is_err()
        {
            loaded.rows.remove(&key);
            loaded.invalid.insert(key);
        }
    }

    Ok(())
}

/// Replace aggregate rows for one file generation.
pub(super) fn replace_aggregate_rows(
    transaction: &Transaction<'_>,
    session_key: &str,
    timezone: Tz,
    generation: i64,
    content_changed: bool,
    aggregates: &FileAggregateSet,
) -> Result<()> {
    if content_changed {
        transaction.execute(
            "DELETE FROM file_aggregates WHERE session_key = ?1",
            params![session_key],
        )?;
    } else {
        transaction.execute(
            "DELETE FROM file_aggregates
             WHERE session_key = ?1 AND timezone = ?2 AND generation = ?3",
            params![session_key, timezone.name(), generation],
        )?;
    }

    let mut statement = transaction.prepare(
        "INSERT INTO file_aggregates (
             session_key, timezone, generation, group_kind, group_key, model,
             input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens,
             total_tokens, fallback_input_tokens, fallback_cached_input_tokens,
             fallback_output_tokens, fallback_reasoning_output_tokens, fallback_total_tokens,
             last_activity_nanos
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
         )",
    )?;
    for (day, aggregate) in &aggregates.days {
        for (model, model_aggregate) in &aggregate.models {
            let usage = breakdown_usage(&model_aggregate.breakdown);
            let fallback = &model_aggregate.breakdown.fallback_usage;
            statement.execute(params![
                session_key,
                timezone.name(),
                generation,
                GROUP_KIND_DAY,
                day.format("%Y-%m-%d").to_string(),
                model,
                u64_to_i64(usage.input)?,
                u64_to_i64(usage.cached_input)?,
                u64_to_i64(usage.output)?,
                u64_to_i64(usage.reasoning_output)?,
                u64_to_i64(usage.total)?,
                u64_to_i64(fallback.input)?,
                u64_to_i64(fallback.cached_input)?,
                u64_to_i64(fallback.output)?,
                u64_to_i64(fallback.reasoning_output)?,
                u64_to_i64(fallback.total)?,
                timestamp_nanos(model_aggregate.last_activity)?,
            ])?;
        }
    }
    Ok(())
}

/// Convert a `DateTime` into a nanosecond timestamp for cache storage.
fn timestamp_nanos(value: DateTime<Utc>) -> Result<i64> {
    value
        .timestamp_nanos_opt()
        .ok_or_else(|| eyre!("timestamp is outside the supported nanosecond range"))
}

/// Convert a nanosecond timestamp from cache storage into `DateTime`.
fn datetime_from_timestamp_nanos(value: i64) -> Result<DateTime<Utc>> {
    let seconds = value.div_euclid(1_000_000_000);
    let nanos = value.rem_euclid(1_000_000_000);
    let nanos = u32::try_from(nanos).wrap_err("invalid scan-index timestamp nanoseconds")?;
    DateTime::<Utc>::from_timestamp(seconds, nanos)
        .ok_or_else(|| eyre!("invalid scan-index timestamp"))
}
