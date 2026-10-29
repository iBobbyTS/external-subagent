//! Tool input schemas, defaults, and bounded input validation.
//!
//! Extracted mechanically from the former private `mcp::server` module; the
//! facade at `crate::mcp` keeps every historical path importable.
use super::errors::{validation_error, PublicErrorEnvelope, ToolError};
use super::types::PublicDecision;
use crate::rpc::TaskPhaseFilter;
use external_core::PermissionMode;
use external_store::TaskOutcome;
use rmcp::handler::server::tool::schema_for_type;
use rmcp::model::JsonObject;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicPermissionMode {
    Build,
    Edit,
    Plan,
    Yolo,
}

impl Default for PublicPermissionMode {
    fn default() -> Self {
        Self::Build
    }
}

impl From<PublicPermissionMode> for PermissionMode {
    fn from(value: PublicPermissionMode) -> Self {
        match value {
            PublicPermissionMode::Build => Self::Build,
            PublicPermissionMode::Edit => Self::Edit,
            PublicPermissionMode::Plan => Self::Plan,
            PublicPermissionMode::Yolo => Self::Yolo,
        }
    }
}

pub(super) const MAX_MESSAGE_BYTES: usize = 16 * 1024;

fn default_wait_time() -> u64 {
    290
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyInput {}

/// `external_subagent_status` input. Every field is optional and defaults to
/// the passive behavior, so `{}` and an omitted argument object are exactly
/// equivalent to the historical zero-argument contract.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusInput {
    /// When true, refresh every configured subagent's model-catalog evidence
    /// (bounded, prompt-free discovery) before projecting this status. The
    /// default false keeps status a passive projection of recorded evidence.
    #[serde(default)]
    pub probe: bool,
}

fn optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
pub(super) fn validate_text(value: &str, field: &str, max: usize) -> Result<(), ToolError> {
    if value.trim().is_empty() || value.len() > max || value.contains('\0') {
        Err(validation_error(format!("{field} is invalid")))
    } else {
        Ok(())
    }
}

#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum ToolOutputSchema<T> {
    Success(T),
    Error(PublicErrorEnvelope),
}

