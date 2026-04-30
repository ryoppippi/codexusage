//! Numeric display formatting helpers.

use super::super::NumberFormat;

/// Format an integer with thousands separators.
pub(in crate::app) fn format_u64(value: u64) -> String {
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
pub(in crate::app) fn format_u64_with(value: u64, number_format: NumberFormat) -> String {
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
pub(in crate::app) fn format_currency(value: f64) -> String {
    format!("${value:.2}")
}
