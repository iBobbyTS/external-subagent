//! The structured tool-error envelope and daemon-error projection.
//!
//! Extracted mechanically from the former single-file `mcp` module; the
//! facade at `crate::mcp` keeps every historical path importable.
use super::types::public_task_id;
use crate::rpc::{RpcError, RpcErrorCode};
use rmcp::{
    handler::server::tool::IntoCallToolResult,
    model::{CallToolResponse, CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicErrorEnvelope {
    pub error: PublicToolErrorBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicToolErrorBody {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub body: PublicToolErrorBody,
    pub legacy_text: String,
}

impl ToolError {
    pub(crate) fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        legacy_text: impl Into<String>,
        component: &'static str,
    ) -> Self {
        Self {
            body: PublicToolErrorBody {
                code: code.into(),
                message: message.into(),
                component: Some(component.into()),
                operation: None,
                request_id: None,
                agent_id: None,
                prompt_count: None,
            },
            legacy_text: legacy_text.into(),
        }
    }

    pub(crate) fn with_operation(mut self, operation: impl Into<String>) -> Self {
        self.body.operation = Some(operation.into());
        self
    }

    pub(crate) fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.body.request_id = Some(request_id.into());
        self
    }

    pub(crate) fn with_agent_id(mut self, agent_id: Option<String>) -> Self {
        if self.body.agent_id.is_none() {
            self.body.agent_id = agent_id.and_then(|value| public_task_id(&value).ok());
        }
        self
    }

    pub(crate) fn with_prompt_count(mut self, prompt_count: u64) -> Self {
        self.body.prompt_count = Some(prompt_count);
        self
    }
}

impl IntoCallToolResult for ToolError {
    fn into_call_tool_result(self) -> Result<CallToolResponse, rmcp::ErrorData> {
        let structured = serde_json::to_value(PublicErrorEnvelope { error: self.body })
            .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?;
        let mut result = CallToolResult::structured_error(structured);
        // structured_error defaults content to serialized JSON. Preserve the
        // bounded legacy text for existing human and string consumers.
        result.content = vec![ContentBlock::text(self.legacy_text)];
        Ok(result.into())
    }
}

pub(crate) fn validation_error(detail: impl Into<String>) -> ToolError {
    let detail = detail.into();
    ToolError::new(
        "validation",
        "request validation failed",
        format!("validation: {detail}"),
        "facade",
    )
}

pub(crate) fn public_error(error: RpcError) -> ToolError {
    let detail = error.message.clone();
    // Admission sites compose the full public message and ship it as the RPC
    // detail (same Design B as the AgentUnknown roster branch below); project
    // a recognized prefix verbatim instead of the static sentence. The list is
    // a prefix set so it can grow without message parsing.
    const PASSTHROUGH_DETAIL_PREFIXES: [&str; 1] = ["dsh model must be"];
    let (code, message) = match error.code {
        RpcErrorCode::Malformed | RpcErrorCode::Validation => (
            "validation",
            if PASSTHROUGH_DETAIL_PREFIXES
                .iter()
                .any(|prefix| detail.starts_with(prefix))
            {
                detail.as_str()
            } else {
                "request validation failed"
            },
        ),
        RpcErrorCode::AgentRequired => ("subagent_required", "subagent is required"),
        // The daemon composes the full public message (with the roster) at the
        // admission/probe/models/list rejection sites and ships it as the RPC
        // detail; project it verbatim. Any other emission site still sends an
        // internal "agent is unknown"-style detail and keeps the static
        // message.
        RpcErrorCode::AgentUnknown => (
            "subagent_unknown",
            if detail.starts_with("subagent is unknown") {
                detail.as_str()
            } else {
                "subagent is unknown"
            },
        ),
        RpcErrorCode::AgentDisabled => ("agent_disabled", "agent is disabled"),
        RpcErrorCode::AgentUnsupported if detail == "CODEX_WRITE_MANIFEST_UNSUPPORTED" => (
            "codex_write_manifest_unsupported",
            "codex does not support non-empty write_manifest",
        ),
        RpcErrorCode::AgentUnsupported => ("agent_unsupported", "agent is unsupported"),
        RpcErrorCode::ModelSelectionUnsupported => (
            "model_selection_unsupported",
            "model selection is unsupported for zcode",
        ),
        RpcErrorCode::Oversized => ("oversized", "bounded response or request was too large"),
        RpcErrorCode::UnknownMethod => ("protocol_error", "daemon method is unavailable"),
        RpcErrorCode::NotFound => ("not_found", "agent task was not found"),
        RpcErrorCode::Conflict => (
            "conflict",
            match detail.as_str() {
                "WORKSPACE_BUSY" => "WORKSPACE_BUSY",
                "MESSAGE_ID_CONFLICT" => "MESSAGE_ID_CONFLICT",
                _ => "durable state conflict",
            },
        ),
        RpcErrorCode::Unavailable if detail == "RUNTIME_COMMAND_FAILED" => (
            "runtime_command_failed",
            "runtime could not complete the command",
        ),
        RpcErrorCode::Timeout => ("timeout", "daemon operation timed out"),
        RpcErrorCode::RuntimeLost => ("runtime_lost", "agent runtime was lost"),
        RpcErrorCode::ResultInvalid => ("result_invalid", "stored task result failed verification"),
        RpcErrorCode::Persistence => ("persistence", "durable store operation failed"),
        RpcErrorCode::Internal => ("internal", "daemon operation failed"),
        RpcErrorCode::Unavailable => (
            "unavailable",
            "subagent daemon could not complete the operation",
        ),
    };
    let agent_id = error.active_agent_id;
    let legacy_text = if let Some(agent_id) = agent_id.as_ref() {
        format!("{code}: {message} (active_agent_id={agent_id})")
    } else if matches!(error.code, RpcErrorCode::Validation | RpcErrorCode::Malformed)
        && detail != message
        && detail.len() <= 512
    {
        // The detail is the whole public explanation for a rejected input, so
        // the de-duplicated legacy form drops the static sentence entirely.
        format!("{code}: {detail}")
    } else if matches!(
        error.code,
        RpcErrorCode::AgentRequired
            | RpcErrorCode::AgentUnknown
            | RpcErrorCode::AgentDisabled
            | RpcErrorCode::AgentUnsupported
            | RpcErrorCode::ModelSelectionUnsupported
    ) && detail != message
        && detail.len() <= 512
    {
        format!("{code}: {message}: {detail}")
    } else {
        format!("{code}: {message}")
    };
    let projected = ToolError::new(code, message, legacy_text, "daemon").with_agent_id(agent_id);
    if matches!(
        error.code,
        RpcErrorCode::Validation
            | RpcErrorCode::Malformed
            | RpcErrorCode::AgentRequired
            | RpcErrorCode::AgentUnknown
            | RpcErrorCode::AgentDisabled
            | RpcErrorCode::AgentUnsupported
            | RpcErrorCode::ModelSelectionUnsupported
    ) {
        projected.with_prompt_count(0)
    } else {
        projected
    }
}

pub(crate) fn public_transport_error(error: std::io::Error) -> ToolError {
    let (code, message, legacy_text) = match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => (
            "timeout",
            "daemon call exceeded its bound",
            "timeout: daemon call exceeded its bound",
        ),
        std::io::ErrorKind::InvalidData => (
            "protocol_error",
            "daemon returned an invalid or oversized frame",
            "protocol_error: daemon returned an invalid or oversized frame",
        ),
        _ => (
            "daemon_unavailable",
            "subagent daemon is unavailable",
            "daemon_unavailable: subagent daemon is unavailable",
        ),
    };
    ToolError::new(code, message, legacy_text, "daemon_transport")
}

