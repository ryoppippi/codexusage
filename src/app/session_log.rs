//! Session JSONL parsing and file scan helpers.

use super::model::{DEFAULT_FALLBACK_MODEL, UsageTotals};
use super::scan_runtime::ScanObserver;
use super::session_files::{SessionFileFormat, session_file_format};
use super::session_lineage::{is_inter_agent_communication_boundary, is_spawned_subagent_metadata};
use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use memchr::memmem::Finder;
use serde::Deserialize;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::marker::PhantomData;
use std::path::Path;
use std::sync::LazyLock;

/// Compact top-level event-message type marker.
static EVENT_MSG_TYPE_FINDER: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new(br#""type":"event_msg""#));
/// Compact token-count payload type marker.
static TOKEN_COUNT_TYPE_FINDER: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new(br#""type":"token_count""#));
/// Compact turn-context entry type marker.
static TURN_CONTEXT_TYPE_FINDER: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new(br#""type":"turn_context""#));
/// Compact Pi message-entry type marker.
static PI_MESSAGE_TYPE_FINDER: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new(br#""type":"message""#));
/// Exact Pi provider marker accepted by this Codex usage scanner.
static PI_CODEX_PROVIDER_FINDER: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new(br#""provider":"openai-codex""#));
/// Pi parent-session header field used to identify copied fork history.
static PI_PARENT_SESSION_FINDER: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new(br#""parentSession":""#));
/// JSON Unicode escape introducer.
static UNICODE_ESCAPE_FINDER: LazyLock<Finder<'static>> = LazyLock::new(|| Finder::new(b"\\u"));
/// Usage-bearing turn-context marker.
const TURN_CONTEXT_MARKER: &[u8] = b"turn_context";
/// Usage-bearing token-count marker.
const TOKEN_COUNT_MARKER: &[u8] = b"token_count";
/// Usage-bearing event-message marker.
const EVENT_MSG_MARKER: &[u8] = b"event_msg";

/// Raw token counters as they appear in session log payloads.
#[derive(Clone, Copy, Debug, Default)]
pub(in crate::app) struct RawUsage {
    /// Input tokens.
    pub(in crate::app) input: u64,
    /// Cached input tokens.
    pub(in crate::app) cached_input: u64,
    /// Output tokens.
    pub(in crate::app) output: u64,
    /// Reasoning tokens.
    pub(in crate::app) reasoning_output: u64,
    /// Total tokens.
    pub(in crate::app) total: u64,
}

impl RawUsage {
    /// Convert raw usage into normalized totals.
    fn into_usage_totals(self) -> UsageTotals {
        UsageTotals {
            input: self.input,
            cached_input: self.cached_input.min(self.input),
            output: self.output,
            reasoning_output: self.reasoning_output,
            total: if self.total > 0 {
                self.total
            } else {
                self.input + self.output
            },
        }
    }

    /// Return the billable total, deriving it when legacy payloads omit `total_tokens`.
    fn billable_total(self) -> u64 {
        if self.total > 0 {
            self.total
        } else {
            self.input.saturating_add(self.output)
        }
    }

    /// Advance cumulative totals with one delta usage payload.
    fn advance(self, delta: RawUsage) -> Self {
        Self {
            input: self.input.saturating_add(delta.input),
            cached_input: self.cached_input.saturating_add(delta.cached_input),
            output: self.output.saturating_add(delta.output),
            reasoning_output: self.reasoning_output.saturating_add(delta.reasoning_output),
            total: self.total.saturating_add(delta.billable_total()),
        }
    }
}

impl UsagePayload {
    /// Convert a deserialized usage payload into raw usage counters.
    pub(in crate::app) fn into_raw_usage(self) -> RawUsage {
        RawUsage {
            input: self.input_tokens,
            cached_input: self
                .cached_input_tokens
                .or(self.cache_read_input_tokens)
                .unwrap_or(0),
            output: self.output_tokens,
            reasoning_output: self.reasoning_output_tokens,
            total: self.total_tokens,
        }
    }
}

/// Incremental parser checkpoint reused when a session file grows by appending new lines.
#[derive(Clone, Debug, Default)]
pub(in crate::app) struct SessionParseCheckpoint {
    /// Byte offset of the next unread position in the file.
    pub(in crate::app) offset: u64,
    /// Previous cumulative usage totals needed to normalize future events.
    pub(in crate::app) previous_totals: Option<RawUsage>,
    /// Last resolved model name.
    pub(in crate::app) current_model: Option<String>,
    /// Whether the remembered model came from fallback inference.
    pub(in crate::app) current_model_is_fallback: bool,
    /// Whether usage should be emitted or is still part of an inherited subagent replay.
    pub(in crate::app) replay_state: ReplayState,
}

/// Usage-emission state for rollout files that may begin with inherited agent history.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::app) enum ReplayState {
    /// The first non-empty record has not been classified yet.
    #[default]
    AwaitingFirstRecord,
    /// A spawned subagent's inherited prefix is being parsed but not emitted.
    SuppressingInherited,
    /// Records belong to this rollout and may emit usage.
    CountingOwn,
}

impl ReplayState {
    /// Encode the state for the persistent scan index.
    pub(in crate::app) const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingFirstRecord => "awaiting_first_record",
            Self::SuppressingInherited => "suppressing_inherited",
            Self::CountingOwn => "counting_own",
        }
    }

    /// Decode a state stored by the persistent scan index.
    pub(in crate::app) fn from_str(value: &str) -> Option<Self> {
        match value {
            "awaiting_first_record" => Some(Self::AwaitingFirstRecord),
            "suppressing_inherited" => Some(Self::SuppressingInherited),
            "counting_own" => Some(Self::CountingOwn),
            _ => None,
        }
    }
}

/// Normalized token usage event emitted by the session parser.
#[derive(Clone, Debug)]
pub(in crate::app) struct TokenUsageEvent<'session, 'model> {
    /// Unique session key including source-root identity.
    pub(in crate::app) session_key: &'session str,
    /// Session identifier.
    pub(in crate::app) session_id: &'session str,
    /// Timestamp as parsed UTC datetime.
    pub(in crate::app) timestamp_utc: DateTime<Utc>,
    /// Model name.
    pub(in crate::app) model: &'model str,
    /// Whether model fallback was used.
    pub(in crate::app) is_fallback_model: bool,
    /// Token totals.
    pub(in crate::app) usage: UsageTotals,
}

/// One parsed JSONL entry.
#[derive(Deserialize)]
struct SessionLogEntry<'a> {
    /// Entry kind.
    #[serde(
        rename = "type",
        borrow,
        default,
        deserialize_with = "deserialize_optional_cow_lossy"
    )]
    entry_type: Option<Cow<'a, str>>,
    /// Event timestamp.
    #[serde(borrow, default, deserialize_with = "deserialize_optional_cow_lossy")]
    timestamp: Option<Cow<'a, str>>,
    /// Entry payload.
    #[serde(
        borrow,
        default,
        deserialize_with = "deserialize_optional_object_lossy"
    )]
    payload: Option<EntryPayload<'a>>,
    /// Pi message payload.
    #[serde(
        borrow,
        default,
        deserialize_with = "deserialize_optional_object_lossy"
    )]
    message: Option<PiMessage<'a>>,
}

