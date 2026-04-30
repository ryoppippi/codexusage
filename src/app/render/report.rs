//! Report table rendering.

use super::super::model::explicit_usage;
use super::super::{
    CacheReadMode, DailyRow, ModelBreakdown, MonthlyRow, NumberFormat, ReportOutput, SessionRow,
    Totals, UsageTotals,
};
use super::number::format_currency;
use super::table::{
    DisplayRow, DisplayRowKind, TableRenderConfig, TableRuleKind, column_widths,
    detect_border_style, detect_table_style, row_table_element, table_rule, table_rule_element,
    write_table_header, write_table_row, write_table_rule, write_table_title,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Render a report output into the human-readable table format.
pub(in crate::app) fn render_report(
    report: &ReportOutput,
    locale: &str,
    number_format: NumberFormat,
    cache_read_mode: CacheReadMode,
) -> String {
    let mut output = match report {
        ReportOutput::Daily { rows, totals, .. } => {
            render_daily_report(rows, totals, locale, number_format, cache_read_mode)
        }
        ReportOutput::Monthly { rows, totals, .. } => {
            render_monthly_report(rows, totals, locale, number_format, cache_read_mode)
        }
        ReportOutput::Session { rows, totals, .. } => {
            render_session_report(rows, totals, locale, number_format, cache_read_mode)
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

/// Render the daily report body.
fn render_daily_report(
    rows: &[DailyRow],
    totals: &Totals,
    locale: &str,
    number_format: NumberFormat,
    cache_read_mode: CacheReadMode,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    let headers = if cache_read_mode == CacheReadMode::Exclude {
        vec![
            "Date",
            "Model",
            "Input",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
        ]
    } else {
        vec![
            "Date",
            "Model",
            "Input",
            "Cache",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
        ]
    };
    render_usage_table(
        "Daily",
        render_config,
        locale,
        &headers,
        daily_display_rows(rows, cache_read_mode),
        totals,
    )
}

/// Render the monthly report body.
fn render_monthly_report(
    rows: &[MonthlyRow],
    totals: &Totals,
    locale: &str,
    number_format: NumberFormat,
    cache_read_mode: CacheReadMode,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    let headers = if cache_read_mode == CacheReadMode::Exclude {
        vec![
            "Month",
            "Model",
            "Input",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
        ]
    } else {
        vec![
            "Month",
            "Model",
            "Input",
            "Cache",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
        ]
    };
    render_usage_table(
        "Monthly",
        render_config,
        locale,
        &headers,
        monthly_display_rows(rows, cache_read_mode),
        totals,
    )
}

/// Render the session report body.
fn render_session_report(
    rows: &[SessionRow],
    totals: &Totals,
    locale: &str,
    number_format: NumberFormat,
    cache_read_mode: CacheReadMode,
) -> String {
    let render_config = TableRenderConfig {
        style: detect_table_style(),
        borders: detect_border_style(),
        number_format,
    };
    let headers = if cache_read_mode == CacheReadMode::Exclude {
        vec![
            "Directory",
            "Session",
            "Model",
            "Input",
            "Output",
            "Reasoning",
            "Total",
            "Cost",
            "Last Activity",
        ]
    } else {
        vec![
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
        ]
    };
    render_usage_table(
        "Session",
        render_config,
        locale,
        &headers,
        session_display_rows(rows, cache_read_mode),
        totals,
    )
}

/// One rendered table row.
fn daily_display_rows(rows: &[DailyRow], cache_read_mode: CacheReadMode) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            display_rows.push(DisplayRow {
                cells: Vec::new(),
                kind: DisplayRowKind::Spacer,
            });
        }
        let mut subtotal = DisplayRow {
            cells: vec![row.date.clone(), "TOTAL".to_string()],
            kind: DisplayRowKind::Subtotal,
        };
        append_usage_cells(
            &mut subtotal.cells,
            row.input_tokens,
            row.cached_input_tokens,
            row.output_tokens,
            row.reasoning_output_tokens,
            row.total_tokens,
            row.cost_usd,
            cache_read_mode,
        );
        display_rows.push(subtotal);
        append_model_display_rows(&mut display_rows, 1, false, &row.models, cache_read_mode);
    }
    display_rows
}

/// Build display rows for a monthly report.
fn monthly_display_rows(rows: &[MonthlyRow], cache_read_mode: CacheReadMode) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            display_rows.push(DisplayRow {
                cells: Vec::new(),
                kind: DisplayRowKind::Spacer,
            });
        }
        let mut subtotal = DisplayRow {
            cells: vec![row.month.clone(), "TOTAL".to_string()],
            kind: DisplayRowKind::Subtotal,
        };
        append_usage_cells(
            &mut subtotal.cells,
            row.input_tokens,
            row.cached_input_tokens,
            row.output_tokens,
            row.reasoning_output_tokens,
            row.total_tokens,
            row.cost_usd,
            cache_read_mode,
        );
        display_rows.push(subtotal);
        append_model_display_rows(&mut display_rows, 1, false, &row.models, cache_read_mode);
    }
    display_rows
}

