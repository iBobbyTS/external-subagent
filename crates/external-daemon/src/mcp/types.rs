//! Public pending-request, question, and decision envelope types plus the
//! public/internal task-id mapping.
//!
//! Extracted mechanically from the former single-file `mcp` module; the
//! facade at `crate::mcp` keeps every historical path importable.
use super::errors::{validation_error, ToolError};
use crate::rpc::{PendingRequestView, QuestionView};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicPendingKind {
    Permission,
    UserInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicOperation {
    Read,
    Write,
    Command,
    Network,
    GitRefMutation,
    UserInput,
    Unknown,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicPendingRequest {
    pub request_id: String,
    pub kind: PublicPendingKind,
    pub tool_name: Option<String>,
    pub operation: PublicOperation,
    pub summary: String,
    /// Full embedded question for user_input requests; truncated marks
    /// questions beyond the embed cap.
    pub question: Option<PublicQuestion>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicQuestion {
    pub text: String,
    pub truncated: bool,
}

impl From<QuestionView> for PublicQuestion {
    fn from(value: QuestionView) -> Self {
        Self {
            text: value.text,
            truncated: value.truncated,
        }
    }
}

impl From<PendingRequestView> for PublicPendingRequest {
    fn from(value: PendingRequestView) -> Self {
        Self {
            request_id: value.request_id,
            kind: match value.kind.as_str() {
                "permission" => PublicPendingKind::Permission,
                _ => PublicPendingKind::UserInput,
            },
            tool_name: value.tool_name,
            operation: match value.operation.as_str() {
                "read" => PublicOperation::Read,
                "write" => PublicOperation::Write,
                "command" => PublicOperation::Command,
                "network" => PublicOperation::Network,
                "git_ref_mutation" => PublicOperation::GitRefMutation,
                "user_input" => PublicOperation::UserInput,
                _ => PublicOperation::Unknown,
            },
            summary: value.summary,
            question: value.question.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicDecision {
    Allow,
    Deny,
    Answer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicResponseDisposition {
    Responded,
    AlreadyResponded,
    InFlight,
}

const MIN_PUBLIC_TASK_ID: u64 = 10_000_000;
const MAX_PUBLIC_TASK_ID: u64 = 99_999_999;

pub(super) fn public_task_id(value: &str) -> Result<u64, ToolError> {
    let id = value
        .parse::<u64>()
        .map_err(|_| validation_error("agent_id is invalid"))?;
    if !(MIN_PUBLIC_TASK_ID..=MAX_PUBLIC_TASK_ID).contains(&id) {
        return Err(validation_error("agent_id is outside the allowed range"));
    }
    Ok(id)
}

pub(super) fn internal_task_id(value: u64) -> Result<String, ToolError> {
    if !(MIN_PUBLIC_TASK_ID..=MAX_PUBLIC_TASK_ID).contains(&value) {
        return Err(validation_error("agent_id is outside the allowed range"));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_wait_projection_keeps_non_bash_respondable_pending_requests() {
        let view = PendingRequestView {
            request_id: "read-request".into(),
            kind: "permission".into(),
            tool_name: Some("Read".into()),
            operation: "read".into(),
            summary: "target input.txt".into(),
            question: None,
        };
        let projected: PublicPendingRequest = view.into();
        assert_eq!(projected.kind, PublicPendingKind::Permission);
        assert_eq!(projected.tool_name.as_deref(), Some("Read"));
        assert_eq!(projected.operation, PublicOperation::Read);
        // The removed handshake fields never leak into the public projection.
        let encoded = serde_json::to_value(&projected).unwrap();
        for gone in ["state", "respondable", "policy_preview"] {
            assert_eq!(encoded.get(gone), None, "{gone} must not leak");
        }
    }
}
