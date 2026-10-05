//! RPC error taxonomy and scheduler/store error mapping.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use crate::SchedulerError;
use external_store::StoreError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcErrorCode {
    Malformed,
    Oversized,
    UnknownMethod,
    Validation,
    AgentRequired,
    AgentUnknown,
    AgentDisabled,
    AgentUnsupported,
    ModelSelectionUnsupported,
    SteerUnsupported,
    NotFound,
    Conflict,
    Persistence,
    Timeout,
    RuntimeLost,
    ResultInvalid,
    Unavailable,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_agent_id: Option<String>,
}

pub(crate) fn truncate_utf8(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut boundary = max_bytes;
        while !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        s.truncate(boundary);
    }
}

impl RpcError {
    pub fn new(code: RpcErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        truncate_utf8(&mut message, 512);
        Self {
            code,
            message,
            active_agent_id: None,
        }
    }

    pub fn new_profile_error(code: RpcErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        crate::rpc::profiles::truncate_json_escaped(
            &mut message,
            crate::rpc::types::MAX_RESPONSE_FRAME_BYTES - 8192,
        );
        Self {
            code,
            message,
            active_agent_id: None,
        }
    }
}

pub(super) fn map_scheduler(error: SchedulerError) -> RpcError {
    match error {
        SchedulerError::Store(error) => map_store(error),
        SchedulerError::InvalidConfig(message) => {
            if message == "daemon_draining" {
                return RpcError::new(RpcErrorCode::Unavailable, "daemon_draining");
            }
            if message == "drain_not_active" || message == "drain_cancel_in_progress" {
                return RpcError::new(RpcErrorCode::Unavailable, message.clone());
            }
            // Preserve the bounded, actionable preparation reason. The MCP
            // facade may still redact it for callers, but RPC diagnostics
            // must distinguish repository, path, budget, and state errors.
            RpcError::new(
                RpcErrorCode::Validation,
                format!("scheduler rejected the operation: {message}"),
            )
        }
        SchedulerError::RuntimeSpawn { agent_id, .. } => {
            let mut error = RpcError::new(RpcErrorCode::RuntimeLost, "runtime operation failed");
            error.active_agent_id = Some(agent_id);
            error
        }
        SchedulerError::LifecycleSink { .. } => {
            RpcError::new(RpcErrorCode::RuntimeLost, "runtime operation failed")
        }
        SchedulerError::RuntimeCommand { .. } => {
            let message = match &error {
                SchedulerError::RuntimeCommand { message, .. } => message.as_str(),
                _ => unreachable!(),
            };
            if message == "steer_unsupported" {
                RpcError::new(RpcErrorCode::SteerUnsupported, message)
            } else if message == "TERMINAL_SEND_UNSUPPORTED" {
                RpcError::new(RpcErrorCode::Validation, message)
            } else if message == "daemon_draining" {
                RpcError::new(RpcErrorCode::Unavailable, message)
            } else {
                RpcError::new(RpcErrorCode::Unavailable, "RUNTIME_COMMAND_FAILED")
            }
        }
        SchedulerError::StartTimeout { agent_id, message } => {
            let mut error = RpcError::new(RpcErrorCode::Timeout, message);
            error.active_agent_id = Some(agent_id);
            error
        }
        SchedulerError::Interrupted { agent_id } => {
            let mut error = RpcError::new(
                RpcErrorCode::Timeout,
                format!("spawn interrupted while session establishment continues for {agent_id}"),
            );
            error.active_agent_id = Some(agent_id);
            error
        }
        SchedulerError::StartFailed {
            agent_id,
            reason,
            message,
        } => {
            let (code, msg) = if reason == "MODEL_REJECTED" {
                (RpcErrorCode::Validation, format!("MODEL_REJECTED: {message}"))
            } else if reason.starts_with("PREPARED_") || reason.starts_with("TASK_ROUTE_") {
                (RpcErrorCode::ResultInvalid, format!("{reason}: {message}"))
            } else if reason == "DRAIN_CANCELLED" || reason == "CANCELLED" {
                (RpcErrorCode::Unavailable, format!("task cancelled: {message}"))
            } else {
                (RpcErrorCode::RuntimeLost, format!("{reason}: {message}"))
            };
            let mut error = RpcError::new(code, msg);
            error.active_agent_id = Some(agent_id);
            error
        }
    }
}

