use super::*;
use filetime::FileTime;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;

#[derive(Debug, Deserialize, PartialEq)]
struct LossyStringField<'a> {
    #[serde(borrow, deserialize_with = "deserialize_optional_cow_lossy")]
    value: Option<Cow<'a, str>>,
}

#[derive(Debug, Deserialize, PartialEq)]
struct LossyU64Field {
    #[serde(deserialize_with = "deserialize_u64_lossy")]
    value: u64,
}

#[derive(Debug, Deserialize, PartialEq)]
struct LossyOptionalU64Field {
    #[serde(deserialize_with = "deserialize_optional_u64_lossy")]
    value: Option<u64>,
}

#[derive(Debug, Deserialize, PartialEq)]
struct ObjectPayload {
    name: String,
}

#[derive(Debug, Deserialize, PartialEq)]
struct LossyObjectField {
    #[serde(deserialize_with = "deserialize_optional_object_lossy")]
    value: Option<ObjectPayload>,
}

fn strip_ansi_sequences(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && matches!(characters.next(), Some('[')) {
            for next in characters.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        output.push(character);
    }
    output
}

fn normalized_table_lines(rendered: &str) -> Vec<String> {
    rendered
        .lines()
        .map(|line| strip_ansi_sequences(line).replace('│', "|"))
        .collect()
}

fn session_payload_with_cwd(cwd: &Path, input_tokens: u64) -> String {
    [
        json!({
            "timestamp": "2026-01-02T00:00:00Z",
            "type": "session_meta",
            "payload": {"cwd": cwd.to_string_lossy()},
        })
        .to_string(),
        json!({
            "type": "turn_context",
            "payload": {"model": "gpt-5"},
        })
        .to_string(),
        json!({
            "timestamp": "2026-01-02T00:05:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": input_tokens,
                        "cached_input_tokens": 0,
                        "output_tokens": 3,
                        "reasoning_output_tokens": 0,
                        "total_tokens": input_tokens + 3,
                    }
                }
            },
        })
        .to_string(),
    ]
    .join("\n")
}

fn watch_snapshot_with_models(models: BTreeMap<String, ModelBreakdown>) -> WatchSnapshot {
    WatchSnapshot {
        date: "2026-01-02".to_string(),
        totals: Totals {
            input_tokens: 60,
            cached_input_tokens: 15,
            output_tokens: 12,
            reasoning_output_tokens: 3,
            total_tokens: 72,
            cost_usd: 1.80,
        },
        burn_rate: BurnRateSnapshot {
            window_duration: Duration::from_secs(30 * 60),
            window_minutes: 30,
            input_tokens_per_hour: 120,
            cached_input_tokens_per_hour: 30,
            output_tokens_per_hour: 24,
            reasoning_output_tokens_per_hour: 6,
            total_tokens_per_hour: 144,
            cost_usd_per_hour: 3.60,
        },
        per_model: models,
        missing_directories: Vec::new(),
        updated_time: "00:30:00".to_string(),
    }
}

#[test]
fn normalize_filter_date_accepts_supported_formats() {
    assert_eq!(
        normalize_filter_date("2025-09-11").expect("date"),
        NaiveDate::from_ymd_opt(2025, 9, 11).expect("naive date")
    );
    assert_eq!(
        normalize_filter_date("20250912").expect("date"),
        NaiveDate::from_ymd_opt(2025, 9, 12).expect("naive date")
    );
    assert!(normalize_filter_date("2025/09/12").is_err());
}

#[test]
fn parse_timezone_rejects_invalid_names() {
    assert_eq!(
        parse_timezone("Europe/Warsaw").expect("timezone"),
        chrono_tz::Europe::Warsaw
    );
    assert!(parse_timezone("Not/A_Timezone").is_err());
}

#[test]
fn normalize_timezone_name_accepts_valid_sources() {
    assert_eq!(
        normalize_timezone_name(":Europe/Warsaw").as_deref(),
        Some("Europe/Warsaw")
    );
    assert!(normalize_timezone_name("Not/A_Timezone").is_none());
}

#[test]
fn timezone_source_helpers_extract_valid_names() {
    assert_eq!(
        timezone_from_etc_timezone_contents("Europe/Warsaw\n").as_deref(),
        Some("Europe/Warsaw")
    );
    assert_eq!(
        timezone_from_localtime_target(Path::new("/usr/share/zoneinfo/Europe/Warsaw")).as_deref(),
        Some("Europe/Warsaw")
    );
    assert_eq!(
        timezone_from_localtime_target(Path::new("../usr/share/zoneinfo/America/New_York"))
            .as_deref(),
        Some("America/New_York")
    );
}

#[test]
fn resolve_session_dirs_prefers_explicit_paths() {
    let explicit = vec![PathBuf::from("/tmp/custom-sessions")];
    assert_eq!(resolve_session_dirs(&explicit), explicit);
}

#[test]
fn project_filter_normalizes_relative_paths_against_process_cwd() {
    let filter = ProjectFilter::from_path(Path::new(".")).expect("filter");
    let current_dir = std::env::current_dir().expect("current dir");

    assert!(
        filter
            .matches_logged_cwd(&current_dir.join("nested"))
            .expect("match cwd")
    );
    assert!(
        !filter
            .matches_logged_cwd(current_dir.parent().expect("parent"))
            .expect("parent cwd")
    );
}

#[test]
fn project_filter_matches_path_components_not_string_prefixes() {
    let temp = TempDir::new().expect("tempdir");
    let filter = ProjectFilter::from_path(&temp.path().join("app")).expect("filter");

    assert!(
        filter
            .matches_logged_cwd(&temp.path().join("app").join("crate"))
            .expect("nested match")
    );
    assert!(
        !filter
            .matches_logged_cwd(&temp.path().join("app-sibling"))
            .expect("sibling match")
    );
}

#[test]
fn extract_model_checks_nested_metadata() {
    let payload =
        serde_json::from_str::<EntryPayload<'_>>(r#"{"info":{"metadata":{"model":"gpt-5"}}}"#)
            .expect("payload");
    assert_eq!(extract_payload_model(&payload), Some("gpt-5"));
}

#[test]
fn normalize_usage_reads_cache_alias() {
    let usage = serde_json::from_str::<UsagePayload>(
            r#"{"input_tokens":100,"cache_read_input_tokens":25,"output_tokens":20,"reasoning_output_tokens":5}"#,
        )
        .expect("usage")
        .into_raw_usage();
    assert_eq!(usage.input, 100);
    assert_eq!(usage.cached_input, 25);
    assert_eq!(usage.output, 20);
    assert_eq!(usage.reasoning_output, 5);
    assert_eq!(usage.total, 0);
}

#[test]
fn normalize_usage_prefers_primary_cached_input_field_when_both_are_present() {
    let usage = serde_json::from_str::<UsagePayload>(
            r#"{"input_tokens":100,"cached_input_tokens":10,"cache_read_input_tokens":25,"output_tokens":20,"total_tokens":120}"#,
        )
        .expect("usage")
        .into_raw_usage();

    assert_eq!(usage.cached_input, 10);
    assert_eq!(usage.total, 120);
}

#[test]
fn lossy_string_deserializer_ignores_non_string_shapes() {
    assert_eq!(
        serde_json::from_str::<LossyStringField<'_>>(r#"{"value":"gpt-5"}"#)
            .expect("string")
            .value
            .as_deref(),
        Some("gpt-5")
    );

    for input in [
        r#"{"value":null}"#,
        r#"{"value":true}"#,
        r#"{"value":-1}"#,
        r#"{"value":7}"#,
        r#"{"value":1.5}"#,
        r#"{"value":["gpt-5"]}"#,
        r#"{"value":{"model":"gpt-5"}}"#,
    ] {
        let field = serde_json::from_str::<LossyStringField<'_>>(input).expect("field");
        assert_eq!(field.value, None);
    }
}

#[test]
fn lossy_u64_deserializers_ignore_non_integer_shapes() {
    assert_eq!(
        serde_json::from_value::<LossyU64Field>(json!({"value": 7}))
            .expect("integer")
            .value,
        7
    );
    assert_eq!(
        serde_json::from_value::<LossyOptionalU64Field>(json!({"value": 7}))
            .expect("optional integer")
            .value,
        Some(7)
    );

    for value in [
        json!(null),
        json!(true),
        json!(-1),
        json!(1.5),
        json!("7"),
        json!(["7"]),
        json!({"value": 7}),
    ] {
        let field = serde_json::from_value::<LossyU64Field>(json!({"value": value}))
            .expect("lossy u64 field");
        assert_eq!(field.value, 0);

        let optional = serde_json::from_value::<LossyOptionalU64Field>(json!({"value": value}))
            .expect("lossy optional u64 field");
        assert_eq!(optional.value, None);
    }
}

#[test]
fn lossy_object_deserializer_ignores_non_object_shapes() {
    assert_eq!(
        serde_json::from_value::<LossyObjectField>(json!({"value": {"name": "codex"}}))
            .expect("object")
            .value,
        Some(ObjectPayload {
            name: "codex".to_string(),
        })
    );

    for value in [
        json!(null),
        json!(true),
        json!(-1),
        json!(7),
        json!(1.5),
        json!("codex"),
        json!(["codex"]),
    ] {
        let field =
            serde_json::from_value::<LossyObjectField>(json!({"value": value})).expect("field");
        assert_eq!(field.value, None);
    }
}

#[test]
fn subtract_usage_saturates_negative_deltas() {
    let delta = subtract_usage(
        RawUsage {
            input: 5,
            cached_input: 2,
            output: 3,
            reasoning_output: 1,
            total: 8,
        },
        Some(RawUsage {
            input: 10,
            cached_input: 4,
            output: 4,
            reasoning_output: 2,
            total: 14,
        }),
    );
    assert_eq!(delta.input, 0);
    assert_eq!(delta.cached_input, 0);
    assert_eq!(delta.output, 0);
    assert_eq!(delta.reasoning_output, 0);
    assert_eq!(delta.total, 0);
}

#[test]
fn split_session_id_handles_nested_paths() {
    let (directory, session) = split_session_id("team/project/session-1");
    assert_eq!(directory, "team/project");
    assert_eq!(session, "session-1");
}

#[test]
fn session_file_round_trip_preserves_dotted_leaf_names() {
    let root = Path::new("/logs");
    let session_id = "team/session.v2";

    let rebuilt = session_file_path(root, session_id);

    assert_eq!(rebuilt, PathBuf::from("/logs/team/session.v2.jsonl"));
    assert_eq!(session_file_id(root, &rebuilt), session_id);
}

#[test]
fn sort_session_entries_breaks_timestamp_ties_by_session_id() {
    let later = DateTime::parse_from_rfc3339("2025-09-11T18:00:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let mut entries = vec![
        (
            "zeta/session".to_string(),
            SessionSummary::new(later, "zeta/session".to_string()),
        ),
        (
            "alpha/session".to_string(),
            SessionSummary::new(later, "alpha/session".to_string()),
        ),
    ];

    sort_session_entries(&mut entries);

    assert_eq!(entries[0].0, "alpha/session");
    assert_eq!(entries[1].0, "zeta/session");
}

#[test]
fn mark_dirty_sessions_keeps_the_most_conservative_refresh_kind() {
    let mut changes = WatchChangeSet::default();
    changes.mark_dirty_sessions(vec!["session-a".to_string()], WatchDirtyKind::AppendOnly);
    changes.mark_dirty_sessions(
        vec!["session-a".to_string(), "session-b".to_string()],
        WatchDirtyKind::FullRebuild,
    );
    changes.mark_dirty_sessions(vec!["session-b".to_string()], WatchDirtyKind::AppendOnly);

    assert_eq!(
        changes.dirty_sessions.get("session-a"),
        Some(&WatchDirtyKind::FullRebuild)
    );
    assert_eq!(
        changes.dirty_sessions.get("session-b"),
        Some(&WatchDirtyKind::FullRebuild)
    );
}

