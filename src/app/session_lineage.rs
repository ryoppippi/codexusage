//! Spawned-session metadata and lineage helpers.

use super::session_files::{SessionFileFormat, session_file_format};
use eyre::{Result, WrapErr};
use memchr::memmem::Finder;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Marker used to avoid parsing unrelated records while looking for a replay boundary.
static INTER_AGENT_BOUNDARY_FINDER: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new(b"inter_agent_communication_metadata"));

/// Metadata needed to filter and attribute one physical rollout file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::app) struct SessionDescriptor {
    /// Thread identifier stored in the first session metadata record.
    pub(in crate::app) thread_id: Option<String>,
    /// Parent thread identifier for newly spawned subagents.
    pub(in crate::app) parent_thread_id: Option<String>,
    /// Working directory recorded when the session started.
    pub(in crate::app) cwd: Option<PathBuf>,
}

/// Minimal top-level entry shape used to confirm a replay boundary.
#[derive(Deserialize)]
struct EntryKind<'a> {
    /// Entry kind.
    #[serde(rename = "type", default)]
    entry_type: Option<&'a str>,
}

/// Parse the first session metadata record into lineage fields.
pub(in crate::app) fn parse_session_descriptor(line: &[u8]) -> Option<SessionDescriptor> {
    let entry = serde_json::from_slice::<serde_json::Value>(line).ok()?;
    match entry.get("type").and_then(serde_json::Value::as_str) {
        Some("session_meta") => {
            let payload = entry.get("payload")?;
            Some(SessionDescriptor {
                thread_id: payload
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
                parent_thread_id: payload
                    .pointer("/source/subagent/thread_spawn/parent_thread_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
                cwd: payload
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from),
            })
        }
        Some("session") => Some(SessionDescriptor {
            cwd: entry
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from),
            ..SessionDescriptor::default()
        }),
        _ => None,
    }
}

/// Read lineage metadata from the first non-empty record of one rollout file.
pub(in crate::app) fn read_session_descriptor(file: &Path) -> Result<Option<SessionDescriptor>> {
    let file_handle = File::open(file)
        .wrap_err_with(|| format!("failed to open session metadata file {}", file.display()))?;
    if session_file_format(file) == Some(SessionFileFormat::Compressed) {
        let decoder = zstd::stream::read::Decoder::new(file_handle)
            .wrap_err_with(|| format!("failed to decode compressed session {}", file.display()))?;
        return read_session_descriptor_from_reader(BufReader::new(decoder));
    }

    read_session_descriptor_from_reader(BufReader::new(file_handle))
}

/// Read the first non-empty record from an already decoded rollout reader.
fn read_session_descriptor_from_reader(
    mut reader: impl BufRead,
) -> Result<Option<SessionDescriptor>> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = trim_ascii_whitespace(&line);
        if !trimmed.is_empty() {
            return Ok(parse_session_descriptor(trimmed));
        }
    }
}

/// Return whether the first rollout record identifies a spawned subagent.
pub(in crate::app) fn is_spawned_subagent_metadata(line: &[u8]) -> bool {
    parse_session_descriptor(line).is_some_and(|descriptor| descriptor.parent_thread_id.is_some())
}

/// Return whether a record marks the end of inherited subagent history.
pub(in crate::app) fn is_inter_agent_communication_boundary(line: &[u8]) -> bool {
    if INTER_AGENT_BOUNDARY_FINDER.find(line).is_none() {
        return false;
    }
    serde_json::from_slice::<EntryKind<'_>>(line)
        .ok()
        .and_then(|entry| entry.entry_type)
        == Some("inter_agent_communication_metadata")
}

/// Trim JSONL whitespace without requiring UTF-8 conversion.
fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let Some(start) = bytes.iter().position(|byte| !byte.is_ascii_whitespace()) else {
        return &[];
    };
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |position| position + 1);
    &bytes[start..end]
}