/// Usage-bearing subset of a Pi assistant message.
#[derive(Default, Deserialize)]
struct PiMessage<'a> {
    /// Message role.
    #[serde(borrow, default, deserialize_with = "deserialize_optional_cow_lossy")]
    role: Option<Cow<'a, str>>,
    /// Model provider.
    #[serde(borrow, default, deserialize_with = "deserialize_optional_cow_lossy")]
    provider: Option<Cow<'a, str>>,
    /// Model identifier.
    #[serde(borrow, default, deserialize_with = "deserialize_optional_cow_lossy")]
    model: Option<Cow<'a, str>>,
    /// Per-response token usage.
    #[serde(default, deserialize_with = "deserialize_optional_object_lossy")]
    usage: Option<PiUsage>,
}

/// Token counters persisted by Pi's provider-neutral runtime.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiUsage {
    /// Non-cached input tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    input: u64,
    /// Output tokens, including reasoning tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    output: u64,
    /// Cache-read input tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    cache_read: u64,
    /// Cache-write input tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    cache_write: u64,
    /// Reasoning tokens, already included in output.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    reasoning: u64,
    /// Provider-reported total tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    total_tokens: u64,
}

/// Payload fields used by turn-context and token-count events.
#[derive(Default, Deserialize)]
pub(in crate::app) struct EntryPayload<'a> {
    /// Payload kind.
    #[serde(
        rename = "type",
        borrow,
        default,
        deserialize_with = "deserialize_optional_cow_lossy"
    )]
    payload_type: Option<Cow<'a, str>>,
    /// Nested event info object.
    #[serde(
        borrow,
        default,
        deserialize_with = "deserialize_optional_object_lossy"
    )]
    info: Option<EntryInfo<'a>>,
    /// Inline usage delta.
    #[serde(default, deserialize_with = "deserialize_optional_object_lossy")]
    last_token_usage: Option<UsagePayload>,
    /// Inline cumulative usage.
    #[serde(default, deserialize_with = "deserialize_optional_object_lossy")]
    total_token_usage: Option<UsagePayload>,
    /// Model lookup fields.
    #[serde(flatten, borrow)]
    model_fields: ModelFields<'a>,
}

