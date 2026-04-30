//! Live watch screen rendering.

use super::super::model::explicit_usage;
use super::super::{
    CacheReadMode, NumberFormat, UsageTotals, WatchSnapshot, scale_cost_per_hour,
    scale_usage_per_hour,
};
use super::number::{format_currency, format_u64_with};
use super::table::{
    DisplayRow, DisplayRowKind, TableElement, TableRenderConfig, TableRuleKind, column_widths,
    detect_border_style, detect_table_style, paint, row_table_element, table_display_width,
    table_rule, table_rule_element, write_table_header, write_table_row, write_table_rule,
};
use std::fmt::Write as _;
use terminal_size::{Width, terminal_size};

/// Render one live watch snapshot.
pub(in crate::app) fn render_watch_screen(
    snapshot: &WatchSnapshot,
    locale: &str,
    number_format: NumberFormat,
    show_model_burn_rate: bool,
    cache_read_mode: CacheReadMode,
) -> String {
    render_watch_screen_with_width(
        snapshot,
        locale,
        number_format,
        show_model_burn_rate,
        cache_read_mode,
        detect_terminal_width(),
    )
}

/// Render one live watch snapshot with an explicit terminal width override.
pub(in crate::app) fn render_watch_screen_with_width(
    snapshot: &WatchSnapshot,
    _locale: &str,
    number_format: NumberFormat,
    show_model_burn_rate: bool,
    cache_read_mode: CacheReadMode,
    terminal_width: Option<usize>,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    let mut output = String::new();
    let _ = writeln!(
        &mut output,
        "{}",
        paint(
            render_config.style,
            TableElement::Title,
            "Current Day Codex Usage Watch"
        )
    );
    let _ = writeln!(
        &mut output,
        "Date: {}  Window: {} minutes",
        snapshot.date, snapshot.burn_rate.window_minutes
    );
    let _ = writeln!(&mut output);
    write_watch_table(
        &mut output,
        render_config,
        snapshot,
        number_format,
        show_model_burn_rate,
        cache_read_mode,
        terminal_width,
    );

    if !snapshot.missing_directories.is_empty() {
        let mut warning = String::from("Warning: missing session directories\n");
        for directory in &snapshot.missing_directories {
            let _ = writeln!(&mut warning, "- {directory}");
        }
        warning.push('\n');
        warning.push_str(&output);
        return warning;
    }

    output
}

/// Detect the current terminal width for live watch-mode wrapping.
fn detect_terminal_width() -> Option<usize> {
    terminal_size().map(|(Width(width), _height)| usize::from(width))
}

/// Render the watch metrics table.
fn write_watch_table(
    output: &mut String,
    render_config: TableRenderConfig,
    snapshot: &WatchSnapshot,
    number_format: NumberFormat,
    show_model_burn_rate: bool,
    cache_read_mode: CacheReadMode,
    terminal_width: Option<usize>,
) {
    let model_columns = if show_model_burn_rate {
        active_watch_burn_columns(snapshot)
    } else {
        Vec::new()
    };
    let model_headers = model_columns
        .iter()
        .map(|column| column.label.clone())
        .collect::<Vec<_>>();
    let rows = watch_rows(snapshot, number_format, &model_columns, cache_read_mode);
    let blocks = watch_table_blocks(
        &model_headers,
        &rows,
        snapshot,
        render_config,
        terminal_width,
    );

    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }

        let headers = watch_block_headers(&model_headers, block);
        let header_refs = headers.iter().map(String::as_str).collect::<Vec<_>>();
        let block_rows = watch_block_rows(&rows, block);
        let updated_row = watch_updated_row(snapshot, block);
        let widths = column_widths(&header_refs, &block_rows, &updated_row, number_format);

        write_table_header(output, render_config, &header_refs, &widths);
        for row in &block_rows {
            write_table_row(
                output,
                render_config,
                &header_refs,
                &widths,
                &row.cells,
                row_table_element(row.kind),
            );
        }
        write_table_row(
            output,
            render_config,
            &header_refs,
            &widths,
            &updated_row.cells,
            row_table_element(updated_row.kind),
        );
        write_table_rule(
            output,
            render_config.style,
            table_rule_element(TableRuleKind::Bottom),
            &table_rule(TableRuleKind::Bottom, render_config.borders, &widths),
        );
    }
}

/// One active per-model burn-rate column in watch mode.
struct WatchBurnColumn {
    /// Header label.
    label: String,
    /// Raw burn-window usage for this column.
    usage: UsageTotals,
    /// Raw burn-window cost for this column.
    cost_usd: f64,
}