#[test]
fn validate_watch_flags_rejects_json_and_date_filters() {
    assert!(validate_watch_flags(false, None, None, None).is_ok());
    assert!(
        validate_watch_flags(true, None, None, None)
            .expect_err("json should fail")
            .to_string()
            .contains("--json")
    );
    assert!(
        validate_watch_flags(false, Some("2026-01-01"), None, NonZeroUsize::new(1),)
            .expect_err("date filters should fail")
            .to_string()
            .contains("--since")
    );
}

#[test]
fn watch_helpers_classify_dirty_events_and_paths() {
    let session_root = PathBuf::from("/tmp/codexusage-tests/sessions");
    let session_path = session_root.join("team").join("session.jsonl");
    let txt_path = session_root.join("team").join("session.txt");

    assert_eq!(
        watch_dirty_kind(NotifyEventKind::Modify(ModifyKind::Data(DataChange::Size))),
        WatchDirtyKind::AppendOnly
    );
    assert_eq!(
        watch_dirty_kind(NotifyEventKind::Create(notify::event::CreateKind::Any)),
        WatchDirtyKind::FullRebuild
    );
    assert!(path_is_under_roots(
        std::slice::from_ref(&session_root),
        &session_path
    ));
    assert!(!path_is_under_roots(
        std::slice::from_ref(&session_root),
        Path::new("/tmp/elsewhere/session.jsonl")
    ));
    assert_eq!(
        watch_event_session_ids(std::slice::from_ref(&session_root), &session_path),
        vec!["team/session".to_string()]
    );
    assert!(watch_event_session_ids(&[session_root], &txt_path).is_empty());
}

#[test]
fn parse_interval_and_report_kind_helpers_cover_cli_routing_paths() {
    assert_eq!(
        parse_interval_seconds("5").expect("interval"),
        Duration::from_secs(5)
    );
    assert!(
        parse_interval_seconds("0")
            .expect_err("zero should fail")
            .contains("greater than 0")
    );
    assert!(
        parse_interval_seconds("abc")
            .expect_err("non-integer should fail")
            .contains("positive integer")
    );
    assert_eq!(ReportKind::from_command(&Command::Daily), ReportKind::Daily);
    assert_eq!(
        ReportKind::from_command(&Command::Monthly),
        ReportKind::Monthly
    );
    assert_eq!(
        ReportKind::from_command(&Command::Session),
        ReportKind::Session
    );
    assert!(is_false(&false));
    assert!(!is_false(&true));
}

#[test]
fn supports_watch_screen_clear_wrapper_matches_term_contract() {
    assert!(!supports_watch_screen_clear(Some("dumb")));
    assert!(supports_watch_screen_clear(Some("xterm-256color")));
}

#[test]
fn run_succeeds_for_json_and_table_reports() {
    let temp = TempDir::new().expect("tempdir");
    let missing = temp.path().join("missing-sessions");

    run(vec![
        OsString::from("codexusage"),
        OsString::from("--json"),
        OsString::from("--offline"),
        OsString::from("--session-dir"),
        missing.clone().into_os_string(),
    ])
    .expect("json run");
    run(vec![
        OsString::from("codexusage"),
        OsString::from("--offline"),
        OsString::from("--session-dir"),
        missing.into_os_string(),
        OsString::from("session"),
    ])
    .expect("table run");
}

#[test]
fn format_helpers_produce_human_readable_output() {
    assert_eq!(format_u64(1_234_567), "1,234,567");
    assert_eq!(format_currency(12.5), "$12.50");
}

#[test]
fn short_number_format_uses_three_significant_digits() {
    assert_eq!(format_u64_with(999, NumberFormat::Short), "999");
    assert_eq!(format_u64_with(1_000, NumberFormat::Short), "1K");
    assert_eq!(format_u64_with(1_234, NumberFormat::Short), "1.23K");
    assert_eq!(format_u64_with(12_345, NumberFormat::Short), "12.3K");
    assert_eq!(format_u64_with(123_456, NumberFormat::Short), "123K");
    assert_eq!(format_u64_with(1_234_567, NumberFormat::Short), "1.23M");
    assert_eq!(format_u64_with(12_345_678, NumberFormat::Short), "12.3M");
    assert_eq!(format_u64_with(123_456_789, NumberFormat::Short), "123M");
    assert_eq!(format_u64_with(1_000_000_000, NumberFormat::Short), "1B");
    assert_eq!(format_u64_with(1_200_000_000, NumberFormat::Short), "1.2B");
    assert_eq!(
        format_u64_with(1_000_000_000_000, NumberFormat::Short),
        "1T"
    );
}

#[test]
fn full_number_format_preserves_grouped_digits() {
    assert_eq!(format_u64_with(1_234_567, NumberFormat::Full), "1,234,567");
}

#[test]
fn calculate_cost_applies_cached_input_pricing() {
    let usage = ModelBreakdown {
        input_tokens: 1_000,
        cached_input_tokens: 200,
        output_tokens: 500,
        reasoning_output_tokens: 0,
        total_tokens: 1_500,
        cost_usd: 0.0,
        is_fallback: false,
        ..ModelBreakdown::default()
    };
    let pricing = Pricing {
        input_cost_per_mtoken: 1.25,
        cached_input_cost_per_mtoken: 0.125,
        output_cost_per_mtoken: 10.0,
    };
    let cost = calculate_cost(&usage, &pricing, CachedInputCostMode::Priced);
    let expected =
        (800.0 / 1_000_000.0) * 1.25 + (200.0 / 1_000_000.0) * 0.125 + (500.0 / 1_000_000.0) * 10.0;
    assert!((cost - expected).abs() < f64::EPSILON);
}

#[test]
fn calculate_cost_can_treat_cached_input_as_free() {
    let usage = UsageTotals {
        input: 1_000,
        cached_input: 200,
        output: 500,
        reasoning_output: 0,
        total: 1_500,
    };
    let pricing = Pricing {
        input_cost_per_mtoken: 1.25,
        cached_input_cost_per_mtoken: 0.125,
        output_cost_per_mtoken: 10.0,
    };

    let cost = calculate_cost_from_usage(&usage, &pricing, CachedInputCostMode::Free);
    let expected = (800.0 / 1_000_000.0) * 1.25 + (500.0 / 1_000_000.0) * 10.0;
    assert!((cost - expected).abs() < f64::EPSILON);
}

#[test]
fn calculate_cost_clamps_cached_input_to_avoid_double_accounting() {
    let usage = UsageTotals {
        input: 100,
        cached_input: 150,
        output: 0,
        reasoning_output: 0,
        total: 100,
    };
    let pricing = Pricing {
        input_cost_per_mtoken: 10.0,
        cached_input_cost_per_mtoken: 1.0,
        output_cost_per_mtoken: 0.0,
    };

    let cost = calculate_cost_from_usage(&usage, &pricing, CachedInputCostMode::Priced);
    let expected = (100.0 / 1_000_000.0) * 1.0;
    assert!((cost - expected).abs() < f64::EPSILON);
}

#[test]
fn render_report_includes_totals_for_daily_and_session_views() {
    let daily = ReportOutput::Daily {
        rows: vec![DailyRow {
            date: "2025-09-11".to_string(),
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 150,
            cost_usd: 0.25,
            models: BTreeMap::from([
                (
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 60,
                        cached_input_tokens: 5,
                        output_tokens: 25,
                        reasoning_output_tokens: 0,
                        total_tokens: 85,
                        cost_usd: 0.0,
                        is_fallback: false,
                        ..ModelBreakdown::default()
                    },
                ),
                (
                    "gpt-5-codex".to_string(),
                    ModelBreakdown {
                        input_tokens: 40,
                        cached_input_tokens: 5,
                        output_tokens: 25,
                        reasoning_output_tokens: 0,
                        total_tokens: 65,
                        cost_usd: 0.0,
                        is_fallback: false,
                        ..ModelBreakdown::default()
                    },
                ),
            ]),
        }],
        totals: Totals {
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 150,
            cost_usd: 0.25,
        },
        missing_directories: Vec::new(),
    };
    let session = ReportOutput::Session {
        rows: vec![SessionRow {
            session_id: "team/session".to_string(),
            directory: "team".to_string(),
            session_file: "session".to_string(),
            last_activity: "2025-09-11T18:00:00.000Z".to_string(),
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 150,
            cost_usd: 0.25,
            models: BTreeMap::from([(
                "gpt-5".to_string(),
                ModelBreakdown {
                    input_tokens: 100,
                    cached_input_tokens: 10,
                    output_tokens: 50,
                    reasoning_output_tokens: 0,
                    total_tokens: 150,
                    cost_usd: 0.0,
                    is_fallback: false,
                    ..ModelBreakdown::default()
                },
            )]),
        }],
        totals: Totals {
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 150,
            cost_usd: 0.25,
        },
        missing_directories: Vec::new(),
    };

    let daily_render = render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Include);
    let session_render = render_report(
        &session,
        "en-US",
        NumberFormat::Short,
        CacheReadMode::Include,
    );
    assert!(daily_render.contains("TOTAL"));
    assert!(daily_render.contains("2025-09-11"));
    assert!(daily_render.contains("Model"));
    assert!(daily_render.contains("gpt-5-codex"));
    assert!(session_render.contains("session"));
    assert!(session_render.contains("gpt-5"));
    assert!(session_render.contains("Last Activity"));
}

#[test]
fn render_report_handles_monthly_rows() {
    let monthly = ReportOutput::Monthly {
        rows: vec![MonthlyRow {
            month: "2025-09".to_string(),
            input_tokens: 120,
            cached_input_tokens: 20,
            output_tokens: 80,
            reasoning_output_tokens: 10,
            total_tokens: 200,
            cost_usd: 0.75,
            models: BTreeMap::new(),
        }],
        totals: Totals {
            input_tokens: 120,
            cached_input_tokens: 20,
            output_tokens: 80,
            reasoning_output_tokens: 10,
            total_tokens: 200,
            cost_usd: 0.75,
        },
        missing_directories: Vec::new(),
    };

    let rendered = render_report(
        &monthly,
        "en-US",
        NumberFormat::Short,
        CacheReadMode::Include,
    );
    assert!(rendered.contains("Monthly Codex Usage Report"));
    assert!(rendered.contains("2025-09"));
}

#[test]
fn render_report_omits_cache_column_when_cache_read_is_excluded() {
    let daily = ReportOutput::Daily {
        rows: vec![DailyRow {
            date: "2025-09-11".to_string(),
            input_tokens: 80,
            cached_input_tokens: 0,
            output_tokens: 20,
            reasoning_output_tokens: 0,
            total_tokens: 100,
            cost_usd: 0.25,
            models: BTreeMap::new(),
        }],
        totals: Totals {
            input_tokens: 80,
            cached_input_tokens: 0,
            output_tokens: 20,
            reasoning_output_tokens: 0,
            total_tokens: 100,
            cost_usd: 0.25,
        },
        missing_directories: Vec::new(),
    };

    let rendered = render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Exclude);

    assert!(!rendered.contains("Cache"));
    assert!(rendered.contains("Input"));
    assert!(rendered.contains("Output"));
}

