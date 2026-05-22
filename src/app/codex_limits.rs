//! Codex account limit fetching for watch mode.

use super::model::DEFAULT_CODEX_HOME_ENV;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `ChatGPT` backend usage endpoint used by Codex.
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Timeout for establishing the usage-status connection.
const LIMIT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Whole-request timeout for usage-status refreshes.
const LIMIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
/// User agent used for backend compatibility.
const CODEX_USER_AGENT: &str = "codex-cli";
/// Product selector for Codex quota windows.
const CODEX_PRODUCT_SKU: &str = "codex";
/// Codex short rolling limit window in minutes.
const CODEX_FIVE_HOUR_WINDOW_MINUTES: i64 = 5 * 60;
/// Codex weekly rolling limit window in minutes.
const CODEX_WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;

/// Current Codex limit status available to the watch renderer.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) enum CodexLimitStatus {
    /// Limit windows were fetched and parsed.
    Available(CodexLimits),
    /// Limit windows are unavailable for a non-fatal reason.
    Unavailable(CodexLimitUnavailableReason),
}

impl CodexLimitStatus {
    /// Build an unavailable status.
    pub(in crate::app) const fn unavailable(reason: CodexLimitUnavailableReason) -> Self {
        Self::Unavailable(reason)
    }
}

/// Non-fatal reasons limit bars cannot be shown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum CodexLimitUnavailableReason {
    /// Limit fetching is disabled by `--offline`.
    Offline,
    /// The auth file does not exist.
    AuthMissing,
    /// The auth file could not be parsed.
    InvalidAuth,
    /// The auth file does not contain `ChatGPT` access-token credentials.
    UnsupportedAuth,
    /// The backend request failed before a usable response was received.
    RequestFailed,
    /// The backend rejected the current access token.
    Unauthorized,
    /// The backend response was not valid usage JSON.
    InvalidResponse,
    /// The response did not include a Codex rate-limit window.
    NoLimitData,
}

impl CodexLimitUnavailableReason {
    /// Return a short display reason.
    pub(in crate::app) const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::AuthMissing => "auth.json missing",
            Self::InvalidAuth => "invalid auth.json",
            Self::UnsupportedAuth => "unsupported auth",
            Self::RequestFailed => "request failed",
            Self::Unauthorized => "unauthorized",
            Self::InvalidResponse => "invalid response",
            Self::NoLimitData => "no limit data",
        }
    }
}

/// Parsed Codex limit windows.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) struct CodexLimits {
    /// Short Codex rolling window.
    pub(in crate::app) five_hour: Option<CodexLimitWindow>,
    /// Long Codex rolling window.
    pub(in crate::app) weekly: Option<CodexLimitWindow>,
}

impl CodexLimits {
    /// Return whether at least one known window is present.
    const fn has_window(&self) -> bool {
        self.five_hour.is_some() || self.weekly.is_some()
    }
}

/// One Codex rate-limit window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) struct CodexLimitWindow {
    /// Percentage of the window that has been used.
    pub(in crate::app) used_percent: f64,
    /// Window width in whole minutes, when reported.
    pub(in crate::app) window_minutes: Option<i64>,
    /// Unix timestamp for the reset time, when reported.
    pub(in crate::app) resets_at_epoch_seconds: Option<i64>,
}

/// Auth credentials needed for the usage endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexAuthCredentials {
    /// Bearer access token.
    access_token: String,
    /// Optional `ChatGPT` account or workspace id.
    account_id: Option<String>,
}

/// Top-level auth file shape needed by this feature.
#[derive(Debug, Deserialize)]
struct AuthFile {
    /// API-key auth marker.
    #[serde(default, rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    /// `ChatGPT` token credentials.
    #[serde(default)]
    tokens: Option<AuthTokens>,
    /// Agent identity marker.
    #[serde(default)]
    agent_identity: Option<String>,
}

/// `ChatGPT` token subset stored in `auth.json`.
#[derive(Debug, Deserialize)]
struct AuthTokens {
    /// Bearer access token.
    access_token: String,
    /// Optional `ChatGPT` account or workspace id.
    #[serde(default)]
    account_id: Option<String>,
}

