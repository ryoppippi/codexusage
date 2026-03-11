use super::{
    DailyRow, ModelBreakdown, MonthlyRow, NumberFormat, ReportOutput, SessionRow, Totals,
    UsageTotals, WatchSnapshot, scale_cost_per_hour, scale_usage_per_hour,
};
use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::io::IsTerminal;

/// Render a report as a text table.
pub(super) fn render_report(
    report: &ReportOutput,
    locale: &str,
    number_format: NumberFormat,
) -> String {
    let mut output = match report {
        ReportOutput::Daily { rows, totals, .. } => {
            render_daily_report(rows, totals, locale, number_format)
        }
        ReportOutput::Monthly { rows, totals, .. } => {
            render_monthly_report(rows, totals, locale, number_format)
        }
        ReportOutput::Session { rows, totals, .. } => {
            render_session_report(rows, totals, locale, number_format)
        }
    };

    let missing_directories = match report {
        ReportOutput::Daily {
            missing_directories,
            ..
        }
        | ReportOutput::Monthly {
            missing_directories,
            ..
        }
        | ReportOutput::Session {
            missing_directories,
            ..
        } => missing_directories,
    };
    if !missing_directories.is_empty() {
        let mut warning = String::from("Warning: missing session directories\n");
        for directory in missing_directories {
            let _ = writeln!(&mut warning, "- {directory}");
        }
        warning.push('\n');
        warning.push_str(&output);
        output = warning;
    }

    output
}

/// Render one live watch snapshot.
pub(super) fn render_watch_screen(
    snapshot: &WatchSnapshot,
    _locale: &str,
    number_format: NumberFormat,
    show_model_burn_rate: bool,
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

/// Render the watch metrics table.
fn write_watch_table(
    output: &mut String,
    render_config: TableRenderConfig,
    snapshot: &WatchSnapshot,
    number_format: NumberFormat,
    show_model_burn_rate: bool,
) {
    let model_columns = if show_model_burn_rate {
        active_watch_burn_columns(snapshot)
    } else {
        Vec::new()
    };
    let mut header_strings = vec!["Metric".to_string(), "Today".to_string()];
    header_strings.extend(model_columns.iter().map(|column| column.label.clone()));
    header_strings.push("Burn Rate (/h)".to_string());
    let headers = header_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let rows = watch_rows(snapshot, number_format, &model_columns);
    let mut updated_cells = vec!["Updated".to_string(), snapshot.date.clone()];
    updated_cells.extend(vec![String::new(); model_columns.len()]);
    updated_cells.push(snapshot.updated_time.clone());
    let updated_row = DisplayRow {
        cells: updated_cells,
        kind: DisplayRowKind::GrandTotal,
    };
    let widths = column_widths(&headers, &rows, &updated_row, number_format);

    write_table_header(output, render_config, &headers, &widths);
    for row in rows {
        write_table_row(
            output,
            render_config,
            &headers,
            &widths,
            &row.cells,
            row_table_element(row.kind),
        );
    }
    write_table_row(
        output,
        render_config,
        &headers,
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

/// Render the daily report body.
fn render_daily_report(
    rows: &[DailyRow],
    totals: &Totals,
    locale: &str,
    number_format: NumberFormat,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    render_usage_table(
        "Daily",
        render_config,
        locale,
        &[
            "Date",
            "Model",
            "Input",
            "Cache",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
        ],
        daily_display_rows(rows),
        totals,
    )
}

/// Render the monthly report body.
fn render_monthly_report(
    rows: &[MonthlyRow],
    totals: &Totals,
    locale: &str,
    number_format: NumberFormat,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    render_usage_table(
        "Monthly",
        render_config,
        locale,
        &[
            "Month",
            "Model",
            "Input",
            "Cache",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
        ],
        monthly_display_rows(rows),
        totals,
    )
}

/// Render the session report body.
fn render_session_report(
    rows: &[SessionRow],
    totals: &Totals,
    locale: &str,
    number_format: NumberFormat,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    render_usage_table(
        "Session",
        render_config,
        locale,
        &[
            "Directory",
            "Session",
            "Model",
            "Input",
            "Cache",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
            "Last Activity",
        ],
        session_display_rows(rows),
        totals,
    )
}

/// One rendered table row.
#[derive(Clone, Debug)]
struct DisplayRow {
    /// Cells in table order.
    cells: Vec<String>,
    /// Visual styling for the row.
    kind: DisplayRowKind,
}

/// Styling group for one display row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DisplayRowKind {
    /// Insert a blank line before the next row.
    Spacer,
    /// Group subtotal row.
    Subtotal,
    /// Per-model child row.
    Detail,
    /// Final grand total row.
    GrandTotal,
}

/// Terminal color mode for table rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TableStyle {
    /// Do not emit ANSI escapes.
    Plain,
    /// Emit 256-color ANSI escapes.
    Ansi256,
}

/// Border style for the rendered table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BorderStyle {
    /// ASCII-safe borders.
    Ascii,
    /// Unicode box-drawing borders.
    Unicode,
}

