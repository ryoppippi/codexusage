//! Persisted and scanned file records for the scan index.

use super::aggregates::FileAggregateSet;
use super::metadata::{ContentHash, FileMetadata, ObservedFile};
use super::{bool_to_i64, i64_to_u64, raw_usage_from_options, u64_to_i64, usage_from_i64};
use crate::app::model::UsageTotals;
use crate::app::report::SessionScanTarget;
use crate::app::session_files::SessionFileFormat;
use crate::app::session_log::{ReplayState, SessionParseCheckpoint};
use eyre::Result;
use rusqlite::{Connection, Transaction, params, params_from_iter};
use std::collections::HashMap;

/// Parser-state version stored with checkpoints.
const PARSER_VERSION: i64 = 2;

/// Full result of parsing one selected file.
#[derive(Clone)]
pub(super) struct ScannedFile {
    /// Selected file target.
    pub(super) target: SessionScanTarget,
    /// Aggregates produced by this scan.
    pub(super) aggregates: FileAggregateSet,
    /// Cache metadata prepared for persistence.
    pub(super) cache_entry: Option<ScannedCacheEntry>,
}

impl ScannedFile {
    /// Return this scan result with replacement aggregates.
    pub(super) fn with_aggregates(self, aggregates: FileAggregateSet) -> Self {
        Self { aggregates, ..self }
    }
}

/// Cache metadata for a scanned file.
#[derive(Clone)]
pub(super) struct ScannedCacheEntry {
    /// Stable session key.
    pub(super) session_key: String,
    /// Canonicalized path string.
    path: String,
    /// Metadata stamp for the parsed prefix.
    metadata: FileMetadata,
    /// Parser checkpoint after scanning.
    checkpoint: SessionParseCheckpoint,
    /// Hash of bytes up to the checkpoint offset.
    content_hash: ContentHash,
}

impl ScannedCacheEntry {
    /// Build cache metadata from one scan result.
    pub(super) fn from_scan(
        target: &SessionScanTarget,
        checkpoint: &SessionParseCheckpoint,
        content_hash: ContentHash,
    ) -> Self {
        let observed = ObservedFile::from_target(target);
        let mut metadata = observed.metadata;
        metadata.size = checkpoint.offset;
        Self {
            session_key: target.session_id.clone(),
            path: observed.path_key,
            metadata,
            checkpoint: checkpoint.clone(),
            content_hash,
        }
    }
}

/// Persisted file record loaded from `SQLite`.
#[derive(Clone)]
pub(super) struct StoredFileRecord {
    /// Stable session key.
    pub(super) session_key: String,
    /// Canonicalized path string.
    pub(super) path: String,
    /// File generation for aggregate rows.
    pub(super) generation: i64,
    /// Parser schema version.
    parser_version: i64,
    /// Metadata stamp for the parsed prefix.
    pub(super) metadata: FileMetadata,
    /// Parser checkpoint.
    pub(super) checkpoint: SessionParseCheckpoint,
    /// Hash of bytes up to the checkpoint offset.
    pub(super) content_hash: ContentHash,
    /// Full-file usage totals.
    pub(super) total: UsageTotals,
    /// Full-file fallback usage totals.
    pub(super) fallback_total: UsageTotals,
}

impl StoredFileRecord {
    /// Return whether this record can apply to the observed target path and parser version.
    pub(super) fn is_compatible_with(&self, observed: &ObservedFile) -> bool {
        self.parser_version == PARSER_VERSION && self.path == observed.path_key
    }

    /// Return whether the observed file can be considered for append validation.
    pub(super) fn can_append_to(&self, observed: &ObservedFile) -> bool {
        self.metadata.file_format == SessionFileFormat::Plain
            && observed.file_format == SessionFileFormat::Plain
            && observed.metadata.size > self.metadata.size
            && self.checkpoint.offset <= self.metadata.size
            && self.metadata.same_identity_as(&observed.metadata)
    }
}

