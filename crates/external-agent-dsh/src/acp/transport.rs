//! Typed shapes for the official DSH ACP JSON-RPC 2.0 stdio transport.
//!
//! Frames are carried by the shared runtime driver in JSON-RPC 2.0 codec
//! mode; this module owns the DSH method names, request params, and strict
//! response projections with the same bounds the S03 catalog probe pinned
//! (session ids and model tokens are opaque, non-empty, NUL-free, and at most
//! 512 bytes).

use serde_json::{json, Value};
use std::path::Path;

pub const INITIALIZE: &str = "initialize";
pub const SESSION_NEW: &str = "session/new";
pub const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
pub const SESSION_PROMPT: &str = "session/prompt";
pub const SESSION_CANCEL: &str = "session/cancel";
pub const SESSION_CLOSE: &str = "session/close";
pub const SESSION_UPDATE: &str = "session/update";
pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
pub const MODELS_LIST: &str = "models/list";

/// DSH ACP advertises exactly one protocol version (S01-pinned).
pub const ACP_PROTOCOL_VERSION: i64 = 1;
pub const ACP_CLIENT_NAME: &str = "external-subagent-dsh";

pub const MAX_SESSION_ID_BYTES: usize = 512;
pub const MAX_STOP_REASON_BYTES: usize = 128;

/// A malformed or unexpected DSH ACP frame shape. Messages stay bounded and
/// never embed raw provider payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeError(pub String);

impl std::fmt::Display for ShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dsh acp shape error: {}", self.0)
    }
}
impl std::error::Error for ShapeError {}

fn opaque_id(value: Option<&Value>, field: &str, max_bytes: usize) -> Result<String, ShapeError> {
    let text = value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= max_bytes && !text.contains('\0'))
        .ok_or_else(|| ShapeError(format!("{field} must be a bounded non-empty string")))?;
    Ok(text.to_owned())
}

pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "clientInfo": {
            "name": ACP_CLIENT_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

pub fn session_new_params(cwd: &Path) -> Value {
    json!({ "cwd": cwd.to_string_lossy() })
}

pub fn set_config_option_params(config_id: &str, value: &str) -> Value {
    json!({ "configId": config_id, "value": value })
}

pub fn prompt_params(prompt: &str) -> Value {
    json!({ "prompt": prompt })
}

pub fn cancel_params(session_id: &str) -> Value {
    json!({ "sessionId": session_id })
}

pub fn close_params(session_id: &str) -> Value {
    json!({ "sessionId": session_id })
}

/// Capabilities observed on `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpCapabilities {
    pub models: bool,
    pub cancel: bool,
    pub permission: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializeResult {
    pub protocol_version: i64,
    pub capabilities: AcpCapabilities,
}

pub fn parse_initialize_result(result: &Value) -> Result<InitializeResult, ShapeError> {
    let protocol_version = result
        .get("protocolVersion")
        .and_then(Value::as_i64)
        .ok_or_else(|| ShapeError("initialize result is missing protocolVersion".into()))?;
    if protocol_version != ACP_PROTOCOL_VERSION {
        return Err(ShapeError(format!(
            "initialize returned unsupported protocolVersion {protocol_version}"
        )));
    }
    let capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
    let flag = |key: &str| {
        capabilities
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    Ok(InitializeResult {
        protocol_version,
        capabilities: AcpCapabilities {
            models: flag("models"),
            cancel: flag("cancel"),
            permission: flag("permission"),
        },
    })
}

/// The build composition requires respondable permissions and cancellation.
/// A server that does not advertise them cannot host S04.A build tasks.
pub fn require_build_capabilities(capabilities: &AcpCapabilities) -> Result<(), ShapeError> {
    if !capabilities.permission {
        return Err(ShapeError(
            "dsh acp server does not advertise the permission capability".into(),
        ));
    }
    if !capabilities.cancel {
        return Err(ShapeError(
            "dsh acp server does not advertise the cancel capability".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNewResult {
    pub session_id: String,
    /// Advertised standard config options (`configOptions`), kept opaque.
    pub config_options: Option<Value>,
}

pub fn parse_session_new_result(result: &Value) -> Result<SessionNewResult, ShapeError> {
    let session_id = opaque_id(
        result.get("sessionId").or_else(|| {
            result
                .get("session")
                .and_then(|session| session.get("sessionId"))
        }),
        "session/new sessionId",
        MAX_SESSION_ID_BYTES,
    )?;
    let config_options = result.get("configOptions").cloned();
    Ok(SessionNewResult {
        session_id,
        config_options,
    })
}

/// Settlement of one `session/prompt` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSettlement {
    pub stop_reason: String,
    pub message_id: Option<String>,
}

pub fn parse_prompt_settlement(result: &Value) -> Result<PromptSettlement, ShapeError> {
    let stop_reason = opaque_id(
        result.get("stopReason"),
        "session/prompt stopReason",
        MAX_STOP_REASON_BYTES,
    )?;
    let message_id = match opaque_id(
        result.get("messageId"),
        "session/prompt messageId",
        MAX_SESSION_ID_BYTES,
    ) {
        Ok(id) => Some(id),
        Err(_) => None,
    };
    Ok(PromptSettlement {
        stop_reason,
        message_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_params_pin_the_client_identity() {
        let params = initialize_params();
        assert_eq!(params["protocolVersion"], ACP_PROTOCOL_VERSION);
        assert_eq!(params["clientInfo"]["name"], ACP_CLIENT_NAME);
    }

    #[test]
    fn initialize_result_requires_protocol_version_one() {
        let result = parse_initialize_result(&json!({
            "protocolVersion": 1,
            "capabilities": {"models": true, "cancel": true, "permission": true}
        }))
        .unwrap();
        assert_eq!(
            result.capabilities,
            AcpCapabilities {
                models: true,
                cancel: true,
                permission: true
            }
        );
        assert!(parse_initialize_result(&json!({"protocolVersion": 2})).is_err());
        assert!(parse_initialize_result(&json!({"capabilities": {}})).is_err());
        assert!(require_build_capabilities(&result.capabilities).is_ok());
        assert!(require_build_capabilities(&AcpCapabilities {
            models: true,
            cancel: false,
            permission: true
        })
        .is_err());
    }

    #[test]
    fn session_new_result_bounds_the_session_id() {
        let result = parse_session_new_result(&json!({"sessionId": "fixture-session"})).unwrap();
        assert_eq!(result.session_id, "fixture-session");
        assert!(result.config_options.is_none());
        let oversized = json!({"sessionId": "x".repeat(MAX_SESSION_ID_BYTES + 1)});
        assert!(parse_session_new_result(&oversized).is_err());
        for invalid in [json!({}), json!({"sessionId": ""}), json!({"sessionId": 7})] {
            assert!(parse_session_new_result(&invalid).is_err());
        }
        let with_options = parse_session_new_result(&json!({
            "sessionId": "s",
            "configOptions": [{"configId": "model"}]
        }))
        .unwrap();
        assert!(with_options.config_options.is_some());
    }

    #[test]
    fn prompt_settlement_keeps_stop_reason_and_optional_message_id() {
        let settlement = parse_prompt_settlement(&json!({
            "stopReason": "end_turn",
            "messageId": "message-1"
        }))
        .unwrap();
        assert_eq!(settlement.stop_reason, "end_turn");
        assert_eq!(settlement.message_id.as_deref(), Some("message-1"));
        let without_message =
            parse_prompt_settlement(&json!({"stopReason": "max_tokens"})).unwrap();
        assert_eq!(without_message.message_id, None);
        assert!(parse_prompt_settlement(&json!({})).is_err());
        assert!(parse_prompt_settlement(
            &json!({"stopReason": "x".repeat(MAX_STOP_REASON_BYTES + 1)})
        )
        .is_err());
    }
}