/// Usage endpoint payload subset.
#[derive(Debug, Deserialize)]
struct UsagePayload {
    /// Protocol-shaped preferred Codex rate-limit snapshot.
    #[serde(default, alias = "rateLimits")]
    rate_limits: Option<ProtocolRateLimitSnapshot>,
    /// Protocol-shaped snapshots keyed by limit id.
    #[serde(default, alias = "rateLimitsByLimitId")]
    rate_limits_by_limit_id: Option<BTreeMap<String, ProtocolRateLimitSnapshot>>,
    /// Primary Codex rate-limit details.
    #[serde(default, alias = "rateLimit")]
    rate_limit: Option<RateLimitDetails>,
    /// Additional metered-feature limits.
    #[serde(default, alias = "additionalRateLimits")]
    additional_rate_limits: Option<Vec<AdditionalRateLimit>>,
}

/// Protocol-shaped Codex rate-limit snapshot.
#[derive(Clone, Debug, Deserialize)]
struct ProtocolRateLimitSnapshot {
    /// Backend limit identifier.
    #[serde(default, alias = "limitId")]
    limit_id: Option<String>,
    /// Short rolling window.
    #[serde(default)]
    primary: Option<ProtocolRateLimitWindow>,
    /// Long rolling window.
    #[serde(default)]
    secondary: Option<ProtocolRateLimitWindow>,
}

/// Protocol-shaped rate-limit window.
#[derive(Clone, Copy, Debug, Deserialize)]
struct ProtocolRateLimitWindow {
    /// Used percentage.
    #[serde(default, alias = "usedPercent")]
    used_percent: Option<f64>,
    /// Window width in minutes.
    #[serde(default, alias = "windowDurationMins")]
    window_duration_mins: Option<i64>,
    /// Reset timestamp.
    #[serde(default, alias = "resetsAt")]
    resets_at: Option<i64>,
}

/// Additional usage-limit payload.
#[derive(Debug, Deserialize)]
struct AdditionalRateLimit {
    /// Backend metered-feature identifier.
    #[serde(default, alias = "meteredFeature")]
    metered_feature: Option<String>,
    /// Human-readable limit identifier.
    #[serde(default, alias = "limitName")]
    limit_name: Option<String>,
    /// Rate-limit windows for this feature.
    #[serde(default, alias = "rateLimit")]
    rate_limit: Option<RateLimitDetails>,
}

/// Primary and secondary rate-limit windows.
#[derive(Clone, Copy, Debug, Deserialize)]
struct RateLimitDetails {
    /// Short rolling window.
    #[serde(default, alias = "primaryWindow", alias = "primary")]
    primary_window: Option<RateLimitWindowPayload>,
    /// Long rolling window.
    #[serde(default, alias = "secondaryWindow", alias = "secondary")]
    secondary_window: Option<RateLimitWindowPayload>,
}

/// Backend representation for one rate-limit window.
#[derive(Clone, Copy, Debug, Deserialize)]
struct RateLimitWindowPayload {
    /// Used percentage.
    #[serde(default, alias = "usedPercent")]
    used_percent: Option<f64>,
    /// Window width in seconds.
    #[serde(default, alias = "limitWindowSeconds")]
    limit_window_seconds: Option<i64>,
    /// Reset timestamp.
    #[serde(default, alias = "resetAt", alias = "resetsAt")]
    reset_at: Option<i64>,
}

/// Fetch Codex account limits for watch mode.
pub(in crate::app) fn fetch_codex_limits() -> CodexLimitStatus {
    let credentials = match load_codex_auth_credentials(&default_codex_auth_path()) {
        Ok(credentials) => credentials,
        Err(reason) => return CodexLimitStatus::unavailable(reason),
    };

    fetch_codex_limits_with_credentials(&credentials)
}

/// Resolve the default Codex auth path.
fn default_codex_auth_path() -> PathBuf {
    codex_home_from_env(
        std::env::var_os(DEFAULT_CODEX_HOME_ENV),
        std::env::var_os("HOME"),
    )
    .join("auth.json")
}

/// Resolve the Codex home directory from environment values.
fn codex_home_from_env(codex_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    codex_home
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

/// Read `ChatGPT` credentials from one auth file.
fn load_codex_auth_credentials(
    auth_path: &Path,
) -> Result<CodexAuthCredentials, CodexLimitUnavailableReason> {
    let raw = std::fs::read_to_string(auth_path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            CodexLimitUnavailableReason::AuthMissing
        } else {
            CodexLimitUnavailableReason::InvalidAuth
        }
    })?;
    codex_auth_credentials_from_json(&raw)
}