pub(super) fn tool_output_schema<T: JsonSchema + 'static>() -> Arc<JsonObject> {
    schema_for_type::<ToolOutputSchema<T>>()
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct AgentSpawnInput {
    #[serde(default, deserialize_with = "optional_non_null")]
    #[schemars(with = "String")]
    pub profile: Option<String>,
    #[serde(default, deserialize_with = "optional_non_null")]
    #[schemars(with = "String")]
    #[serde(rename = "subagent")]
    pub agent: Option<String>,
    pub repository: String,
    #[serde(default, deserialize_with = "optional_non_null")]
    #[schemars(with = "PublicPermissionMode")]
    pub permission_mode: Option<PublicPermissionMode>,
    pub prompt: String,
    /// Caller write scope for the task. codex and agy reject a non-empty
    /// manifest before task creation (codex_write_manifest_unsupported /
    /// agy_write_manifest_unsupported); dsh admits a non-empty manifest in
    /// build through the guarded manifest-build composition (at most 256
    /// entries and 64 KiB serialized, `["."]` keeps the legacy build
    /// composition) and keeps requiring an empty manifest in plan; other
    /// subagents use the protected workspace scope.
    #[serde(default)]
    pub write_manifest: Vec<String>,
    #[serde(default, deserialize_with = "optional_non_null")]
    #[schemars(with = "String")]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "optional_non_null")]
    #[schemars(with = "String")]
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct AgentInput {
    #[schemars(range(min = 10000000, max = 99999999))]
    pub agent_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentListInput {
    #[serde(default, deserialize_with = "optional_non_null")]
    #[schemars(with = "String")]
    #[serde(rename = "subagent")]
    pub agent: Option<String>,
    #[serde(default, deserialize_with = "optional_non_null")]
    pub repository: Option<String>,
    #[serde(default, deserialize_with = "optional_non_null")]
    pub phase: Option<PublicTaskPhase>,
    #[serde(default, deserialize_with = "optional_non_null")]
    pub outcome: Option<PublicOutcomeFilter>,
    #[serde(default, deserialize_with = "optional_non_null")]
    pub cursor: Option<String>,
    #[schemars(range(min = 1, max = 100))]
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

fn default_list_limit() -> usize {
    100
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PublicTaskPhase {
    Queued,
    Preparing,
    Running,
    WaitingInput,
    Cancelling,
    Terminal,
}

impl From<PublicTaskPhase> for TaskPhaseFilter {
    fn from(value: PublicTaskPhase) -> Self {
        match value {
            PublicTaskPhase::Queued => Self::Queued,
            PublicTaskPhase::Preparing => Self::Preparing,
            PublicTaskPhase::Running => Self::Running,
            PublicTaskPhase::WaitingInput => Self::WaitingInput,
            PublicTaskPhase::Cancelling => Self::Cancelling,
            PublicTaskPhase::Terminal => Self::Terminal,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PublicOutcomeFilter {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    RuntimeLost,
    ResultInvalid,
}

impl From<PublicOutcomeFilter> for TaskOutcome {
    fn from(value: PublicOutcomeFilter) -> Self {
        match value {
            PublicOutcomeFilter::Completed => Self::Completed,
            PublicOutcomeFilter::Failed => Self::Failed,
            PublicOutcomeFilter::Cancelled => Self::Cancelled,
            PublicOutcomeFilter::TimedOut => Self::TimedOut,
            PublicOutcomeFilter::RuntimeLost => Self::RuntimeLost,
            PublicOutcomeFilter::ResultInvalid => Self::ResultInvalid,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentWaitInput {
    /// The wait target: one agent id (the historical scalar shape) or a
    /// list of 1..=16 agent ids awaited together. The response shape
    /// follows the input: scalar in → single wait object; list in → an
    /// object keyed by agent id.
    pub agent_id: AgentIdOrList,
    #[serde(default = "default_wait_time")]
    #[schemars(range(min = 0, max = 299))]
    pub wait_time: u64,
    #[serde(default)]
    pub message_id: Option<String>,
}

/// The wait target: one agent id (the historical scalar shape) or a list of
/// up to 16 agent ids awaited together. The wire tolerance below is
/// one-directional compatibility for MCP host clients that serialize
/// non-string arguments as JSON strings (observed with dsh-mcp-client
/// stringifying arrays; scalar-id strings accepted too): a string is
/// accepted when it parses as a JSON array of ids or as a bare decimal id.
/// The schema still declares integer | array; the tolerant branch exists so
/// a broken host cannot lock users out of wait, not to widen the contract.
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(
    untagged,
    expecting = "agent_id must be an agent id or a list of agent ids"
)]
pub enum AgentIdOrList {
    Single(#[schemars(range(min = 10000000, max = 99999999))] u64),
    Multiple(Vec<u64>),
}

impl AgentIdOrList {
    /// Decode the tolerant string form: either a JSON-encoded array of ids
    /// (e.g. `"[10000001, 10000002]"`) or a bare decimal id string
    /// (e.g. `"10000001"`). Returns None for anything else (malformed
    /// JSON, non-integer or non-array JSON, junk text) — those stay hard
    /// errors.
    fn decode_string_form(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') {
            let parsed: serde_json::Value = serde_json::from_str(trimmed).ok()?;
            let values = parsed.as_array()?;
            let mut ids = Vec::with_capacity(values.len());
            for value in values {
                ids.push(value.as_u64()?);
            }
            if ids.is_empty() {
                return None;
            }
            Some(Self::Multiple(ids))
        } else {
            trimmed.parse::<u64>().ok().map(Self::Single)
        }
    }
}

impl<'de> serde::Deserialize<'de> for AgentIdOrList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = AgentIdOrList;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "agent id, list of agent ids, or a string encoding either form",
                )
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(AgentIdOrList::Single(value))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut ids = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(value) = seq.next_element::<u64>()? {
                    ids.push(value);
                }
                Ok(AgentIdOrList::Multiple(ids))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                AgentIdOrList::decode_string_form(value).ok_or_else(|| {
                    E::custom("agent_id string encodes neither an id nor a list of ids")
                })
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicSendMode {
    Queue,
    Steer,
}

impl PublicSendMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSendInput {
    #[schemars(range(min = 10000000, max = 99999999))]
    pub agent_id: u64,
    #[serde(default, deserialize_with = "optional_non_null")]
    pub message_id: Option<String>,
    pub mode: PublicSendMode,
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRespondInput {
    #[schemars(range(min = 10000000, max = 99999999))]
    pub agent_id: u64,
    pub request_id: String,
    pub decision: PublicDecision,
    /// Answer content; required and non-empty exactly when the decision is
    /// answer.
    #[serde(default, deserialize_with = "optional_non_null")]
    pub content: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentResultInput {
    #[schemars(range(min = 10000000, max = 99999999))]
    pub agent_id: u64,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_result_limit")]
    #[schemars(range(min = 1, max = 262144))]
    pub limit: usize,
}

pub(super) fn default_result_limit() -> usize {
    crate::rpc::MAX_RESULT_CHUNK_BYTES
}

#[cfg(test)]
mod wait_input_tests {
    use super::AgentIdOrList;

    #[test]
    fn wait_agent_id_accepts_scalar_and_list_forms() {
        let scalar: AgentIdOrList =
            serde_json::from_str("10000001").expect("scalar id must deserialize");
        assert_eq!(scalar, AgentIdOrList::Single(10000001));
        let list: AgentIdOrList =
            serde_json::from_str("[10000001, 10000002]").expect("id list must deserialize");
        assert_eq!(
            list,
            AgentIdOrList::Multiple(vec![10000001, 10000002])
        );
        // The union rejects non-id shapes instead of coercing. A bare
        // decimal id STRING is the tolerant scalar form (hosts that
        // stringify arguments); junk stays rejected.
        let bare: AgentIdOrList =
            serde_json::from_str("\"10000001\"").expect("bare id string must deserialize");
        assert_eq!(bare, AgentIdOrList::Single(10000001));
        assert!(serde_json::from_str::<AgentIdOrList>("[]").is_ok());
        assert!(serde_json::from_str::<AgentIdOrList>("null").is_err());
        assert!(serde_json::from_str::<AgentIdOrList>("\"abc\"").is_err());
    }

    #[test]
    fn wait_agent_id_accepts_a_json_encoded_list_string_for_broken_hosts() {
        // dsh-mcp-client serializes array arguments as a JSON string; the
        // tolerant branch decodes exactly that shape into the equivalent
        // list form.
        let encoded: AgentIdOrList = serde_json::from_str(
            "\"[10000001, 10000002, 10000001]\"",
        )
        .expect("JSON-encoded list string must deserialize");
        assert_eq!(
            encoded,
            AgentIdOrList::Multiple(vec![10000001, 10000002, 10000001])
        );
        // Whitespace inside the encoded array is tolerated.
        let spaced: AgentIdOrList = serde_json::from_str(
            "\"[10001468, 10001469, 10001470]\"",
        )
        .expect("spaced encoded list must deserialize");
        assert_eq!(
            spaced,
            AgentIdOrList::Multiple(vec![10001468, 10001469, 10001470])
        );
        // Scalar strings in their bare decimal form are accepted too; junk,
        // empty arrays, non-integer elements, and malformed JSON stay hard
        // errors.
        let bare: AgentIdOrList =
            serde_json::from_str("\"12345678\"").expect("bare scalar string must deserialize");
        assert_eq!(bare, AgentIdOrList::Single(12345678));
        assert!(serde_json::from_str::<AgentIdOrList>("\"[]\"").is_err());
        assert!(serde_json::from_str::<AgentIdOrList>("\"[\\\"a\\\"]\"").is_err());
        assert!(serde_json::from_str::<AgentIdOrList>("\"[10000001\"").is_err());
        assert!(serde_json::from_str::<AgentIdOrList>("\"hello\"").is_err());
    }
}