#[test]
fn render_report_groups_model_rows_under_daily_subtotal() {
    let daily = ReportOutput::Daily {
        rows: vec![DailyRow {
            date: "2025-09-11".to_string(),
            input_tokens: 120,
            cached_input_tokens: 20,
            output_tokens: 80,
            reasoning_output_tokens: 10,
            total_tokens: 200,
            cost_usd: 0.75,
            models: BTreeMap::from([
                (
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 70,
                        cached_input_tokens: 10,
                        output_tokens: 40,
                        reasoning_output_tokens: 5,
                        total_tokens: 110,
                        cost_usd: 0.0,
                        is_fallback: false,
                        ..ModelBreakdown::default()
                    },
                ),
                (
                    "gpt-5-codex".to_string(),
                    ModelBreakdown {
                        input_tokens: 50,
                        cached_input_tokens: 10,
                        output_tokens: 40,
                        reasoning_output_tokens: 5,
                        total_tokens: 90,
                        cost_usd: 0.0,
                        is_fallback: false,
                        ..ModelBreakdown::default()
                    },
                ),
            ]),
        }],
        totals: Totals {
            input_tokens: 120,
            cached_input_tokens: 20,
            output_tokens: 80,
            reasoning_output_tokens: 10,
            total_tokens: 200,
            cost_usd: 0.75,
        },
        missing_directories: Vec::new(),
    };

    let rendered = render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Include);
    let subtotal = rendered
        .lines()
        .find(|line| line.contains("2025-09-11") && line.contains("TOTAL"))
        .expect("subtotal row");
    let gpt5 = rendered
        .lines()
        .find(|line| line.contains("gpt-5") && !line.contains("TOTAL"))
        .expect("gpt-5 row");
    let codex = rendered
        .lines()
        .find(|line| line.contains("gpt-5-codex"))
        .expect("gpt-5-codex row");

    assert!(subtotal.contains("120"));
    assert!(!gpt5.contains("2025-09-11"));
    assert!(gpt5.contains("70"));
    assert!(codex.contains("50"));
}

#[test]
fn render_report_keeps_last_activity_on_session_subtotal_only() {
    let session = ReportOutput::Session {
        rows: vec![SessionRow {
            session_id: "team/session".to_string(),
            directory: "team".to_string(),
            session_file: "session".to_string(),
            last_activity: "2025-09-11T18:00:00.000Z".to_string(),
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 150,
            cost_usd: 0.25,
            models: BTreeMap::from([(
                "gpt-5".to_string(),
                ModelBreakdown {
                    input_tokens: 100,
                    cached_input_tokens: 10,
                    output_tokens: 50,
                    reasoning_output_tokens: 0,
                    total_tokens: 150,
                    cost_usd: 0.0,
                    is_fallback: false,
                    ..ModelBreakdown::default()
                },
            )]),
        }],
        totals: Totals {
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 150,
            cost_usd: 0.25,
        },
        missing_directories: Vec::new(),
    };

    let rendered = render_report(
        &session,
        "en-US",
        NumberFormat::Short,
        CacheReadMode::Include,
    );
    let subtotal = rendered
        .lines()
        .find(|line| line.contains("session") && line.contains("TOTAL"))
        .expect("subtotal row");
    let model_row = rendered
        .lines()
        .find(|line| line.contains("gpt-5"))
        .expect("model row");

    assert!(subtotal.contains("2025-09-11T18:00:00.000Z"));
    assert!(!model_row.contains("2025-09-11T18:00:00.000Z"));
}

#[test]
fn render_report_splits_mixed_fallback_and_explicit_usage_for_same_model() {
    let report = ReportOutput::Daily {
        rows: vec![DailyRow {
            date: "2025-09-11".to_string(),
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 150,
            cost_usd: 0.25,
            models: BTreeMap::from([(
                "gpt-5".to_string(),
                ModelBreakdown {
                    input_tokens: 100,
                    cached_input_tokens: 10,
                    output_tokens: 50,
                    reasoning_output_tokens: 0,
                    total_tokens: 150,
                    cost_usd: 0.20,
                    fallback_usage: UsageTotals {
                        input: 20,
                        cached_input: 0,
                        output: 10,
                        reasoning_output: 0,
                        total: 30,
                    },
                    fallback_cost_usd: 0.05,
                    is_fallback: true,
                },
            )]),
        }],
        totals: Totals {
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 150,
            cost_usd: 0.25,
        },
        missing_directories: Vec::new(),
    };

    let rendered = render_report(
        &report,
        "en-US",
        NumberFormat::Short,
        CacheReadMode::Include,
    );
    let explicit_row = rendered
        .lines()
        .find(|line| line.contains("  gpt-5") && !line.contains("(fallback)"))
        .expect("explicit row");
    let fallback_row = rendered
        .lines()
        .find(|line| line.contains("  gpt-5 (fallback)"))
        .expect("fallback row");

    assert!(explicit_row.contains("80"));
    assert!(explicit_row.contains("$0.20"));
    assert!(fallback_row.contains("20"));
    assert!(fallback_row.contains("$0.05"));
}

#[test]
fn detect_table_style_requires_tty_and_256_color_support() {
    assert_eq!(
        detect_table_style_for(true, Some("xterm-256color"), None, false),
        TableStyle::Ansi256
    );
    assert_eq!(
        detect_table_style_for(true, Some("xterm-256color"), None, true),
        TableStyle::Plain
    );
    assert_eq!(
        detect_table_style_for(false, Some("xterm-256color"), None, false),
        TableStyle::Plain
    );
    assert_eq!(
        detect_table_style_for(true, Some("xterm"), Some("truecolor"), false),
        TableStyle::Ansi256
    );
}

#[test]
fn detect_border_style_requires_tty_and_utf8_locale() {
    assert_eq!(
        detect_border_style_for(true, Some("en_US.UTF-8"), None, None),
        BorderStyle::Unicode
    );
    assert_eq!(
        detect_border_style_for(true, None, Some("pl_PL.utf8"), None),
        BorderStyle::Unicode
    );
    assert_eq!(
        detect_border_style_for(true, Some("C"), None, None),
        BorderStyle::Ascii
    );
    assert_eq!(
        detect_border_style_for(false, Some("en_US.UTF-8"), None, None),
        BorderStyle::Ascii
    );
}

#[test]
fn unicode_border_helpers_emit_box_drawing_characters() {
    let headers = ["Date", "Model"];
    let widths = vec![10, 8];
    let row = format_data_row(
        &headers,
        BorderStyle::Unicode,
        &widths,
        &["2025-09-11".to_string(), "TOTAL".to_string()],
    );

    assert_eq!(
        table_rule(TableRuleKind::Top, BorderStyle::Unicode, &widths),
        "┌────────────┬──────────┐"
    );
    assert!(row.starts_with('│'));
    assert!(row.contains(" │ "));
    assert!(row.ends_with('│'));
}

#[test]
fn ascii_border_helpers_preserve_ascii_fallback() {
    let headers = ["Date", "Model"];
    let widths = vec![10, 8];
    let row = format_data_row(
        &headers,
        BorderStyle::Ascii,
        &widths,
        &["2025-09-11".to_string(), "TOTAL".to_string()],
    );

    assert_eq!(
        table_rule(TableRuleKind::Top, BorderStyle::Ascii, &widths),
        "+------------+----------+"
    );
    assert!(row.starts_with('|'));
    assert!(row.contains(" | "));
    assert!(row.ends_with('|'));
}

#[test]
fn paint_only_emits_ansi_sequences_when_enabled() {
    let plain = paint(TableStyle::Plain, TableElement::Header, "Header");
    let styled = paint(TableStyle::Ansi256, TableElement::Header, "Header");

    assert_eq!(plain, "Header");
    assert!(styled.starts_with("\u{1b}["));
    assert!(styled.ends_with("\u{1b}[0m"));
}

#[test]
fn styled_rows_keep_border_color_separate_from_row_color() {
    let mut output = String::new();
    let headers = ["Date", "Model"];
    let widths = vec![10, 8];
    let cells = vec!["2025-09-11".to_string(), "TOTAL".to_string()];

    write_table_row(
        &mut output,
        TableRenderConfig {
            style: TableStyle::Ansi256,
            borders: BorderStyle::Unicode,
            number_format: NumberFormat::Short,
        },
        &headers,
        &widths,
        &cells,
        TableElement::Subtotal,
    );

    assert!(output.contains("\u{1b}[38;5;24m│\u{1b}[0m"));
    assert!(output.contains("\u{1b}[1;38;5;117m 2025-09-11 \u{1b}[0m"));
    assert!(!output.contains("\u{1b}[1;38;5;117m│"));
}

#[test]
fn render_report_surfaces_missing_directories() {
    let daily = ReportOutput::Daily {
        rows: Vec::new(),
        totals: Totals::default(),
        missing_directories: vec!["/tmp/missing-a".to_string(), "/tmp/missing-b".to_string()],
    };

    let rendered = render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Include);
    assert!(rendered.contains("Warning: missing session directories"));
    assert!(rendered.contains("/tmp/missing-a"));
    assert!(rendered.contains("/tmp/missing-b"));
}

#[test]
fn render_report_shortens_token_columns_but_not_cost_by_default() {
    let daily = ReportOutput::Daily {
        rows: vec![DailyRow {
            date: "2025-09-11".to_string(),
            input_tokens: 100_000,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 100_050,
            cost_usd: 1234.5,
            models: BTreeMap::new(),
        }],
        totals: Totals {
            input_tokens: 100_000,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 0,
            total_tokens: 100_050,
            cost_usd: 1234.5,
        },
        missing_directories: Vec::new(),
    };

    let short = render_report(&daily, "en-US", NumberFormat::Short, CacheReadMode::Include);
    let full = render_report(&daily, "en-US", NumberFormat::Full, CacheReadMode::Include);

    assert!(short.contains("100K"));
    assert!(short.contains("$1234.50"));
    assert!(full.contains("100,000"));
    assert!(!full.contains("100K"));
}

#[test]
fn full_number_format_keeps_table_frame_aligned_for_grouped_digits() {
    let daily = ReportOutput::Daily {
        rows: vec![DailyRow {
            date: "2025-09-11".to_string(),
            input_tokens: 1_000,
            cached_input_tokens: 2_000,
            output_tokens: 3_000,
            reasoning_output_tokens: 4_000,
            total_tokens: 5_000,
            cost_usd: 12.5,
            models: BTreeMap::new(),
        }],
        totals: Totals {
            input_tokens: 1_000,
            cached_input_tokens: 2_000,
            output_tokens: 3_000,
            reasoning_output_tokens: 4_000,
            total_tokens: 5_000,
            cost_usd: 12.5,
        },
        missing_directories: Vec::new(),
    };

    let rendered = render_report(&daily, "en-US", NumberFormat::Full, CacheReadMode::Include);
    let lines = rendered
        .lines()
        .map(strip_ansi_sequences)
        .collect::<Vec<_>>();
    let top = lines
        .iter()
        .find(|line| line.starts_with('+') || line.starts_with('┌'))
        .expect("top border");
    let subtotal = lines
        .iter()
        .find(|line| line.contains("2025-09-11") && line.contains("1,000"))
        .expect("subtotal row");

    assert_eq!(top.chars().count(), subtotal.chars().count());
}

#[test]
fn collect_session_files_recurses_and_filters_extensions() {
    let temp = TempDir::new().expect("tempdir");
    let nested = temp.path().join("nested");
    fs::create_dir_all(&nested).expect("mkdir");
    fs::write(nested.join("a.jsonl"), "").expect("jsonl");
    fs::write(nested.join("b.txt"), "").expect("txt");

    let mut files = Vec::new();
    collect_session_files(temp.path(), &mut files).expect("collect");

    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with("a.jsonl"));
}