/// Rendering controls for the human-readable table.
#[derive(Clone, Copy, Debug)]
pub(super) struct TableRenderConfig {
    /// ANSI styling mode.
    pub(super) style: TableStyle,
    /// Border glyph mode.
    pub(super) borders: BorderStyle,
    /// Numeric display mode.
    pub(super) number_format: NumberFormat,
}

/// Detect the best table style for the current stdout stream.
///
/// This auto-detect behavior is intentional: the user explicitly chose richer ANSI output on
/// capable terminals and plain text fallback elsewhere.
fn detect_table_style() -> TableStyle {
    detect_table_style_for(
        std::io::stdout().is_terminal(),
        env::var("TERM").ok().as_deref(),
        env::var("COLORTERM").ok().as_deref(),
        env::var_os("NO_COLOR").is_some(),
    )
}

/// Detect the best border style for the current stdout stream.
///
/// This auto-detect behavior is intentional: the user explicitly chose Unicode box-drawing on
/// UTF-8 terminals and ASCII fallback elsewhere.
fn detect_border_style() -> BorderStyle {
    detect_border_style_for(
        std::io::stdout().is_terminal(),
        env::var("LC_ALL").ok().as_deref(),
        env::var("LC_CTYPE").ok().as_deref(),
        env::var("LANG").ok().as_deref(),
    )
}

/// Decide whether Unicode box-drawing is safe for the current environment.
pub(super) fn detect_border_style_for(
    stdout_is_terminal: bool,
    lc_all: Option<&str>,
    lc_ctype: Option<&str>,
    lang: Option<&str>,
) -> BorderStyle {
    if !stdout_is_terminal {
        return BorderStyle::Ascii;
    }

    let locale = lc_all
        .filter(|value| !value.is_empty())
        .or(lc_ctype.filter(|value| !value.is_empty()))
        .or(lang.filter(|value| !value.is_empty()))
        .unwrap_or_default()
        .to_ascii_lowercase();

    if locale.contains("utf-8") || locale.contains("utf8") {
        BorderStyle::Unicode
    } else {
        BorderStyle::Ascii
    }
}

/// Decide whether styled output should be enabled.
pub(super) fn detect_table_style_for(
    stdout_is_terminal: bool,
    term: Option<&str>,
    colorterm: Option<&str>,
    no_color: bool,
) -> TableStyle {
    if no_color || !stdout_is_terminal {
        return TableStyle::Plain;
    }

    let term = term.unwrap_or_default();
    if term.is_empty() || term == "dumb" {
        return TableStyle::Plain;
    }

    let colorterm = colorterm.unwrap_or_default();
    if term.contains("256color")
        || colorterm.eq_ignore_ascii_case("truecolor")
        || colorterm.eq_ignore_ascii_case("24bit")
    {
        TableStyle::Ansi256
    } else {
        TableStyle::Plain
    }
}

