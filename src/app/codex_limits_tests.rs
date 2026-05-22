use super::*;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn codex_home_from_env_prefers_codex_home() {
    let resolved = codex_home_from_env(
        Some(OsString::from("/tmp/codex-home")),
        Some(OsString::from("/tmp/home")),
    );

    assert_eq!(resolved, PathBuf::from("/tmp/codex-home"));
}

#[test]
fn codex_home_from_env_falls_back_to_home_dot_codex() {
    let resolved = codex_home_from_env(None, Some(OsString::from("/tmp/home")));

    assert_eq!(resolved, PathBuf::from("/tmp/home/.codex"));
}

#[test]
fn codex_home_from_env_falls_back_to_relative_dot_codex() {
    let resolved = codex_home_from_env(None, None);

    assert_eq!(resolved, PathBuf::from(".codex"));
}

#[test]
fn load_codex_auth_credentials_reports_missing_file() {
    let temp = TempDir::new().expect("tempdir");
    let result = load_codex_auth_credentials(&temp.path().join("auth.json"));

    assert_eq!(result, Err(CodexLimitUnavailableReason::AuthMissing));
}

#[test]
fn codex_auth_credentials_rejects_invalid_json() {
    let result = codex_auth_credentials_from_json("not-json");

    assert_eq!(result, Err(CodexLimitUnavailableReason::InvalidAuth));
}

#[test]
fn codex_auth_credentials_rejects_api_key_auth() {
    let result = codex_auth_credentials_from_json(r#"{"OPENAI_API_KEY":"sk-test"}"#);

    assert_eq!(result, Err(CodexLimitUnavailableReason::UnsupportedAuth));
}

#[test]
fn codex_auth_credentials_reads_chatgpt_tokens() {
    let result = codex_auth_credentials_from_json(
        r#"{"tokens":{"access_token":" access-token ","account_id":" account-123 "}}"#,
    )
    .expect("credentials");

    assert_eq!(
        result,
        CodexAuthCredentials {
            access_token: "access-token".to_string(),
            account_id: Some("account-123".to_string()),
        }
    );
}

#[test]
fn load_codex_auth_credentials_reads_auth_file() {
    let temp = TempDir::new().expect("tempdir");
    let auth_path = temp.path().join("auth.json");
    fs::write(
        &auth_path,
        r#"{"tokens":{"access_token":"access-token","account_id":"account-123"}}"#,
    )
    .expect("write auth");

    let result = load_codex_auth_credentials(&auth_path).expect("credentials");

    assert_eq!(result.access_token, "access-token");
    assert_eq!(result.account_id.as_deref(), Some("account-123"));
}

#[test]
fn usage_request_headers_select_codex_product() {
    let headers = usage_request_headers(&CodexAuthCredentials {
        access_token: "access-token".to_string(),
        account_id: Some("account-123".to_string()),
    })
    .expect("headers");

    assert_eq!(
        headers
            .get("authorization")
            .expect("authorization")
            .to_str()
            .expect("valid header"),
        "Bearer access-token"
    );
    assert_eq!(
        headers
            .get("user-agent")
            .expect("user agent")
            .to_str()
            .expect("valid header"),
        "codex-cli"
    );
    assert_eq!(
        headers
            .get("OAI-Product-Sku")
            .expect("product sku")
            .to_str()
            .expect("valid header"),
        "codex"
    );
    assert_eq!(
        headers
            .get("ChatGPT-Account-ID")
            .expect("account id")
            .to_str()
            .expect("valid header"),
        "account-123"
    );
}

#[test]
fn usage_payload_maps_primary_and_secondary_windows() {
    let status = codex_limits_from_usage_payload(
        r#"{
            "rate_limit": {
                "primary_window": {
                    "used_percent": 42,
                    "limit_window_seconds": 18000,
                    "reset_at": 1770000000
                },
                "secondary_window": {
                    "used_percent": 9,
                    "limit_window_seconds": 604800,
                    "reset_at": 1770100000
                }
            }
        }"#,
    );

    assert_eq!(
        status,
        CodexLimitStatus::Available(CodexLimits {
            five_hour: Some(CodexLimitWindow {
                used_percent: 42.0,
                window_minutes: Some(300),
                resets_at_epoch_seconds: Some(1_770_000_000),
            }),
            weekly: Some(CodexLimitWindow {
                used_percent: 9.0,
                window_minutes: Some(10_080),
                resets_at_epoch_seconds: Some(1_770_100_000),
            }),
        })
    );
}

#[test]
fn usage_payload_selects_backend_windows_by_duration() {
    let status = codex_limits_from_usage_payload(
        r#"{
            "rate_limit": {
                "primary_window": {
                    "used_percent": 88,
                    "limit_window_seconds": 3600
                },
                "secondary_window": {
                    "used_percent": 9,
                    "limit_window_seconds": 604800
                }
            },
            "additional_rate_limits": [
                {
                    "metered_feature": "codex",
                    "rate_limit": {
                        "primary_window": {
                            "used_percent": 42,
                            "limit_window_seconds": 18000
                        }
                    }
                }
            ]
        }"#,
    );

    assert_eq!(
        status,
        CodexLimitStatus::Available(CodexLimits {
            five_hour: Some(CodexLimitWindow {
                used_percent: 42.0,
                window_minutes: Some(300),
                resets_at_epoch_seconds: None,
            }),
            weekly: Some(CodexLimitWindow {
                used_percent: 9.0,
                window_minutes: Some(10_080),
                resets_at_epoch_seconds: None,
            }),
        })
    );
}