/// Parse `ChatGPT` credentials from auth JSON.
fn codex_auth_credentials_from_json(
    raw: &str,
) -> Result<CodexAuthCredentials, CodexLimitUnavailableReason> {
    let auth: AuthFile =
        serde_json::from_str(raw).map_err(|_| CodexLimitUnavailableReason::InvalidAuth)?;
    let Some(tokens) = auth.tokens else {
        let has_other_auth = auth.openai_api_key.is_some() || auth.agent_identity.is_some();
        return Err(if has_other_auth {
            CodexLimitUnavailableReason::UnsupportedAuth
        } else {
            CodexLimitUnavailableReason::InvalidAuth
        });
    };
    let access_token = tokens.access_token.trim();
    if access_token.is_empty() {
        return Err(CodexLimitUnavailableReason::InvalidAuth);
    }
    let account_id = tokens
        .account_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    Ok(CodexAuthCredentials {
        access_token: access_token.to_string(),
        account_id,
    })
}

/// Fetch the usage payload using already resolved credentials.
fn fetch_codex_limits_with_credentials(credentials: &CodexAuthCredentials) -> CodexLimitStatus {
    let Ok(client) = Client::builder()
        .connect_timeout(LIMIT_CONNECT_TIMEOUT)
        .timeout(LIMIT_REQUEST_TIMEOUT)
        .build()
    else {
        return CodexLimitStatus::unavailable(CodexLimitUnavailableReason::RequestFailed);
    };
    let Some(headers) = usage_request_headers(credentials) else {
        return CodexLimitStatus::unavailable(CodexLimitUnavailableReason::InvalidAuth);
    };
    let Ok(response) = client.get(CODEX_USAGE_URL).headers(headers).send() else {
        return CodexLimitStatus::unavailable(CodexLimitUnavailableReason::RequestFailed);
    };
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return CodexLimitStatus::unavailable(CodexLimitUnavailableReason::Unauthorized);
    }
    if !response.status().is_success() {
        return CodexLimitStatus::unavailable(CodexLimitUnavailableReason::RequestFailed);
    }
    let Ok(body) = response.text() else {
        return CodexLimitStatus::unavailable(CodexLimitUnavailableReason::RequestFailed);
    };

    codex_limits_from_usage_payload(&body)
}

/// Build headers for one usage request.
fn usage_request_headers(credentials: &CodexAuthCredentials) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    let auth = HeaderValue::from_str(&format!("Bearer {}", credentials.access_token)).ok()?;
    headers.insert(AUTHORIZATION, auth);
    headers.insert(USER_AGENT, HeaderValue::from_static(CODEX_USER_AGENT));
    headers.insert(
        "OAI-Product-Sku",
        HeaderValue::from_static(CODEX_PRODUCT_SKU),
    );
    if let Some(account_id) = credentials.account_id.as_deref() {
        let account = HeaderValue::from_str(account_id).ok()?;
        headers.insert("ChatGPT-Account-ID", account);
    }
    Some(headers)
}

/// Parse Codex limit windows from one usage response body.
fn codex_limits_from_usage_payload(raw: &str) -> CodexLimitStatus {
    let Ok(payload) = serde_json::from_str::<UsagePayload>(raw) else {
        return CodexLimitStatus::unavailable(CodexLimitUnavailableReason::InvalidResponse);
    };
    if let Some(limits) = codex_protocol_limits(&payload) {
        return codex_limit_status_from_limits(limits);
    }
    let Some(limits) = codex_backend_limits(payload) else {
        return CodexLimitStatus::unavailable(CodexLimitUnavailableReason::NoLimitData);
    };
    codex_limit_status_from_limits(limits)
}

/// Build a status from parsed limit windows.
fn codex_limit_status_from_limits(limits: CodexLimits) -> CodexLimitStatus {
    if limits.has_window() {
        CodexLimitStatus::Available(limits)
    } else {
        CodexLimitStatus::unavailable(CodexLimitUnavailableReason::NoLimitData)
    }
}