/// Build display rows for a daily report.
fn daily_display_rows(rows: &[DailyRow]) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            display_rows.push(DisplayRow {
                cells: Vec::new(),
                kind: DisplayRowKind::Spacer,
            });
        }
        display_rows.push(DisplayRow {
            cells: vec![
                row.date.clone(),
                "TOTAL".to_string(),
                row.input_tokens.to_string(),
                row.cached_input_tokens.to_string(),
                row.output_tokens.to_string(),
                row.reasoning_output_tokens.to_string(),
                row.total_tokens.to_string(),
                format_currency(row.cost_usd),
            ],
            kind: DisplayRowKind::Subtotal,
        });
        append_model_display_rows(&mut display_rows, 1, false, &row.models);
    }
    display_rows
}

/// Build display rows for a monthly report.
fn monthly_display_rows(rows: &[MonthlyRow]) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            display_rows.push(DisplayRow {
                cells: Vec::new(),
                kind: DisplayRowKind::Spacer,
            });
        }
        display_rows.push(DisplayRow {
            cells: vec![
                row.month.clone(),
                "TOTAL".to_string(),
                row.input_tokens.to_string(),
                row.cached_input_tokens.to_string(),
                row.output_tokens.to_string(),
                row.reasoning_output_tokens.to_string(),
                row.total_tokens.to_string(),
                format_currency(row.cost_usd),
            ],
            kind: DisplayRowKind::Subtotal,
        });
        append_model_display_rows(&mut display_rows, 1, false, &row.models);
    }
    display_rows
}

/// Build display rows for a session report.
fn session_display_rows(rows: &[SessionRow]) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            display_rows.push(DisplayRow {
                cells: Vec::new(),
                kind: DisplayRowKind::Spacer,
            });
        }
        display_rows.push(DisplayRow {
            cells: vec![
                if row.directory.is_empty() {
                    "-".to_string()
                } else {
                    row.directory.clone()
                },
                row.session_file.clone(),
                "TOTAL".to_string(),
                row.input_tokens.to_string(),
                row.cached_input_tokens.to_string(),
                row.output_tokens.to_string(),
                row.reasoning_output_tokens.to_string(),
                row.total_tokens.to_string(),
                format_currency(row.cost_usd),
                row.last_activity.clone(),
            ],
            kind: DisplayRowKind::Subtotal,
        });
        append_model_display_rows(&mut display_rows, 2, true, &row.models);
    }
    display_rows
}

/// Append child rows for every model in a group.
fn append_model_display_rows(
    display_rows: &mut Vec<DisplayRow>,
    columns_before_model: usize,
    include_last_activity_column: bool,
    models: &BTreeMap<String, ModelBreakdown>,
) {
    for (model, breakdown) in models {
        let explicit_usage = explicit_usage(breakdown);
        if explicit_usage.has_usage() {
            display_rows.push(model_display_row(
                columns_before_model,
                include_last_activity_column,
                model,
                &explicit_usage,
                breakdown.cost_usd,
            ));
        }

        if breakdown.fallback_usage.has_usage() {
            display_rows.push(model_display_row(
                columns_before_model,
                include_last_activity_column,
                &format!("{model} (fallback)"),
                &breakdown.fallback_usage,
                breakdown.fallback_cost_usd,
            ));
        }
    }
}

