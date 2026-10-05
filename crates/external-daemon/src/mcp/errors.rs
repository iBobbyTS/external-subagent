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
    let message = if is_profile_error(&detail) {
        detail.clone()
    } else {
        "request validation failed".to_string()
    };
    ToolError::new(
        "validation",
        message,
        format!("validation: {detail}"),
        "facade",
    )
}

pub(crate) fn is_profile_error(detail: &str) -> bool {
    crate::rpc::profiles::PROFILE_ERROR_PREFIXES
        .iter()
        .any(|prefix| detail.starts_with(prefix))
}

#[allow(dead_code)]
pub(crate) fn public_error(error: RpcError) -> ToolError {
    public_error_for_op(error, "")
}

pub(crate) fn public_error_for_op(error: RpcError, operation: &str) -> ToolError {
    let detail = error.message.clone();
    // Admission sites compose the full public message and ship it as the RPC
    // detail (same Design B as the AgentUnknown roster branch below); project
    // a recognized prefix verbatim instead of the static sentence. The list is
    // a prefix set so it can grow without message parsing.
    const PASSTHROUGH_DETAIL_PREFIXES: [&str; 3] =
        ["dsh model must be", "zcode model must be", "agy model must be"];
    let (code, message) = match error.code {
        RpcErrorCode::Malformed | RpcErrorCode::Validation => (
            "validation",
            if PASSTHROUGH_DETAIL_PREFIXES
                .iter()
                .any(|prefix| detail.starts_with(prefix))
                || (operation == "spawn" && detail.starts_with("MODEL_REJECTED"))
                || is_profile_error(&detail)
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
            if is_profile_error(&detail) || detail.starts_with("subagent is unknown") {
                detail.as_str()
            } else {
                "subagent is unknown"
            },
        ),
        RpcErrorCode::AgentDisabled => (
            "agent_disabled",
            if is_profile_error(&detail) {
                detail.as_str()
            } else {
                "agent is disabled"
            },
        ),
        RpcErrorCode::AgentUnsupported if detail == "CODEX_WRITE_MANIFEST_UNSUPPORTED" => (
            "codex_write_manifest_unsupported",
            "codex does not support non-empty write_manifest",
        ),
        // agy's admission refusals ship a sentinel detail (like codex's
        // manifest) so the RPC rejection stays diagnosable while the public
        // `error.code` stays machine-distinct without message parsing.
        RpcErrorCode::AgentUnsupported if detail == "AGY_WRITE_MANIFEST_UNSUPPORTED" => (
            "agy_write_manifest_unsupported",
            "agy does not support non-empty write_manifest",
        ),
        RpcErrorCode::AgentUnsupported if detail == "AGY_PERMISSION_MODE_UNSUPPORTED" => (
            "agy_permission_mode_unsupported",
            "agy supports only the build and yolo permission modes",
        ),
        RpcErrorCode::AgentUnsupported => (
            "agent_unsupported",
            if is_profile_error(&detail) {
                detail.as_str()
            } else {
                "agent is unsupported"
            },
        ),
        RpcErrorCode::SteerUnsupported => ("steer_unsupported", "该 subagent 不支持 steer"),
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
        RpcErrorCode::Unavailable if operation == "spawn" => ("unavailable", detail.as_str()),
        RpcErrorCode::Unavailable if detail == "RUNTIME_COMMAND_FAILED" => (
            "runtime_command_failed",
            "runtime could not complete the command",
        ),
        RpcErrorCode::Timeout if operation == "spawn" => ("timeout", detail.as_str()),
        RpcErrorCode::Timeout => ("timeout", "daemon operation timed out"),
        RpcErrorCode::RuntimeLost if operation == "spawn" => ("runtime_lost", detail.as_str()),
        RpcErrorCode::RuntimeLost => ("runtime_lost", "agent runtime was lost"),
        RpcErrorCode::ResultInvalid if operation == "spawn" => ("result_invalid", detail.as_str()),
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
    } else if is_profile_error(&detail) {
        format!("{code}: {detail}")
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
            | RpcErrorCode::SteerUnsupported
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
            | RpcErrorCode::SteerUnsupported
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
    fn steer_unsupported_has_a_distinct_public_code_and_zero_prompts() {
        let error = public_error(RpcError::new(RpcErrorCode::SteerUnsupported, "steer_unsupported"));
        assert_eq!(error.body.code, "steer_unsupported");
        assert_eq!(error.body.prompt_count, Some(0));
    }

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
    fn agy_manifest_and_permission_rejections_have_distinct_public_codes() {
        let manifest = public_error(RpcError::new(
            RpcErrorCode::AgentUnsupported,
            "AGY_WRITE_MANIFEST_UNSUPPORTED",
        ));
        assert_eq!(manifest.body.code, "agy_write_manifest_unsupported");
        assert_eq!(manifest.body.message, "agy does not support non-empty write_manifest");
        assert_eq!(manifest.body.prompt_count, Some(0));

        let permission = public_error(RpcError::new(
            RpcErrorCode::AgentUnsupported,
            "AGY_PERMISSION_MODE_UNSUPPORTED",
        ));
        assert_eq!(permission.body.code, "agy_permission_mode_unsupported");
        assert_eq!(
            permission.body.message,
            "agy supports only the build and yolo permission modes"
        );
        assert_eq!(permission.body.prompt_count, Some(0));
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
    fn agy_model_format_detail_is_projected_verbatim_with_zero_prompts() {
        let detail =
            "agy model must be a bare slug without a ':' or '/' separator or whitespace; the token contains whitespace";
        let projected = public_error(RpcError::new(RpcErrorCode::Validation, detail));
        assert_eq!(projected.body.code, "validation");
        assert_eq!(projected.body.message, detail);
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

    #[test]
    fn profile_error_is_preserved_through_mcp_projection() {
        let msg = "profile 'missing' not found; available profiles: [a, b, c]";
        let err = RpcError::new_profile_error(RpcErrorCode::Validation, msg);
        let projected = public_error(err);
        assert_eq!(projected.body.code, "validation");
        assert_eq!(projected.body.message, msg);
        assert_eq!(projected.legacy_text, format!("validation: {msg}"));

        // Profile error with large list (> 512 bytes) preserves full list without truncation
        let large_msg = format!(
            "profile 'missing' not found; available profiles: [{}]",
            "p".repeat(600)
        );
        let err_large = RpcError::new_profile_error(RpcErrorCode::Validation, &large_msg);
        let projected_large = public_error(err_large);
        assert_eq!(projected_large.body.code, "validation");
        assert_eq!(projected_large.body.message, large_msg);
        assert_eq!(
            projected_large.legacy_text,
            format!("validation: {large_msg}")
        );

        // Profile error with large Unicode list (> 512 bytes)
        let mut unicode_names = Vec::new();
        for i in 0..30 {
            unicode_names.push(format!("中文配置预设名称_{:02}", i));
        }
        let unicode_msg = format!(
            "profile 'missing' not found; available profiles: [{}]",
            unicode_names.join(", ")
        );
        assert!(unicode_msg.len() > 512);
        let err_unicode = RpcError::new_profile_error(RpcErrorCode::Validation, &unicode_msg);
        let projected_unicode = public_error(err_unicode);
        assert_eq!(projected_unicode.body.code, "validation");
        assert_eq!(projected_unicode.body.message, unicode_msg);
        assert_eq!(
            projected_unicode.legacy_text,
            format!("validation: {unicode_msg}")
        );
    }

    #[test]
    fn non_profile_validation_error_with_profile_substrings_does_not_project_detail() {
        // write_manifest counterexample:
        // scheduler rejects with "scheduler rejected the operation: invalid path ../available profiles:: path must be repository-relative"
        let rpc_err = RpcError::new(
            RpcErrorCode::Validation,
            "scheduler rejected the operation: invalid path ../available profiles:: path must be repository-relative",
        );
        let tool_err = public_error_for_op(rpc_err, "spawn");
        assert_eq!(
            tool_err.body.message,
            "request validation failed",
            "Non-profile request containing 'available profiles:' must NOT project detail into body.message"
        );

        let rpc_err2 = RpcError::new(
            RpcErrorCode::Validation,
            "scheduler rejected the operation: invalid path ../profile file 'test': path must be repository-relative",
        );
        let tool_err2 = public_error_for_op(rpc_err2, "spawn");
        assert_eq!(
            tool_err2.body.message,
            "request validation failed",
            "Non-profile request containing 'profile file \'' must NOT project detail into body.message"
        );
    }

    #[test]
    fn profile_derived_field_admission_error_is_preserved_through_mcp_projection() {
        let msg = "profile file '/path/to/dsh_bad.toml' is invalid: field 'model': dsh model must be '{provider}:{model}'; the ':' separator is missing; available profiles: [good]";
        let rpc_err = RpcError::new_profile_error(RpcErrorCode::Validation, msg);
        let tool_err = public_error_for_op(rpc_err, "spawn");
        assert_eq!(tool_err.body.code, "validation");
        assert_eq!(tool_err.body.message, msg);
        assert_eq!(tool_err.legacy_text, format!("validation: {msg}"));

        // AgentUnsupported preserves agent_unsupported code and projects full profile detail
        let agy_msg = "profile file '/path/to/agy_bad.toml' is invalid: field 'permission_mode': AGY_PERMISSION_MODE_UNSUPPORTED; available profiles: [good]";
        let rpc_agy_err = RpcError::new_profile_error(RpcErrorCode::AgentUnsupported, agy_msg);
        let tool_agy_err = public_error_for_op(rpc_agy_err, "spawn");
        assert_eq!(tool_agy_err.body.code, "agent_unsupported");
        assert_eq!(tool_agy_err.body.message, agy_msg);
        assert_eq!(
            tool_agy_err.legacy_text,
            format!("agent_unsupported: {agy_msg}")
        );
    }
}