#[test]
fn scan_session_file_skips_bad_json_and_errors_on_invalid_timestamp() {
    let temp = TempDir::new().expect("tempdir");
    let sessions = temp.path().join("sessions");
    let session_file = sessions.join("project").join("session.jsonl");
    fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
    fs::write(
            &session_file,
            concat!(
                "not-json\n",
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"bad-timestamp\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1,\"cached_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0,\"total_tokens\":2}}}}\n"
            ),
        )
        .expect("write session");

    let mut builder = ReportBuilder::new(ReportKind::Daily, chrono_tz::UTC, None, None);
    let error =
        scan_session_file(&session_file, "project/session", &mut builder).expect_err("error");
    assert!(error.to_string().contains("invalid timestamp"));
}

#[test]
fn line_might_affect_usage_accepts_relevant_markers() {
    assert!(line_might_affect_usage(
        r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#
    ));
    assert!(line_might_affect_usage(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count"}}"#
    ));
    assert!(line_might_affect_usage(
        r#"{ "timestamp":"2026-01-01T00:00:00Z", "type":"event_msg", "payload":{"type":"token_count"} }"#
    ));
}

#[test]
fn line_might_affect_usage_rejects_irrelevant_lines() {
    assert!(!line_might_affect_usage(r#"{"type":"response_item"}"#));
    assert!(!line_might_affect_usage(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"agent_reasoning"}}"#
    ));
    assert!(!line_might_affect_usage("not-json"));
}

#[test]
fn line_might_affect_usage_fails_open_for_escaped_json_strings() {
    assert!(line_might_affect_usage(
        r#"{"type":"turn\u005fcontext","payload":{"model":"gpt-5"}}"#
    ));
    assert!(line_might_affect_usage(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token\u005fcount"}}"#
    ));
}

#[test]
fn scan_session_file_advances_cumulative_state_after_last_usage() {
    let temp = TempDir::new().expect("tempdir");
    let sessions = temp.path().join("sessions");
    let session_file = sessions.join("session.jsonl");
    fs::create_dir_all(&sessions).expect("mkdir");
    fs::write(
            &session_file,
            [
                r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#,
                r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110}}}}"#,
                r#"{"timestamp":"2026-01-01T00:01:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":30,"output_tokens":5,"total_tokens":35}}}}"#,
                r#"{"timestamp":"2026-01-01T00:02:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":150,"output_tokens":20,"total_tokens":170}}}}"#,
            ]
            .join("\n"),
        )
        .expect("write session");

    let mut builder = ReportBuilder::new(ReportKind::Session, chrono_tz::UTC, None, None);
    scan_session_file(&session_file, "session", &mut builder).expect("scan");

    let report = builder
        .finish(
            &PricingCatalog::default(),
            UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
            Vec::new(),
        )
        .expect("report");
    let ReportOutput::Session { rows, .. } = report else {
        panic!("expected session report");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].input_tokens, 150);
    assert_eq!(rows[0].output_tokens, 20);
    assert_eq!(rows[0].total_tokens, 170);
}

#[test]
fn parse_token_usage_line_tracks_turn_context_and_metadata_model() {
    let mut previous_totals = None;
    let mut current_model = None;
    let mut current_model_is_fallback = false;

    let turn_context = parse_token_usage_line(
        r#"{"type":"turn_context","payload":{"metadata":{"model":"gpt-5-mini"}}}"#,
        "session-key",
        "session-id",
        &mut previous_totals,
        &mut current_model,
        &mut current_model_is_fallback,
    )
    .expect("turn context parse");
    assert!(turn_context.is_none());
    assert_eq!(current_model.as_deref(), Some("gpt-5-mini"));
    assert!(!current_model_is_fallback);

    let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":12,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1,"total_tokens":15}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

    assert_eq!(event.model, "gpt-5-mini");
    assert!(!event.is_fallback_model);
    assert_eq!(
        event.usage,
        UsageTotals {
            input: 12,
            cached_input: 2,
            output: 3,
            reasoning_output: 1,
            total: 15,
        }
    );
}

#[test]
fn parse_token_usage_line_uses_fallback_model_until_explicit_model_arrives() {
    let mut previous_totals = None;
    let mut current_model = None;
    let mut current_model_is_fallback = false;

    let first_event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":8,"output_tokens":2,"total_tokens":10}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("first event parse")
        .expect("first token usage event");
    assert_eq!(first_event.model, DEFAULT_FALLBACK_MODEL);
    assert!(first_event.is_fallback_model);

    let second_event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:01:00Z","type":"event_msg","payload":{"type":"token_count","model":"gpt-5","info":{"last_token_usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("second event parse")
        .expect("second token usage event");
    assert_eq!(second_event.model, "gpt-5");
    assert!(!second_event.is_fallback_model);
}

#[test]
fn parse_token_usage_line_does_not_fall_back_to_payload_usage_when_info_exists() {
    let mut previous_totals = None;
    let mut current_model = None;
    let mut current_model_is_fallback = false;

    let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","last_token_usage":{"input_tokens":8,"output_tokens":2,"total_tokens":10},"info":{"model":"gpt-5"}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse");

    assert!(event.is_none());
}

#[test]
fn parse_token_usage_line_accepts_escaped_model_strings() {
    let mut previous_totals = None;
    let mut current_model = None;
    let mut current_model_is_fallback = false;

    let turn_context = parse_token_usage_line(
        r#"{"type":"turn_context","payload":{"metadata":{"model":"gpt\u002d5"}}}"#,
        "session-key",
        "session-id",
        &mut previous_totals,
        &mut current_model,
        &mut current_model_is_fallback,
    )
    .expect("turn context parse");
    assert!(turn_context.is_none());

    let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

    assert_eq!(event.model, "gpt-5");
    assert!(!event.is_fallback_model);
}

#[test]
fn parse_token_usage_line_prefers_explicit_model_over_nested_metadata() {
    let mut previous_totals = None;
    let mut current_model = None;
    let mut current_model_is_fallback = false;

    let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","model":"gpt-5","info":{"metadata":{"model":"gpt-5-mini"},"last_token_usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

    assert_eq!(event.model, "gpt-5");
}

#[test]
fn parse_token_usage_line_ignores_invalid_optional_subfields() {
    let mut previous_totals = None;
    let mut current_model = None;
    let mut current_model_is_fallback = false;

    let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"metadata":"invalid","last_token_usage":"invalid","total_token_usage":{"input_tokens":9,"output_tokens":1,"total_tokens":10}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

    assert_eq!(event.usage.total, 10);
    assert_eq!(event.model, DEFAULT_FALLBACK_MODEL);
    assert!(event.is_fallback_model);
}

#[test]
fn parse_token_usage_line_keeps_usage_when_one_counter_has_wrong_type() {
    let mut previous_totals = None;
    let mut current_model = None;
    let mut current_model_is_fallback = false;

    let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":2,"reasoning_output_tokens":"invalid","total_tokens":9}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

    assert_eq!(event.usage.input, 7);
    assert_eq!(event.usage.output, 2);
    assert_eq!(event.usage.reasoning_output, 0);
    assert_eq!(event.usage.total, 9);
}

#[test]
fn parse_token_usage_line_keeps_usage_when_model_field_has_wrong_type() {
    let mut previous_totals = None;
    let mut current_model = Some("gpt-5".to_string());
    let mut current_model_is_fallback = false;

    let event = parse_token_usage_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","model":["invalid"],"info":{"last_token_usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}}"#,
            "session-key",
            "session-id",
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )
        .expect("event parse")
        .expect("token usage event");

    assert_eq!(event.model, "gpt-5");
    assert!(!event.is_fallback_model);
    assert_eq!(event.usage.total, 5);
}

#[test]
fn session_reports_prefer_longer_duplicate_files() {
    let first = TempDir::new().expect("first");
    let second = TempDir::new().expect("second");
    let first_sessions = first.path().join("sessions");
    let second_sessions = second.path().join("sessions");
    let first_file = first_sessions.join("project").join("session.jsonl");
    let second_file = second_sessions.join("project").join("session.jsonl");
    fs::create_dir_all(first_file.parent().expect("first parent")).expect("mkdir first");
    fs::create_dir_all(second_file.parent().expect("second parent")).expect("mkdir second");
    let short_payload = concat!(
        "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
        "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n"
    );
    let long_payload = concat!(
        "{\"timestamp\":\"2025-09-11T18:00:00.000Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
        "{\"timestamp\":\"2025-09-11T18:01:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":10,\"reasoning_output_tokens\":0,\"total_tokens\":110}}}}\n",
        "{\"timestamp\":\"2025-09-11T18:02:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":50,\"cached_input_tokens\":0,\"output_tokens\":5,\"reasoning_output_tokens\":0,\"total_tokens\":55}}}}\n"
    );
    fs::write(&first_file, short_payload).expect("write first");
    fs::write(&second_file, long_payload).expect("write second");

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
            cached_input_cost_mode: CachedInputCostMode::Priced,
            cache_read_mode: CacheReadMode::Include,
            session_dirs: vec![first_sessions, second_sessions],
            project_dir: None,
            parallelism: ScannerParallelism::Auto,
        },
    )
    .expect("report");

    match report {
        ReportOutput::Session { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].session_id, "project/session");
            assert_eq!(rows[0].input_tokens, 150);
            assert_eq!(rows[0].output_tokens, 15);
            assert_eq!(rows[0].total_tokens, 165);
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

#[test]
fn build_watch_snapshot_tracks_current_day_totals_and_bounded_hourly_burn() {
    let temp = TempDir::new().expect("tempdir");
    let sessions = temp.path().join("sessions");
    let session_file = sessions.join("project").join("session.jsonl");
    fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
    fs::write(
            &session_file,
            [
                r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#,
                r#"{"timestamp":"2026-01-01T23:40:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10,"reasoning_output_tokens":4,"total_tokens":110}}}}"#,
                r#"{"timestamp":"2026-01-02T00:05:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":20,"cached_input_tokens":5,"output_tokens":3,"reasoning_output_tokens":1,"total_tokens":23}}}}"#,
                r#"{"timestamp":"2026-01-02T00:20:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":30,"cached_input_tokens":10,"output_tokens":7,"reasoning_output_tokens":2,"total_tokens":37}}}}"#,
            ]
            .join("\n"),
        )
        .expect("write session");

    let now = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let options = WatchOptions {
        timezone: "UTC".to_string(),
        locale: "en-US".to_string(),
        number_format: NumberFormat::Short,
        offline: true,
        refresh_pricing: false,
        cached_input_cost_mode: CachedInputCostMode::Priced,
        cache_read_mode: CacheReadMode::Include,
        session_dirs: vec![sessions],
        project_dir: None,
        parallelism: ScannerParallelism::Auto,
        interval: Duration::from_secs(5),
        show_model_burn_rate: true,
        #[cfg(debug_assertions)]
        debug: DebugRuntimeOptions::default(),
    };
    let snapshot = build_watch_snapshot_at(&options, now).expect("watch snapshot");

    assert_eq!(snapshot.date, "2026-01-02");
    assert_eq!(snapshot.totals.input_tokens, 50);
    assert_eq!(snapshot.totals.cached_input_tokens, 15);
    assert_eq!(snapshot.totals.output_tokens, 10);
    assert_eq!(snapshot.totals.reasoning_output_tokens, 3);
    assert_eq!(snapshot.totals.total_tokens, 60);
    assert_eq!(snapshot.burn_rate.window_minutes, 30);
    assert_eq!(snapshot.burn_rate.input_tokens_per_hour, 100);
    assert_eq!(snapshot.burn_rate.cached_input_tokens_per_hour, 30);
    assert_eq!(snapshot.burn_rate.output_tokens_per_hour, 20);
    assert_eq!(snapshot.burn_rate.reasoning_output_tokens_per_hour, 6);
    assert_eq!(snapshot.burn_rate.total_tokens_per_hour, 120);
    assert_eq!(snapshot.updated_time, "00:30:00");
    let gpt5 = snapshot.per_model.get("gpt-5").expect("gpt-5 model");
    assert_eq!(gpt5.input_tokens, 50);
    assert_eq!(gpt5.cached_input_tokens, 15);
    assert_eq!(gpt5.total_tokens, 60);

    let mut free_cache_options = options.clone();
    free_cache_options.cached_input_cost_mode = CachedInputCostMode::Free;
    let free_cache_snapshot =
        build_watch_snapshot_at(&free_cache_options, now).expect("watch snapshot");
    assert_eq!(free_cache_snapshot.totals.input_tokens, 50);
    assert_eq!(free_cache_snapshot.totals.cached_input_tokens, 15);
    assert!(
        (free_cache_snapshot.totals.cost_usd
            - ((35.0 / 1_000_000.0) * 1.25 + (10.0 / 1_000_000.0) * 10.0))
            .abs()
            < f64::EPSILON
    );
    assert!(
        (free_cache_snapshot.burn_rate.cost_usd_per_hour
            - ((70.0 / 1_000_000.0) * 1.25 + (20.0 / 1_000_000.0) * 10.0))
            .abs()
            < f64::EPSILON
    );

    let mut exclude_cache_options = options.clone();
    exclude_cache_options.cache_read_mode = CacheReadMode::Exclude;
    let exclude_cache_snapshot =
        build_watch_snapshot_at(&exclude_cache_options, now).expect("watch snapshot");
    assert_eq!(exclude_cache_snapshot.totals.input_tokens, 35);
    assert_eq!(exclude_cache_snapshot.totals.cached_input_tokens, 0);
    assert_eq!(exclude_cache_snapshot.totals.total_tokens, 45);
    assert_eq!(exclude_cache_snapshot.burn_rate.input_tokens_per_hour, 70);
    assert_eq!(
        exclude_cache_snapshot
            .burn_rate
            .cached_input_tokens_per_hour,
        0
    );
    assert_eq!(exclude_cache_snapshot.burn_rate.total_tokens_per_hour, 90);
}