/// Build one child row for a model breakdown.
fn model_display_row(
    columns_before_model: usize,
    include_last_activity_column: bool,
    model_label: &str,
    usage: &UsageTotals,
    cost_usd: f64,
) -> DisplayRow {
    let mut cells = vec![String::new(); columns_before_model];
    cells.push(format!("  {model_label}"));
    cells.extend_from_slice(&[
        usage.input.to_string(),
        usage.cached_input.to_string(),
        usage.output.to_string(),
        usage.reasoning_output.to_string(),
        usage.total.to_string(),
        format_currency(cost_usd),
    ]);
    if include_last_activity_column {
        cells.push(String::new());
    }

    DisplayRow {
        cells,
        kind: DisplayRowKind::Detail,
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

/// Build one metric row for watch mode with optional per-model burn columns.
fn watch_metric_row(
    metric: &str,
    today: String,
    per_model: Vec<String>,
    burn_rate: String,
) -> DisplayRow {
    let mut cells = Vec::with_capacity(per_model.len() + 3);
    cells.push(metric.to_string());
    cells.push(today);
    cells.extend(per_model);
    cells.push(burn_rate);
    DisplayRow {
        cells,
        kind: DisplayRowKind::Subtotal,
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
) -> Vec<DisplayRow> {
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

    vec![
        watch_metric_row(
            "Input",
            format_u64_with(snapshot.totals.input_tokens, number_format),
            token_cells(|usage| usage.input),
            format_u64_with(snapshot.burn_rate.input_tokens_per_hour, number_format),
        ),
        watch_metric_row(
            "Cache",
            format_u64_with(snapshot.totals.cached_input_tokens, number_format),
            token_cells(|usage| usage.cached_input),
            format_u64_with(
                snapshot.burn_rate.cached_input_tokens_per_hour,
                number_format,
            ),
        ),
        watch_metric_row(
            "Output",
            format_u64_with(snapshot.totals.output_tokens, number_format),
            token_cells(|usage| usage.output),
            format_u64_with(snapshot.burn_rate.output_tokens_per_hour, number_format),
        ),
        watch_metric_row(
            "Reasoning",
            format_u64_with(snapshot.totals.reasoning_output_tokens, number_format),
            token_cells(|usage| usage.reasoning_output),
            format_u64_with(
                snapshot.burn_rate.reasoning_output_tokens_per_hour,
                number_format,
            ),
        ),
        watch_metric_row(
            "Total",
            format_u64_with(snapshot.totals.total_tokens, number_format),
            token_cells(|usage| usage.total),
            format_u64_with(snapshot.burn_rate.total_tokens_per_hour, number_format),
        ),
        watch_metric_row(
            "Cost",
            format_currency(snapshot.totals.cost_usd),
            cost_cells,
            format_currency(snapshot.burn_rate.cost_usd_per_hour),
        ),
    ]
}

/// Return the explicit portion of a mixed model breakdown.
pub(super) fn explicit_usage(breakdown: &ModelBreakdown) -> UsageTotals {
    UsageTotals {
        input: breakdown
            .input_tokens
            .saturating_sub(breakdown.fallback_usage.input),
        cached_input: breakdown
            .cached_input_tokens
            .saturating_sub(breakdown.fallback_usage.cached_input),
        output: breakdown
            .output_tokens
            .saturating_sub(breakdown.fallback_usage.output),
        reasoning_output: breakdown
            .reasoning_output_tokens
            .saturating_sub(breakdown.fallback_usage.reasoning_output),
        total: breakdown
            .total_tokens
            .saturating_sub(breakdown.fallback_usage.total),
    }
}

/// Render a rectangular table with grouped rows and a totals row.
fn render_usage_table(
    title: &str,
    render_config: TableRenderConfig,
    _locale: &str,
    headers: &[&str],
    rows: Vec<DisplayRow>,
    totals: &Totals,
) -> String {
    let mut all_rows = rows;
    let grand_total_row = grand_total_row(headers, totals);
    let widths = column_widths(
        headers,
        &all_rows,
        &grand_total_row,
        render_config.number_format,
    );
    all_rows.push(grand_total_row);

    let mut output = String::new();
    write_table_title(&mut output, render_config.style, title);
    let _ = writeln!(&mut output);
    write_table_header(&mut output, render_config, headers, &widths);
    for row in all_rows {
        if row.kind == DisplayRowKind::Spacer {
            write_table_rule(
                &mut output,
                render_config.style,
                table_rule_element(TableRuleKind::GroupSeparator),
                &table_rule(
                    TableRuleKind::GroupSeparator,
                    render_config.borders,
                    &widths,
                ),
            );
            continue;
        }
        write_table_row(
            &mut output,
            render_config,
            headers,
            &widths,
            &row.cells,
            row_table_element(row.kind),
        );
    }
    write_table_rule(
        &mut output,
        render_config.style,
        table_rule_element(TableRuleKind::Bottom),
        &table_rule(TableRuleKind::Bottom, render_config.borders, &widths),
    );
    output
}

/// Build the final grand total row for a table.
fn grand_total_row(headers: &[&str], totals: &Totals) -> DisplayRow {
    let cells = if headers.first() == Some(&"Directory") {
        vec![
            String::new(),
            String::new(),
            "GRAND TOTAL".to_string(),
            totals.input_tokens.to_string(),
            totals.cached_input_tokens.to_string(),
            totals.output_tokens.to_string(),
            totals.reasoning_output_tokens.to_string(),
            totals.total_tokens.to_string(),
            format_currency(totals.cost_usd),
            String::new(),
        ]
    } else {
        vec![
            String::new(),
            "GRAND TOTAL".to_string(),
            totals.input_tokens.to_string(),
            totals.cached_input_tokens.to_string(),
            totals.output_tokens.to_string(),
            totals.reasoning_output_tokens.to_string(),
            totals.total_tokens.to_string(),
            format_currency(totals.cost_usd),
        ]
    };

    DisplayRow {
        cells,
        kind: DisplayRowKind::GrandTotal,
    }
}

/// Compute the display width of every column.
fn column_widths(
    headers: &[&str],
    rows: &[DisplayRow],
    grand_total_row: &DisplayRow,
    number_format: NumberFormat,
) -> Vec<usize> {
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in rows.iter().chain(std::iter::once(grand_total_row)) {
        for (index, cell) in row.cells.iter().enumerate() {
            widths[index] =
                widths[index].max(format_table_cell(headers, index, cell, number_format).len());
        }
    }
    widths
}

/// Write the table title.
fn write_table_title(output: &mut String, style: TableStyle, title: &str) {
    let _ = writeln!(
        output,
        "{}",
        paint(
            style,
            TableElement::Title,
            &format!("{title} Codex Usage Report")
        )
    );
}

/// Write the table header and separator.
fn write_table_header(
    output: &mut String,
    render_config: TableRenderConfig,
    headers: &[&str],
    widths: &[usize],
) {
    write_table_rule(
        output,
        render_config.style,
        table_rule_element(TableRuleKind::Top),
        &table_rule(TableRuleKind::Top, render_config.borders, widths),
    );
    let header_cells = headers
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    write_table_row(
        output,
        render_config,
        headers,
        widths,
        &header_cells,
        TableElement::Header,
    );
    write_table_rule(
        output,
        render_config.style,
        table_rule_element(TableRuleKind::HeaderSeparator),
        &table_rule(
            TableRuleKind::HeaderSeparator,
            render_config.borders,
            widths,
        ),
    );
}

/// Format one data row with aligned cells.
#[cfg(test)]
pub(super) fn format_data_row(
    headers: &[&str],
    borders: BorderStyle,
    widths: &[usize],
    cells: &[String],
) -> String {
    let theme = border_theme(borders);
    let body = format_aligned_cells(headers, widths, cells, NumberFormat::Full)
        .join(&theme.vertical.to_string());
    format!("{}{}{}", theme.vertical, body, theme.vertical)
}

/// Render one row with border segments styled independently from the cell text.
pub(super) fn write_table_row(
    output: &mut String,
    render_config: TableRenderConfig,
    headers: &[&str],
    widths: &[usize],
    cells: &[String],
    cell_element: TableElement,
) {
    let theme = border_theme(render_config.borders);
    let border = paint(
        render_config.style,
        TableElement::Border,
        &theme.vertical.to_string(),
    );
    let separator = paint(
        render_config.style,
        TableElement::Border,
        &theme.vertical.to_string(),
    );
    let styled_cells = format_aligned_cells(headers, widths, cells, render_config.number_format)
        .into_iter()
        .map(|cell| paint(render_config.style, cell_element, &cell))
        .collect::<Vec<_>>();
    let _ = writeln!(
        output,
        "{}{}{}",
        border,
        styled_cells.join(&separator),
        border
    );
}

/// Align row cells before borders or colors are applied.
fn format_aligned_cells(
    headers: &[&str],
    widths: &[usize],
    cells: &[String],
    number_format: NumberFormat,
) -> Vec<String> {
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let display = format_table_cell(headers, index, cell, number_format);
            let formatted = if index <= text_column_limit(headers)
                || headers[index] == "Directory"
                || headers[index] == "Session"
                || headers[index] == "Model"
                || headers[index] == "Last Activity"
            {
                format!("{display:width$}", width = widths[index])
            } else {
                format!("{display:>width$}", width = widths[index])
            };
            format!(" {formatted} ")
        })
        .collect()
}

/// Format one table cell according to its column semantics.
fn format_table_cell(
    headers: &[&str],
    index: usize,
    cell: &str,
    number_format: NumberFormat,
) -> String {
    if is_token_column(headers[index]) {
        cell.parse::<u64>().map_or_else(
            |_| cell.to_string(),
            |value| format_u64_with(value, number_format),
        )
    } else {
        cell.to_string()
    }
}

/// Return whether the table column contains token counts.
fn is_token_column(header: &str) -> bool {
    matches!(header, "Input" | "Cache" | "Output" | "Reasoning" | "Total")
}

/// One row-rule kind for the table frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TableRuleKind {
    /// Top border.
    Top,
    /// Header separator.
    HeaderSeparator,
    /// Group separator between report sections.
    GroupSeparator,
    /// Bottom border.
    Bottom,
}