/// Nested info object inside token-count events.
#[derive(Default, Deserialize)]
struct EntryInfo<'a> {
    /// Usage delta.
    #[serde(default, deserialize_with = "deserialize_optional_object_lossy")]
    last_token_usage: Option<UsagePayload>,
    /// Cumulative usage.
    #[serde(default, deserialize_with = "deserialize_optional_object_lossy")]
    total_token_usage: Option<UsagePayload>,
    /// Model lookup fields.
    #[serde(flatten, borrow)]
    model_fields: ModelFields<'a>,
}

/// Common model lookup fields reused across payload shapes.
#[derive(Default, Deserialize)]
struct ModelFields<'a> {
    /// Primary model field.
    #[serde(borrow, default, deserialize_with = "deserialize_optional_cow_lossy")]
    model: Option<Cow<'a, str>>,
    /// Alternate model field.
    #[serde(
        rename = "model_name",
        borrow,
        default,
        deserialize_with = "deserialize_optional_cow_lossy"
    )]
    model_name: Option<Cow<'a, str>>,
    /// Nested metadata lookup.
    #[serde(
        borrow,
        default,
        deserialize_with = "deserialize_optional_object_lossy"
    )]
    metadata: Option<ModelMetadata<'a>>,
}

/// Nested metadata container.
#[derive(Deserialize)]
struct ModelMetadata<'a> {
    /// Model name from metadata.
    #[serde(borrow, default, deserialize_with = "deserialize_optional_cow_lossy")]
    model: Option<Cow<'a, str>>,
}

/// Deserialize an optional string field while ignoring invalid scalar shapes.
pub(in crate::app) fn deserialize_optional_cow_lossy<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Cow<'de, str>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalCowVisitor;

    impl<'de> serde::de::Visitor<'de> for OptionalCowVisitor {
        type Value = Option<Cow<'de, str>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an optional string")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_optional_cow_lossy(deserializer)
        }

        fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(Cow::Borrowed(value)))
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(Cow::Owned(value.to_string())))
        }

        fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(Cow::Owned(value)))
        }

        fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_u64<E>(self, _value: u64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(OptionalCowVisitor)
}

/// Deserialize a token counter while treating invalid field types as zero.
pub(in crate::app) fn deserialize_u64_lossy<'de, D>(
    deserializer: D,
) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct LossyU64Visitor;

    impl<'de> serde::de::Visitor<'de> for LossyU64Visitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an integer token count")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_u64_lossy(deserializer)
        }

        fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value)
        }

        fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_borrowed_str<E>(self, _value: &'de str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_str<E>(self, _value: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_string<E>(self, _value: String) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(0)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(0)
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {}
            Ok(0)
        }
    }

    deserializer.deserialize_any(LossyU64Visitor)
}

/// Deserialize an optional token counter while ignoring invalid field types.
pub(in crate::app) fn deserialize_optional_u64_lossy<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalU64Visitor;

    impl<'de> serde::de::Visitor<'de> for OptionalU64Visitor {
        type Value = Option<u64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an optional integer token count")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_optional_u64_lossy(deserializer)
        }

        fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(value))
        }

        fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_borrowed_str<E>(self, _value: &'de str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_str<E>(self, _value: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_string<E>(self, _value: String) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(OptionalU64Visitor)
}

/// Deserialize an optional object while ignoring wrong-type values.
pub(in crate::app) fn deserialize_optional_object_lossy<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    struct OptionalObjectVisitor<T>(PhantomData<T>);

    impl<'de, T> serde::de::Visitor<'de> for OptionalObjectVisitor<T>
    where
        T: serde::Deserialize<'de>,
    {
        type Value = Option<T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an optional object")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_optional_object_lossy(deserializer)
        }

        fn visit_map<A>(self, map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let value = T::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
            Ok(Some(value))
        }

        fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_u64<E>(self, _value: u64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_borrowed_str<E>(self, _value: &'de str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_str<E>(self, _value: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_string<E>(self, _value: String) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(OptionalObjectVisitor::<T>(PhantomData))
}

/// Usage payload read directly from JSON.
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror the Codex JSON payload shape verbatim"
)]
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub(in crate::app) struct UsagePayload {
    /// Input tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    input_tokens: u64,
    /// Cached input tokens.
    #[serde(default, deserialize_with = "deserialize_optional_u64_lossy")]
    cached_input_tokens: Option<u64>,
    /// Legacy cached input token field.
    #[serde(default, deserialize_with = "deserialize_optional_u64_lossy")]
    cache_read_input_tokens: Option<u64>,
    /// Output tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    output_tokens: u64,
    /// Reasoning output tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    reasoning_output_tokens: u64,
    /// Total tokens.
    #[serde(default, deserialize_with = "deserialize_u64_lossy")]
    total_tokens: u64,
}