#[test]
fn build_watch_snapshot_respects_project_dir_filter() {
    let temp = TempDir::new().expect("tempdir");
    let sessions = temp.path().join("sessions");
    let project = temp.path().join("project");
    let matching_file = sessions.join("matching").join("session.jsonl");
    let sibling_file = sessions.join("sibling").join("session.jsonl");
    fs::create_dir_all(matching_file.parent().expect("matching parent")).expect("mkdir");
    fs::create_dir_all(sibling_file.parent().expect("sibling parent")).expect("mkdir");
    fs::write(
        &matching_file,
        session_payload_with_cwd(&project.join("crate"), 20),
    )
    .expect("write matching");
    fs::write(
        &sibling_file,
        session_payload_with_cwd(&temp.path().join("project-sibling"), 100),
    )
    .expect("write sibling");

    let now = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let options = WatchOptions {
        timezone: "UTC".to_string(),
        locale: "en-US".to_string(),
        number_format: NumberFormat::Short,
        offline: true,
        refresh_pricing: false,
        cached_input_cost_mode: CachedInputCostMode::Priced,
        cache_read_mode: CacheReadMode::Include,
        session_dirs: vec![sessions],
        project_dir: Some(project),
        parallelism: ScannerParallelism::Auto,
        interval: Duration::from_secs(5),
        show_model_burn_rate: false,
        #[cfg(debug_assertions)]
        debug: DebugRuntimeOptions::default(),
    };
    let snapshot = build_watch_snapshot_at(&options, now).expect("watch snapshot");

    assert_eq!(snapshot.totals.input_tokens, 20);
    assert_eq!(snapshot.totals.total_tokens, 23);
}

#[test]
fn watch_runtime_refresh_reuses_unchanged_file_cache() {
    let temp = TempDir::new().expect("tempdir");
    let sessions = temp.path().join("sessions");
    let session_file = sessions.join("today").join("session.jsonl");
    fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
    let valid_payload = concat!(
        "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
        "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
    );
    fs::write(&session_file, valid_payload).expect("write valid file");
    let original_metadata = fs::metadata(&session_file).expect("metadata");
    let original_mtime = original_metadata.modified().expect("modified time");

    let now = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let timezone = chrono_tz::UTC;
    let mut runtime = WatchRuntimeState::load(
        std::slice::from_ref(&sessions),
        None,
        ScannerParallelism::Auto,
        timezone,
        now,
    )
    .expect("runtime");
    let initial_snapshot = runtime
        .snapshot(
            &PricingCatalog::default(),
            UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
            now,
            false,
        )
        .expect("initial snapshot");
    assert_eq!(initial_snapshot.totals.total_tokens, 23);

    let mut invalid_payload = String::from(
        "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n\
             {\"timestamp\":\"bad-timestamp\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}\n",
    );
    invalid_payload.push_str(&" ".repeat(valid_payload.len() - invalid_payload.len()));
    fs::write(&session_file, invalid_payload).expect("write invalid file");
    filetime::set_file_mtime(&session_file, FileTime::from_system_time(original_mtime))
        .expect("restore mtime");

    runtime
        .refresh(
            &[sessions],
            None,
            ScannerParallelism::Auto,
            now,
            WatchChangeSet::default(),
        )
        .expect("refresh");
    let refreshed_snapshot = runtime
        .snapshot(
            &PricingCatalog::default(),
            UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
            now,
            false,
        )
        .expect("refreshed snapshot");
    assert_eq!(refreshed_snapshot.totals.input_tokens, 20);
    assert_eq!(refreshed_snapshot.totals.output_tokens, 3);
    assert_eq!(refreshed_snapshot.totals.total_tokens, 23);
}

#[test]
fn watch_runtime_snapshot_ignores_future_dated_events_until_time_advances() {
    let temp = TempDir::new().expect("tempdir");
    let sessions = temp.path().join("sessions");
    let session_file = sessions.join("today").join("session.jsonl");
    fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
    let payload = concat!(
        "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
        "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
        "{\"timestamp\":\"2026-01-02T00:45:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":5,\"output_tokens\":1,\"total_tokens\":6}}}}\n"
    );
    fs::write(&session_file, payload).expect("write session");

    let timezone = chrono_tz::UTC;
    let loaded_at = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let runtime = WatchRuntimeState::load(
        std::slice::from_ref(&sessions),
        None,
        ScannerParallelism::Auto,
        timezone,
        loaded_at,
    )
    .expect("runtime");

    let early_snapshot = runtime
        .snapshot(
            &PricingCatalog::default(),
            UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
            loaded_at,
            false,
        )
        .expect("early snapshot");
    assert_eq!(early_snapshot.totals.total_tokens, 23);
    assert_eq!(early_snapshot.burn_rate.total_tokens_per_hour, 46);

    let later_now = DateTime::parse_from_rfc3339("2026-01-02T00:50:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let mut runtime = runtime;
    runtime
        .refresh(
            std::slice::from_ref(&sessions),
            None,
            ScannerParallelism::Auto,
            later_now,
            WatchChangeSet::default(),
        )
        .expect("advance runtime");
    let later_snapshot = runtime
        .snapshot(
            &PricingCatalog::default(),
            UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
            later_now,
            false,
        )
        .expect("later snapshot");
    assert_eq!(later_snapshot.totals.total_tokens, 29);
    assert_eq!(later_snapshot.burn_rate.total_tokens_per_hour, 35);
}

#[test]
fn watch_runtime_refreshes_dirty_session_after_append() {
    let temp = TempDir::new().expect("tempdir");
    let sessions = temp.path().join("sessions");
    let session_file = sessions.join("today").join("session.jsonl");
    fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
    fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
            ),
        )
        .expect("write session");

    let timezone = chrono_tz::UTC;
    let now = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let mut runtime = WatchRuntimeState::load(
        std::slice::from_ref(&sessions),
        None,
        ScannerParallelism::Auto,
        timezone,
        now,
    )
    .expect("runtime");

    fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:20:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":5,\"output_tokens\":1,\"total_tokens\":6}}}}\n"
            ),
        )
        .expect("append session");

    runtime
        .refresh(
            std::slice::from_ref(&sessions),
            None,
            ScannerParallelism::Auto,
            now,
            WatchChangeSet {
                dirty_sessions: HashMap::from([(
                    "today/session".to_string(),
                    WatchDirtyKind::AppendOnly,
                )]),
                discovery_due: false,
            },
        )
        .expect("refresh dirty session");
    let snapshot = runtime
        .snapshot(
            &PricingCatalog::default(),
            UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
            now,
            false,
        )
        .expect("snapshot");

    assert_eq!(snapshot.totals.total_tokens, 29);
    assert_eq!(snapshot.totals.input_tokens, 25);
    assert_eq!(snapshot.totals.output_tokens, 4);
}

#[test]
fn watch_runtime_rebuilds_longer_rewritten_file_instead_of_appending() {
    let temp = TempDir::new().expect("tempdir");
    let sessions = temp.path().join("sessions");
    let session_file = sessions.join("today").join("session.jsonl");
    fs::create_dir_all(session_file.parent().expect("parent")).expect("mkdir");
    fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
            ),
        )
        .expect("write session");

    let timezone = chrono_tz::UTC;
    let now = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let mut runtime = WatchRuntimeState::load(
        std::slice::from_ref(&sessions),
        None,
        ScannerParallelism::Auto,
        timezone,
        now,
    )
    .expect("runtime");

    fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":50,\"output_tokens\":10,\"total_tokens\":60}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:20:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}\n"
            ),
        )
        .expect("rewrite longer session");

    runtime
        .refresh(
            std::slice::from_ref(&sessions),
            None,
            ScannerParallelism::Auto,
            now,
            WatchChangeSet {
                dirty_sessions: HashMap::from([(
                    "today/session".to_string(),
                    WatchDirtyKind::FullRebuild,
                )]),
                discovery_due: false,
            },
        )
        .expect("refresh rewritten session");
    let snapshot = runtime
        .snapshot(
            &PricingCatalog::default(),
            UsagePresentation::new(CachedInputCostMode::Priced, CacheReadMode::Include),
            now,
            false,
        )
        .expect("snapshot");

    assert_eq!(snapshot.totals.total_tokens, 62);
    assert_eq!(snapshot.totals.input_tokens, 51);
    assert_eq!(snapshot.totals.output_tokens, 11);
}

#[test]
fn resolve_session_target_across_roots_re_resolves_duplicates() {
    let first = TempDir::new().expect("first");
    let second = TempDir::new().expect("second");
    let first_sessions = first.path().join("sessions");
    let second_sessions = second.path().join("sessions");
    let selected_file = first_sessions.join("project").join("session.jsonl");
    let duplicate_file = second_sessions.join("project").join("session.jsonl");
    let removed_file = first_sessions.join("gone").join("session.jsonl");
    fs::create_dir_all(selected_file.parent().expect("selected parent")).expect("mkdir first");
    fs::create_dir_all(duplicate_file.parent().expect("duplicate parent")).expect("mkdir second");
    fs::create_dir_all(removed_file.parent().expect("removed parent")).expect("mkdir gone");
    fs::write(&selected_file, "first version in preferred root").expect("write selected");
    fs::write(&duplicate_file, "backup").expect("write duplicate");
    fs::write(&removed_file, "gone").expect("write removed");

    let session_dirs = vec![first_sessions.clone(), second_sessions.clone()];
    let original = resolve_session_target_across_roots(&session_dirs, None, "project/session")
        .expect("resolve selected")
        .expect("selected target");
    assert_eq!(original.path, selected_file);

    fs::remove_file(&selected_file).expect("remove selected");
    fs::remove_file(&removed_file).expect("remove file");

    let refreshed = resolve_session_target_across_roots(&session_dirs, None, "project/session")
        .expect("refresh")
        .expect("fallback target");
    assert_eq!(refreshed.session_id, "project/session");
    assert_eq!(refreshed.path, duplicate_file);
    assert_eq!(refreshed.bytes, 6);
    assert!(
        resolve_session_target_across_roots(&session_dirs, None, "gone/session")
            .expect("missing session")
            .is_none()
    );
}

