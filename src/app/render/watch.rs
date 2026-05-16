//! Live watch screen rendering.

use super::super::model::{BurnRateHistoryPoint, explicit_usage};
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
use terminal_size::{Height, Width, terminal_size};

/// Number of terminal rows used by the watch cost graph body.
const WATCH_GRAPH_BODY_HEIGHT: usize = 4;
/// Number of terminal graph body rows as an exact floating point denominator.
const WATCH_GRAPH_BODY_HEIGHT_F64: f64 = 4.0;
/// Number of vertical subunits represented by one graph body row.
const WATCH_GRAPH_ROW_UNITS: usize = 8;
/// Total vertical subunits represented by the graph body.
const WATCH_GRAPH_VERTICAL_UNITS: usize = WATCH_GRAPH_BODY_HEIGHT * WATCH_GRAPH_ROW_UNITS;
/// Total vertical subunits as an exact floating point denominator.
const WATCH_GRAPH_VERTICAL_UNITS_F64: f64 = 32.0;

/// Render one live watch snapshot.
pub(in crate::app) fn render_watch_screen(
    snapshot: &WatchSnapshot,
    locale: &str,
    number_format: NumberFormat,
    show_model_burn_rate: bool,
    cache_read_mode: CacheReadMode,
) -> String {
    let terminal_size = detect_terminal_size();
    render_watch_screen_with_size(
        snapshot,
        locale,
        number_format,
        show_model_burn_rate,
        cache_read_mode,
        terminal_size.map(|(width, _height)| width),
        terminal_size.map(|(_width, height)| height),
    )
}

/// Render one live watch snapshot with an explicit terminal width override.
#[cfg(test)]
pub(in crate::app) fn render_watch_screen_with_width(
    snapshot: &WatchSnapshot,
    locale: &str,
    number_format: NumberFormat,
    show_model_burn_rate: bool,
    cache_read_mode: CacheReadMode,
    terminal_width: Option<usize>,
) -> String {
    render_watch_screen_with_size(
        snapshot,
        locale,
        number_format,
        show_model_burn_rate,
        cache_read_mode,
        terminal_width,
        None,
    )
}

/// Render one live watch snapshot with explicit terminal dimensions.
pub(in crate::app) fn render_watch_screen_with_size(
    snapshot: &WatchSnapshot,
    _locale: &str,
    number_format: NumberFormat,
    show_model_burn_rate: bool,
    cache_read_mode: CacheReadMode,
    terminal_width: Option<usize>,
    terminal_height: Option<usize>,
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

    let warning = watch_missing_directory_warning(snapshot);
    let reserved_lines =
        rendered_line_count(&output) + warning.as_deref().map_or(0, rendered_line_count);
    if let Some(graph) = render_watch_graph(
        snapshot,
        render_config,
        terminal_width,
        terminal_height,
        reserved_lines,
    ) {
        let _ = writeln!(&mut output);
        output.push_str(&graph);
    }

    if let Some(mut warning) = warning {
        warning.push_str(&output);
        return warning;
    }

    output
}

/// Build the missing-directory warning prefix for one watch snapshot.
fn watch_missing_directory_warning(snapshot: &WatchSnapshot) -> Option<String> {
    if snapshot.missing_directories.is_empty() {
        return None;
    }

    let mut warning = String::from("Warning: missing session directories\n");
    for directory in &snapshot.missing_directories {
        let _ = writeln!(&mut warning, "- {directory}");
    }
    warning.push('\n');
    Some(warning)
}

/// Detect the current terminal size for live watch-mode wrapping.
fn detect_terminal_size() -> Option<(usize, usize)> {
    terminal_size().map(|(Width(width), Height(height))| (usize::from(width), usize::from(height)))
}

/// Cost graph horizon selected by available terminal space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WatchGraphHorizon {
    /// Render the past eight hours.
    EightHours,
    /// Render the past four hours.
    FourHours,
}

impl WatchGraphHorizon {
    /// Return the expected number of 15-minute samples including both endpoints.
    const fn point_count(self) -> usize {
        match self {
            Self::EightHours => 33,
            Self::FourHours => 17,
        }
    }
}

