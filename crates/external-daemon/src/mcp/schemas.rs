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
    #[serde(rename = "subagent")]
    pub agent: Option<String>,
    pub repository: String,
    #[serde(default)]
    pub permission_mode: PublicPermissionMode,
    pub prompt: String,
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
    #[schemars(range(min = 10000000, max = 99999999))]
    pub agent_id: u64,
    #[serde(default = "default_wait_time")]
    #[schemars(range(min = 0, max = 299))]
    pub wait_time: u64,
    #[serde(default)]
    pub message_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSendInput {
    #[schemars(range(min = 10000000, max = 99999999))]
    pub agent_id: u64,
    #[serde(default, deserialize_with = "optional_non_null")]
    pub message_id: Option<String>,
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
