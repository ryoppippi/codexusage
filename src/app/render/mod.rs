//! Human-readable table rendering helpers.

mod number;
mod report;
mod table;
mod watch;

pub(super) use report::render_report;
pub(super) use watch::render_watch_screen;

#[cfg(test)]
pub(super) use number::{format_currency, format_u64, format_u64_with};
#[cfg(test)]
pub(super) use table::{
    BorderStyle, TableElement, TableRenderConfig, TableRuleKind, TableStyle,
    detect_border_style_for, detect_table_style_for, format_data_row, paint, table_rule,
    write_table_row,
};
#[cfg(test)]
pub(super) use watch::render_watch_screen_with_width;