/// Render the compact cost burn-rate graph when terminal space allows it.
fn render_watch_graph(
    snapshot: &WatchSnapshot,
    render_config: TableRenderConfig,
    terminal_width: Option<usize>,
    terminal_height: Option<usize>,
    reserved_lines: usize,
) -> Option<String> {
    for horizon in [WatchGraphHorizon::EightHours, WatchGraphHorizon::FourHours] {
        let Some(points) = burn_history_for_horizon(&snapshot.burn_history, horizon) else {
            continue;
        };
        let Some(graph) = format_watch_graph(points, render_config) else {
            continue;
        };
        if graph_fits(&graph, terminal_width, terminal_height, reserved_lines) {
            return Some(graph);
        }
    }
    None
}

/// Return the suffix of graph history needed for one horizon.
fn burn_history_for_horizon(
    history: &[BurnRateHistoryPoint],
    horizon: WatchGraphHorizon,
) -> Option<&[BurnRateHistoryPoint]> {
    let count = horizon.point_count();
    let start = history.len().checked_sub(count)?;
    history.get(start..).filter(|points| points.len() == count)
}

/// Format one graph block.
fn format_watch_graph(
    points: &[BurnRateHistoryPoint],
    render_config: TableRenderConfig,
) -> Option<String> {
    let first = points.first()?;
    let last = points.last()?;
    let max_cost = max_graph_cost(points);
    let plot_rows = cost_plot_rows(points, max_cost, render_config.borders);
    let axis = graph_axis_line(first, last, points.len(), render_config.borders);
    let headers = ["Burn Rate History"];
    let cost_labels = cost_legend_labels(max_cost, render_config.borders);
    let cost_label_width = cost_labels
        .iter()
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    let rows = plot_rows
        .into_iter()
        .zip(cost_labels)
        .map(|(row, label)| DisplayRow {
            cells: vec![append_cost_legend(&row, &label, cost_label_width)],
            kind: DisplayRowKind::Subtotal,
        })
        .collect::<Vec<_>>();
    let axis_row = DisplayRow {
        cells: vec![axis],
        kind: DisplayRowKind::Subtotal,
    };
    let widths = column_widths(&headers, &rows, &axis_row, render_config.number_format);
    let mut output = String::new();

    write_table_header(&mut output, render_config, &headers, &widths);
    for row in &rows {
        write_table_row(
            &mut output,
            render_config,
            &headers,
            &widths,
            &row.cells,
            row_table_element(row.kind),
        );
    }
    write_table_row(
        &mut output,
        render_config,
        &headers,
        &widths,
        &axis_row.cells,
        row_table_element(axis_row.kind),
    );
    write_table_rule(
        &mut output,
        render_config.style,
        table_rule_element(TableRuleKind::Bottom),
        &table_rule(TableRuleKind::Bottom, render_config.borders, &widths),
    );
    if output.ends_with('\n') {
        output.pop();
    }

    Some(output)
}

/// Build right-side cost scale labels for the fixed-height graph.
fn cost_legend_labels(max_cost: f64, borders: super::table::BorderStyle) -> Vec<String> {
    (1..=WATCH_GRAPH_BODY_HEIGHT)
        .rev()
        .map(|level| {
            let level_f64 = u32::try_from(level).map_or(0.0, f64::from);
            let cost = max_cost * level_f64 / WATCH_GRAPH_BODY_HEIGHT_F64;
            cost_legend_label(cost, borders)
        })
        .collect()
}

/// Build one right-side cost scale label.
fn cost_legend_label(cost: f64, borders: super::table::BorderStyle) -> String {
    let marker = match borders {
        super::table::BorderStyle::Unicode => '─',
        super::table::BorderStyle::Ascii => '-',
    };
    format!("{marker} {}/h", format_currency(cost))
}

/// Append one left-aligned cost label after the graph plot area.
fn append_cost_legend(line: &str, label: &str, label_width: usize) -> String {
    format!("{line}  {label:<label_width$}")
}

/// Return whether the graph block fits the available terminal area.
fn graph_fits(
    graph: &str,
    terminal_width: Option<usize>,
    terminal_height: Option<usize>,
    reserved_lines: usize,
) -> bool {
    if let Some(width) = terminal_width
        && graph.lines().any(|line| rendered_width(line) > width)
    {
        return false;
    }
    if let Some(height) = terminal_height {
        let needed_lines = reserved_lines
            .saturating_add(1)
            .saturating_add(rendered_line_count(graph));
        if needed_lines > height {
            return false;
        }
    }
    true
}

/// Return the largest finite cost in a graph point set.
fn max_graph_cost(points: &[BurnRateHistoryPoint]) -> f64 {
    points
        .iter()
        .map(|point| point.cost_usd_per_hour)
        .filter(|value| value.is_finite())
        .fold(0.0, f64::max)
}

