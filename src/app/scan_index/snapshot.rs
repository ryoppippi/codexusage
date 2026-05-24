//! Bulk-loaded scan-index rows for one report run.

use super::AggregateLoad;
use super::aggregates::{FileAggregateSet, load_aggregate_rows, load_aggregate_rows_for_keys};
use super::records::{StoredFileRecord, load_file_records, load_file_records_for_keys};
use chrono_tz::Tz;
use eyre::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

/// Above this many selected keys, a full snapshot is cheaper than many dynamic `IN` chunks.
const FULL_SNAPSHOT_KEY_THRESHOLD: usize = 4_096;

/// Scan-index rows needed to plan one report without per-file `SQLite` lookups.
pub(super) struct ScanIndexSnapshot {
    /// File rows keyed by session key.
    records: HashMap<String, StoredFileRecord>,
    /// Aggregate rows keyed by session key and file generation.
    aggregates: HashMap<(String, i64), FileAggregateSet>,
    /// Aggregate keys with malformed content.
    invalid_aggregates: HashSet<(String, i64)>,
}

impl ScanIndexSnapshot {
    /// Load file rows and aggregate rows for the selected session keys only.
    pub(super) fn load_selected(
        connection: &Connection,
        timezone: Tz,
        session_keys: &[&str],
    ) -> Result<Self> {
        if session_keys.len() > FULL_SNAPSHOT_KEY_THRESHOLD {
            return Ok(Self::from_parts(
                load_file_records(connection)?,
                load_aggregate_rows(connection, timezone)?,
            ));
        }

        Ok(Self::from_parts(
            load_file_records_for_keys(connection, session_keys)?,
            load_aggregate_rows_for_keys(connection, timezone, session_keys)?,
        ))
    }

    /// Build a snapshot from loaded row groups.
    fn from_parts(
        records: HashMap<String, StoredFileRecord>,
        aggregate_rows: super::aggregates::LoadedAggregateRows,
    ) -> Self {
        Self {
            records,
            aggregates: aggregate_rows.rows,
            invalid_aggregates: aggregate_rows.invalid,
        }
    }

    /// Remove and return one file record.
    pub(super) fn take_record(&mut self, session_key: &str) -> Option<StoredFileRecord> {
        self.records.remove(session_key)
    }

    /// Remove, validate, and return aggregate rows for one file record.
    pub(super) fn take_aggregates(&mut self, record: &StoredFileRecord) -> AggregateLoad {
        let key = (record.session_key.clone(), record.generation);
        if self.invalid_aggregates.remove(&key) {
            return AggregateLoad::MissingOrInvalid;
        }
        let aggregates = self.aggregates.remove(&key).unwrap_or_default();
        if !aggregates.matches_expected_totals(&record.total, &record.fallback_total) {
            return AggregateLoad::MissingOrInvalid;
        }
        AggregateLoad::Valid(aggregates)
    }
}