#[test]
fn watch_event_session_ids_cover_all_matching_roots() {
    let roots = vec![PathBuf::from("/logs"), PathBuf::from("/logs/project")];
    let path = PathBuf::from("/logs/project/a.jsonl");
    let mut session_ids = watch_event_session_ids(&roots, &path);
    session_ids.sort();

    assert_eq!(session_ids, vec!["a".to_string(), "project/a".to_string()]);
}

#[test]
fn watch_event_source_marks_newly_available_root_for_rediscovery() {
    let temp = TempDir::new().expect("tempdir");
    let late_root = temp.path().join("late-root");
    let mut source =
        WatchEventSource::new_polling(std::slice::from_ref(&late_root)).expect("watch source");

    assert!(!source.watched_roots.contains(&late_root));

    fs::create_dir_all(&late_root).expect("create late root");

    assert!(
        source
            .sync_session_dirs(std::slice::from_ref(&late_root))
            .expect("sync session dirs"),
        "late root should trigger immediate rediscovery",
    );
    assert!(source.watched_roots.contains(&late_root));
}

#[test]
fn scan_session_file_from_checkpoint_reads_only_appended_suffix() {
    let temp = TempDir::new().expect("tempdir");
    let session_file = temp.path().join("session.jsonl");
    fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
            ),
        )
        .expect("write session");

    let mut initial_events = Vec::new();
    let checkpoint = scan_session_file_from_checkpoint(
        &session_file,
        "project/session",
        &SessionParseCheckpoint::default(),
        |event| initial_events.push(event.usage.total),
    )
    .expect("initial scan");
    assert_eq!(initial_events, vec![23]);

    fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:10:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":5,\"output_tokens\":1,\"total_tokens\":6}}}}\n"
            ),
        )
        .expect("append session");

    let mut appended_events = Vec::new();
    let updated =
        scan_session_file_from_checkpoint(&session_file, "project/session", &checkpoint, |event| {
            appended_events.push((event.model.to_string(), event.usage.total));
        })
        .expect("incremental scan");
    assert_eq!(appended_events, vec![("gpt-5".to_string(), 6)]);
    assert!(updated.offset > checkpoint.offset);
}

#[test]
fn scan_session_file_from_checkpoint_retries_unterminated_jsonl_record() {
    let temp = TempDir::new().expect("tempdir");
    let session_file = temp.path().join("session.jsonl");
    fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:10:00Z\",\"type\":\"event_msg\""
            ),
        )
        .expect("write partial session");

    let checkpoint = scan_session_file_from_checkpoint(
        &session_file,
        "project/session",
        &SessionParseCheckpoint::default(),
        |_| {},
    )
    .expect("scan with partial line");
    assert_eq!(
            checkpoint.offset,
            u64::try_from(
                concat!(
                    "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                    "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n"
                )
                .len()
            )
            .expect("offset fits")
        );

    fs::write(
            &session_file,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-01-02T00:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"output_tokens\":3,\"total_tokens\":23}}}}\n",
                "{\"timestamp\":\"2026-01-02T00:10:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":5,\"output_tokens\":1,\"total_tokens\":6}}}}\n"
            ),
        )
        .expect("finish session");

    let mut appended_totals = Vec::new();
    let updated =
        scan_session_file_from_checkpoint(&session_file, "project/session", &checkpoint, |event| {
            appended_totals.push(event.usage.total);
        })
        .expect("rescan completed line");

    assert_eq!(appended_totals, vec![6]);
    assert!(updated.offset > checkpoint.offset);
}

#[test]
fn cached_watch_file_burn_events_skip_older_history() {
    let event = |timestamp: &str, total: u64| OwnedWatchEvent {
        timestamp_utc: DateTime::parse_from_rfc3339(timestamp)
            .expect("timestamp")
            .with_timezone(&Utc),
        model: "gpt-5".to_string(),
        is_fallback_model: false,
        usage: UsageTotals {
            input: total,
            cached_input: 0,
            output: 0,
            reasoning_output: 0,
            total,
        },
    };
    let cached = CachedWatchFile {
        target: SessionScanTarget {
            path: PathBuf::from("/tmp/session.jsonl"),
            session_id: "project/session".to_string(),
            bytes: 0,
            modified: None,
        },
        parser_checkpoint: SessionParseCheckpoint::default(),
        current_day_events: vec![
            event("2026-01-02T00:05:00Z", 5),
            event("2026-01-02T00:30:00Z", 7),
            event("2026-01-02T00:45:00Z", 11),
        ],
        visible_end: 0,
        burn_start: 0,
    };

    let window_start = DateTime::parse_from_rfc3339("2026-01-02T00:30:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let window_end = DateTime::parse_from_rfc3339("2026-01-02T00:59:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let mut cached = cached;
    cached.recompute_bounds(window_start, window_end);
    let suffix = cached.burn_events();

    assert_eq!(suffix.len(), 2);
    assert_eq!(suffix[0].usage.total, 7);
    assert_eq!(suffix[1].usage.total, 11);
}

#[test]
fn render_watch_screen_includes_totals_and_burn_rate_columns() {
    let rendered = render_watch_screen(
        &WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 50,
                cached_input_tokens: 15,
                output_tokens: 10,
                reasoning_output_tokens: 3,
                total_tokens: 60,
                cost_usd: 1.25,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(30 * 60),
                window_minutes: 30,
                input_tokens_per_hour: 100,
                cached_input_tokens_per_hour: 30,
                output_tokens_per_hour: 20,
                reasoning_output_tokens_per_hour: 6,
                total_tokens_per_hour: 120,
                cost_usd_per_hour: 2.5,
            },
            per_model: BTreeMap::new(),
            missing_directories: vec!["/tmp/missing".to_string()],
            updated_time: "00:30:00".to_string(),
        },
        "en-US",
        NumberFormat::Short,
        false,
        CacheReadMode::Include,
    );

    assert!(rendered.contains("Current Day Codex Usage Watch"));
    assert!(rendered.contains("Burn Rate (/h)"));
    assert!(rendered.contains("2026-01-02"));
    assert!(rendered.contains("Updated"));
    assert!(rendered.contains("00:30:00"));
    assert!(rendered.contains("Input"));
    assert!(rendered.contains("$2.50"));
    assert!(rendered.contains("/tmp/missing"));
}

#[test]
fn render_watch_screen_omits_cache_metric_when_cache_read_is_excluded() {
    let rendered = render_watch_screen(
        &WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 35,
                cached_input_tokens: 0,
                output_tokens: 10,
                reasoning_output_tokens: 3,
                total_tokens: 45,
                cost_usd: 1.25,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(30 * 60),
                window_minutes: 30,
                input_tokens_per_hour: 70,
                cached_input_tokens_per_hour: 0,
                output_tokens_per_hour: 20,
                reasoning_output_tokens_per_hour: 6,
                total_tokens_per_hour: 90,
                cost_usd_per_hour: 2.5,
            },
            per_model: BTreeMap::new(),
            missing_directories: Vec::new(),
            updated_time: "00:30:00".to_string(),
        },
        "en-US",
        NumberFormat::Short,
        false,
        CacheReadMode::Exclude,
    );

    assert!(!rendered.contains("Cache"));
    assert!(rendered.contains("Input"));
    assert!(rendered.contains("Output"));
}

#[test]
fn render_watch_screen_honors_number_format() {
    let snapshot = WatchSnapshot {
        date: "2026-01-02".to_string(),
        totals: Totals {
            input_tokens: 100_000,
            cached_input_tokens: 2_000,
            output_tokens: 3_000,
            reasoning_output_tokens: 4_000,
            total_tokens: 107_000,
            cost_usd: 12.5,
        },
        burn_rate: BurnRateSnapshot {
            window_duration: Duration::from_secs(60 * 60),
            window_minutes: 60,
            input_tokens_per_hour: 100_000,
            cached_input_tokens_per_hour: 2_000,
            output_tokens_per_hour: 3_000,
            reasoning_output_tokens_per_hour: 4_000,
            total_tokens_per_hour: 107_000,
            cost_usd_per_hour: 12.5,
        },
        per_model: BTreeMap::new(),
        missing_directories: Vec::new(),
        updated_time: "00:30:00".to_string(),
    };

    let short = render_watch_screen(
        &snapshot,
        "en-US",
        NumberFormat::Short,
        false,
        CacheReadMode::Include,
    );
    let full = render_watch_screen(
        &snapshot,
        "en-US",
        NumberFormat::Full,
        false,
        CacheReadMode::Include,
    );

    assert!(short.contains("100K"));
    assert!(!short.contains("100,000"));
    assert!(full.contains("100,000"));
}

#[test]
fn render_watch_screen_omits_per_model_columns_when_flag_is_disabled() {
    let rendered = render_watch_screen(
        &WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 50,
                cached_input_tokens: 15,
                output_tokens: 10,
                reasoning_output_tokens: 3,
                total_tokens: 60,
                cost_usd: 1.25,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(30 * 60),
                window_minutes: 30,
                input_tokens_per_hour: 100,
                cached_input_tokens_per_hour: 30,
                output_tokens_per_hour: 20,
                reasoning_output_tokens_per_hour: 6,
                total_tokens_per_hour: 120,
                cost_usd_per_hour: 2.5,
            },
            per_model: BTreeMap::from([(
                "gpt-5".to_string(),
                ModelBreakdown {
                    input_tokens: 20,
                    cached_input_tokens: 5,
                    output_tokens: 4,
                    reasoning_output_tokens: 1,
                    total_tokens: 24,
                    cost_usd: 0.5,
                    fallback_usage: UsageTotals::default(),
                    fallback_cost_usd: 0.0,
                    is_fallback: false,
                },
            )]),
            missing_directories: Vec::new(),
            updated_time: "00:30:00".to_string(),
        },
        "en-US",
        NumberFormat::Short,
        false,
        CacheReadMode::Include,
    );

    assert!(!rendered.contains("| gpt-5 "));
}

