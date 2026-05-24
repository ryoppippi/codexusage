//! CLI orchestration and report building.

#[cfg(test)]
use crate::pricing::{Pricing, PricingCatalog};
#[cfg(test)]
use chrono::{DateTime, NaiveDate, Utc};
#[cfg(test)]
use chrono_tz::Tz;
#[cfg(test)]
use clap::Parser;
#[cfg(test)]
use notify::{
    EventKind as NotifyEventKind,
    event::{DataChange, ModifyKind},
};
#[cfg(test)]
use serde::Deserialize;
#[cfg(test)]
use std::borrow::Cow;
#[cfg(test)]
use std::collections::{BTreeMap, HashMap};
#[cfg(test)]
use std::ffi::OsString;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::num::NonZeroUsize;
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::SystemTime;

/// Human-readable table rendering helpers.
mod render;

/// Report preparation, aggregation, and session target selection.
mod report;

/// Persistent incremental scan index for report commands.
mod scan_index;

/// CLI-only scan progress and debug runtime helpers.
mod scan_runtime;

/// Codex account limit fetching for watch mode.
mod codex_limits;

/// Watch-mode runtime and incremental refresh helpers.
mod watch;

/// CLI parsing and command dispatch.
mod cli;

/// Shared app data models and presentation options.
mod model;

/// Session JSONL parsing and file scan helpers.
mod session_log;

pub use cli::run;
use model::WatchSnapshot;
#[cfg(test)]
use model::is_false;
#[cfg(test)]
use model::{
    BurnRateHistoryPoint, BurnRateSnapshot, DEFAULT_FALLBACK_MODEL, UsagePresentation, WatchOptions,
};
pub use model::{
    CacheReadMode, CachedInputCostMode, DailyRow, ModelBreakdown, MonthlyRow, NumberFormat,
    ReportKind, ReportOptions, ReportOutput, ScannerParallelism, SessionRow, Totals, UsageTotals,
};
#[cfg(test)]
use render::{render_report, render_watch_screen};
#[cfg(test)]
use report::{
    ProjectFilter, ReportBuilder, SessionFileMetadata, SessionScanTarget, balanced_scan_chunks,
    calculate_cost, calculate_cost_from_usage, collect_session_files, collect_session_scan_targets,
    normalize_filter_date, normalize_timezone_name, parse_timezone, resolve_report_date_filters,
    resolve_scan_worker_count, resolve_session_dirs, resolve_session_target_across_roots,
    scan_session_file, session_file_id, session_file_path, sort_session_entries, split_session_id,
    timezone_from_etc_timezone_contents, timezone_from_localtime_target,
};
pub use report::{build_report, build_report_with_scan_index};
#[cfg(test)]
use scan_index::{IndexedScanRequest, ScanIndexConfig, scan_selected_session_targets_with_index};
#[cfg(test)]
use scan_runtime::NoopScanBatchRunner;
#[cfg(test)]
use session_log::{RawUsage, SessionParseCheckpoint};
#[cfg(test)]
use watch::validate_watch_flags;
pub(in crate::app) use watch::{scale_cost_per_hour, scale_usage_per_hour};

#[cfg(test)]
use cli::{Cli, Command, parse_interval_seconds};
#[cfg(all(test, debug_assertions))]
use model::DebugRuntimeOptions;
#[cfg(test)]
use render::{
    BorderStyle, TableElement, TableRenderConfig, TableRuleKind, TableStyle,
    detect_border_style_for, detect_table_style_for, format_currency, format_data_row, format_u64,
    format_u64_with, paint, render_watch_screen_with_size, table_rule, write_table_row,
};
#[cfg(test)]
use report::SessionSummary;
#[cfg(test)]
use session_log::{
    EntryPayload, UsagePayload, deserialize_optional_cow_lossy, deserialize_optional_object_lossy,
    deserialize_optional_u64_lossy, deserialize_u64_lossy, extract_payload_model,
    line_might_affect_usage, parse_token_usage_line, scan_session_file_from_checkpoint,
    subtract_usage,
};
#[cfg(test)]
use watch::{
    CachedWatchFile, OwnedWatchEvent, WatchChangeSet, WatchDirtyKind, WatchEventSource,
    WatchRuntimeState, build_watch_snapshot_at, display_window_minutes, path_is_under_roots,
    remaining_watch_sleep, resolve_local_midnight_utc, supports_watch_screen_clear,
    supports_watch_screen_clear_with_platform, watch_codex_limits_refresh_due, watch_dirty_kind,
    watch_event_session_ids, watch_pricing_refresh_due,
};

#[cfg(test)]
#[allow(clippy::missing_docs_in_private_items)]
mod behavior_checks;