/// One logical metric row in the watch table before block projection.
#[derive(Clone, Debug, Eq, PartialEq)]
struct WatchMetricRow {
    /// Left-hand row label repeated in every block.
    metric: &'static str,
    /// Current-day aggregate value.
    today: String,
    /// Per-model burn-rate cells in model order.
    per_model: Vec<String>,
    /// Aggregate burn-rate value.
    burn_rate: String,
}

/// One stacked watch-table block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WatchTableBlock {
    /// Whether this block includes the `Today` aggregate column.
    include_today: bool,
    /// Start offset inside the per-model column list.
    model_start: usize,
    /// Exclusive end offset inside the per-model column list.
    model_end: usize,
    /// Whether this block includes the aggregate burn-rate column.
    include_burn_rate: bool,
}

impl WatchTableBlock {
    /// Build one block descriptor over a contiguous model column range.
    const fn new(
        include_today: bool,
        model_start: usize,
        model_end: usize,
        include_burn_rate: bool,
    ) -> Self {
        Self {
            include_today,
            model_start,
            model_end,
            include_burn_rate,
        }
    }

    /// Return how many model columns are projected into this block.
    const fn model_count(self) -> usize {
        self.model_end.saturating_sub(self.model_start)
    }
}

/// Return the active per-model burn-rate columns.
fn active_watch_burn_columns(snapshot: &WatchSnapshot) -> Vec<WatchBurnColumn> {
    let mut columns = Vec::new();
    for (model, breakdown) in &snapshot.per_model {
        let explicit = explicit_usage(breakdown);
        if explicit.has_usage() || breakdown.cost_usd > 0.0 {
            columns.push(WatchBurnColumn {
                label: format!("{model} /h"),
                usage: explicit,
                cost_usd: breakdown.cost_usd,
            });
        }

        if breakdown.fallback_usage.has_usage() || breakdown.fallback_cost_usd > 0.0 {
            columns.push(WatchBurnColumn {
                label: format!("{model} (fallback) /h"),
                usage: breakdown.fallback_usage.clone(),
                cost_usd: breakdown.fallback_cost_usd,
            });
        }
    }
    columns
}

/// Build all metric rows for watch mode.
fn watch_rows(
    snapshot: &WatchSnapshot,
    number_format: NumberFormat,
    model_columns: &[WatchBurnColumn],
    cache_read_mode: CacheReadMode,
) -> Vec<WatchMetricRow> {
    let token_cells = |select: fn(&UsageTotals) -> u64| {
        model_columns
            .iter()
            .map(|column| {
                scale_usage_per_hour(select(&column.usage), snapshot.burn_rate.window_duration)
            })
            .map(|value| format_u64_with(value, number_format))
            .collect::<Vec<_>>()
    };
    let cost_cells = model_columns
        .iter()
        .map(|column| {
            format_currency(scale_cost_per_hour(
                column.cost_usd,
                snapshot.burn_rate.window_duration,
            ))
        })
        .collect::<Vec<_>>();

    let mut rows = vec![WatchMetricRow {
        metric: "Input",
        today: format_u64_with(snapshot.totals.input_tokens, number_format),
        per_model: token_cells(|usage| usage.input),
        burn_rate: format_u64_with(snapshot.burn_rate.input_tokens_per_hour, number_format),
    }];
    if cache_read_mode == CacheReadMode::Include {
        rows.push(WatchMetricRow {
            metric: "Cache",
            today: format_u64_with(snapshot.totals.cached_input_tokens, number_format),
            per_model: token_cells(|usage| usage.cached_input),
            burn_rate: format_u64_with(
                snapshot.burn_rate.cached_input_tokens_per_hour,
                number_format,
            ),
        });
    }
    rows.extend([
        WatchMetricRow {
            metric: "Output",
            today: format_u64_with(snapshot.totals.output_tokens, number_format),
            per_model: token_cells(|usage| usage.output),
            burn_rate: format_u64_with(snapshot.burn_rate.output_tokens_per_hour, number_format),
        },
        WatchMetricRow {
            metric: "Reasoning",
            today: format_u64_with(snapshot.totals.reasoning_output_tokens, number_format),
            per_model: token_cells(|usage| usage.reasoning_output),
            burn_rate: format_u64_with(
                snapshot.burn_rate.reasoning_output_tokens_per_hour,
                number_format,
            ),
        },
        WatchMetricRow {
            metric: "Total",
            today: format_u64_with(snapshot.totals.total_tokens, number_format),
            per_model: token_cells(|usage| usage.total),
            burn_rate: format_u64_with(snapshot.burn_rate.total_tokens_per_hour, number_format),
        },
        WatchMetricRow {
            metric: "Cost",
            today: format_currency(snapshot.totals.cost_usd),
            per_model: cost_cells,
            burn_rate: format_currency(snapshot.burn_rate.cost_usd_per_hour),
        },
    ]);
    rows
}

