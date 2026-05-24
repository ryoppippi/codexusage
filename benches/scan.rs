use codexusage::app::{
    CacheReadMode, CachedInputCostMode, NumberFormat, ReportKind, ReportOptions,
    ScannerParallelism, build_report, build_report_with_scan_index,
};
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::json;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

fn base_options(session_dirs: Vec<std::path::PathBuf>) -> ReportOptions {
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
        cached_input_cost_mode: CachedInputCostMode::Priced,
        cache_read_mode: CacheReadMode::Include,
        session_dirs,
        project_dir: None,
        parallelism: ScannerParallelism::Auto,
    }
}

fn write_session_file(root: &std::path::Path, relative_path: &str, contents: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    fs::write(path, contents).expect("write fixture");
}

fn session_meta_line(cwd: &std::path::Path) -> String {
    json!({
        "timestamp": "2025-09-11T18:00:00.000Z",
        "type": "session_meta",
        "payload": {"cwd": cwd.to_string_lossy()},
    })
    .to_string()
}

fn event_timestamp(index: usize) -> String {
    let hour = 18 + ((index / 60) % 6);
    let minute = index % 60;
    format!("2025-09-11T{hour:02}:{minute:02}:00.000Z")
}

fn parser_benchmark(criterion: &mut Criterion) {
    let fixture = TempDir::new().expect("tempdir");
    let sessions_dir = fixture.path().join("sessions");
    let payload = (0..1_000)
        .map(|index| {
            json!({
                "timestamp": event_timestamp(index),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 1_200,
                            "cached_input_tokens": 200,
                            "output_tokens": 500,
                            "reasoning_output_tokens": 0,
                            "total_tokens": 1_700
                        }
                    }
                }
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let fixture_contents = format!(
        "{{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5\"}}}}\n{payload}\n"
    );
    write_session_file(&sessions_dir, "project/session.jsonl", &fixture_contents);

    let options = base_options(vec![sessions_dir]);
    criterion.bench_function("daily_report_scan_1000_last_usage_events", |bench| {
        bench.iter(|| {
            let report = build_report(ReportKind::Daily, &options).expect("build report");
            std::hint::black_box(report);
        });
    });
}

fn cached_scan_index_benchmark(criterion: &mut Criterion) {
    let fixture = TempDir::new().expect("tempdir");
    let sessions_dir = fixture.path().join("sessions");
    let index_path = fixture.path().join("scan-index.sqlite3");
    let payload = (0..1_000)
        .map(|index| {
            json!({
                "timestamp": event_timestamp(index),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 1_200,
                            "cached_input_tokens": 200,
                            "output_tokens": 500,
                            "reasoning_output_tokens": 0,
                            "total_tokens": 1_700
                        }
                    }
                }
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let fixture_contents = format!(
        "{{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5\"}}}}\n{payload}\n"
    );
    write_session_file(&sessions_dir, "project/session.jsonl", &fixture_contents);

    let options = base_options(vec![sessions_dir]);
    let _warm =
        build_report_with_scan_index(ReportKind::Daily, &options, &index_path).expect("warm index");
    criterion.bench_function("daily_report_scan_index_cached_1000_events", |bench| {
        bench.iter(|| {
            let report = build_report_with_scan_index(ReportKind::Daily, &options, &index_path)
                .expect("build indexed report");
            std::hint::black_box(report);
        });
    });
}

fn cached_many_file_scan_index_benchmark(criterion: &mut Criterion) {
    let fixture = TempDir::new().expect("tempdir");
    let sessions_dir = fixture.path().join("sessions");
    let index_path = fixture.path().join("scan-index.sqlite3");
    let fixture_contents = concat!(
        "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
        "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1200,\"cached_input_tokens\":200,\"output_tokens\":500,\"reasoning_output_tokens\":0,\"total_tokens\":1700}}}}\n",
    );
    for index in 0..300 {
        write_session_file(
            &sessions_dir,
            &format!("project/session-{index:03}.jsonl"),
            fixture_contents,
        );
    }

    let options = base_options(vec![sessions_dir]);
    let _warm =
        build_report_with_scan_index(ReportKind::Daily, &options, &index_path).expect("warm index");
    criterion.bench_function("daily_report_scan_index_cached_300_files", |bench| {
        bench.iter(|| {
            let report = build_report_with_scan_index(ReportKind::Daily, &options, &index_path)
                .expect("build indexed report");
            std::hint::black_box(report);
        });
    });
}

fn cumulative_usage_benchmark(criterion: &mut Criterion) {
    let fixture = TempDir::new().expect("tempdir");
    let sessions_dir = fixture.path().join("sessions");
    let payload = (0..1_000)
        .scan((0_u64, 0_u64), |totals, index| {
            totals.0 += 120;
            totals.1 += 50;
            Some(
                json!({
                    "timestamp": event_timestamp(index),
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": totals.0,
                                "cached_input_tokens": 0,
                                "output_tokens": totals.1,
                                "reasoning_output_tokens": 0,
                                "total_tokens": totals.0 + totals.1
                            }
                        }
                    }
                })
                .to_string(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let fixture_contents = format!(
        "{{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5\"}}}}\n{payload}\n"
    );
    write_session_file(&sessions_dir, "project/session.jsonl", &fixture_contents);

    let options = base_options(vec![sessions_dir]);
    criterion.bench_function("daily_report_scan_1000_cumulative_events", |bench| {
        bench.iter(|| {
            let report = build_report(ReportKind::Daily, &options).expect("build report");
            std::hint::black_box(report);
        });
    });
}

fn duplicate_root_selection_benchmark(criterion: &mut Criterion) {
    let fixture = TempDir::new().expect("tempdir");
    let first_root = fixture.path().join("sessions-a");
    let second_root = fixture.path().join("sessions-b");
    let short_payload = concat!(
        "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
        "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n"
    );
    let long_payload = concat!(
        "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
        "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n",
        "{\"timestamp\":\"2025-09-11T18:02:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":50,\"cached_input_tokens\":0,\"output_tokens\":5,\"reasoning_output_tokens\":0,\"total_tokens\":55}}}}\n"
    );
    for index in 0..150 {
        let relative_path = format!("project-{index}/session.jsonl");
        write_session_file(&first_root, &relative_path, short_payload);
        write_session_file(&second_root, &relative_path, long_payload);
    }

    let options = base_options(vec![first_root, second_root]);
    criterion.bench_function(
        "session_report_duplicate_root_selection_150_sessions",
        |bench| {
            bench.iter(|| {
                let report = build_report(ReportKind::Session, &options).expect("build report");
                std::hint::black_box(report);
            });
        },
    );
}

fn project_filter_discovery_benchmark(criterion: &mut Criterion) {
    let fixture = TempDir::new().expect("tempdir");
    let sessions_dir = fixture.path().join("sessions");
    let project_dir = fixture.path().join("project");
    let other_dir = fixture.path().join("project-sibling");
    let usage_payload = concat!(
        "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
        "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n"
    );
    for index in 0..300 {
        let cwd = if index % 2 == 0 {
            project_dir.join(format!("crate-{index}"))
        } else {
            other_dir.join(format!("crate-{index}"))
        };
        let relative_path = format!("project-filter/session-{index}.jsonl");
        write_session_file(
            &sessions_dir,
            &relative_path,
            &format!("{}\n{usage_payload}", session_meta_line(&cwd)),
        );
    }

    let mut options = base_options(vec![sessions_dir]);
    options.project_dir = Some(project_dir);
    criterion.bench_function("daily_report_project_filter_300_sessions", |bench| {
        bench.iter(|| {
            let report = build_report(ReportKind::Daily, &options).expect("build report");
            std::hint::black_box(report);
        });
    });
}

fn mixed_workload_benchmark(criterion: &mut Criterion) {
    let fixture = TempDir::new().expect("tempdir");
    let sessions_dir = fixture.path().join("sessions");
    let irrelevant_noise = (0..900)
        .map(|index| {
            json!({
                "timestamp": event_timestamp(index),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "text": format!("noise-{index}"),
                    "details": {
                        "status": "ok",
                        "sequence": index
                    }
                }
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    let relevant_usage = (0..350)
        .map(|index| {
            json!({
                "timestamp": event_timestamp(index),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 1_000 + index,
                            "cached_input_tokens": 100,
                            "output_tokens": 400,
                            "reasoning_output_tokens": 0,
                            "total_tokens": 1_400 + index
                        }
                    }
                }
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    let escaped_turn_contexts = (0..150)
        .map(|_| {
            r#"{"type":"turn\u005fcontext","payload":{"metadata":{"model":"gpt\u002d5"}}}"#
                .to_string()
        })
        .collect::<Vec<_>>();
    let fixture_contents = std::iter::once(
        r#"{"timestamp":"2025-09-11T18:00:00.000Z","type":"turn_context","payload":{"model":"gpt-5"}}"#
            .to_string(),
    )
    .chain(irrelevant_noise)
    .chain(relevant_usage)
    .chain(escaped_turn_contexts)
    .collect::<Vec<_>>()
    .join("\n");
    write_session_file(
        &sessions_dir,
        "project/session.jsonl",
        &format!("{fixture_contents}\n"),
    );

    let options = base_options(vec![sessions_dir]);
    criterion.bench_function("daily_report_scan_mixed_workload_1401_lines", |bench| {
        bench.iter(|| {
            let report = build_report(ReportKind::Daily, &options).expect("build report");
            std::hint::black_box(report);
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(50)
        .measurement_time(Duration::from_secs(10));
    targets =
        parser_benchmark,
        cached_scan_index_benchmark,
        cached_many_file_scan_index_benchmark,
        cumulative_usage_benchmark,
        duplicate_root_selection_benchmark,
        project_filter_discovery_benchmark,
        mixed_workload_benchmark
}
criterion_main!(benches);