pub(super) fn map_store(error: StoreError) -> RpcError {
    match error {
        StoreError::LegacySchemaUnsupported => RpcError::new(
            RpcErrorCode::Persistence,
            "STORE_SCHEMA_VERSION_UNSUPPORTED",
        ),
        StoreError::Sqlite(_) => {
            RpcError::new(RpcErrorCode::Persistence, "durable store operation failed")
        }
        StoreError::Conflict(message) if message.starts_with("WORKSPACE_BUSY") => {
            let active_agent_id = message
                .strip_prefix("WORKSPACE_BUSY active_agent_id=")
                .map(str::to_owned);
            let mut error = RpcError::new(RpcErrorCode::Conflict, "WORKSPACE_BUSY");
            error.active_agent_id = active_agent_id;
            error
        }
        StoreError::Conflict(message)
            if message == "message id is already bound to different content"
                || message == "MESSAGE_ID_CONFLICT"
                || (message.starts_with("message ") && message.ends_with(" already exists")) =>
        {
            RpcError::new(RpcErrorCode::Conflict, "MESSAGE_ID_CONFLICT")
        }
        StoreError::Conflict(_) => RpcError::new(RpcErrorCode::Conflict, "durable state conflict"),
        StoreError::InvalidState(_) => RpcError::new(
            RpcErrorCode::Validation,
            "durable state rejected the operation",
        ),
    }
}

#[cfg(test)]
mod error_classification_tests {
    use super::*;
    #[test]
    fn preserves_safe_reasons_without_exposing_store_or_runtime_details() {
        let conflict = map_store(StoreError::Conflict(
            "message id is already bound to different content".into(),
        ));
        assert_eq!(conflict.code, RpcErrorCode::Conflict);
        assert_eq!(conflict.message, "MESSAGE_ID_CONFLICT");
        let rejection = map_scheduler(SchedulerError::RuntimeCommand {
            agent_id: "a".into(),
            message: "sensitive".into(),
        });
        assert_eq!(rejection.code, RpcErrorCode::Unavailable);
        assert_eq!(rejection.message, "RUNTIME_COMMAND_FAILED");
    }

    #[test]
    fn map_scheduler_preserves_start_failed_reasons_and_active_agent_id() {
        // 1. SESSION_START_FAILED -> RuntimeLost
        let err_session = map_scheduler(SchedulerError::StartFailed {
            agent_id: "10000001".into(),
            reason: "SESSION_START_FAILED".into(),
            message: "handshake failed".into(),
        });
        assert_eq!(err_session.code, RpcErrorCode::RuntimeLost);
        assert_eq!(err_session.message, "SESSION_START_FAILED: handshake failed");
        assert_eq!(err_session.active_agent_id.as_deref(), Some("10000001"));

        // 2. MODEL_REJECTED -> Validation
        let err_model = map_scheduler(SchedulerError::StartFailed {
            agent_id: "10000002".into(),
            reason: "MODEL_REJECTED".into(),
            message: "unknown model".into(),
        });
        assert_eq!(err_model.code, RpcErrorCode::Validation);
        assert_eq!(err_model.message, "MODEL_REJECTED: unknown model");
        assert_eq!(err_model.active_agent_id.as_deref(), Some("10000002"));

        // 3. PREPARED_* -> ResultInvalid
        let err_prepared = map_scheduler(SchedulerError::StartFailed {
            agent_id: "10000003".into(),
            reason: "PREPARED_LAUNCH_FAILED".into(),
            message: "invalid json".into(),
        });
        assert_eq!(err_prepared.code, RpcErrorCode::ResultInvalid);
        assert_eq!(err_prepared.message, "PREPARED_LAUNCH_FAILED: invalid json");
        assert_eq!(err_prepared.active_agent_id.as_deref(), Some("10000003"));

        // 4. DRAIN_CANCELLED / CANCELLED -> Unavailable
        let err_drain = map_scheduler(SchedulerError::StartFailed {
            agent_id: "10000004".into(),
            reason: "DRAIN_CANCELLED".into(),
            message: "daemon draining".into(),
        });
        assert_eq!(err_drain.code, RpcErrorCode::Unavailable);
        assert_eq!(err_drain.message, "task cancelled: daemon draining");
        assert_eq!(err_drain.active_agent_id.as_deref(), Some("10000004"));
    }

