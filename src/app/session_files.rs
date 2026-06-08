//! Session rollout path and reader helpers.

use eyre::{Result, WrapErr};
use std::fs;
use std::path::{Path, PathBuf};

/// Canonical uncompressed session file extension.
const JSONL_EXTENSION: &str = "jsonl";
/// Suffix used by Codex for zstd-compressed session rollouts.
const COMPRESSED_SUFFIX: &str = ".zst";
/// Full compressed rollout suffix.
const COMPRESSED_JSONL_SUFFIX: &str = ".jsonl.zst";

/// Physical representation of one session rollout file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum SessionFileFormat {
    /// Plain JSONL file.
    Plain,
    /// Zstd-compressed JSONL file.
    Compressed,
}

impl SessionFileFormat {
    /// Return whether this file is zstd-compressed.
    pub(in crate::app) const fn is_compressed(self) -> bool {
        matches!(self, Self::Compressed)
    }

    /// Stable string stored in the scan index.
    pub(in crate::app) const fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Compressed => "compressed",
        }
    }

    /// Parse a scan-index representation string.
    pub(in crate::app) fn from_str(value: &str) -> Option<Self> {
        match value {
            "plain" => Some(Self::Plain),
            "compressed" => Some(Self::Compressed),
            _ => None,
        }
    }
}

/// Return the recognized session file format for one path.
pub(in crate::app) fn session_file_format(path: &Path) -> Option<SessionFileFormat> {
    let extension = path.extension().and_then(|extension| extension.to_str())?;
    if extension.eq_ignore_ascii_case(JSONL_EXTENSION) {
        return Some(SessionFileFormat::Plain);
    }
    if extension.eq_ignore_ascii_case(&COMPRESSED_SUFFIX[1..])
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| ends_with_ignore_ascii_case(name, COMPRESSED_JSONL_SUFFIX))
    {
        return Some(SessionFileFormat::Compressed);
    }
    None
}

/// Return whether a path is a supported session file.
#[cfg(test)]
pub(in crate::app) fn is_session_file_path(path: &Path) -> bool {
    session_file_format(path).is_some()
}

/// Return whether a discovered file with a known format should be ignored.
pub(in crate::app) fn should_skip_format(path: &Path, file_format: SessionFileFormat) -> bool {
    file_format == SessionFileFormat::Compressed && plain_session_path(path).is_file()
}

/// Convert either representation into the logical plain `.jsonl` path.
pub(in crate::app) fn plain_session_path(path: &Path) -> PathBuf {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return path.to_path_buf();
    };
    if !ends_with_ignore_ascii_case(file_name, COMPRESSED_SUFFIX) {
        return path.to_path_buf();
    }

    let plain_name = &file_name[..file_name.len() - COMPRESSED_SUFFIX.len()];
    path.with_file_name(plain_name)
}

/// Return the compressed `.jsonl.zst` path for a logical session path.
pub(in crate::app) fn compressed_session_path(path: &Path) -> PathBuf {
    if session_file_format(path) == Some(SessionFileFormat::Compressed) {
        return path.to_path_buf();
    }
    let mut file_name = path.file_name().map_or_else(
        || std::ffi::OsStr::new("session.jsonl").to_os_string(),
        std::ffi::OsStr::to_os_string,
    );
    file_name.push(COMPRESSED_SUFFIX);
    path.with_file_name(file_name)
}

/// Resolve the existing plain or compressed representation for one logical path.
pub(in crate::app) fn existing_session_path(logical_path: &Path) -> Result<Option<PathBuf>> {
    let plain_path = plain_session_path(logical_path);
    match fs::metadata(&plain_path) {
        Ok(metadata) if metadata.is_file() => return Ok(Some(plain_path)),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).wrap_err_with(|| {
                format!("failed to access session file {}", plain_path.display())
            });
        }
    }

    let compressed_path = compressed_session_path(&plain_path);
    match fs::metadata(&compressed_path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(compressed_path)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "failed to access compressed session file {}",
                compressed_path.display()
            )
        }),
    }
}

/// Return whether an ASCII string ends with a suffix, ignoring ASCII case.
fn ends_with_ignore_ascii_case(value: &str, suffix: &str) -> bool {
    value
        .get(value.len().saturating_sub(suffix.len())..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}