/// Static characters used to draw a table.
#[derive(Clone, Copy, Debug)]
struct BorderTheme {
    /// Horizontal line segment.
    horizontal: char,
    /// Outer vertical border.
    vertical: char,
    /// Left corner/junction.
    left: char,
    /// Inner junction.
    middle: char,
    /// Right corner/junction.
    right: char,
}

/// Return the concrete drawing theme for one border style.
fn border_theme(style: BorderStyle) -> BorderTheme {
    match style {
        BorderStyle::Ascii => BorderTheme {
            horizontal: '-',
            vertical: '|',
            left: '+',
            middle: '+',
            right: '+',
        },
        BorderStyle::Unicode => BorderTheme {
            horizontal: '─',
            vertical: '│',
            left: '┌',
            middle: '┬',
            right: '┐',
        },
    }
}

/// Return the concrete drawing theme for one border rule.
fn rule_theme(kind: TableRuleKind, style: BorderStyle) -> BorderTheme {
    match (style, kind) {
        (BorderStyle::Ascii, _) => border_theme(style),
        (BorderStyle::Unicode, TableRuleKind::Top) => BorderTheme {
            horizontal: '─',
            vertical: '│',
            left: '┌',
            middle: '┬',
            right: '┐',
        },
        (BorderStyle::Unicode, TableRuleKind::HeaderSeparator | TableRuleKind::GroupSeparator) => {
            BorderTheme {
                horizontal: '─',
                vertical: '│',
                left: '├',
                middle: '┼',
                right: '┤',
            }
        }
        (BorderStyle::Unicode, TableRuleKind::Bottom) => BorderTheme {
            horizontal: '─',
            vertical: '│',
            left: '└',
            middle: '┴',
            right: '┘',
        },
    }
}