#[test]
fn render_watch_screen_includes_per_model_columns_and_skips_zero_usage_models() {
    let rendered = render_watch_screen(
        &WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 50,
                cached_input_tokens: 15,
                output_tokens: 10,
                reasoning_output_tokens: 3,
                total_tokens: 60,
                cost_usd: 1.25,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(30 * 60),
                window_minutes: 30,
                input_tokens_per_hour: 100,
                cached_input_tokens_per_hour: 30,
                output_tokens_per_hour: 20,
                reasoning_output_tokens_per_hour: 6,
                total_tokens_per_hour: 120,
                cost_usd_per_hour: 2.5,
            },
            per_model: BTreeMap::from([
                (
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 20,
                        cached_input_tokens: 5,
                        output_tokens: 4,
                        reasoning_output_tokens: 1,
                        total_tokens: 24,
                        cost_usd: 0.5,
                        fallback_usage: UsageTotals::default(),
                        fallback_cost_usd: 0.0,
                        is_fallback: false,
                    },
                ),
                (
                    "gpt-5-codex".to_string(),
                    ModelBreakdown {
                        input_tokens: 30,
                        cached_input_tokens: 10,
                        output_tokens: 6,
                        reasoning_output_tokens: 2,
                        total_tokens: 36,
                        cost_usd: 0.75,
                        fallback_usage: UsageTotals::default(),
                        fallback_cost_usd: 0.0,
                        is_fallback: false,
                    },
                ),
                (
                    "zero-model".to_string(),
                    ModelBreakdown {
                        input_tokens: 0,
                        cached_input_tokens: 0,
                        output_tokens: 0,
                        reasoning_output_tokens: 0,
                        total_tokens: 0,
                        cost_usd: 0.0,
                        fallback_usage: UsageTotals::default(),
                        fallback_cost_usd: 0.0,
                        is_fallback: false,
                    },
                ),
            ]),
            missing_directories: Vec::new(),
            updated_time: "00:30:00".to_string(),
        },
        "en-US",
        NumberFormat::Short,
        true,
        CacheReadMode::Include,
    );

    assert!(rendered.contains("gpt-5"));
    assert!(rendered.contains("gpt-5-codex"));
    assert!(!rendered.contains("zero-model"));
    assert!(rendered.contains("Burn Rate (/h)"));
    assert!(rendered.contains("$1.00"));
    assert!(rendered.contains("$1.50"));
}

#[test]
fn supports_watch_screen_clear_rejects_dumb_term() {
    assert!(supports_watch_screen_clear_with_platform(
        Some("xterm-256color"),
        false,
        false
    ));
    assert!(!supports_watch_screen_clear_with_platform(
        Some("dumb"),
        false,
        false
    ));
    assert!(supports_watch_screen_clear_with_platform(
        None, false, false
    ));
}

#[test]
fn supports_watch_screen_clear_accepts_windows_console_probe() {
    assert!(!supports_watch_screen_clear_with_platform(
        None, true, false
    ));
    assert!(supports_watch_screen_clear_with_platform(
        Some("xterm-256color"),
        true,
        false
    ));
    assert!(supports_watch_screen_clear_with_platform(None, true, true));
}

#[test]
fn display_window_minutes_rounds_up_partial_minutes() {
    assert_eq!(display_window_minutes(Duration::from_secs(0)), 0);
    assert_eq!(display_window_minutes(Duration::from_secs(1)), 1);
    assert_eq!(display_window_minutes(Duration::from_secs(59)), 1);
    assert_eq!(display_window_minutes(Duration::from_secs(61)), 2);
}

#[test]
fn remaining_watch_sleep_subtracts_elapsed_work() {
    assert_eq!(
        remaining_watch_sleep(Duration::from_secs(5), Duration::from_secs(2)),
        Duration::from_secs(3)
    );
    assert_eq!(
        remaining_watch_sleep(Duration::from_secs(5), Duration::from_secs(7)),
        Duration::from_secs(0)
    );
}

#[test]
fn scale_usage_per_hour_preserves_partial_second_windows() {
    assert_eq!(scale_usage_per_hour(2, Duration::from_millis(500)), 14_400);
    assert_eq!(scale_usage_per_hour(1, Duration::from_millis(250)), 14_400);
}

#[test]
fn render_watch_screen_places_burn_rate_column_last() {
    let rendered = render_watch_screen(
        &WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 10,
                cached_input_tokens: 0,
                output_tokens: 2,
                reasoning_output_tokens: 1,
                total_tokens: 12,
                cost_usd: 0.25,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(30 * 60),
                window_minutes: 30,
                input_tokens_per_hour: 20,
                cached_input_tokens_per_hour: 0,
                output_tokens_per_hour: 4,
                reasoning_output_tokens_per_hour: 2,
                total_tokens_per_hour: 24,
                cost_usd_per_hour: 0.5,
            },
            per_model: BTreeMap::from([(
                "gpt-5".to_string(),
                ModelBreakdown {
                    input_tokens: 10,
                    cached_input_tokens: 0,
                    output_tokens: 2,
                    reasoning_output_tokens: 1,
                    total_tokens: 12,
                    cost_usd: 0.25,
                    fallback_usage: UsageTotals::default(),
                    fallback_cost_usd: 0.0,
                    is_fallback: false,
                },
            )]),
            missing_directories: Vec::new(),
            updated_time: "00:30:00".to_string(),
        },
        "en-US",
        NumberFormat::Full,
        true,
        CacheReadMode::Include,
    );

    let lines = normalized_table_lines(&rendered);
    let header = lines
        .iter()
        .find(|line| line.contains("Metric") && line.contains("Burn Rate (/h)"))
        .expect("header");
    assert!(header.contains("Metric"));
    assert!(header.contains("Today"));
    assert!(header.contains("gpt-5 /h"));
    assert!(header.ends_with("Burn Rate (/h) |"));
}

#[test]
fn render_watch_screen_scales_each_model_burn_column_independently() {
    let rendered = render_watch_screen(
        &WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 2,
                cached_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 2,
                cost_usd: 0.02,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(59 * 60),
                window_minutes: 59,
                input_tokens_per_hour: 2,
                cached_input_tokens_per_hour: 0,
                output_tokens_per_hour: 0,
                reasoning_output_tokens_per_hour: 0,
                total_tokens_per_hour: 2,
                cost_usd_per_hour: 0.02,
            },
            per_model: BTreeMap::from([
                (
                    "gpt-5".to_string(),
                    ModelBreakdown {
                        input_tokens: 1,
                        cached_input_tokens: 0,
                        output_tokens: 0,
                        reasoning_output_tokens: 0,
                        total_tokens: 1,
                        cost_usd: 0.01,
                        fallback_usage: UsageTotals::default(),
                        fallback_cost_usd: 0.0,
                        is_fallback: false,
                    },
                ),
                (
                    "gpt-5-codex".to_string(),
                    ModelBreakdown {
                        input_tokens: 1,
                        cached_input_tokens: 0,
                        output_tokens: 0,
                        reasoning_output_tokens: 0,
                        total_tokens: 1,
                        cost_usd: 0.01,
                        fallback_usage: UsageTotals::default(),
                        fallback_cost_usd: 0.0,
                        is_fallback: false,
                    },
                ),
            ]),
            missing_directories: Vec::new(),
            updated_time: "00:30:00".to_string(),
        },
        "en-US",
        NumberFormat::Full,
        true,
        CacheReadMode::Include,
    );

    let lines = normalized_table_lines(&rendered);
    let input_row = lines
        .iter()
        .find(|line| line.contains("| Input "))
        .expect("input row");
    let cells = input_row
        .split('|')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(cells, vec!["Input", "2", "1", "1", "2"]);
}

#[test]
fn render_watch_screen_includes_fallback_model_columns() {
    let rendered = render_watch_screen(
        &WatchSnapshot {
            date: "2026-01-02".to_string(),
            totals: Totals {
                input_tokens: 2,
                cached_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 2,
                cost_usd: 0.02,
            },
            burn_rate: BurnRateSnapshot {
                window_duration: Duration::from_secs(30 * 60),
                window_minutes: 30,
                input_tokens_per_hour: 4,
                cached_input_tokens_per_hour: 0,
                output_tokens_per_hour: 0,
                reasoning_output_tokens_per_hour: 0,
                total_tokens_per_hour: 4,
                cost_usd_per_hour: 0.04,
            },
            per_model: BTreeMap::from([(
                "gpt-5".to_string(),
                ModelBreakdown {
                    input_tokens: 2,
                    cached_input_tokens: 0,
                    output_tokens: 0,
                    reasoning_output_tokens: 0,
                    total_tokens: 2,
                    cost_usd: 0.01,
                    fallback_usage: UsageTotals {
                        input: 1,
                        cached_input: 0,
                        output: 0,
                        reasoning_output: 0,
                        total: 1,
                    },
                    fallback_cost_usd: 0.01,
                    is_fallback: true,
                },
            )]),
            missing_directories: Vec::new(),
            updated_time: "00:30:00".to_string(),
        },
        "en-US",
        NumberFormat::Full,
        true,
        CacheReadMode::Include,
    );

    let lines = normalized_table_lines(&rendered);
    let header = lines
        .iter()
        .find(|line| line.contains("Metric") && line.contains("Burn Rate (/h)"))
        .expect("header");
    assert!(header.contains("gpt-5 /h"));
    assert!(header.contains("gpt-5 (fallback) /h"));

    let input_row = lines
        .iter()
        .find(|line| line.contains("| Input "))
        .expect("input row");
    let cells = input_row
        .split('|')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(cells, vec!["Input", "2", "2", "2", "4"]);
}

#[test]
fn render_watch_screen_wraps_model_columns_into_stacked_tables() {
    let snapshot = watch_snapshot_with_models(BTreeMap::from([
        (
            "gpt-5".to_string(),
            ModelBreakdown {
                input_tokens: 20,
                cached_input_tokens: 5,
                output_tokens: 4,
                reasoning_output_tokens: 1,
                total_tokens: 24,
                cost_usd: 0.60,
                fallback_usage: UsageTotals::default(),
                fallback_cost_usd: 0.0,
                is_fallback: false,
            },
        ),
        (
            "gpt-5-codex".to_string(),
            ModelBreakdown {
                input_tokens: 20,
                cached_input_tokens: 5,
                output_tokens: 4,
                reasoning_output_tokens: 1,
                total_tokens: 24,
                cost_usd: 0.60,
                fallback_usage: UsageTotals::default(),
                fallback_cost_usd: 0.0,
                is_fallback: false,
            },
        ),
        (
            "gpt-4.1-mini".to_string(),
            ModelBreakdown {
                input_tokens: 20,
                cached_input_tokens: 5,
                output_tokens: 4,
                reasoning_output_tokens: 1,
                total_tokens: 24,
                cost_usd: 0.60,
                fallback_usage: UsageTotals::default(),
                fallback_cost_usd: 0.0,
                is_fallback: false,
            },
        ),
    ]));

    let rendered = super::render::render_watch_screen_with_width(
        &snapshot,
        "en-US",
        NumberFormat::Full,
        true,
        CacheReadMode::Include,
        Some(40),
    );

    let lines = normalized_table_lines(&rendered);
    let header_lines = lines
        .iter()
        .filter(|line| line.contains("Metric"))
        .collect::<Vec<_>>();
    assert_eq!(header_lines.len(), 3);
    assert_eq!(
        header_lines
            .iter()
            .filter(|line| line.contains("Today"))
            .count(),
        1
    );
    assert_eq!(
        header_lines
            .iter()
            .filter(|line| line.contains("Burn Rate (/h)"))
            .count(),
        1
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("| Input "))
            .count(),
        3
    );
    assert!(header_lines[0].contains("gpt-4.1-mini /h"));
    assert!(header_lines[1].contains("gpt-5 /h"));
    assert!(header_lines[2].contains("gpt-5-codex /h"));
}

#[test]
fn render_watch_screen_keeps_single_table_when_width_is_sufficient() {
    let snapshot = watch_snapshot_with_models(BTreeMap::from([
        (
            "gpt-5".to_string(),
            ModelBreakdown {
                input_tokens: 30,
                cached_input_tokens: 8,
                output_tokens: 5,
                reasoning_output_tokens: 2,
                total_tokens: 35,
                cost_usd: 0.75,
                fallback_usage: UsageTotals::default(),
                fallback_cost_usd: 0.0,
                is_fallback: false,
            },
        ),
        (
            "gpt-5-codex".to_string(),
            ModelBreakdown {
                input_tokens: 30,
                cached_input_tokens: 7,
                output_tokens: 7,
                reasoning_output_tokens: 1,
                total_tokens: 37,
                cost_usd: 1.05,
                fallback_usage: UsageTotals::default(),
                fallback_cost_usd: 0.0,
                is_fallback: false,
            },
        ),
    ]));

    let rendered = super::render::render_watch_screen_with_width(
        &snapshot,
        "en-US",
        NumberFormat::Full,
        true,
        CacheReadMode::Include,
        Some(200),
    );

    let lines = normalized_table_lines(&rendered);
    let header_lines = lines
        .iter()
        .filter(|line| line.contains("Metric"))
        .collect::<Vec<_>>();
    assert_eq!(header_lines.len(), 1);
    assert!(header_lines[0].contains("Today"));
    assert!(header_lines[0].contains("gpt-5 /h"));
    assert!(header_lines[0].contains("gpt-5-codex /h"));
    assert!(header_lines[0].contains("Burn Rate (/h)"));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("| Input "))
            .count(),
        1
    );
}

