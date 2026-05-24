//! Scan-index configuration and path resolution.

use std::path::PathBuf;

/// Configuration for report scan-index usage.
pub(in crate::app) struct ScanIndexConfig {
    /// Whether the scan index should be used.
    pub(in crate::app) enabled: bool,
    /// Optional explicit database path.
    pub(in crate::app) path: Option<PathBuf>,
}

impl ScanIndexConfig {
    /// Return the default enabled scan-index configuration.
    pub(in crate::app) const fn enabled() -> Self {
        Self {
            enabled: true,
            path: None,
        }
    }

    /// Return a disabled scan-index configuration.
    pub(in crate::app) const fn disabled() -> Self {
        Self {
            enabled: false,
            path: None,
        }
    }

    /// Return an enabled configuration with an explicit database path.
    pub(in crate::app) fn with_path(path: PathBuf) -> Self {
        Self {
            enabled: true,
            path: Some(path),
        }
    }
}

/// Resolve the default on-disk scan-index path.
pub(in crate::app) fn default_scan_index_path() -> PathBuf {
    if let Some(base) = dirs::cache_dir() {
        return base.join("codexusage").join("scan-index.sqlite3");
    }

    PathBuf::from(".codexusage-scan-index.sqlite3")
}