/// Build one table border rule line.
pub(super) fn table_rule(kind: TableRuleKind, borders: BorderStyle, widths: &[usize]) -> String {
    let theme = rule_theme(kind, borders);
    let segments = widths
        .iter()
        .map(|width| theme.horizontal.to_string().repeat(width + 2))
        .collect::<Vec<_>>();
    format!(
        "{}{}{}",
        theme.left,
        segments.join(&theme.middle.to_string()),
        theme.right
    )
}

/// Write one border rule with styling.
fn write_table_rule(output: &mut String, style: TableStyle, element: TableElement, line: &str) {
    let _ = writeln!(output, "{}", paint(style, element, line));
}

/// How many leading columns should left-align in this table.
fn text_column_limit(headers: &[&str]) -> usize {
    if headers.first() == Some(&"Directory") {
        2
    } else {
        1
    }
}

/// One styleable table element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TableElement {
    /// Report title.
    Title,
    /// Header row.
    Header,
    /// Separator row.
    Border,
    /// Group subtotal row.
    Subtotal,
    /// Model detail row.
    Detail,
    /// Grand total row.
    GrandTotal,
}

/// Map one display row kind to its table element style.
fn row_table_element(kind: DisplayRowKind) -> TableElement {
    match kind {
        DisplayRowKind::Subtotal => TableElement::Subtotal,
        DisplayRowKind::Spacer | DisplayRowKind::Detail => TableElement::Detail,
        DisplayRowKind::GrandTotal => TableElement::GrandTotal,
    }
}