    #[test]
    fn map_scheduler_timeout_and_interrupted_variants_preserve_active_agent_id() {
        let err_timeout = map_scheduler(SchedulerError::StartTimeout {
            agent_id: "10000005".into(),
            message: "timed out waiting for session".into(),
        });
        assert_eq!(err_timeout.code, RpcErrorCode::Timeout);
        assert_eq!(err_timeout.message, "timed out waiting for session");
        assert_eq!(err_timeout.active_agent_id.as_deref(), Some("10000005"));

        let err_interrupted = map_scheduler(SchedulerError::Interrupted {
            agent_id: "10000006".into(),
        });
        assert_eq!(err_interrupted.code, RpcErrorCode::Timeout);
        assert_eq!(
            err_interrupted.message,
            "spawn interrupted while session establishment continues for 10000006"
        );
        assert_eq!(err_interrupted.active_agent_id.as_deref(), Some("10000006"));

        let err_runtime_spawn = map_scheduler(SchedulerError::RuntimeSpawn {
            agent_id: "10000007".into(),
            message: "thread spawn failed".into(),
        });
        assert_eq!(err_runtime_spawn.code, RpcErrorCode::RuntimeLost);
        assert_eq!(err_runtime_spawn.message, "runtime operation failed");
        assert_eq!(err_runtime_spawn.active_agent_id.as_deref(), Some("10000007"));
    }

    #[test]
    fn truncate_utf8_is_character_boundary_safe() {
        // ASCII truncates exactly at limit
        let mut ascii = "a".repeat(600);
        super::truncate_utf8(&mut ascii, 512);
        assert_eq!(ascii.len(), 512);

        // UTF-8 multi-byte characters: 3-byte '中'
        // 170 * 3 = 510 bytes, + 1 '中' = 513 bytes
        let mut chinese = "中".repeat(171); // 513 bytes
        super::truncate_utf8(&mut chinese, 512);
        assert_eq!(chinese.len(), 510); // Stepped back to 510 to avoid splitting '中'
        assert_eq!(chinese, "中".repeat(170));

        // RpcError::new does not panic on UTF-8 multi-byte boundary
        let err = RpcError::new(RpcErrorCode::Validation, "中".repeat(171));
        assert_eq!(err.message.len(), 510);

        // new_profile_error is exempt from 512-byte truncate
        let big_msg = "profile_name_".repeat(50); // 650 bytes
        let err_profile = RpcError::new_profile_error(RpcErrorCode::Validation, &big_msg);
        assert_eq!(err_profile.message, big_msg);

        // new_profile_error truncates when JSON-escaped length exceeds MAX_RESPONSE_FRAME_BYTES - 8192
        let huge_backslashes = "\\".repeat(crate::rpc::types::MAX_RESPONSE_FRAME_BYTES);
        let err_huge = RpcError::new_profile_error(RpcErrorCode::Validation, &huge_backslashes);
        let escaped_len = crate::rpc::profiles::json_escaped_byte_len(&err_huge.message);
        assert!(escaped_len <= crate::rpc::types::MAX_RESPONSE_FRAME_BYTES - 8192);
    }
}