/// Shared inputs for stacked watch-table layout decisions.
struct WatchBlockLayoutContext<'a> {
    /// Ordered per-model header labels.
    model_headers: &'a [String],
    /// Logical watch rows projected into blocks during layout.
    rows: &'a [WatchMetricRow],
    /// Snapshot metadata used by the repeated `Updated` row.
    snapshot: &'a WatchSnapshot,
    /// Shared table rendering configuration.
    render_config: TableRenderConfig,
    /// Maximum width available for one rendered table block.
    terminal_width: usize,
}

impl WatchBlockLayoutContext<'_> {
    /// Pick the minimal stacked block layout from one model-column offset.
    fn best_layout_from(
        &self,
        model_start: usize,
        include_today: bool,
        best_tails: &[Option<Vec<WatchTableBlock>>],
    ) -> Option<Vec<WatchTableBlock>> {
        let mut best_layout = None;
        for model_end in (model_start + 1..=self.model_headers.len()).rev() {
            let block = WatchTableBlock::new(
                include_today,
                model_start,
                model_end,
                model_end == self.model_headers.len(),
            );
            if !self.block_fits_or_forces(block) {
                continue;
            }

            let candidate = if block.include_burn_rate {
                Some(vec![block])
            } else {
                best_tails
                    .get(model_end)
                    .and_then(|tail| tail.as_ref())
                    .map(|tail| {
                        let mut layout = Vec::with_capacity(tail.len() + 1);
                        layout.push(block);
                        layout.extend(tail.iter().copied());
                        layout
                    })
            };

            if let Some(candidate) = candidate {
                let should_replace = best_layout
                    .as_ref()
                    .is_none_or(|existing: &Vec<WatchTableBlock>| candidate.len() < existing.len());
                if should_replace {
                    best_layout = Some(candidate);
                }
            }
        }
        best_layout
    }

    /// Return whether the block fits the terminal or qualifies for forced one-column overflow.
    fn block_fits_or_forces(&self, block: WatchTableBlock) -> bool {
        if self.block_width(block) <= self.terminal_width {
            return true;
        }
        block.model_count() == 1
    }

    /// Compute the rendered width of one stacked watch block.
    fn block_width(&self, block: WatchTableBlock) -> usize {
        let headers = watch_block_headers(self.model_headers, &block);
        let header_refs = headers.iter().map(String::as_str).collect::<Vec<_>>();
        let projected_rows = watch_block_rows(self.rows, &block);
        let updated_row = watch_updated_row(self.snapshot, &block);
        let widths = column_widths(
            &header_refs,
            &projected_rows,
            &updated_row,
            self.render_config.number_format,
        );
        table_display_width(&widths)
    }
}

/// Build the stacked watch-table blocks for the current terminal width.
fn watch_table_blocks(
    model_headers: &[String],
    rows: &[WatchMetricRow],
    snapshot: &WatchSnapshot,
    render_config: TableRenderConfig,
    terminal_width: Option<usize>,
) -> Vec<WatchTableBlock> {
    let full_block = WatchTableBlock::new(true, 0, model_headers.len(), true);
    let Some(terminal_width) = terminal_width else {
        return vec![full_block];
    };
    let layout = WatchBlockLayoutContext {
        model_headers,
        rows,
        snapshot,
        render_config,
        terminal_width,
    };
    if model_headers.is_empty() || layout.block_width(full_block) <= terminal_width {
        return vec![full_block];
    }

    let mut best_tails = vec![None; model_headers.len() + 1];
    best_tails[model_headers.len()] = Some(Vec::new());

    for model_start in (0..model_headers.len()).rev() {
        best_tails[model_start] = layout.best_layout_from(model_start, false, &best_tails);
    }

    layout
        .best_layout_from(0, true, &best_tails)
        .unwrap_or_else(|| vec![full_block])
}

/// Build the projected headers for one stacked watch block.
fn watch_block_headers(model_headers: &[String], block: &WatchTableBlock) -> Vec<String> {
    let mut headers = Vec::with_capacity(block.model_count() + 3);
    headers.push("Metric".to_string());
    if block.include_today {
        headers.push("Today".to_string());
    }
    headers.extend(
        model_headers[block.model_start..block.model_end]
            .iter()
            .cloned(),
    );
    if block.include_burn_rate {
        headers.push("Burn Rate (/h)".to_string());
    }
    headers
}