/// Build display rows for a session report.
fn session_display_rows(rows: &[SessionRow], cache_read_mode: CacheReadMode) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            display_rows.push(DisplayRow {
                cells: Vec::new(),
                kind: DisplayRowKind::Spacer,
            });
        }
        let mut subtotal = DisplayRow {
            cells: vec![
                if row.directory.is_empty() {
                    "-".to_string()
                } else {
                    row.directory.clone()
                },
                row.session_file.clone(),
                "TOTAL".to_string(),
            ],
            kind: DisplayRowKind::Subtotal,
        };
        append_usage_cells(
            &mut subtotal.cells,
            row.input_tokens,
            row.cached_input_tokens,
            row.output_tokens,
            row.reasoning_output_tokens,
            row.total_tokens,
            row.cost_usd,
            cache_read_mode,
        );
        subtotal.cells.push(row.last_activity.clone());
        display_rows.push(subtotal);
        append_model_display_rows(&mut display_rows, 2, true, &row.models, cache_read_mode);
    }
    display_rows
}

/// Append usage and cost cells in the active cache-read column layout.
#[allow(
    clippy::too_many_arguments,
    reason = "keeps display-row construction explicit at each call site"
)]
fn append_usage_cells(
    cells: &mut Vec<String>,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
    cost_usd: f64,
    cache_read_mode: CacheReadMode,
) {
    cells.push(input_tokens.to_string());
    if cache_read_mode == CacheReadMode::Include {
        cells.push(cached_input_tokens.to_string());
    }
    cells.push(output_tokens.to_string());
    cells.push(reasoning_output_tokens.to_string());
    cells.push(total_tokens.to_string());
    cells.push(format_currency(cost_usd));
}

/// Append child rows for every model in a group.
fn append_model_display_rows(
    display_rows: &mut Vec<DisplayRow>,
    columns_before_model: usize,
    include_last_activity_column: bool,
    models: &BTreeMap<String, ModelBreakdown>,
    cache_read_mode: CacheReadMode,
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
                cache_read_mode,
            ));
        }

        if breakdown.fallback_usage.has_usage() {
            display_rows.push(model_display_row(
                columns_before_model,
                include_last_activity_column,
                &format!("{model} (fallback)"),
                &breakdown.fallback_usage,
                breakdown.fallback_cost_usd,
                cache_read_mode,
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
    cache_read_mode: CacheReadMode,
) -> DisplayRow {
    let mut cells = vec![String::new(); columns_before_model];
    cells.push(format!("  {model_label}"));
    append_usage_cells(
        &mut cells,
        usage.input,
        usage.cached_input,
        usage.output,
        usage.reasoning_output,
        usage.total,
        cost_usd,
        cache_read_mode,
    );
    if include_last_activity_column {
        cells.push(String::new());
    }

    DisplayRow {
        cells,
        kind: DisplayRowKind::Detail,
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
    let cache_read_mode = if headers.contains(&"Cache") {
        CacheReadMode::Include
    } else {
        CacheReadMode::Exclude
    };
    let mut cells = if headers.first() == Some(&"Directory") {
        vec![String::new(), String::new(), "GRAND TOTAL".to_string()]
    } else {
        vec![String::new(), "GRAND TOTAL".to_string()]
    };
    append_usage_cells(
        &mut cells,
        totals.input_tokens,
        totals.cached_input_tokens,
        totals.output_tokens,
        totals.reasoning_output_tokens,
        totals.total_tokens,
        totals.cost_usd,
        cache_read_mode,
    );
    if headers.first() == Some(&"Directory") {
        cells.push(String::new());
    }

    DisplayRow {
        cells,
        kind: DisplayRowKind::GrandTotal,
    }
}