#[test]
fn usage_payload_maps_protocol_rate_limits_schema() {
    let status = codex_limits_from_usage_payload(
        r#"{
            "rateLimits": {
                "limitId": "codex",
                "primary": {
                    "usedPercent": 25,
                    "windowDurationMins": 300,
                    "resetsAt": 1770000000
                },
                "secondary": {
                    "usedPercent": 91,
                    "windowDurationMins": 10080,
                    "resetsAt": 1770100000
                }
            }
        }"#,
    );

    assert_eq!(
        status,
        CodexLimitStatus::Available(CodexLimits {
            five_hour: Some(CodexLimitWindow {
                used_percent: 25.0,
                window_minutes: Some(300),
                resets_at_epoch_seconds: Some(1_770_000_000),
            }),
            weekly: Some(CodexLimitWindow {
                used_percent: 91.0,
                window_minutes: Some(10_080),
                resets_at_epoch_seconds: Some(1_770_100_000),
            }),
        })
    );
}

#[test]
fn usage_payload_selects_protocol_windows_by_duration() {
    let status = codex_limits_from_usage_payload(
        r#"{
            "rateLimits": {
                "limitId": "codex",
                "primary": {
                    "usedPercent": 9,
                    "windowDurationMins": 10080
                },
                "secondary": {
                    "usedPercent": 42,
                    "windowDurationMins": 300
                }
            }
        }"#,
    );

    assert_eq!(
        status,
        CodexLimitStatus::Available(CodexLimits {
            five_hour: Some(CodexLimitWindow {
                used_percent: 42.0,
                window_minutes: Some(300),
                resets_at_epoch_seconds: None,
            }),
            weekly: Some(CodexLimitWindow {
                used_percent: 9.0,
                window_minutes: Some(10_080),
                resets_at_epoch_seconds: None,
            }),
        })
    );
}

#[test]
fn usage_payload_prefers_protocol_rate_limits_by_codex_id() {
    let status = codex_limits_from_usage_payload(
        r#"{
            "rateLimitsByLimitId": {
                "other": {
                    "limitId": "other",
                    "primary": {"usedPercent": 90}
                },
                "codex": {
                    "limitId": "codex",
                    "primary": {"usedPercent": 7}
                }
            }
        }"#,
    );

    assert_eq!(
        status,
        CodexLimitStatus::Available(CodexLimits {
            five_hour: Some(CodexLimitWindow {
                used_percent: 7.0,
                window_minutes: None,
                resets_at_epoch_seconds: None,
            }),
            weekly: None,
        })
    );
}

#[test]
fn usage_payload_accepts_camel_case_fields() {
    let status = codex_limits_from_usage_payload(
        r#"{
            "rateLimit": {
                    "primaryWindow": {
                        "usedPercent": 125,
                        "limitWindowSeconds": 18000,
                        "resetAt": 123
                    }
            }
        }"#,
    );

    assert_eq!(
        status,
        CodexLimitStatus::Available(CodexLimits {
            five_hour: Some(CodexLimitWindow {
                used_percent: 100.0,
                window_minutes: Some(300),
                resets_at_epoch_seconds: Some(123),
            }),
            weekly: None,
        })
    );
}

#[test]
fn usage_payload_falls_back_to_additional_codex_limit() {
    let status = codex_limits_from_usage_payload(
        r#"{
            "additional_rate_limits": [
                {
                    "metered_feature": "codex",
                    "rate_limit": {
                        "primary_window": {"used_percent": 7}
                    }
                }
            ]
        }"#,
    );

    assert_eq!(
        status,
        CodexLimitStatus::Available(CodexLimits {
            five_hour: Some(CodexLimitWindow {
                used_percent: 7.0,
                window_minutes: None,
                resets_at_epoch_seconds: None,
            }),
            weekly: None,
        })
    );
}

#[test]
fn usage_payload_reports_invalid_json() {
    let status = codex_limits_from_usage_payload("not-json");

    assert_eq!(
        status,
        CodexLimitStatus::Unavailable(CodexLimitUnavailableReason::InvalidResponse)
    );
}

#[test]
fn usage_payload_reports_missing_windows() {
    let status = codex_limits_from_usage_payload(r#"{"rate_limit":{}}"#);

    assert_eq!(
        status,
        CodexLimitStatus::Unavailable(CodexLimitUnavailableReason::NoLimitData)
    );
}

#[test]
fn offline_watch_start_does_not_call_fetcher() {
    let status = codex_limit_status_for_watch_start(true, || {
        panic!("offline mode must not fetch limits");
    });

    assert_eq!(
        status,
        CodexLimitStatus::Unavailable(CodexLimitUnavailableReason::Offline)
    );
}

#[test]
fn online_watch_start_calls_fetcher() {
    let status = codex_limit_status_for_watch_start(false, || {
        CodexLimitStatus::Unavailable(CodexLimitUnavailableReason::RequestFailed)
    });

    assert_eq!(
        status,
        CodexLimitStatus::Unavailable(CodexLimitUnavailableReason::RequestFailed)
    );
}