pub(crate) fn protocol_error() -> ToolError {
    ToolError::new(
        "protocol_error",
        "unexpected daemon response",
        "protocol_error: unexpected daemon response",
        "facade",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_manifest_rejection_has_a_distinct_public_code_and_zero_prompts() {
        let error = public_error(RpcError::new(
            RpcErrorCode::AgentUnsupported,
            "CODEX_WRITE_MANIFEST_UNSUPPORTED",
        ));
        assert_eq!(error.body.code, "codex_write_manifest_unsupported");
        assert_eq!(error.body.prompt_count, Some(0));
        assert_eq!(error.body.agent_id, None);
    }

    #[test]
    fn unknown_subagent_projection_carries_the_composed_roster_message() {
        let composed = public_error(RpcError::new(
            RpcErrorCode::AgentUnknown,
            "subagent is unknown, available subagents are [\"codex\", \"dsh\"]",
        ));
        assert_eq!(composed.body.code, "subagent_unknown");
        assert_eq!(
            composed.body.message,
            "subagent is unknown, available subagents are [\"codex\", \"dsh\"]"
        );
        // detail == message here, so the legacy text must not repeat the roster.
        assert_eq!(
            composed.legacy_text,
            "subagent_unknown: subagent is unknown, available subagents are [\"codex\", \"dsh\"]"
        );
        assert_eq!(composed.body.prompt_count, Some(0));

        let plain = public_error(RpcError::new(RpcErrorCode::AgentUnknown, "agent is unknown"));
        assert_eq!(plain.body.code, "subagent_unknown");
        assert_eq!(plain.body.message, "subagent is unknown");
        assert_eq!(
            plain.legacy_text,
            "subagent_unknown: subagent is unknown: agent is unknown"
        );
    }

    #[test]
    fn errors_distinguish_conflicts_rejections_and_unreachable_socket() {
        assert_eq!(
            public_error(RpcError::new(RpcErrorCode::Conflict, "MESSAGE_ID_CONFLICT")).legacy_text,
            "conflict: MESSAGE_ID_CONFLICT"
        );
        assert_eq!(
            public_error(RpcError::new(
                RpcErrorCode::Conflict,
                "private store details"
            ))
            .legacy_text,
            "conflict: durable state conflict"
        );
        assert!(public_error(RpcError::new(
            RpcErrorCode::Unavailable,
            "RUNTIME_COMMAND_FAILED"
        ))
        .legacy_text
        .starts_with("runtime_command_failed:"));
        assert!(public_transport_error(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        ))
        .legacy_text
        .starts_with("daemon_unavailable:"));
    }

    #[test]
    fn workspace_busy_preserves_code_message_and_active_agent() {
        let mut error = RpcError::new(RpcErrorCode::Conflict, "WORKSPACE_BUSY");
        error.active_agent_id = Some("10000042".into());
        let rendered = public_error(error);
        assert!(rendered.legacy_text.starts_with("conflict: WORKSPACE_BUSY"));
        assert!(rendered.legacy_text.contains("active_agent_id=10000042"));
        assert_eq!(rendered.body.code, "conflict");
        assert_eq!(rendered.body.agent_id, Some(10000042));
    }

    #[test]
    fn structured_error_keeps_legacy_text_and_machine_fields_in_sync() {
        let result = public_transport_error(std::io::Error::from(std::io::ErrorKind::TimedOut))
            .with_operation("wait")
            .with_request_id("request-7")
            .with_agent_id(Some("10000007".into()))
            .into_call_tool_result()
            .unwrap();
        let rmcp::model::CallToolResponse::Complete(result) = result else {
            panic!("expected complete tool result")
        };
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "timeout: daemon call exceeded its bound"
        );
        let error = &result.structured_content.unwrap()["error"];
        assert_eq!(error["code"], "timeout");
        assert_eq!(error["component"], "daemon_transport");
        assert_eq!(error["operation"], "wait");
        assert_eq!(error["request_id"], "request-7");
        assert_eq!(error["agent_id"], serde_json::json!(10000007));
    }

    #[test]
    fn typed_sentinels_keep_distinct_machine_codes_without_message_parsing() {
        let cases = [
            (
                RpcErrorCode::Unavailable,
                "RUNTIME_COMMAND_FAILED",
                "runtime_command_failed",
            ),
            (RpcErrorCode::Timeout, "private timeout detail", "timeout"),
            (
                RpcErrorCode::RuntimeLost,
                "private driver detail",
                "runtime_lost",
            ),
            (
                RpcErrorCode::ResultInvalid,
                "private digest detail",
                "result_invalid",
            ),
            (RpcErrorCode::NotFound, "private lookup detail", "not_found"),
        ];
        for (kind, detail, expected) in cases {
            let projected = public_error(RpcError::new(kind, detail));
            assert_eq!(projected.body.code, expected);
            assert!(projected.legacy_text.starts_with(expected));
        }
        let validation = public_error(RpcError::new(
            RpcErrorCode::Validation,
            "agent_id is invalid",
        ));
        assert_eq!(validation.body.code, "validation");
        assert!(validation.legacy_text.contains("agent_id is invalid"));
    }

    #[test]
    fn dsh_model_format_detail_is_projected_verbatim_with_zero_prompts() {
        let detail = "dsh model must be {provider}:{model}; the ':' separator is missing";
        let projected = public_error(RpcError::new(RpcErrorCode::Validation, detail));
        assert_eq!(projected.body.code, "validation");
        assert_eq!(projected.body.message, detail);
        // The passthrough makes detail == message, so the legacy branch emits
        // one sentence without the static "request validation failed" middle.
        assert_eq!(projected.legacy_text, format!("validation: {detail}"));
        assert_eq!(projected.body.prompt_count, Some(0));
    }

    #[test]
    fn non_prefix_validation_and_malformed_keep_the_static_message_deduplicated() {
        let detail = "codex requires an explicit model or agents.codex.default_model";
        let projected = public_error(RpcError::new(RpcErrorCode::Validation, detail));
        assert_eq!(projected.body.code, "validation");
        assert_eq!(projected.body.message, "request validation failed");
        assert_eq!(projected.legacy_text, format!("validation: {detail}"));
        assert_eq!(projected.body.prompt_count, Some(0));

        // Malformed shares the public code, the de-duplication and the
        // zero-prompt envelope.
        let malformed = public_error(RpcError::new(RpcErrorCode::Malformed, "frame is malformed"));
        assert_eq!(malformed.body.code, "validation");
        assert_eq!(malformed.body.message, "request validation failed");
        assert_eq!(malformed.legacy_text, "validation: frame is malformed");
        assert_eq!(malformed.body.prompt_count, Some(0));
    }

    #[test]
    fn model_selection_rejection_legacy_collapses_to_one_sentence() {
        let projected = public_error(RpcError::new(
            RpcErrorCode::ModelSelectionUnsupported,
            "model selection is unsupported for zcode",
        ));
        assert_eq!(projected.body.code, "model_selection_unsupported");
        assert_eq!(
            projected.body.message,
            "model selection is unsupported for zcode"
        );
        assert_eq!(
            projected.legacy_text,
            "model_selection_unsupported: model selection is unsupported for zcode"
        );
        assert_eq!(projected.body.prompt_count, Some(0));
    }

    #[test]
    fn capability_composites_keep_their_legacy_shape_after_validation_dedup() {
        // Roster stays exactly as before the Validation change.
        let roster = public_error(RpcError::new(RpcErrorCode::AgentUnknown, "agent is unknown"));
        assert_eq!(roster.body.code, "subagent_unknown");
        assert_eq!(roster.body.message, "subagent is unknown");
        assert_eq!(
            roster.legacy_text,
            "subagent_unknown: subagent is unknown: agent is unknown"
        );
        assert_eq!(roster.body.prompt_count, Some(0));
        // The other composite members keep `{code}: {message}: {detail}`.
        let unsupported = public_error(RpcError::new(
            RpcErrorCode::AgentUnsupported,
            "private capability detail",
        ));
        assert_eq!(
            unsupported.legacy_text,
            "agent_unsupported: agent is unsupported: private capability detail"
        );
        assert_eq!(unsupported.body.prompt_count, Some(0));
    }
}