/// Scan one JSONL session file and feed every parsed event into the provided callback.
#[cfg(test)]
pub(in crate::app) fn scan_session_file_with(
    file: &Path,
    session_id: &str,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<()> {
    let _ = scan_session_file_from_checkpoint(
        file,
        session_id,
        &SessionParseCheckpoint::default(),
        |event| {
            on_event(event);
        },
    )?;
    Ok(())
}

/// Scan one JSONL session file and feed every parsed event into the provided callback.
pub(in crate::app) fn scan_session_file_with_callback_and_observer_with_format<O>(
    file: &Path,
    session_id: &str,
    file_format: SessionFileFormat,
    observer: &O,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<()>
where
    O: ScanObserver,
{
    let _ = scan_session_file_from_checkpoint_with_observer_and_format(
        file,
        session_id,
        file_format,
        &SessionParseCheckpoint::default(),
        observer,
        |event| on_event(event),
    )?;
    observer.on_file_complete();
    Ok(())
}

/// Scan one JSONL session file from a stored parser checkpoint.
#[cfg(test)]
pub(in crate::app) fn scan_session_file_from_checkpoint(
    file: &Path,
    session_id: &str,
    checkpoint: &SessionParseCheckpoint,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint> {
    scan_session_file_from_checkpoint_with_format(
        file,
        session_id,
        session_file_format(file).unwrap_or(SessionFileFormat::Plain),
        checkpoint,
        |event| on_event(event),
    )
}

/// Scan one session file from a stored parser checkpoint with known physical format.
pub(in crate::app) fn scan_session_file_from_checkpoint_with_format(
    file: &Path,
    session_id: &str,
    file_format: SessionFileFormat,
    checkpoint: &SessionParseCheckpoint,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint> {
    scan_session_file_from_checkpoint_inner(
        file,
        session_id,
        file_format,
        checkpoint,
        || {},
        |_| {},
        |event| {
            on_event(event);
        },
    )
}

/// Scan one JSONL session file from a stored parser checkpoint.
pub(in crate::app) fn scan_session_file_from_checkpoint_with_observer<O>(
    file: &Path,
    session_id: &str,
    checkpoint: &SessionParseCheckpoint,
    observer: &O,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint>
where
    O: ScanObserver,
{
    scan_session_file_from_checkpoint_with_observer_and_format(
        file,
        session_id,
        session_file_format(file).unwrap_or(SessionFileFormat::Plain),
        checkpoint,
        observer,
        |event| on_event(event),
    )
}

/// Scan one session file from a stored parser checkpoint with known physical format.
pub(in crate::app) fn scan_session_file_from_checkpoint_with_observer_and_format<O>(
    file: &Path,
    session_id: &str,
    file_format: SessionFileFormat,
    checkpoint: &SessionParseCheckpoint,
    observer: &O,
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint>
where
    O: ScanObserver,
{
    scan_session_file_from_checkpoint_inner(
        file,
        session_id,
        file_format,
        checkpoint,
        || observer.before_file_open(),
        |_| {},
        |event| on_event(event),
    )
}

/// Scan one session file from a checkpoint with known format and expose consumed bytes.
pub(in crate::app) fn scan_session_file_from_checkpoint_with_observer_and_bytes_and_format<O>(
    file: &Path,
    session_id: &str,
    file_format: SessionFileFormat,
    checkpoint: &SessionParseCheckpoint,
    observer: &O,
    mut on_bytes: impl FnMut(&[u8]),
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint>
where
    O: ScanObserver,
{
    scan_session_file_from_checkpoint_inner(
        file,
        session_id,
        file_format,
        checkpoint,
        || observer.before_file_open(),
        |bytes| on_bytes(bytes),
        |event| on_event(event),
    )
}

/// Scan one JSONL session file from a stored parser checkpoint using shared parse mechanics.
fn scan_session_file_from_checkpoint_inner(
    file: &Path,
    session_id: &str,
    file_format: SessionFileFormat,
    checkpoint: &SessionParseCheckpoint,
    before_file_open: impl FnOnce(),
    mut on_bytes: impl FnMut(&[u8]),
    mut on_event: impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint> {
    before_file_open();

    if file_format == SessionFileFormat::Compressed {
        let metadata = std::fs::metadata(file)
            .wrap_err_with(|| format!("failed to read session file metadata {}", file.display()))?;
        let file_handle = File::open(file)?;
        let decoder = zstd::stream::read::Decoder::new(file_handle)
            .wrap_err_with(|| format!("failed to decode compressed session {}", file.display()))?;
        return scan_session_reader(
            BufReader::new(decoder),
            session_id,
            &SessionParseCheckpoint::default(),
            0,
            Some(metadata.len()),
            &mut on_bytes,
            &mut on_event,
        );
    }

    let mut file = File::open(file)?;
    let pi_fork_started_at = if checkpoint.offset > 0 {
        read_pi_fork_start_timestamp(&mut file)?
    } else {
        None
    };
    file.seek(SeekFrom::Start(checkpoint.offset))?;
    let reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut previous_totals = checkpoint.previous_totals;
    let mut current_model = checkpoint.current_model.clone();
    let mut current_model_is_fallback = checkpoint.current_model_is_fallback;
    let mut replay_state = checkpoint.replay_state;
    let mut pi_fork_started_at = pi_fork_started_at;
    let mut reader = reader;
    let mut offset = checkpoint.offset;
    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader.read_until(b'\n', &mut line)?;
        if bytes_read == 0 {
            break;
        }
        let next_offset = offset.saturating_add(u64::try_from(bytes_read).unwrap_or(u64::MAX));
        let trimmed = trim_ascii_whitespace(&line);
        if trimmed.is_empty() {
            on_bytes(&line);
            offset = next_offset;
            continue;
        }
        if !line.ends_with(b"\n") && serde_json::from_slice::<SessionLogEntry<'_>>(trimmed).is_err()
        {
            offset = line_start_offset;
            break;
        }
        classify_replay_record(trimmed, &mut replay_state);
        pi_fork_started_at = pi_fork_started_at.or_else(|| pi_fork_start_timestamp(trimmed));
        if !line_might_affect_usage_bytes(trimmed) {
            on_bytes(&line);
            offset = next_offset;
            continue;
        }
        if let Some(event) = parse_token_usage_line_bytes(
            trimmed,
            session_id,
            session_id,
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )? && replay_state == ReplayState::CountingOwn
            && pi_fork_started_at.is_none_or(|started_at| event.timestamp_utc >= started_at)
        {
            on_event(&event);
        }
        on_bytes(&line);
        offset = next_offset;
    }
    Ok(SessionParseCheckpoint {
        offset,
        previous_totals,
        current_model,
        current_model_is_fallback,
        replay_state,
    })
}

/// Scan one decoded JSONL reader and emit normalized usage events.
fn scan_session_reader(
    mut reader: impl BufRead,
    session_id: &str,
    checkpoint: &SessionParseCheckpoint,
    start_offset: u64,
    complete_offset: Option<u64>,
    on_bytes: &mut impl FnMut(&[u8]),
    on_event: &mut impl FnMut(&TokenUsageEvent<'_, '_>),
) -> Result<SessionParseCheckpoint> {
    let mut line = Vec::new();
    let mut previous_totals = checkpoint.previous_totals;
    let mut current_model = checkpoint.current_model.clone();
    let mut current_model_is_fallback = checkpoint.current_model_is_fallback;
    let mut replay_state = checkpoint.replay_state;
    let mut pi_fork_started_at = None;
    let mut offset = start_offset;
    let mut stopped_at_partial_record = false;
    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader.read_until(b'\n', &mut line)?;
        if bytes_read == 0 {
            break;
        }
        let next_offset = offset.saturating_add(u64::try_from(bytes_read).unwrap_or(u64::MAX));
        let trimmed = trim_ascii_whitespace(&line);
        if trimmed.is_empty() {
            on_bytes(&line);
            offset = next_offset;
            continue;
        }
        if !line.ends_with(b"\n") && serde_json::from_slice::<SessionLogEntry<'_>>(trimmed).is_err()
        {
            offset = line_start_offset;
            stopped_at_partial_record = true;
            break;
        }
        classify_replay_record(trimmed, &mut replay_state);
        pi_fork_started_at = pi_fork_started_at.or_else(|| pi_fork_start_timestamp(trimmed));
        if !line_might_affect_usage_bytes(trimmed) {
            on_bytes(&line);
            offset = next_offset;
            continue;
        }
        if let Some(event) = parse_token_usage_line_bytes(
            trimmed,
            session_id,
            session_id,
            &mut previous_totals,
            &mut current_model,
            &mut current_model_is_fallback,
        )? && replay_state == ReplayState::CountingOwn
            && pi_fork_started_at.is_none_or(|started_at| event.timestamp_utc >= started_at)
        {
            on_event(&event);
        }
        on_bytes(&line);
        offset = next_offset;
    }
    Ok(SessionParseCheckpoint {
        offset: if stopped_at_partial_record {
            offset
        } else {
            complete_offset.unwrap_or(offset)
        },
        previous_totals,
        current_model,
        current_model_is_fallback,
        replay_state,
    })
}

/// Recover a Pi fork cutoff before scanning an appended suffix.
fn read_pi_fork_start_timestamp(file: &mut File) -> Result<Option<DateTime<Utc>>> {
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = trim_ascii_whitespace(&line);
        if !trimmed.is_empty() {
            return Ok(pi_fork_start_timestamp(trimmed));
        }
    }
}

/// Read the creation timestamp from a Pi fork header that precedes copied history.
fn pi_fork_start_timestamp(line: &[u8]) -> Option<DateTime<Utc>> {
    PI_PARENT_SESSION_FINDER.find(line)?;
    let entry = serde_json::from_slice::<serde_json::Value>(line).ok()?;
    if entry.get("type").and_then(serde_json::Value::as_str) != Some("session")
        || entry
            .get("parentSession")
            .and_then(serde_json::Value::as_str)
            .is_none()
    {
        return None;
    }
    DateTime::parse_from_rfc3339(entry.get("timestamp")?.as_str()?)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

/// Advance inherited-history handling from one complete non-empty JSONL record.
fn classify_replay_record(line: &[u8], replay_state: &mut ReplayState) {
    match replay_state {
        ReplayState::AwaitingFirstRecord => {
            *replay_state = if is_spawned_subagent_metadata(line) {
                ReplayState::SuppressingInherited
            } else {
                ReplayState::CountingOwn
            };
        }
        ReplayState::SuppressingInherited if is_inter_agent_communication_boundary(line) => {
            *replay_state = ReplayState::CountingOwn;
        }
        ReplayState::SuppressingInherited | ReplayState::CountingOwn => {}
    }
}

/// Return whether one JSONL line might affect usage aggregation.
#[cfg(test)]
pub(in crate::app) fn line_might_affect_usage(line: &str) -> bool {
    line_might_affect_usage_bytes(line.as_bytes())
}

/// Return whether one JSONL byte record might affect usage aggregation.
fn line_might_affect_usage_bytes(line: &[u8]) -> bool {
    line_has_exact_usage_type_markers(line)
        || line_has_pi_codex_usage_markers(line)
        || contains_escaped_usage_marker(line)
}

/// Return whether a compact Pi record can contain accepted Codex usage.
fn line_has_pi_codex_usage_markers(line: &[u8]) -> bool {
    PI_MESSAGE_TYPE_FINDER.find(line).is_some() && PI_CODEX_PROVIDER_FINDER.find(line).is_some()
}

/// Return whether one compact JSON record has exact usage-bearing type markers.
fn line_has_exact_usage_type_markers(line: &[u8]) -> bool {
    TURN_CONTEXT_TYPE_FINDER.find(line).is_some()
        || (TOKEN_COUNT_TYPE_FINDER.find(line).is_some()
            && EVENT_MSG_TYPE_FINDER.find(line).is_some())
}

/// Return whether a line contains an escaped form of a usage-bearing marker.
fn contains_escaped_usage_marker(line: &[u8]) -> bool {
    if UNICODE_ESCAPE_FINDER.find(line).is_none() {
        return false;
    }

    if contains_escaped_marker(line, TURN_CONTEXT_MARKER) {
        return true;
    }

    let token_count_present = TOKEN_COUNT_TYPE_FINDER.find(line).is_some()
        || contains_escaped_marker(line, TOKEN_COUNT_MARKER);
    token_count_present
        && (EVENT_MSG_TYPE_FINDER.find(line).is_some()
            || contains_escaped_marker(line, EVENT_MSG_MARKER))
}

/// Return whether a line contains an escaped form of one marker.
fn contains_escaped_marker(line: &[u8], marker: &[u8]) -> bool {
    let Some(&first) = marker.first() else {
        return false;
    };
    memchr::memchr2_iter(first, b'\\', line).any(|offset| {
        let candidate = &line[offset..];
        escaped_marker_matches(candidate, marker)
    })
}

/// Return whether a candidate starts with a marker that contains at least one JSON Unicode escape.
fn escaped_marker_matches(mut candidate: &[u8], marker: &[u8]) -> bool {
    let mut escaped = false;
    for &expected in marker {
        if candidate.first() == Some(&expected) {
            candidate = &candidate[1..];
            continue;
        }
        if let Some(remainder) = consume_json_ascii_escape(candidate, expected) {
            candidate = remainder;
            escaped = true;
            continue;
        }
        return false;
    }
    escaped
}

/// Consume a `\u00XX` escape that represents the expected ASCII byte.
fn consume_json_ascii_escape(candidate: &[u8], expected: u8) -> Option<&[u8]> {
    let [slash, prefix, first, second, third, fourth] = candidate.get(..6)?.try_into().ok()?;
    if slash != b'\\' || prefix != b'u' {
        return None;
    }
    let value = (u16::from(hex_value(first)?) << 12)
        | (u16::from(hex_value(second)?) << 8)
        | (u16::from(hex_value(third)?) << 4)
        | u16::from(hex_value(fourth)?);
    if value == u16::from(expected) {
        Some(&candidate[6..])
    } else {
        None
    }
}

/// Convert one ASCII hexadecimal digit into its value.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Trim JSONL record whitespace without converting the line to UTF-8.
fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let Some(start) = bytes.iter().position(|byte| !byte.is_ascii_whitespace()) else {
        return &[];
    };
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |position| position + 1);
    &bytes[start..end]
}

/// Parse one JSONL line into a token-usage event when applicable.
#[cfg(test)]
pub(in crate::app) fn parse_token_usage_line<'session, 'model>(
    line: &str,
    session_key: &'session str,
    session_id: &'session str,
    previous_totals: &mut Option<RawUsage>,
    current_model: &'model mut Option<String>,
    current_model_is_fallback: &mut bool,
) -> Result<Option<TokenUsageEvent<'session, 'model>>> {
    parse_token_usage_line_bytes(
        line.as_bytes(),
        session_key,
        session_id,
        previous_totals,
        current_model,
        current_model_is_fallback,
    )
}

/// Parse one JSONL byte record into a token-usage event when applicable.
fn parse_token_usage_line_bytes<'session, 'model>(
    line: &[u8],
    session_key: &'session str,
    session_id: &'session str,
    previous_totals: &mut Option<RawUsage>,
    current_model: &'model mut Option<String>,
    current_model_is_fallback: &mut bool,
) -> Result<Option<TokenUsageEvent<'session, 'model>>> {
    let Some(entry) = parse_session_log_entry(line)? else {
        return Ok(None);
    };
    let Some(entry_type) = entry.entry_type.as_deref() else {
        return Ok(None);
    };
    if entry_type == "message" {
        return parse_pi_token_usage_entry(
            &entry,
            session_key,
            session_id,
            current_model,
            current_model_is_fallback,
        );
    }
    if entry_type == "turn_context" {
        if let Some(model) = entry.payload.as_ref().and_then(extract_payload_model) {
            remember_model(current_model, model);
            *current_model_is_fallback = false;
        }
        return Ok(None);
    }
    if entry_type != "event_msg" {
        return Ok(None);
    }
    let Some(payload) = entry.payload.as_ref() else {
        return Ok(None);
    };
    if payload.payload_type.as_deref() != Some("token_count") {
        return Ok(None);
    }
    let Some(timestamp) = entry.timestamp.as_deref() else {
        return Ok(None);
    };
    let Some(usage) = extract_event_usage(payload, previous_totals) else {
        return Ok(None);
    };
    let (model, is_fallback_model) =
        resolve_event_model(payload, current_model, current_model_is_fallback);
    let timestamp_utc = DateTime::parse_from_rfc3339(timestamp)
        .wrap_err_with(|| format!("invalid timestamp {timestamp}"))?
        .with_timezone(&Utc);
    Ok(Some(TokenUsageEvent {
        session_key,
        session_id,
        timestamp_utc,
        model,
        is_fallback_model,
        usage,
    }))
}

/// Parse an accepted Pi assistant message into one normalized usage event.
fn parse_pi_token_usage_entry<'session, 'model>(
    entry: &SessionLogEntry<'_>,
    session_key: &'session str,
    session_id: &'session str,
    current_model: &'model mut Option<String>,
    current_model_is_fallback: &mut bool,
) -> Result<Option<TokenUsageEvent<'session, 'model>>> {
    let Some(message) = entry.message.as_ref() else {
        return Ok(None);
    };
    if message.role.as_deref() != Some("assistant")
        || message.provider.as_deref() != Some("openai-codex")
    {
        return Ok(None);
    }
    let Some(model) = message
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        return Ok(None);
    };
    let Some(raw) = message.usage else {
        return Ok(None);
    };
    let input = raw
        .input
        .saturating_add(raw.cache_read)
        .saturating_add(raw.cache_write);
    let total = if raw.total_tokens > 0 {
        raw.total_tokens
    } else {
        input.saturating_add(raw.output)
    };
    let usage = UsageTotals {
        input,
        cached_input: raw.cache_read.min(input),
        output: raw.output,
        reasoning_output: raw.reasoning.min(raw.output),
        total,
    };
    if usage.input == 0 && usage.output == 0 {
        return Ok(None);
    }
    let Some(timestamp) = entry.timestamp.as_deref() else {
        return Ok(None);
    };
    let timestamp_utc = DateTime::parse_from_rfc3339(timestamp)
        .wrap_err_with(|| format!("invalid timestamp {timestamp}"))?
        .with_timezone(&Utc);
    remember_model(current_model, model);
    *current_model_is_fallback = false;
    Ok(Some(TokenUsageEvent {
        session_key,
        session_id,
        timestamp_utc,
        model: current_model
            .as_deref()
            .expect("Pi message model was just stored"),
        is_fallback_model: false,
        usage,
    }))
}

/// Deserialize one session entry from bytes, preserving invalid UTF-8 scan errors.
fn parse_session_log_entry(line: &[u8]) -> Result<Option<SessionLogEntry<'_>>> {
    match serde_json::from_slice::<SessionLogEntry<'_>>(line) {
        Ok(entry) => Ok(Some(entry)),
        Err(_) if std::str::from_utf8(line).is_err() => {
            Err(eyre::eyre!("session log line is not valid UTF-8"))
        }
        Err(_) => Ok(None),
    }
}

/// Extract normalized usage from one token-count payload.
fn extract_event_usage(
    payload: &EntryPayload<'_>,
    previous_totals: &mut Option<RawUsage>,
) -> Option<UsageTotals> {
    let (last_usage, total_usage) = if payload.info.is_some() {
        (
            info_usage(payload, UsageKind::Last),
            info_usage(payload, UsageKind::Total),
        )
    } else {
        (
            payload
                .last_token_usage
                .as_ref()
                .copied()
                .map(UsagePayload::into_raw_usage),
            payload
                .total_token_usage
                .as_ref()
                .copied()
                .map(UsagePayload::into_raw_usage),
        )
    };
    let mut raw_usage = last_usage;
    if raw_usage.is_none()
        && let Some(total_usage) = total_usage
    {
        raw_usage = Some(subtract_usage(total_usage, *previous_totals));
    }
    if let Some(total_usage) = total_usage {
        *previous_totals = Some(total_usage);
    } else if let Some(last_usage) = last_usage {
        *previous_totals = Some(previous_totals.unwrap_or_default().advance(last_usage));
    }
    let usage = raw_usage?.into_usage_totals();
    if usage.input == 0
        && usage.cached_input == 0
        && usage.output == 0
        && usage.reasoning_output == 0
    {
        return None;
    }
    Some(usage)
}

/// Resolve the model for one token-count payload and keep parser state in sync.
fn resolve_event_model<'a>(
    payload: &EntryPayload<'_>,
    current_model: &'a mut Option<String>,
    current_model_is_fallback: &mut bool,
) -> (&'a str, bool) {
    if let Some(model) = extract_payload_model(payload) {
        remember_model(current_model, model);
        *current_model_is_fallback = false;
    }
    if current_model.is_none() {
        remember_model(current_model, DEFAULT_FALLBACK_MODEL);
        *current_model_is_fallback = true;
    }

    (
        current_model
            .as_deref()
            .expect("resolved event model should always be present"),
        *current_model_is_fallback,
    )
}