#[test]
fn watch_pricing_refresh_due_uses_cache_age_and_retry_backoff() {
    let temp = TempDir::new().expect("tempdir");
    let cache_path = temp.path().join("pricing-cache.json");
    let now = SystemTime::UNIX_EPOCH + Duration::from_hours(100);
    let fresh_refreshed_at =
        i64::try_from(Duration::from_hours(77).as_secs()).expect("fresh timestamp fits");
    fs::write(
        &cache_path,
        format!("{{\"refreshed_at_epoch_seconds\":{fresh_refreshed_at},\"models\":{{}}}}"),
    )
    .expect("write fresh cache");

    assert!(!watch_pricing_refresh_due(&cache_path, now, false, None).expect("fresh decision"));

    let stale_refreshed_at =
        i64::try_from(Duration::from_hours(75).as_secs()).expect("stale timestamp fits");
    fs::write(
        &cache_path,
        format!("{{\"refreshed_at_epoch_seconds\":{stale_refreshed_at},\"models\":{{}}}}"),
    )
    .expect("write stale cache");

    assert!(watch_pricing_refresh_due(&cache_path, now, false, None).expect("stale decision"));
    assert!(
        !watch_pricing_refresh_due(&cache_path, now, false, Some(now - Duration::from_mins(4)),)
            .expect("backoff decision")
    );
}

#[test]
fn resolve_local_midnight_utc_handles_midnight_dst_skip() {
    let day = NaiveDate::from_ymd_opt(2024, 4, 26).expect("day");
    let resolved = resolve_local_midnight_utc(chrono_tz::Africa::Cairo, day).expect("start");

    assert_eq!(
        resolved,
        DateTime::parse_from_rfc3339("2024-04-25T22:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc)
    );
}

#[test]
fn cli_accepts_watch_interval_flag() {
    let cli = Cli::try_parse_from(["codexusage", "watch", "--interval", "15"]).expect("cli");

    let Some(Command::Watch {
        interval,
        per_model_burn_rate,
    }) = cli.command
    else {
        panic!("expected watch command");
    };
    assert_eq!(interval, Duration::from_secs(15));
    assert!(!per_model_burn_rate);
}

#[test]
fn cli_accepts_watch_per_model_burn_rate_flag() {
    let cli = Cli::try_parse_from(["codexusage", "watch", "--per-model-burn-rate"]).expect("cli");

    let Some(Command::Watch {
        interval,
        per_model_burn_rate,
    }) = cli.command
    else {
        panic!("expected watch command");
    };
    assert_eq!(interval, Duration::from_secs(5));
    assert!(per_model_burn_rate);
}

#[test]
fn cli_accepts_no_cache_cost_as_global_flag() {
    let cli = Cli::try_parse_from(["codexusage", "--no-cache-cost", "daily"]).expect("cli");

    assert_eq!(
        cli.cache_cost.cached_input_cost_mode(),
        CachedInputCostMode::Free
    );
    assert_eq!(cli.command, Some(Command::Daily));

    let watch_cli = Cli::try_parse_from(["codexusage", "watch", "--no-cache-cost"]).expect("cli");

    assert_eq!(
        watch_cli.cache_cost.cached_input_cost_mode(),
        CachedInputCostMode::Free
    );
    assert!(matches!(watch_cli.command, Some(Command::Watch { .. })));
}

#[test]
fn cli_accepts_exclude_cache_read_as_global_flag() {
    let cli = Cli::try_parse_from(["codexusage", "--exclude-cache-read", "daily"]).expect("cli");

    assert_eq!(
        cli.cache_cost.cached_input_cost_mode(),
        CachedInputCostMode::Free
    );
    assert_eq!(cli.cache_cost.cache_read_mode(), CacheReadMode::Exclude);
    assert_eq!(cli.command, Some(Command::Daily));

    let watch_cli =
        Cli::try_parse_from(["codexusage", "watch", "--exclude-cache-read"]).expect("cli");

    assert_eq!(
        watch_cli.cache_cost.cached_input_cost_mode(),
        CachedInputCostMode::Free
    );
    assert_eq!(
        watch_cli.cache_cost.cache_read_mode(),
        CacheReadMode::Exclude
    );
    assert!(matches!(watch_cli.command, Some(Command::Watch { .. })));
}

#[test]
fn cli_accepts_project_dir_as_global_flag() {
    let cli =
        Cli::try_parse_from(["codexusage", "--project-dir", "/tmp/project", "daily"]).expect("cli");

    assert_eq!(cli.project.project_dir, Some(PathBuf::from("/tmp/project")));
    assert!(!cli.project.current_dir);
    assert_eq!(cli.command, Some(Command::Daily));

    let watch_cli = Cli::try_parse_from(["codexusage", "watch", "--project-dir", "/tmp/project"])
        .expect("watch cli");
    assert_eq!(
        watch_cli.project.project_dir,
        Some(PathBuf::from("/tmp/project"))
    );
    assert!(matches!(watch_cli.command, Some(Command::Watch { .. })));
}

#[test]
fn cli_accepts_current_dir_project_shortcut() {
    let cli = Cli::try_parse_from(["codexusage", "--current-dir", "monthly"]).expect("cli");

    assert!(cli.project.current_dir);
    assert_eq!(cli.project.project_dir, None);
    assert_eq!(cli.command, Some(Command::Monthly));
}

#[test]
fn cli_rejects_project_dir_with_current_dir() {
    let error = Cli::try_parse_from([
        "codexusage",
        "--project-dir",
        "/tmp/project",
        "--current-dir",
        "daily",
    ])
    .expect_err("conflicting project filters should fail");

    let rendered = error.to_string();
    assert!(rendered.contains("--project-dir"));
    assert!(rendered.contains("--current-dir"));
}

#[test]
fn run_rejects_watch_json_output() {
    let error = run(["codexusage", "--json", "watch"]
        .into_iter()
        .map(OsString::from))
    .expect_err("watch json should fail");

    assert!(error.to_string().contains("watch"));
    assert!(error.to_string().contains("--json"));
}

#[test]
fn run_rejects_watch_date_filters() {
    let error = run(["codexusage", "--since", "2026-01-01", "watch"]
        .into_iter()
        .map(OsString::from))
    .expect_err("watch with date filters should fail");

    let rendered = error.to_string();
    assert!(rendered.contains("watch"));
    assert!(rendered.contains("--since"));
}

#[test]
fn cli_accepts_threads_flag() {
    let cli = Cli::try_parse_from(["codexusage", "--threads", "1", "daily"]).expect("cli");

    assert_eq!(cli.threads, NonZeroUsize::new(1));
}

#[cfg(debug_assertions)]
#[test]
fn cli_accepts_debug_simulate_slow_disk_flag() {
    let cli =
        Cli::try_parse_from(["codexusage", "--debug-simulate-slow-disk", "daily"]).expect("cli");

    assert!(cli.debug.simulate_slow_disk);
}

#[test]
fn cli_accepts_daily_last_days_flag() {
    let cli = Cli::try_parse_from(["codexusage", "daily", "--last-days", "7"]).expect("cli");

    assert_eq!(cli.command, Some(Command::Daily));
    assert_eq!(cli.last_days, NonZeroUsize::new(7));
}

#[test]
fn cli_accepts_daily_last_days_short_flag() {
    let cli = Cli::try_parse_from(["codexusage", "daily", "-L", "7"]).expect("cli");

    assert_eq!(cli.command, Some(Command::Daily));
    assert_eq!(cli.last_days, NonZeroUsize::new(7));
}

#[test]
fn cli_accepts_last_days_for_implicit_daily() {
    let cli = Cli::try_parse_from(["codexusage", "--last-days", "7"]).expect("cli");

    assert_eq!(cli.command, None);
    assert_eq!(cli.last_days, NonZeroUsize::new(7));
}

#[test]
fn cli_rejects_zero_last_days() {
    let error = Cli::try_parse_from(["codexusage", "daily", "--last-days", "0"])
        .expect_err("zero last_days should fail");

    let rendered = error.to_string();
    assert!(rendered.contains("--last-days"));
    assert!(rendered.contains('0'));
}

#[test]
fn effective_filters_reject_last_days_with_since() {
    let timezone = "UTC".parse::<Tz>().expect("timezone");
    let now_utc = DateTime::parse_from_rfc3339("2025-09-11T12:00:00+00:00")
        .expect("timestamp")
        .with_timezone(&Utc);
    let error = resolve_report_date_filters(
        ReportKind::Daily,
        Some(NonZeroUsize::new(7).expect("non-zero")),
        Some("2025-09-10"),
        None,
        timezone,
        now_utc,
    )
    .expect_err("conflicting date filters should fail");

    let rendered = error.to_string();
    assert!(rendered.contains("last_days"));
    assert!(rendered.contains("since/until"));
}

#[test]
fn effective_filters_reject_last_days_with_until() {
    let timezone = "UTC".parse::<Tz>().expect("timezone");
    let now_utc = DateTime::parse_from_rfc3339("2025-09-11T12:00:00+00:00")
        .expect("timestamp")
        .with_timezone(&Utc);
    let error = resolve_report_date_filters(
        ReportKind::Daily,
        Some(NonZeroUsize::new(7).expect("non-zero")),
        None,
        Some("2025-09-12"),
        timezone,
        now_utc,
    )
    .expect_err("conflicting date filters should fail");

    let rendered = error.to_string();
    assert!(rendered.contains("last_days"));
    assert!(rendered.contains("since/until"));
}

#[test]
fn effective_last_days_window_uses_selected_timezone_today() {
    let now_utc = DateTime::parse_from_rfc3339("2025-09-11T22:30:00+00:00")
        .expect("timestamp")
        .with_timezone(&Utc);
    let timezone = "Europe/Warsaw".parse::<Tz>().expect("timezone");
    let (since, until) = resolve_report_date_filters(
        ReportKind::Daily,
        Some(NonZeroUsize::new(2).expect("non-zero")),
        None,
        None,
        timezone,
        now_utc,
    )
    .expect("filters");

    assert_eq!(
        since,
        Some(NaiveDate::from_ymd_opt(2025, 9, 11).expect("since"))
    );
    assert_eq!(
        until,
        Some(NaiveDate::from_ymd_opt(2025, 9, 12).expect("until"))
    );
}

#[test]
fn cli_rejects_zero_threads() {
    let error = Cli::try_parse_from(["codexusage", "--threads", "0", "daily"])
        .expect_err("zero threads should fail");

    let rendered = error.to_string();
    assert!(rendered.contains("--threads"));
    assert!(rendered.contains('0'));
}

#[test]
fn resolve_scan_worker_count_caps_explicit_threads_to_workload() {
    assert_eq!(
        resolve_scan_worker_count(
            ScannerParallelism::Fixed(NonZeroUsize::new(4).expect("non-zero")),
            2,
        ),
        2
    );
}