/// Render graph points as fixed-height high-resolution bar plot rows.
fn cost_plot_rows(
    points: &[BurnRateHistoryPoint],
    max_cost: f64,
    borders: super::table::BorderStyle,
) -> Vec<String> {
    (1..=WATCH_GRAPH_BODY_HEIGHT)
        .rev()
        .map(|level| {
            points
                .iter()
                .map(|point| {
                    graph_column_glyph(
                        graph_column_height(point.cost_usd_per_hour, max_cost),
                        level,
                        borders,
                    )
                })
                .collect()
        })
        .collect()
}

/// Return the graph glyph for one high-resolution bar column at one text row.
fn graph_column_glyph(
    column_height: usize,
    row_level: usize,
    borders: super::table::BorderStyle,
) -> char {
    match borders {
        super::table::BorderStyle::Ascii => {
            let glyphs = [' ', '.', ':', '-', '=', '+', '*', '#', '%'];
            let filled_units = graph_row_filled_units(column_height, row_level);
            glyphs[filled_units]
        }
        super::table::BorderStyle::Unicode => {
            let glyphs = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
            let filled_units = graph_row_filled_units(column_height, row_level);
            glyphs[filled_units]
        }
    }
}

/// Return how many vertical subunits are filled inside one text row.
fn graph_row_filled_units(column_height: usize, row_level: usize) -> usize {
    let row_bottom_units = row_level.saturating_sub(1) * WATCH_GRAPH_ROW_UNITS;
    column_height
        .saturating_sub(row_bottom_units)
        .min(WATCH_GRAPH_ROW_UNITS)
}

/// Map a cost value onto the fixed graph height.
fn graph_column_height(value: f64, max_value: f64) -> usize {
    if !value.is_finite() || !max_value.is_finite() || value <= 0.0 || max_value <= 0.0 {
        return 0;
    }
    for height in 1..=WATCH_GRAPH_VERTICAL_UNITS {
        let height_f64 = u32::try_from(height).map_or(WATCH_GRAPH_VERTICAL_UNITS_F64, f64::from);
        let threshold = max_value * height_f64 / WATCH_GRAPH_VERTICAL_UNITS_F64;
        if value <= threshold {
            return height;
        }
    }
    WATCH_GRAPH_VERTICAL_UNITS
}

/// Build the graph axis row with start and end labels aligned on one line.
fn graph_axis_line(
    first: &BurnRateHistoryPoint,
    last: &BurnRateHistoryPoint,
    point_count: usize,
    borders: super::table::BorderStyle,
) -> String {
    let horizontal = match borders {
        super::table::BorderStyle::Unicode => '─',
        super::table::BorderStyle::Ascii => '-',
    };
    let first_width = first.end_time.chars().count();
    let last_width = last.end_time.chars().count();
    let separator_width = point_count
        .saturating_sub(first_width)
        .saturating_sub(last_width)
        .saturating_sub(2);
    format!(
        "{} {} {}",
        first.end_time,
        horizontal.to_string().repeat(separator_width),
        last.end_time
    )
}

/// Count rendered lines in one output block.
fn rendered_line_count(value: &str) -> usize {
    value.lines().count()
}

/// Return the terminal-column width of one rendered graph line.
fn rendered_width(value: &str) -> usize {
    let mut width = 0;
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            let _ = chars.next();
            for sequence_character in chars.by_ref() {
                if sequence_character.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        width += 1;
    }
    width
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
            burn_history: Vec::new(),
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

    fn graph_points(values: &[f64]) -> Vec<BurnRateHistoryPoint> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| BurnRateHistoryPoint {
                end_time: format!("{index:02}:00"),
                cost_usd_per_hour: *value,
            })
            .collect()
    }

    #[test]
    fn cost_plot_rows_use_border_specific_height_palettes() {
        let points = graph_points(&[0.0, 0.25, 2.0]);

        assert_eq!(
            cost_plot_rows(&points, 2.0, BorderStyle::Ascii),
            vec!["  %", "  %", "  %", " =%"]
        );
        assert_eq!(
            cost_plot_rows(&points, 2.0, BorderStyle::Unicode),
            vec!["  █", "  █", "  █", " ▄█"]
        );
    }

    #[test]
    fn append_cost_legend_left_aligns_varying_label_widths() {
        assert_eq!(
            append_cost_legend("plot", "- $9.00/h", 10),
            "plot  - $9.00/h "
        );
        assert_eq!(
            append_cost_legend("plot", "- $10.00/h", 10),
            "plot  - $10.00/h"
        );
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