/// Extract a model name from a payload shape.
pub(in crate::app) fn extract_payload_model<'a>(payload: &'a EntryPayload<'a>) -> Option<&'a str> {
    let info_fields = payload.info.as_ref().map(|info| &info.model_fields);
    [
        info_fields.and_then(|fields| fields.model.as_deref()),
        info_fields.and_then(|fields| fields.model_name.as_deref()),
        payload.model_fields.model.as_deref(),
        payload.model_fields.model_name.as_deref(),
        info_fields.and_then(metadata_model),
        metadata_model(&payload.model_fields),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|model| !model.is_empty())
}

/// Extract the metadata model from one model lookup container.
fn metadata_model<'a>(fields: &'a ModelFields<'a>) -> Option<&'a str> {
    fields
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.model.as_deref())
}

/// Remember the last resolved model while reusing existing allocation capacity.
fn remember_model(current_model: &mut Option<String>, model: &str) {
    match current_model {
        Some(current) => {
            current.clear();
            current.push_str(model);
        }
        None => *current_model = Some(model.to_string()),
    }
}

/// Usage field selectors reused across payload levels.
#[derive(Clone, Copy)]
enum UsageKind {
    /// Event delta usage.
    Last,
    /// Cumulative usage.
    Total,
}

/// Extract one usage payload from the nested `info` object when present.
fn info_usage(payload: &EntryPayload<'_>, usage_kind: UsageKind) -> Option<RawUsage> {
    let info = payload.info.as_ref()?;
    match usage_kind {
        UsageKind::Last => info
            .last_token_usage
            .as_ref()
            .copied()
            .map(UsagePayload::into_raw_usage),
        UsageKind::Total => info
            .total_token_usage
            .as_ref()
            .copied()
            .map(UsagePayload::into_raw_usage),
    }
}

/// Convert cumulative totals into a delta.
pub(in crate::app) fn subtract_usage(current: RawUsage, previous: Option<RawUsage>) -> RawUsage {
    let previous = previous.unwrap_or_default();
    RawUsage {
        input: current.input.saturating_sub(previous.input),
        cached_input: current.cached_input.saturating_sub(previous.cached_input),
        output: current.output.saturating_sub(previous.output),
        reasoning_output: current
            .reasoning_output
            .saturating_sub(previous.reasoning_output),
        total: current.total.saturating_sub(previous.total),
    }
}