/// Project the logical watch rows into one stacked block.
fn watch_block_rows(rows: &[WatchMetricRow], block: &WatchTableBlock) -> Vec<DisplayRow> {
    rows.iter()
        .map(|row| {
            let mut cells = Vec::with_capacity(block.model_count() + 3);
            cells.push(row.metric.to_string());
            if block.include_today {
                cells.push(row.today.clone());
            }
            cells.extend(
                row.per_model[block.model_start..block.model_end]
                    .iter()
                    .cloned(),
            );
            if block.include_burn_rate {
                cells.push(row.burn_rate.clone());
            }
            DisplayRow {
                cells,
                kind: DisplayRowKind::Subtotal,
            }
        })
        .collect()
}

/// Build the repeated `Updated` row for one stacked block.
fn watch_updated_row(snapshot: &WatchSnapshot, block: &WatchTableBlock) -> DisplayRow {
    let mut cells = Vec::with_capacity(block.model_count() + 3);
    cells.push("Updated".to_string());
    if block.include_today {
        cells.push(snapshot.date.clone());
    }
    cells.extend(vec![String::new(); block.model_count()]);
    if block.include_burn_rate {
        cells.push(snapshot.updated_time.clone());
    }
    DisplayRow {
        cells,
        kind: DisplayRowKind::GrandTotal,
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{BurnRateSnapshot, Totals};
    use super::super::table::{BorderStyle, TableStyle};
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn watch_layout_snapshot() -> WatchSnapshot {
        WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 3,
                cached_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 3,
                cost_usd: 0.03,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(30 * 60),
                window_minutes: 30,
                input_tokens_per_hour: 6,
                cached_input_tokens_per_hour: 0,
                output_tokens_per_hour: 0,
                reasoning_output_tokens_per_hour: 0,
                total_tokens_per_hour: 6,
                cost_usd_per_hour: 0.06,
            },
            per_model: BTreeMap::new(),
            missing_directories: Vec::new(),
            updated_time: "00:30:00".to_string(),
        }
    }

    fn watch_layout_rows() -> Vec<WatchMetricRow> {
        vec![WatchMetricRow {
            metric: "Input",
            today: "3".to_string(),
            per_model: vec!["1".to_string(), "1".to_string(), "1".to_string()],
            burn_rate: "6".to_string(),
        }]
    }

    #[test]
    fn watch_table_blocks_keep_single_block_when_width_is_sufficient() {
        let blocks = watch_table_blocks(
            &[
                "alpha /h".to_string(),
                "beta /h".to_string(),
                "gamma /h".to_string(),
            ],
            &watch_layout_rows(),
            &watch_layout_snapshot(),
            TableRenderConfig {
                style: TableStyle::Plain,
                borders: BorderStyle::Ascii,
                number_format: NumberFormat::Full,
            },
            Some(120),
        );

        assert_eq!(blocks, vec![WatchTableBlock::new(true, 0, 3, true)]);
    }

    #[test]
    fn watch_table_blocks_split_into_minimal_stacked_layout() {
        let blocks = watch_table_blocks(
            &[
                "alpha /h".to_string(),
                "beta /h".to_string(),
                "gamma /h".to_string(),
            ],
            &watch_layout_rows(),
            &watch_layout_snapshot(),
            TableRenderConfig {
                style: TableStyle::Plain,
                borders: BorderStyle::Ascii,
                number_format: NumberFormat::Full,
            },
            Some(24),
        );

        assert_eq!(
            blocks,
            vec![
                WatchTableBlock::new(true, 0, 1, false),
                WatchTableBlock::new(false, 1, 2, false),
                WatchTableBlock::new(false, 2, 3, true),
            ]
        );
    }

    #[test]
    fn watch_table_blocks_measure_unicode_borders_in_terminal_columns() {
        let snapshot = watch_layout_snapshot();
        let rows = watch_layout_rows();
        let model_headers = vec!["alpha /h".to_string()];
        let render_config = TableRenderConfig {
            style: TableStyle::Plain,
            borders: BorderStyle::Unicode,
            number_format: NumberFormat::Full,
        };
        let layout = WatchBlockLayoutContext {
            model_headers: &model_headers,
            rows: &rows,
            snapshot: &snapshot,
            render_config,
            terminal_width: usize::MAX,
        };
        let full_block = WatchTableBlock::new(true, 0, 1, true);
        let measured_width = layout.block_width(full_block);

        assert_eq!(measured_width, 52);
        assert_eq!(
            watch_table_blocks(
                &model_headers,
                &rows,
                &snapshot,
                render_config,
                Some(measured_width),
            ),
            vec![full_block]
        );
    }
}