/// Map one rule kind to the border styling bucket.
fn table_rule_element(kind: TableRuleKind) -> TableElement {
    match kind {
        TableRuleKind::Top
        | TableRuleKind::HeaderSeparator
        | TableRuleKind::GroupSeparator
        | TableRuleKind::Bottom => TableElement::Border,
    }
}

/// Apply ANSI styling when enabled.
pub(super) fn paint(style: TableStyle, element: TableElement, text: &str) -> String {
    match style {
        TableStyle::Plain => text.to_string(),
        TableStyle::Ansi256 => {
            let sequence = match element {
                TableElement::Title => "\u{1b}[1;38;5;81m",
                TableElement::Header => "\u{1b}[1;38;5;45m",
                TableElement::Border => "\u{1b}[38;5;24m",
                TableElement::Subtotal => "\u{1b}[1;38;5;117m",
                TableElement::Detail => "\u{1b}[38;5;153m",
                TableElement::GrandTotal => "\u{1b}[1;38;5;39m",
            };
            format!("{sequence}{text}\u{1b}[0m")
        }
    }
}

/// Format an integer with group separators.
pub(super) fn format_u64(value: u64) -> String {
    let raw = value.to_string();
    let mut output = String::new();
    let chars = raw.chars().rev().collect::<Vec<_>>();
    for (index, character) in chars.iter().enumerate() {
        if index > 0 && index % 3 == 0 {
            output.push(',');
        }
        output.push(*character);
    }
    output.chars().rev().collect()
}

/// Format an integer according to the selected display mode.
pub(super) fn format_u64_with(value: u64, number_format: NumberFormat) -> String {
    match number_format {
        NumberFormat::Full => format_u64(value),
        NumberFormat::Short => format_u64_short(value),
    }
}

/// Format an integer using 3 significant digits and K/M/B/T suffixes.
fn format_u64_short(value: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1_000, "K"),
        (1_000_000, "M"),
        (1_000_000_000, "B"),
        (1_000_000_000_000, "T"),
    ];

    if value < UNITS[0].0 {
        return value.to_string();
    }

    let mut unit_index = UNITS
        .iter()
        .enumerate()
        .rfind(|(_index, (divisor, _suffix))| value >= *divisor)
        .map_or(0, |(index, _unit)| index);

    loop {
        let (divisor, suffix) = UNITS[unit_index];
        let whole = value / divisor;
        let decimals: u32 = if whole >= 100 {
            0
        } else if whole >= 10 {
            1
        } else {
            2
        };
        let multiplier = 10_u128.pow(decimals);
        let rounded_units =
            ((u128::from(value) * multiplier) + (u128::from(divisor) / 2)) / u128::from(divisor);

        if rounded_units >= 1_000 * multiplier && unit_index + 1 < UNITS.len() {
            unit_index += 1;
            continue;
        }

        return format_short_with_suffix(rounded_units, decimals, suffix);
    }
}

/// Format a rounded abbreviated value and trim redundant trailing zeros.
fn format_short_with_suffix(value: u128, decimals: u32, suffix: &str) -> String {
    if decimals == 0 {
        return format!("{value}{suffix}");
    }

    let divisor = 10_u128.pow(decimals);
    let integer = value / divisor;
    let fractional = value % divisor;
    let fractional_width = usize::try_from(decimals).expect("decimal width fits usize");
    let mut number = format!("{integer}.{fractional:0fractional_width$}");
    while number.ends_with('0') {
        number.pop();
    }
    if number.ends_with('.') {
        number.pop();
    }
    format!("{number}{suffix}")
}

/// Format USD with two decimal places.
pub(super) fn format_currency(value: f64) -> String {
    format!("${value:.2}")
}