/// Raw file row before lossy validation.
pub(super) struct RawStoredFileRecord {
    /// Stable session key.
    session_key: String,
    /// Canonicalized path string.
    path: String,
    /// File generation.
    generation: i64,
    /// Parser schema version.
    parser_version: i64,
    /// Physical file representation.
    file_format: String,
    /// Indexed prefix size.
    size: i64,
    /// Modification time in nanoseconds since Unix epoch.
    mtime_ns: Option<i64>,
    /// Unix device identifier.
    dev: Option<i64>,
    /// Unix inode identifier.
    ino: Option<i64>,
    /// Unix ctime in nanoseconds since Unix epoch.
    ctime_ns: Option<i64>,
    /// Parser checkpoint offset.
    checkpoint_offset: i64,
    /// Previous cumulative input tokens.
    previous_input: Option<i64>,
    /// Previous cumulative cached input tokens.
    previous_cached_input: Option<i64>,
    /// Previous cumulative output tokens.
    previous_output: Option<i64>,
    /// Previous cumulative reasoning output tokens.
    previous_reasoning_output: Option<i64>,
    /// Previous cumulative total tokens.
    previous_total: Option<i64>,
    /// Last resolved model.
    current_model: Option<String>,
    /// Whether the current model came from fallback inference.
    current_model_is_fallback: i64,
    /// Inherited-history handling state.
    replay_state: String,
    /// Encoded hash of bytes up to the checkpoint offset.
    content_hash: String,
    /// Full-file input tokens.
    total_input: i64,
    /// Full-file cached input tokens.
    total_cached_input: i64,
    /// Full-file output tokens.
    total_output: i64,
    /// Full-file reasoning output tokens.
    total_reasoning_output: i64,
    /// Full-file total tokens.
    total_tokens: i64,
    /// Full-file fallback input tokens.
    fallback_input: i64,
    /// Full-file fallback cached input tokens.
    fallback_cached_input: i64,
    /// Full-file fallback output tokens.
    fallback_output: i64,
    /// Full-file fallback reasoning output tokens.
    fallback_reasoning_output: i64,
    /// Full-file fallback total tokens.
    fallback_total: i64,
}

impl RawStoredFileRecord {
    /// Build a raw file row from `SQLite`.
    pub(super) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            session_key: row.get(0)?,
            path: row.get(1)?,
            generation: row.get(2)?,
            parser_version: row.get(3)?,
            file_format: row.get(4)?,
            size: row.get(5)?,
            mtime_ns: row.get(6)?,
            dev: row.get(7)?,
            ino: row.get(8)?,
            ctime_ns: row.get(9)?,
            checkpoint_offset: row.get(10)?,
            previous_input: row.get(11)?,
            previous_cached_input: row.get(12)?,
            previous_output: row.get(13)?,
            previous_reasoning_output: row.get(14)?,
            previous_total: row.get(15)?,
            current_model: row.get(16)?,
            current_model_is_fallback: row.get(17)?,
            replay_state: row.get(18)?,
            content_hash: row.get(19)?,
            total_input: row.get(20)?,
            total_cached_input: row.get(21)?,
            total_output: row.get(22)?,
            total_reasoning_output: row.get(23)?,
            total_tokens: row.get(24)?,
            fallback_input: row.get(25)?,
            fallback_cached_input: row.get(26)?,
            fallback_output: row.get(27)?,
            fallback_reasoning_output: row.get(28)?,
            fallback_total: row.get(29)?,
        })
    }

    /// Convert a raw row into a valid stored record.
    pub(super) fn into_valid_record(self) -> Option<StoredFileRecord> {
        let checkpoint_offset = i64_to_u64(self.checkpoint_offset)?;
        let metadata = FileMetadata {
            file_format: SessionFileFormat::from_str(&self.file_format)?,
            size: i64_to_u64(self.size)?,
            mtime_ns: self.mtime_ns,
            dev: self.dev,
            ino: self.ino,
            ctime_ns: self.ctime_ns,
        };
        if checkpoint_offset != metadata.size {
            return None;
        }
        let previous_totals = raw_usage_from_options(
            self.previous_input,
            self.previous_cached_input,
            self.previous_output,
            self.previous_reasoning_output,
            self.previous_total,
        )
        .ok()?;
        Some(StoredFileRecord {
            session_key: self.session_key,
            path: self.path,
            generation: (self.generation > 0).then_some(self.generation)?,
            parser_version: self.parser_version,
            metadata,
            checkpoint: SessionParseCheckpoint {
                offset: checkpoint_offset,
                previous_totals,
                current_model: self.current_model,
                current_model_is_fallback: self.current_model_is_fallback != 0,
                replay_state: ReplayState::from_str(&self.replay_state)?,
            },
            content_hash: ContentHash::decode(&self.content_hash)?,
            total: usage_from_i64(
                self.total_input,
                self.total_cached_input,
                self.total_output,
                self.total_reasoning_output,
                self.total_tokens,
            )?,
            fallback_total: usage_from_i64(
                self.fallback_input,
                self.fallback_cached_input,
                self.fallback_output,
                self.fallback_reasoning_output,
                self.fallback_total,
            )?,
        })
    }
}