/// Select protocol-shaped Codex rate-limit windows from a usage payload.
fn codex_protocol_limits(payload: &UsagePayload) -> Option<CodexLimits> {
    let snapshot = payload
        .rate_limits_by_limit_id
        .as_ref()
        .and_then(|by_id| by_id.get("codex"))
        .or_else(|| {
            payload
                .rate_limits_by_limit_id
                .as_ref()?
                .values()
                .find(|snapshot| snapshot.limit_id.as_deref() == Some("codex"))
        })
        .or_else(|| {
            payload
                .rate_limits
                .as_ref()
                .filter(|snapshot| snapshot.limit_id.as_deref().is_none_or(|id| id == "codex"))
        })?;

    codex_limits_from_windows([
        protocol_rate_limit_window(snapshot.primary),
        protocol_rate_limit_window(snapshot.secondary),
    ])
}

/// Select backend-shaped Codex rate-limit windows from a usage payload.
fn codex_backend_limits(payload: UsagePayload) -> Option<CodexLimits> {
    let mut windows = Vec::new();
    if let Some(details) = payload.rate_limit {
        append_rate_limit_windows(&mut windows, details);
    }
    if let Some(additional_limits) = payload.additional_rate_limits {
        for limit in additional_limits
            .into_iter()
            .filter(is_codex_additional_limit)
        {
            if let Some(details) = limit.rate_limit {
                append_rate_limit_windows(&mut windows, details);
            }
        }
    }

    codex_limits_from_windows(windows)
}

/// Return whether an additional usage-limit block describes Codex.
fn is_codex_additional_limit(limit: &AdditionalRateLimit) -> bool {
    limit.metered_feature.as_deref() == Some("codex")
        || limit.limit_name.as_deref() == Some("codex")
}

/// Append parsed backend rate-limit windows to a candidate set.
fn append_rate_limit_windows(
    windows: &mut Vec<Option<CodexLimitWindow>>,
    details: RateLimitDetails,
) {
    windows.extend([
        rate_limit_window(details.primary_window),
        rate_limit_window(details.secondary_window),
    ]);
}

/// Choose Codex 5h and weekly buckets from parsed candidate windows.
fn codex_limits_from_windows(
    windows: impl IntoIterator<Item = Option<CodexLimitWindow>>,
) -> Option<CodexLimits> {
    let mut five_hour = None;
    let mut weekly = None;
    let mut unknown_duration = Vec::new();
    for window in windows.into_iter().flatten() {
        match window.window_minutes {
            Some(CODEX_FIVE_HOUR_WINDOW_MINUTES) if five_hour.is_none() => {
                five_hour = Some(window);
            }
            Some(CODEX_WEEKLY_WINDOW_MINUTES) if weekly.is_none() => {
                weekly = Some(window);
            }
            Some(_) => {}
            None => unknown_duration.push(window),
        }
    }

    let mut unknown_duration = unknown_duration.into_iter();
    if five_hour.is_none() {
        five_hour = unknown_duration.next();
    }
    if weekly.is_none() {
        weekly = unknown_duration.next();
    }

    let limits = CodexLimits { five_hour, weekly };
    limits.has_window().then_some(limits)
}

/// Convert one protocol-shaped window into renderer data.
fn protocol_rate_limit_window(window: Option<ProtocolRateLimitWindow>) -> Option<CodexLimitWindow> {
    let window = window?;
    let used_percent = window.used_percent?;
    if !used_percent.is_finite() {
        return None;
    }
    Some(CodexLimitWindow {
        used_percent: used_percent.clamp(0.0, 100.0),
        window_minutes: window.window_duration_mins.filter(|minutes| *minutes > 0),
        resets_at_epoch_seconds: window.resets_at,
    })
}

/// Convert one backend window into renderer data.
fn rate_limit_window(window: Option<RateLimitWindowPayload>) -> Option<CodexLimitWindow> {
    let window = window?;
    let used_percent = window.used_percent?;
    if !used_percent.is_finite() {
        return None;
    }
    Some(CodexLimitWindow {
        used_percent: used_percent.clamp(0.0, 100.0),
        window_minutes: window
            .limit_window_seconds
            .filter(|seconds| *seconds > 0)
            .map(|seconds| (seconds + 59) / 60),
        resets_at_epoch_seconds: window.reset_at,
    })
}

/// Return the initial limit status for watch startup.
pub(in crate::app) fn codex_limit_status_for_watch_start(
    offline: bool,
    fetch: impl FnOnce() -> CodexLimitStatus,
) -> CodexLimitStatus {
    if offline {
        CodexLimitStatus::unavailable(CodexLimitUnavailableReason::Offline)
    } else {
        fetch()
    }
}

#[cfg(test)]
#[path = "codex_limits_tests.rs"]
mod tests;
