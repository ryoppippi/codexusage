use codexusage::app::{NumberFormat, ReportKind, ReportOptions, build_report};
use criterion::{Criterion, criterion_group, criterion_main};
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

fn parser_benchmark(criterion: &mut Criterion) {
    let fixture = TempDir::new().expect("tempdir");
    let sessions_dir = fixture.path().join("sessions");
    let session_path = sessions_dir.join("project").join("session.jsonl");
    fs::create_dir_all(session_path.parent().expect("parent")).expect("create dirs");
    let payload = (0..1_000)
        .map(|index| {
            format!(
                "{{\"timestamp\":\"2025-09-11T18:{minute:02}:00.000Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"input_tokens\":1200,\"cached_input_tokens\":200,\"output_tokens\":500,\"reasoning_output_tokens\":0,\"total_tokens\":1700}}}}}}}}",
                minute = index % 60
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let fixture_contents = format!(
        "{{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5\"}}}}\n{payload}\n"
    );
    fs::write(&session_path, fixture_contents).expect("write fixture");

    let options = ReportOptions {
        since: None,
        until: None,
        timezone: "UTC".to_string(),
        locale: "en-US".to_string(),
        number_format: NumberFormat::Short,
        json: true,
        offline: true,
        refresh_pricing: false,
        session_dirs: vec![sessions_dir],
    };

    criterion.bench_function("daily_report_scan_1000_events", |bench| {
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
    targets = parser_benchmark
}
criterion_main!(benches);
