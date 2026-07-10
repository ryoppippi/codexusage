//! Schema management for the scan index database.

use eyre::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Current `SQLite` schema version.
const SCHEMA_VERSION: i64 = 4;

/// Initialize or recreate the scan-index schema.
pub(super) fn initialize_schema(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    reset_schema_if_needed(&transaction)?;
    ensure_schema_tables(&transaction)?;
    write_schema_version(&transaction)?;
    transaction.commit()?;
    Ok(())
}

/// Drop advisory index tables when their schema version is stale.
fn reset_schema_if_needed(transaction: &Transaction<'_>) -> Result<()> {
    let existing_version = existing_schema_version(transaction)?;
    if existing_version.as_deref() != Some(&SCHEMA_VERSION.to_string()) {
        transaction.execute_batch(
            "DROP TABLE IF EXISTS file_aggregates;
             DROP TABLE IF EXISTS files;
             DROP TABLE IF EXISTS meta;",
        )?;
    }
    Ok(())
}

/// Read the stored scan-index schema version, if the meta table exists.
fn existing_schema_version(transaction: &Transaction<'_>) -> Result<Option<String>> {
    let meta_exists = transaction
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !meta_exists {
        return Ok(None);
    }

    Ok(transaction
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

/// Ensure the current scan-index tables and indexes exist.
fn ensure_schema_tables(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS files (
             session_key TEXT PRIMARY KEY,
             path TEXT NOT NULL,
             generation INTEGER NOT NULL,
             parser_version INTEGER NOT NULL,
             file_format TEXT NOT NULL,
             size INTEGER NOT NULL,
             mtime_ns INTEGER,
             dev INTEGER,
             ino INTEGER,
             ctime_ns INTEGER,
             checkpoint_offset INTEGER NOT NULL,
             previous_input INTEGER,
             previous_cached_input INTEGER,
             previous_output INTEGER,
             previous_reasoning_output INTEGER,
             previous_total INTEGER,
             current_model TEXT,
             current_model_is_fallback INTEGER NOT NULL,
             replay_state TEXT NOT NULL,
             content_hash TEXT NOT NULL,
             total_input INTEGER NOT NULL,
             total_cached_input INTEGER NOT NULL,
             total_output INTEGER NOT NULL,
             total_reasoning_output INTEGER NOT NULL,
             total_tokens INTEGER NOT NULL,
             fallback_input INTEGER NOT NULL,
             fallback_cached_input INTEGER NOT NULL,
             fallback_output INTEGER NOT NULL,
             fallback_reasoning_output INTEGER NOT NULL,
             fallback_total INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS file_aggregates (
             session_key TEXT NOT NULL,
             timezone TEXT NOT NULL,
             generation INTEGER NOT NULL,
             group_kind TEXT NOT NULL,
             group_key TEXT NOT NULL,
             model TEXT NOT NULL,
             input_tokens INTEGER NOT NULL,
             cached_input_tokens INTEGER NOT NULL,
             output_tokens INTEGER NOT NULL,
             reasoning_output_tokens INTEGER NOT NULL,
             total_tokens INTEGER NOT NULL,
             fallback_input_tokens INTEGER NOT NULL,
             fallback_cached_input_tokens INTEGER NOT NULL,
             fallback_output_tokens INTEGER NOT NULL,
             fallback_reasoning_output_tokens INTEGER NOT NULL,
             fallback_total_tokens INTEGER NOT NULL,
             last_activity_nanos INTEGER NOT NULL,
             PRIMARY KEY (
                 session_key,
                 timezone,
                 generation,
                 group_kind,
                 group_key,
                 model
             ),
             FOREIGN KEY (session_key) REFERENCES files(session_key) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS file_aggregates_lookup
             ON file_aggregates(session_key, timezone, generation, group_kind);",
    )?;
    Ok(())
}

/// Persist the current schema version into the meta table.
fn write_schema_version(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}
