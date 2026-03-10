use codexusage::app::{
    NumberFormat, ReportKind, ReportOptions, ReportOutput, ScannerParallelism, build_report,
};
use std::fs;
use std::num::NonZeroUsize;
use tempfile::TempDir;

fn write_session(temp: &TempDir, relative_path: &str, contents: &str) {
    let path = temp.path().join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write session");
}

fn options(session_dir: &std::path::Path) -> ReportOptions {
    ReportOptions {
        since: None,
        until: None,
        last_days: None,
        timezone: "UTC".to_string(),
        locale: "en-US".to_string(),
        number_format: NumberFormat::Short,
        json: true,
        offline: true,
        refresh_pricing: false,
        session_dirs: vec![session_dir.to_path_buf()],
        parallelism: ScannerParallelism::Auto,
    }
}

#[test]
fn monthly_report_groups_rows_by_month() {
    let temp = TempDir::new().expect("tempdir");
    write_session(
        &temp,
        "sessions/project/session.jsonl",
        concat!(
            "{\"timestamp\":\"2025-08-31T23:30:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5-codex\"}}\n",
            "{\"timestamp\":\"2025-08-31T23:31:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1000,\"cached_input_tokens\":200,\"output_tokens\":500,\"reasoning_output_tokens\":0,\"total_tokens\":1500}}}}\n",
            "{\"timestamp\":\"2025-09-01T00:31:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":800,\"cached_input_tokens\":100,\"output_tokens\":250,\"reasoning_output_tokens\":0,\"total_tokens\":1050}}}}\n"
        ),
    );

    let report = build_report(ReportKind::Monthly, &options(&temp.path().join("sessions")))
        .expect("build report");

    match report {
        ReportOutput::Monthly { rows, totals, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].month, "2025-08");
            assert_eq!(rows[1].month, "2025-09");
            assert!(
                rows[0].cost_usd > 0.0,
                "embedded pricing should price gpt-5-codex"
            );
            assert_eq!(totals.total_tokens, 2_550);
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

#[test]
fn date_filters_are_inclusive() {
    let temp = TempDir::new().expect("tempdir");
    write_session(
        &temp,
        "sessions/project/session.jsonl",
        concat!(
            "{\"timestamp\":\"2025-09-10T23:30:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2025-09-10T23:31:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n",
            "{\"timestamp\":\"2025-09-11T23:31:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":200,\"cached_input_tokens\":0,\"output_tokens\":20,\"reasoning_output_tokens\":0,\"total_tokens\":220}}}}\n",
            "{\"timestamp\":\"2025-09-12T23:31:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":300,\"cached_input_tokens\":0,\"output_tokens\":30,\"reasoning_output_tokens\":0,\"total_tokens\":330}}}}\n"
        ),
    );

    let mut report_options = options(&temp.path().join("sessions"));
    report_options.since = Some("2025-09-11".to_string());
    report_options.until = Some("20250912".to_string());

    let report = build_report(ReportKind::Daily, &report_options).expect("build report");

    match report {
        ReportOutput::Daily { rows, totals, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].date, "2025-09-11");
            assert_eq!(rows[1].date, "2025-09-12");
            assert_eq!(totals.input_tokens, 500);
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

#[test]
fn timezone_conversion_happens_before_daily_grouping() {
    let temp = TempDir::new().expect("tempdir");
    write_session(
        &temp,
        "sessions/project/session.jsonl",
        concat!(
            "{\"timestamp\":\"2025-09-11T22:30:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2025-09-11T22:31:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":400,\"cached_input_tokens\":0,\"output_tokens\":40,\"reasoning_output_tokens\":0,\"total_tokens\":440}}}}\n"
        ),
    );

    let mut report_options = options(&temp.path().join("sessions"));
    report_options.timezone = "Europe/Warsaw".to_string();
    let report = build_report(ReportKind::Daily, &report_options).expect("build report");

    match report {
        ReportOutput::Daily { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].date, "2025-09-12");
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

#[test]
fn last_days_is_rejected_for_non_daily_reports() {
    let temp = TempDir::new().expect("tempdir");
    write_session(
        &temp,
        "sessions/project/session.jsonl",
        concat!(
            "{\"timestamp\":\"2025-09-11T22:30:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2025-09-11T22:31:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":400,\"cached_input_tokens\":0,\"output_tokens\":40,\"reasoning_output_tokens\":0,\"total_tokens\":440}}}}\n"
        ),
    );

    let mut report_options = options(&temp.path().join("sessions"));
    report_options.last_days = Some(NonZeroUsize::new(2).expect("non-zero"));
    let error = build_report(ReportKind::Monthly, &report_options)
        .expect_err("monthly should reject last_days");

    let rendered = error.to_string();
    assert!(rendered.contains("last_days"));
    assert!(rendered.contains("daily"));
}