/// Maximum number of selected keys bound into one dynamic `SQLite` query.
const QUERY_KEY_CHUNK_SIZE: usize = 900;

/// File-row projection shared by full and selected snapshot loads.
const FILE_RECORD_COLUMNS: &str = "session_key, path, generation, parser_version, file_format, \
 size, mtime_ns, dev, ino, ctime_ns, checkpoint_offset, previous_input, previous_cached_input, \
 previous_output, previous_reasoning_output, previous_total, current_model, \
 current_model_is_fallback, replay_state, content_hash, total_input, total_cached_input, \
 total_output, total_reasoning_output, total_tokens, fallback_input, \
 fallback_cached_input, fallback_output, fallback_reasoning_output, fallback_total";

/// Load all valid file records in one `SQLite` scan.
pub(super) fn load_file_records(
    connection: &Connection,
) -> Result<HashMap<String, StoredFileRecord>> {
    let query = format!("SELECT {FILE_RECORD_COLUMNS} FROM files");
    let mut statement = connection.prepare_cached(&query)?;
    let rows = statement.query_map([], RawStoredFileRecord::from_row)?;
    let mut records = HashMap::new();
    for row in rows {
        if let Some(record) = row?.into_valid_record() {
            records.insert(record.session_key.clone(), record);
        }
    }
    Ok(records)
}

/// Load valid file records for the selected session keys only.
pub(super) fn load_file_records_for_keys(
    connection: &Connection,
    session_keys: &[&str],
) -> Result<HashMap<String, StoredFileRecord>> {
    let mut records = HashMap::new();
    if session_keys.is_empty() {
        return Ok(records);
    }

    for chunk in session_keys.chunks(QUERY_KEY_CHUNK_SIZE) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "SELECT {FILE_RECORD_COLUMNS} FROM files WHERE session_key IN ({placeholders})"
        );
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map(
            params_from_iter(chunk.iter().copied()),
            RawStoredFileRecord::from_row,
        )?;
        for row in rows {
            if let Some(record) = row?.into_valid_record() {
                records.insert(record.session_key.clone(), record);
            }
        }
    }

    Ok(records)
}

/// Update one file row only if it still matches the record observed before rebuild scanning.
pub(super) fn update_file_record_conditionally(
    transaction: &Transaction<'_>,
    record: &StoredFileRecord,
    generation: i64,
    cache_entry: &ScannedCacheEntry,
    aggregates: &FileAggregateSet,
) -> Result<usize> {
    let total = aggregates.total_usage();
    let fallback_total = aggregates.fallback_usage();
    Ok(transaction.execute(
        "UPDATE files SET generation = ?1, parser_version = ?2, file_format = ?3, size = ?4, \
         mtime_ns = ?5, dev = ?6, ino = ?7, ctime_ns = ?8, checkpoint_offset = ?9, \
         previous_input = ?10, previous_cached_input = ?11, previous_output = ?12, \
         previous_reasoning_output = ?13, previous_total = ?14, current_model = ?15, \
         current_model_is_fallback = ?16, replay_state = ?17, content_hash = ?18, \
         total_input = ?19, total_cached_input = ?20, total_output = ?21, \
         total_reasoning_output = ?22, total_tokens = ?23, fallback_input = ?24, \
         fallback_cached_input = ?25, fallback_output = ?26, \
         fallback_reasoning_output = ?27, fallback_total = ?28 \
         WHERE session_key = ?29 AND path = ?30 AND generation = ?31 AND size = ?32 \
         AND checkpoint_offset = ?33",
        params![
            generation,
            PARSER_VERSION,
            cache_entry.metadata.file_format.as_str(),
            u64_to_i64(cache_entry.metadata.size)?,
            cache_entry.metadata.mtime_ns,
            cache_entry.metadata.dev,
            cache_entry.metadata.ino,
            cache_entry.metadata.ctime_ns,
            u64_to_i64(cache_entry.checkpoint.offset)?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.input))
                .transpose()?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.cached_input))
                .transpose()?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.output))
                .transpose()?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.reasoning_output))
                .transpose()?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.total))
                .transpose()?,
            cache_entry.checkpoint.current_model.as_deref(),
            bool_to_i64(cache_entry.checkpoint.current_model_is_fallback),
            cache_entry.checkpoint.replay_state.as_str(),
            cache_entry.content_hash.encode(),
            u64_to_i64(total.input)?,
            u64_to_i64(total.cached_input)?,
            u64_to_i64(total.output)?,
            u64_to_i64(total.reasoning_output)?,
            u64_to_i64(total.total)?,
            u64_to_i64(fallback_total.input)?,
            u64_to_i64(fallback_total.cached_input)?,
            u64_to_i64(fallback_total.output)?,
            u64_to_i64(fallback_total.reasoning_output)?,
            u64_to_i64(fallback_total.total)?,
            record.session_key.as_str(),
            record.path.as_str(),
            record.generation,
            u64_to_i64(record.metadata.size)?,
            u64_to_i64(record.checkpoint.offset)?,
        ],
    )?)
}

