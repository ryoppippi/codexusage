use codexusage::app::{
    NumberFormat, ReportKind, ReportOptions, ReportOutput, ScannerParallelism, build_report,
};
use codexusage::pricing::{CacheDecision, decide_cache_action};
use serde_json::json;
use std::fs;
use std::num::NonZeroUsize;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

fn write_session_file(temp: &TempDir, relative_path: &str, contents: &str) {
    let path = temp.path().join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create session directory");
    }
    fs::write(path, contents).expect("write session file");
}

fn base_options(session_dir: &std::path::Path) -> ReportOptions {
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
fn daily_report_uses_last_usage_and_total_delta_fallback() {
    let temp = TempDir::new().expect("tempdir");
    write_session_file(
        &temp,
        "sessions/project-a/session-1.jsonl",
        concat!(
            "{\"timestamp\":\"2025-09-11T18:25:30.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2025-09-11T18:25:40.670Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1200,\"cached_input_tokens\":200,\"output_tokens\":500,\"reasoning_output_tokens\":0,\"total_tokens\":1700},\"total_token_usage\":{\"input_tokens\":1200,\"cached_input_tokens\":200,\"output_tokens\":500,\"reasoning_output_tokens\":0,\"total_tokens\":1700}}}}\n",
            "{\"timestamp\":\"2025-09-12T00:00:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":2000,\"cached_input_tokens\":300,\"output_tokens\":800,\"reasoning_output_tokens\":0,\"total_tokens\":2800}}}}\n"
        ),
    );

    let report = build_report(
        ReportKind::Daily,
        &base_options(&temp.path().join("sessions")),
    )
    .expect("build report");

    match report {
        ReportOutput::Daily { rows, totals, .. } => {
            assert_eq!(rows.len(), 2, "expected one row per day");
            assert_eq!(rows[0].date, "2025-09-11");
            assert_eq!(rows[0].input_tokens, 1_200);
            assert_eq!(rows[0].cached_input_tokens, 200);
            assert_eq!(rows[1].date, "2025-09-12");
            assert_eq!(rows[1].input_tokens, 800);
            assert_eq!(rows[1].cached_input_tokens, 100);
            assert_eq!(totals.input_tokens, 2_000);
            assert_eq!(totals.output_tokens, 800);
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

#[test]
fn session_report_marks_fallback_models_when_metadata_is_missing() {
    let temp = TempDir::new().expect("tempdir");
    write_session_file(
        &temp,
        "sessions/legacy/session-legacy.jsonl",
        "{\"timestamp\":\"2025-09-15T13:00:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":5000,\"cached_input_tokens\":0,\"output_tokens\":1000,\"reasoning_output_tokens\":0,\"total_tokens\":6000}}}}\n",
    );

    let report = build_report(
        ReportKind::Session,
        &base_options(&temp.path().join("sessions")),
    )
    .expect("build report");

    match report {
        ReportOutput::Session { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let row = &rows[0];
            let model = row.models.get("gpt-5").expect("fallback model");
            assert!(model.is_fallback, "fallback model should be marked");
            assert_eq!(row.directory, "legacy");
            assert_eq!(row.session_file, "session-legacy");
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

#[test]
fn duplicate_session_ids_across_roots_prefer_the_longer_file() {
    let first = TempDir::new().expect("first");
    let second = TempDir::new().expect("second");
    write_session_file(
        &first,
        "sessions/project/session.jsonl",
        concat!(
            "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n"
        ),
    );
    write_session_file(
        &second,
        "sessions/project/session.jsonl",
        concat!(
            "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n",
            "{\"timestamp\":\"2025-09-11T18:02:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":50,\"cached_input_tokens\":0,\"output_tokens\":5,\"reasoning_output_tokens\":0,\"total_tokens\":55}}}}\n"
        ),
    );

    let report = build_report(
        ReportKind::Session,
        &ReportOptions {
            since: None,
            until: None,
            last_days: None,
            timezone: "UTC".to_string(),
            locale: "en-US".to_string(),
            number_format: NumberFormat::Short,
            json: true,
            offline: true,
            refresh_pricing: false,
            session_dirs: vec![
                first.path().join("sessions"),
                second.path().join("sessions"),
            ],
            parallelism: ScannerParallelism::Auto,
        },
    )
    .expect("build report");

    match report {
        ReportOutput::Session { rows, totals, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].session_id, "project/session");
            assert_eq!(rows[0].input_tokens, 150);
            assert_eq!(rows[0].output_tokens, 15);
            assert_eq!(totals.total_tokens, 165);
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

#[test]
fn session_last_activity_uses_selected_timezone() {
    let temp = TempDir::new().expect("tempdir");
    write_session_file(
        &temp,
        "sessions/project/session.jsonl",
        concat!(
            "{\"timestamp\":\"2025-01-15T23:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2025-01-15T23:00:30.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n"
        ),
    );

    let report = build_report(
        ReportKind::Session,
        &ReportOptions {
            since: None,
            until: None,
            last_days: None,
            timezone: "Europe/Warsaw".to_string(),
            locale: "en-US".to_string(),
            number_format: NumberFormat::Short,
            json: true,
            offline: true,
            refresh_pricing: false,
            session_dirs: vec![temp.path().join("sessions")],
            parallelism: ScannerParallelism::Auto,
        },
    )
    .expect("build report");

    match report {
        ReportOutput::Session { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].last_activity, "2025-01-16T00:00:30.000+01:00");
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

#[test]
fn missing_directories_are_reported_without_failing_the_run() {
    let temp = TempDir::new().expect("tempdir");
    let mut options = base_options(&temp.path().join("missing"));
    options.session_dirs.push(temp.path().join("also-missing"));

    let report = build_report(ReportKind::Monthly, &options).expect("build report");

    match report {
        ReportOutput::Monthly {
            rows,
            totals,
            missing_directories,
        } => {
            assert!(
                rows.is_empty(),
                "missing directories should not invent rows"
            );
            assert_eq!(totals.total_tokens, 0);
            assert_eq!(missing_directories.len(), 2);
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

#[test]
fn explicit_single_thread_mode_matches_multi_worker_results() {
    let temp = TempDir::new().expect("tempdir");
    for index in 0..4 {
        let session_contents = [
            json!({
                "timestamp": "2025-09-11T18:00:00.000Z",
                "type": "turn_context",
                "payload": {"model": "gpt-5"},
            })
            .to_string(),
            json!({
                "timestamp": format!("2025-09-11T18:0{index}:00.000Z"),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 100 + index,
                            "cached_input_tokens": 10,
                            "output_tokens": 20 + index,
                            "reasoning_output_tokens": 0,
                            "total_tokens": 120 + (index * 2),
                        }
                    }
                }
            })
            .to_string(),
            String::new(),
        ]
        .join("\n");
        write_session_file(
            &temp,
            &format!("sessions/project/session-{index}.jsonl"),
            &session_contents,
        );
    }

    let session_dir = temp.path().join("sessions");
    let mut single_threaded = base_options(&session_dir);
    single_threaded.parallelism =
        ScannerParallelism::Fixed(NonZeroUsize::new(1).expect("non-zero"));
    let mut multi_worker = base_options(&session_dir);
    multi_worker.parallelism = ScannerParallelism::Fixed(NonZeroUsize::new(2).expect("non-zero"));

    let single_report =
        build_report(ReportKind::Session, &single_threaded).expect("single-threaded report");
    let multi_report =
        build_report(ReportKind::Session, &multi_worker).expect("multi-worker report");

    assert_eq!(single_report, multi_report);
}

#[test]
fn fresh_pricing_cache_skips_refresh_but_force_refresh_overrides_it() {
    let temp = TempDir::new().expect("tempdir");
    let cache_path = temp.path().join("pricing-cache.json");
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000);
    let refreshed_at_epoch_seconds = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("duration")
        .as_secs();
    let refreshed_at_epoch_seconds =
        i64::try_from(refreshed_at_epoch_seconds).expect("timestamp fits in i64");
    fs::write(
        &cache_path,
        format!("{{\"refreshed_at_epoch_seconds\":{refreshed_at_epoch_seconds},\"models\":{{}}}}"),
    )
    .expect("write cache");
    let ttl = Duration::from_hours(24);

    let decision = decide_cache_action(&cache_path, now, ttl, false, false).expect("decision");
    assert_eq!(decision, CacheDecision::UseCache);

    let forced = decide_cache_action(&cache_path, now, ttl, false, true).expect("decision");
    assert_eq!(forced, CacheDecision::Refresh);
}

#[test]
fn offline_mode_never_refreshes_pricing() {
    let temp = TempDir::new().expect("tempdir");
    let cache_path = temp.path().join("pricing-cache.json");
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000);
    let ttl = Duration::from_hours(24);

    let decision = decide_cache_action(&cache_path, now, ttl, true, false).expect("decision");
    assert_eq!(decision, CacheDecision::UseCache);
}

#[test]
fn invalid_timezone_is_rejected() {
    let temp = TempDir::new().expect("tempdir");
    let error = build_report(
        ReportKind::Daily,
        &ReportOptions {
            since: None,
            until: None,
            last_days: None,
            timezone: "Europe/Warswa".to_string(),
            locale: "en-US".to_string(),
            number_format: NumberFormat::Short,
            json: true,
            offline: true,
            refresh_pricing: false,
            session_dirs: vec![temp.path().join("sessions")],
            parallelism: ScannerParallelism::Auto,
        },
    )
    .expect_err("invalid timezone should fail");

    assert!(error.to_string().contains("invalid timezone"));
}

#[test]
fn stale_payload_timestamp_triggers_refresh_even_with_fresh_file_mtime() {
    let temp = TempDir::new().expect("tempdir");
    let cache_path = temp.path().join("pricing-cache.json");
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000);
    let refreshed_at_epoch_seconds = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("duration")
        .as_secs();
    let refreshed_at_epoch_seconds = i64::try_from(refreshed_at_epoch_seconds)
        .expect("timestamp fits in i64")
        - i64::try_from(Duration::from_hours(48).as_secs()).expect("duration fits in i64");
    fs::write(
        &cache_path,
        format!("{{\"refreshed_at_epoch_seconds\":{refreshed_at_epoch_seconds},\"models\":{{}}}}"),
    )
    .expect("write cache");

    let decision = decide_cache_action(&cache_path, now, Duration::from_hours(24), false, false)
        .expect("decision");
    assert_eq!(decision, CacheDecision::Refresh);
}
