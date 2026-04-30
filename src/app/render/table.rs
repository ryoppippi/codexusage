//! Shared table styling and layout helpers.

use super::super::NumberFormat;
use super::number::format_u64_with;
use std::env;
use std::fmt::Write as _;
use std::io::IsTerminal;

/// One logical row rendered by the table layout helpers.
#[derive(Clone, Debug)]
pub(super) struct DisplayRow {
    /// Cells in table order.
    pub(super) cells: Vec<String>,
    /// Visual styling for the row.
    pub(super) kind: DisplayRowKind,
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
pub(in crate::app) enum TableStyle {
    /// Do not emit ANSI escapes.
    Plain,
    /// Emit 256-color ANSI escapes.
    Ansi256,
}

/// Border style for the rendered table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum BorderStyle {
    /// ASCII-safe borders.
    Ascii,
    /// Unicode box-drawing borders.
    Unicode,
}

/// Rendering controls for the human-readable table.
#[derive(Clone, Copy, Debug)]
pub(in crate::app) struct TableRenderConfig {
    /// ANSI styling mode.
    pub(in crate::app) style: TableStyle,
    /// Border glyph mode.
    pub(in crate::app) borders: BorderStyle,
    /// Numeric display mode.
    pub(in crate::app) number_format: NumberFormat,
}

/// Detect the best table style for the current stdout stream.
///
/// This auto-detect behavior is intentional: the user explicitly chose richer ANSI output on
/// capable terminals and plain text fallback elsewhere.
pub(super) fn detect_table_style() -> TableStyle {
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
pub(super) fn detect_border_style() -> BorderStyle {
    detect_border_style_for(
        std::io::stdout().is_terminal(),
        env::var("LC_ALL").ok().as_deref(),
        env::var("LC_CTYPE").ok().as_deref(),
        env::var("LANG").ok().as_deref(),
    )
}

/// Decide whether Unicode box-drawing is safe for the current environment.
pub(in crate::app) fn detect_border_style_for(
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
pub(in crate::app) fn detect_table_style_for(
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

/// Compute display widths for every table column.
pub(super) fn column_widths(
    headers: &[&str],
    rows: &[DisplayRow],
    grand_total_row: &DisplayRow,
    number_format: NumberFormat,
) -> Vec<usize> {
    let mut widths = headers
        .iter()
        .map(|header| display_width(header))
        .collect::<Vec<_>>();
    for row in rows.iter().chain(std::iter::once(grand_total_row)) {
        for (index, cell) in row.cells.iter().enumerate() {
            widths[index] = widths[index].max(display_width(&format_table_cell(
                headers,
                index,
                cell,
                number_format,
            )));
        }
    }
    widths
}

/// Write the table title.
pub(super) fn write_table_title(output: &mut String, style: TableStyle, title: &str) {
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
pub(super) fn write_table_header(
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
pub(in crate::app) fn format_data_row(
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
pub(in crate::app) fn write_table_row(
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
            let formatted = if should_left_align(headers[index], &display) {
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
pub(in crate::app) enum TableRuleKind {
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
pub(in crate::app) fn table_rule(
    kind: TableRuleKind,
    borders: BorderStyle,
    widths: &[usize],
) -> String {
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
pub(super) fn write_table_rule(
    output: &mut String,
    style: TableStyle,
    element: TableElement,
    line: &str,
) {
    let _ = writeln!(output, "{}", paint(style, element, line));
}

/// Return the rendered table width in terminal columns.
pub(super) fn table_display_width(widths: &[usize]) -> usize {
    widths.iter().sum::<usize>() + (widths.len() * 3) + 1
}

/// Return the number of terminal columns occupied by this string.
fn display_width(value: &str) -> usize {
    value.chars().count()
}

/// Return whether this cell should be left-aligned.
fn should_left_align(header: &str, display: &str) -> bool {
    if header == "Today" && display.contains('-') {
        return true;
    }
    matches!(
        header,
        "Date" | "Month" | "Metric" | "Directory" | "Session" | "Model" | "Last Activity"
    )
}

/// One styleable table element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum TableElement {
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
pub(super) fn row_table_element(kind: DisplayRowKind) -> TableElement {
    match kind {
        DisplayRowKind::Subtotal => TableElement::Subtotal,
        DisplayRowKind::Spacer | DisplayRowKind::Detail => TableElement::Detail,
        DisplayRowKind::GrandTotal => TableElement::GrandTotal,
    }
}

/// Map one rule kind to the border styling bucket.
pub(super) fn table_rule_element(kind: TableRuleKind) -> TableElement {
    match kind {
        TableRuleKind::Top
        | TableRuleKind::HeaderSeparator
        | TableRuleKind::GroupSeparator
        | TableRuleKind::Bottom => TableElement::Border,
    }
}

/// Apply ANSI styling when enabled.
pub(in crate::app) fn paint(style: TableStyle, element: TableElement, text: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_column_right_aligns_numbers_but_keeps_updated_date_readable() {
        let headers = ["Metric", "Today", "Burn Rate (/h)"];
        let widths = [7, 10, 14];
        let numeric_row = format_data_row(
            &headers,
            BorderStyle::Ascii,
            &widths,
            &["Input".to_string(), "120".to_string(), "240".to_string()],
        );
        let updated_row = format_data_row(
            &headers,
            BorderStyle::Ascii,
            &widths,
            &[
                "Updated".to_string(),
                "2026-01-02".to_string(),
                "00:30:00".to_string(),
            ],
        );

        let numeric_cells = numeric_row.split('|').collect::<Vec<_>>();
        assert_eq!(numeric_cells[2], "        120 ");

        let updated_cells = updated_row.split('|').collect::<Vec<_>>();
        assert_eq!(updated_cells[2], " 2026-01-02 ");
    }
}