/// Insert or replace one file row.
pub(super) fn upsert_file_record(
    transaction: &Transaction<'_>,
    generation: i64,
    cache_entry: &ScannedCacheEntry,
    aggregates: &FileAggregateSet,
) -> Result<()> {
    let total = aggregates.total_usage();
    let fallback_total = aggregates.fallback_usage();
    transaction.execute(
        "INSERT INTO files (
             session_key, path, generation, parser_version, file_format, size, mtime_ns, dev,
             ino, ctime_ns, checkpoint_offset, previous_input, previous_cached_input,
             previous_output, previous_reasoning_output, previous_total, current_model,
             current_model_is_fallback, replay_state, content_hash, total_input, total_cached_input,
             total_output, total_reasoning_output, total_tokens, fallback_input,
             fallback_cached_input, fallback_output,
             fallback_reasoning_output, fallback_total
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
             ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30
         )
         ON CONFLICT(session_key) DO UPDATE SET
             path = excluded.path,
             generation = excluded.generation,
             parser_version = excluded.parser_version,
             file_format = excluded.file_format,
             size = excluded.size,
             mtime_ns = excluded.mtime_ns,
             dev = excluded.dev,
             ino = excluded.ino,
             ctime_ns = excluded.ctime_ns,
             checkpoint_offset = excluded.checkpoint_offset,
             previous_input = excluded.previous_input,
             previous_cached_input = excluded.previous_cached_input,
             previous_output = excluded.previous_output,
             previous_reasoning_output = excluded.previous_reasoning_output,
             previous_total = excluded.previous_total,
             current_model = excluded.current_model,
             current_model_is_fallback = excluded.current_model_is_fallback,
             replay_state = excluded.replay_state,
             content_hash = excluded.content_hash,
             total_input = excluded.total_input,
             total_cached_input = excluded.total_cached_input,
             total_output = excluded.total_output,
             total_reasoning_output = excluded.total_reasoning_output,
             total_tokens = excluded.total_tokens,
             fallback_input = excluded.fallback_input,
             fallback_cached_input = excluded.fallback_cached_input,
             fallback_output = excluded.fallback_output,
             fallback_reasoning_output = excluded.fallback_reasoning_output,
             fallback_total = excluded.fallback_total",
        params![
            cache_entry.session_key.as_str(),
            cache_entry.path.as_str(),
            generation,
            PARSER_VERSION,
            cache_entry.metadata.file_format.as_str(),
            u64_to_i64(cache_entry.metadata.size)?,
            cache_entry.metadata.mtime_ns,
            cache_entry.metadata.dev,
            cache_entry.metadata.ino,
            cache_entry.metadata.ctime_ns,
            u64_to_i64(cache_entry.checkpoint.offset)?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.input))
                .transpose()?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.cached_input))
                .transpose()?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.output))
                .transpose()?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.reasoning_output))
                .transpose()?,
            cache_entry
                .checkpoint
                .previous_totals
                .map(|usage| u64_to_i64(usage.total))
                .transpose()?,
            cache_entry.checkpoint.current_model.as_deref(),
            bool_to_i64(cache_entry.checkpoint.current_model_is_fallback),
            cache_entry.checkpoint.replay_state.as_str(),
            cache_entry.content_hash.encode(),
            u64_to_i64(total.input)?,
            u64_to_i64(total.cached_input)?,
            u64_to_i64(total.output)?,
            u64_to_i64(total.reasoning_output)?,
            u64_to_i64(total.total)?,
            u64_to_i64(fallback_total.input)?,
            u64_to_i64(fallback_total.cached_input)?,
            u64_to_i64(fallback_total.output)?,
            u64_to_i64(fallback_total.reasoning_output)?,
            u64_to_i64(fallback_total.total)?,
        ],
    )?;
    Ok(())
}
