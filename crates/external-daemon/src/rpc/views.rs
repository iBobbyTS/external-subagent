//! Pure response projections shared by the RPC service branches.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use super::errors::{RpcError, RpcErrorCode};
use super::types::{MAX_REQUEST_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES, MAX_WAIT};
use crate::{
    agent_status::ProbeScope,
    observation::{ObservationCoverage, ObservedReasoning, ObservedTool},
    MessageDisposition, PassiveActivitySnapshot, ResponseDisposition,
};
use external_store::{StoredPendingRequest, StoredTaskResult, TaskOutcome, TaskPhase, TaskRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReceiptView {
    pub message_id: String,
    pub state: String,
    pub failure_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ComponentStateView {
    Ready,
    Degraded,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMaturityView {
    BetaReady,
    ExperimentalUnverifiedRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemStatusView {
    /// Diagnostic-only MCP tool surface version. Never used for admission.
    pub mcp_version: String,
    pub service_generation: String,
    pub components: BTreeMap<String, ComponentStateView>,
    pub capabilities: AgentCapabilitiesView,
    #[serde(default)]
    pub agents: Vec<AgentStatusView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<DaemonIdentityView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatusView {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_version: Option<String>,
    pub config_revision: u64,
    pub configured: bool,
    pub enabled: bool,
    pub spawn_supported: bool,
    pub transport_support: AgentTransportSupportView,
    pub permission_modes: Vec<AgentPermissionModeView>,
    pub model_selection: AgentModelSelectionCapabilityView,
    pub effort_selection: AgentEffortSelectionCapabilityView,
    pub local: AgentScopeStatusView,
    pub auth: AgentScopeStatusView,
    pub hi: AgentScopeStatusView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTransportView {
    ZcodeAppServer,
    DshAcp,
    CodexAppServer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTransportSupportView {
    pub transport: AgentTransportView,
    pub probe: bool,
    pub spawn: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPermissionModeView {
    Build,
    Edit,
    Plan,
    Yolo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentModelSelectionModeView {
    NativeOnly,
    CatalogToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModelSelectionCapabilityView {
    pub supported: bool,
    pub mode: AgentModelSelectionModeView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEffortSelectionModeView {
    ClosedSet,
    PassthroughToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEffortSelectionCapabilityView {
    pub supported: bool,
    pub mode: AgentEffortSelectionModeView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScopeStatusView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_path: Option<String>,
    pub state: ComponentStateView,
    pub scope: ProbeScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub checked_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonIdentityView {
    pub daemon: ComponentIdentityView,
    pub models: ModelIdentityView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentIdentityView {
    pub component: String,
    pub version: String,
    pub artifact: ArtifactIdentityView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactIdentityView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentityView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured: Option<ModelIdentityFactView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentityFactView {
    pub value: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilitiesView {
    pub max_rpc_request_frame_bytes: usize,
    pub max_rpc_response_frame_bytes: usize,
    pub max_wait_ms: u64,
    pub maturity: BTreeMap<String, CapabilityMaturityView>,
    pub observation: ObservationCapabilityView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationCapabilityView {
    pub public_reasoning_default: bool,
    pub defaults: ObservationDefaultsView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationDefaultsView {
    pub top_tools: usize,
    pub recent_calls_per_tool: usize,
    pub reasoning_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskObservationView {
    pub schema: String,
    pub agent_id: String,
    pub count_scope: String,
    pub tools: Vec<ObservedTool>,
    pub reasoning: Option<ObservedReasoning>,
    pub coverage: ObservationCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub agent_id: String,
    /// Single lifecycle status: queued | preparing | running | waiting_input |
    /// cancelling | completed | failed | cancelled | timed_out | runtime_lost |
    /// result_invalid | closed. `closed` subsumes the terminal outcome; the
    /// stored result keeps it queryable through task_result.
    pub status: String,
    pub session_id: Option<String>,
    pub input_identity: InputIdentityView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputIdentityView {
    #[serde(default)]
    pub subagent: Option<String>,
    #[serde(default)]
    pub config_revision: Option<u64>,
    #[serde(default)]
    pub adapter_version: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_source: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub workspace_path: Option<String>,
    pub permission_mode: Option<String>,
}

#[cfg(test)]
pub(crate) fn flat_identity(view: &InputIdentityView) -> Option<external_core::AdmissionIdentity> {
    Some(external_core::AdmissionIdentity {
        agent: view.subagent.clone()?,
        config_revision: view.config_revision?,
        adapter_version: view.adapter_version.clone()?,
        model: view.model.clone(),
        model_source: view.model_source.clone()?,
        effort: view.effort.clone(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryStatusView {
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskActivityView {
    pub latest_text_tail: String,
    pub latest_text_truncated: bool,
    /// Verified-public reasoning tail (bounded to 200 Unicode characters);
    /// empty when the runtime source is not verified.
    pub latest_reasoning: String,
    /// Tool calls started in the last 60 seconds, across all tools.
    pub tool_calls_last_60s: u64,
    pub telemetry_status: TelemetryStatusView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResultView {
    pub outcome: TaskOutcome,
    pub final_text: String,
    pub partial: bool,
    pub offset: usize,
    pub total_bytes: usize,
    pub next_offset: Option<usize>,
    pub complete: bool,
}

/// Embedded view of an answerable user-input question. wait is the only
/// retrieval surface; `truncated` marks questions beyond the embed cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionView {
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDispositionView {
    Queued,
    Delivered,
    AlreadyDelivered,
    Failed,
}

impl From<MessageDisposition> for MessageDispositionView {
    fn from(value: MessageDisposition) -> Self {
        match value {
            MessageDisposition::Queued => Self::Queued,
            MessageDisposition::Delivered => Self::Delivered,
            MessageDisposition::AlreadyDelivered => Self::AlreadyDelivered,
            MessageDisposition::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseDispositionView {
    Responded,
    AlreadyResponded,
    InFlight,
}

impl From<ResponseDisposition> for ResponseDispositionView {
    fn from(value: ResponseDisposition) -> Self {
        match value {
            ResponseDisposition::Responded => Self::Responded,
            ResponseDisposition::AlreadyResponded => Self::AlreadyResponded,
            ResponseDisposition::InFlight => Self::InFlight,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseOutcomeView {
    pub disposition: ResponseDispositionView,
    pub requested_decision: String,
    pub effective_decision: String,
    pub policy_overrode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_reason_code: Option<String>,
}

impl From<crate::ResponseOutcome> for ResponseOutcomeView {
    fn from(value: crate::ResponseOutcome) -> Self {
        Self {
            disposition: value.disposition.into(),
            requested_decision: value.requested_decision,
            effective_decision: value.effective_decision,
            policy_overrode: value.policy_overrode,
            policy_reason_code: value.policy_reason_code,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequestView {
    pub request_id: String,
    pub kind: String,
    pub tool_name: Option<String>,
    pub operation: String,
    pub summary: String,
    /// Full embedded question for user_input requests (the only retrieval
    /// surface); truncated marks questions beyond the embed cap.
    pub question: Option<QuestionView>,
}

/// Bounded question exposure for answerable user-input requests. The summary
/// prefix keeps even a worst-case 100-request wait projection inside the
/// response frame cap, while the embedded question carries a generous page —
/// wait is its only retrieval surface, so a cut is marked truncated instead
/// of silently shortened.
pub(super) const MAX_QUESTION_SUMMARY_BYTES: usize = 2048;
pub(super) const MAX_QUESTION_EMBED_BYTES: usize = 16 * 1024;

fn user_input_question(payload_json: &str) -> Option<String> {
    serde_json::from_str::<Value>(payload_json)
        .ok()
        .and_then(|params| {
            params
                .get("question")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn sanitized_user_input_summary(payload_json: &str) -> String {
    user_input_question(payload_json)
        .map(|question| {
            let total = question.chars().count();
            let prefix = truncate_at_char_boundary(&question, MAX_QUESTION_SUMMARY_BYTES);
            let shown = prefix.chars().count();
            if shown < total {
                format!("question {prefix} [+{} more chars]", total - shown)
            } else {
                format!("question {question}")
            }
        })
        .unwrap_or_else(|| "user input request".into())
}

fn user_input_question_page(payload_json: &str) -> Option<QuestionView> {
    let question = user_input_question(payload_json)?;
    // wait is the only retrieval surface for questions, so embed generously;
    // a question beyond the cap is honestly marked truncated.
    let text = truncate_at_char_boundary(&question, MAX_QUESTION_EMBED_BYTES);
    Some(QuestionView {
        truncated: text.len() < question.len(),
        text,
    })
}

pub(super) fn truncate_at_char_boundary(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

pub(super) fn result_page_bounds(
    text: &str,
    offset: usize,
    limit: usize,
) -> Result<(usize, Option<usize>), RpcError> {
    paged_text_bounds(text, offset, limit, "result")
}

fn paged_text_bounds(
    text: &str,
    offset: usize,
    limit: usize,
    field: &str,
) -> Result<(usize, Option<usize>), RpcError> {
    let total_bytes = text.len();
    if offset > total_bytes || !text.is_char_boundary(offset) {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            format!("{field} offset is outside the {field}"),
        ));
    }
    let mut end = offset.saturating_add(limit).min(total_bytes);
    while end > offset && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && offset < total_bytes {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            format!("{field} limit does not include a complete UTF-8 character"),
        ));
    }
    let next_offset = (end < total_bytes).then_some(end);
    debug_assert!(next_offset.is_none_or(|next| next > offset));
    Ok((end, next_offset))
}

pub(super) fn agent_capabilities() -> AgentCapabilitiesView {
    let maturity = BTreeMap::new();
    AgentCapabilitiesView {
        max_rpc_request_frame_bytes: MAX_REQUEST_FRAME_BYTES,
        max_rpc_response_frame_bytes: MAX_RESPONSE_FRAME_BYTES,
        max_wait_ms: MAX_WAIT.as_millis() as u64,
        maturity,
        observation: ObservationCapabilityView {
            public_reasoning_default: true,
            defaults: ObservationDefaultsView {
                top_tools: 3,
                recent_calls_per_tool: 5,
                reasoning_chars: 200,
            },
        },
    }
}

pub(super) fn task_view(task: TaskRecord) -> TaskView {
    let prepared = serde_json::from_str::<serde_json::Value>(&task.prepared_launch_json).ok();
    let permission_mode = prepared.as_ref().and_then(|v| {
        v.get("permission_mode")
            .and_then(|x| x.as_str())
            .map(str::to_owned)
    });
    let workspace_path = Some(task.workspace_path.clone());
    let admission: Option<external_core::AdmissionIdentity> = prepared
        .as_ref()
        .and_then(|v| v.get("admission"))
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let status = if task.closed_at.is_some() {
        "closed".to_owned()
    } else {
        match task.phase {
            TaskPhase::Queued => "queued".to_owned(),
            TaskPhase::Preparing => "preparing".to_owned(),
            TaskPhase::Running => "running".to_owned(),
            TaskPhase::WaitingInput => "waiting_input".to_owned(),
            TaskPhase::Cancelling => "cancelling".to_owned(),
            TaskPhase::Terminal => match task.outcome {
                Some(TaskOutcome::Completed) => "completed".to_owned(),
                Some(TaskOutcome::Failed) => "failed".to_owned(),
                Some(TaskOutcome::Cancelled) => "cancelled".to_owned(),
                Some(TaskOutcome::TimedOut) => "timed_out".to_owned(),
                Some(TaskOutcome::RuntimeLost) => "runtime_lost".to_owned(),
                Some(TaskOutcome::ResultInvalid) => "result_invalid".to_owned(),
                None => "terminal".to_owned(),
            },
        }
    };
    TaskView {
        agent_id: task.agent_id,
        status,
        session_id: task.session_id,
        input_identity: InputIdentityView {
            subagent: admission.as_ref().map(|identity| identity.agent.clone()),
            config_revision: admission.as_ref().map(|identity| identity.config_revision),
            adapter_version: admission
                .as_ref()
                .map(|identity| identity.adapter_version.clone()),
            model: admission
                .as_ref()
                .and_then(|identity| identity.model.clone()),
            model_source: admission
                .as_ref()
                .map(|identity| identity.model_source.clone()),
            effort: admission
                .as_ref()
                .and_then(|identity| identity.effort.clone()),
            workspace_path,
            permission_mode,
        },
    }
}

pub(super) fn task_activity_view(
    _phase: TaskPhase,
    snapshot: Option<PassiveActivitySnapshot>,
) -> TaskActivityView {
    let Some(snapshot) = snapshot else {
        return TaskActivityView {
            latest_text_tail: String::new(),
            latest_text_truncated: false,
            latest_reasoning: String::new(),
            tool_calls_last_60s: 0,
            telemetry_status: TelemetryStatusView::Unavailable,
        };
    };
    TaskActivityView {
        latest_text_tail: snapshot.latest_text_tail,
        latest_text_truncated: snapshot.latest_text_truncated,
        latest_reasoning: snapshot.latest_reasoning,
        tool_calls_last_60s: snapshot.window_60s.tool_calls_started,
        telemetry_status: if snapshot.telemetry_degraded {
            TelemetryStatusView::Degraded
        } else {
            TelemetryStatusView::Healthy
        },
    }
}

#[cfg(test)]
mod activity_projection_tests {
    use super::{agent_capabilities, task_activity_view, TelemetryStatusView};
    use crate::{PassiveActivitySnapshot, PassiveActivityWindow};
    use external_store::TaskPhase;

    fn snapshot() -> PassiveActivitySnapshot {
        PassiveActivitySnapshot {
            revision: 7,
            last_runtime_event_at: Some(1_000),
            last_activity_age_ms: Some(250),
            model_request_active: true,
            model_request_age_ms: Some(900),
            model_last_delta_age_ms: Some(300),
            latest_text_tail: "preserved tail".into(),
            latest_text_updated_at: Some(950),
            latest_text_truncated: true,
            latest_reasoning: "recent reasoning".into(),
            active_tools: Vec::new(),
            oldest_active_tool_age_ms: None,
            window_60s: PassiveActivityWindow {
                tool_calls_started: 4,
                ..PassiveActivityWindow::default()
            },
            telemetry_degraded: true,
        }
    }

    #[test]
    fn activity_projection_is_the_slim_liveness_surface() {
        let activity = task_activity_view(TaskPhase::Running, Some(snapshot()));
        assert_eq!(activity.latest_text_tail, "preserved tail");
        assert!(activity.latest_text_truncated);
        assert_eq!(activity.latest_reasoning, "recent reasoning");
        assert_eq!(activity.tool_calls_last_60s, 4);
        assert_eq!(activity.telemetry_status, TelemetryStatusView::Degraded);
        // The dropped signals never leak into the serialized projection.
        let encoded = serde_json::to_value(&activity).unwrap();
        for gone in [
            "state",
            "model_request_active",
            "model_request_age_ms",
            "active_tools",
            "window_60s",
            "latest_progress",
            "last_activity_age_ms",
        ] {
            assert_eq!(encoded.get(gone), None, "{gone} must not leak");
        }
    }

    #[test]
    fn missing_runtime_snapshot_reports_unavailable_telemetry() {
        let activity = task_activity_view(TaskPhase::Queued, None);
        assert_eq!(activity.latest_text_tail, "");
        assert!(!activity.latest_text_truncated);
        assert_eq!(activity.latest_reasoning, "");
        assert_eq!(activity.tool_calls_last_60s, 0);
        assert_eq!(activity.telemetry_status, TelemetryStatusView::Unavailable);
    }

    #[test]
    fn status_reports_the_observation_contract_defaults() {
        let observation = agent_capabilities().observation;
        assert!(observation.public_reasoning_default);
        assert_eq!(observation.defaults.top_tools, 3);
        assert_eq!(observation.defaults.recent_calls_per_tool, 5);
        assert_eq!(observation.defaults.reasoning_chars, 200);
    }
}

impl From<StoredTaskResult> for TaskResultView {
    fn from(stored: StoredTaskResult) -> Self {
        let total_bytes = stored.result.final_text.len();
        Self {
            outcome: stored.result.outcome,
            final_text: stored.result.final_text,
            partial: stored.result.partial,
            offset: 0,
            total_bytes,
            next_offset: None,
            complete: true,
        }
    }
}

pub(super) fn pending_request_view(request: StoredPendingRequest) -> PendingRequestView {
    if request.request_type == "user_input" {
        return PendingRequestView {
            request_id: request.request_id,
            kind: "user_input".into(),
            tool_name: None,
            operation: "user_input".into(),
            summary: sanitized_user_input_summary(&request.payload_json),
            question: user_input_question_page(&request.payload_json),
        };
    }
    let params = serde_json::from_str::<Value>(&request.payload_json).ok();
    let tool_name = params
        .as_ref()
        .and_then(|value| value.get("toolName"))
        .and_then(Value::as_str)
        .map(|value| value.chars().take(64).collect::<String>());
    let operation = tool_name
        .as_deref()
        .map(operation_category)
        .unwrap_or("unknown")
        .to_owned();
    let summary = params
        .as_ref()
        .map(sanitized_permission_summary)
        .unwrap_or_else(|| "unrecognized permission request".into());
    PendingRequestView {
        request_id: request.request_id,
        kind: "permission".into(),
        tool_name,
        operation,
        summary,
        question: None,
    }
}

#[cfg(test)]
mod result_paging_tests {
    use super::{result_page_bounds, InputIdentityView, TaskResultView, TaskView};
    use crate::rpc::{RpcResponse, RpcSuccess, MAX_RESPONSE_FRAME_BYTES, MAX_RESULT_CHUNK_BYTES};
    use external_store::TaskOutcome;

    fn task() -> TaskView {
        TaskView {
            agent_id: "a".repeat(256),
            status: "completed".into(),
            session_id: None,
            input_identity: InputIdentityView {
                subagent: None,
                config_revision: None,
                adapter_version: None,
                model: None,
                model_source: None,
                effort: None,
                workspace_path: None,
                permission_mode: None,
            },
        }
    }

    #[test]
    fn non_terminal_pages_always_advance() {
        assert_eq!(result_page_bounds("abcdef", 0, 3).unwrap(), (3, Some(3)));
        assert_eq!(result_page_bounds("abcdef", 3, 3).unwrap(), (6, None));
    }

    #[test]
    fn too_small_utf8_page_is_rejected_instead_of_stalling() {
        let error = result_page_bounds("你a", 0, 1).unwrap_err();
        assert_eq!(error.code, super::RpcErrorCode::Validation);
        assert_eq!(result_page_bounds("你a", 0, 3).unwrap(), (3, Some(3)));
        assert!(result_page_bounds("你a", 1, 3).is_err());
    }

    #[test]
    fn worst_case_encoded_result_response_and_newline_fit_the_frame() {
        let text = "\u{0}".repeat(MAX_RESULT_CHUNK_BYTES);
        let response = RpcResponse::success(
            "q".repeat(128),
            RpcSuccess::TaskResult {
                task: task(),
                result: Some(TaskResultView {
                    outcome: TaskOutcome::Completed,
                    final_text: text,
                    partial: false,
                    offset: 0,
                    total_bytes: MAX_RESULT_CHUNK_BYTES + 1,
                    next_offset: Some(MAX_RESULT_CHUNK_BYTES),
                    complete: false,
                }),
            },
        );
        assert!(serde_json::to_vec(&response).unwrap().len() + 1 <= MAX_RESPONSE_FRAME_BYTES);
    }

    #[test]
    fn transport_projection_does_not_change_the_stored_outcome() {
        let view = TaskResultView {
            outcome: TaskOutcome::Failed,
            final_text: "failure".into(),
            partial: true,
            offset: 0,
            total_bytes: 7,
            next_offset: None,
            complete: true,
        };
        assert_eq!(view.outcome, TaskOutcome::Failed);
        assert!(view.partial);
    }
}

pub fn running_component_identity(component: &str, version: &str) -> ComponentIdentityView {
    running_component_identity_from(component, version, env::current_exe().ok())
}

fn running_component_identity_from(
    component: &str,
    version: &str,
    executable: Option<PathBuf>,
) -> ComponentIdentityView {
    let (path, sha256) = executable.map_or((None, None), |path| {
        let hash = File::open(&path).ok().and_then(|mut file| {
            let mut hasher = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer).ok()?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            Some(format!("{:x}", hasher.finalize()))
        });
        (Some(path.to_string_lossy().into_owned()), hash)
    });
    ComponentIdentityView {
        component: component.into(),
        version: version.into(),
        artifact: ArtifactIdentityView { path, sha256 },
    }
}

fn operation_category(tool_name: &str) -> &'static str {
    match tool_name.to_ascii_lowercase().as_str() {
        "read" | "grep" | "glob" => "read",
        "write" | "edit" | "delete" | "move" => "write",
        "bash" | "execute" | "terminal" => "command",
        "network" => "network",
        "git_ref_mutation" => "git_ref_mutation",
        _ => "unknown",
    }
}

fn sanitized_permission_summary(params: &Value) -> String {
    let input = params.get("input").unwrap_or(&Value::Null);
    let leaf = |name: &str| {
        input
            .get(name)
            .and_then(Value::as_str)
            .and_then(|value| std::path::Path::new(value).file_name())
            .map(|value| value.to_string_lossy().chars().take(96).collect::<String>())
    };
    if let Some(program) = leaf("program") {
        let count = input
            .get("args")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        return format!("command {program} with {count} arguments");
    }
    if let Some(path) = leaf("path")
        .or_else(|| leaf("destination"))
        .or_else(|| leaf("source"))
    {
        return format!("target {path}");
    }
    if params
        .get("toolName")
        .and_then(Value::as_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("git_ref_mutation"))
    {
        return "Git reference mutation".into();
    }
    "permission request".into()
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::rpc::{RpcOutcome, RpcResponse, RpcSuccess, MCP_VERSION};
    use std::io::Write;

    #[test]
    fn missing_executable_fact_leaves_artifact_unknown() {
        let identity = running_component_identity_from("daemon", "1.2.3", None);
        assert_eq!(identity.component, "daemon");
        assert_eq!(identity.version, "1.2.3");
        assert_eq!(identity.artifact.path, None);
        assert_eq!(identity.artifact.sha256, None);
    }

    #[test]
    fn artifact_hash_is_bound_to_the_reported_running_path() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("old-running-daemon");
        let disk_payload = directory.path().join("new-distributed-daemon");
        File::create(&executable)
            .unwrap()
            .write_all(b"old")
            .unwrap();
        File::create(&disk_payload)
            .unwrap()
            .write_all(b"new")
            .unwrap();
        let identity = running_component_identity_from("daemon", "1.2.3", Some(executable.clone()));
        assert_eq!(identity.artifact.path.as_deref(), executable.to_str());
        assert_eq!(
            identity.artifact.sha256.as_deref(),
            Some("cba06b5736faf67e54b07b561eae94395e774c517a7d910a54369e1263ccfbd4")
        );
        assert_ne!(identity.artifact.path.as_deref(), disk_payload.to_str());
    }

    #[test]
    fn status_identity_reports_only_daemon_and_configured_models() {
        let identity = DaemonIdentityView {
            daemon: running_component_identity_from("daemon", "0.1.0", None),
            models: ModelIdentityView {
                configured: Some(ModelIdentityFactView {
                    value: "configured-model".into(),
                    source: "session_create_configuration".into(),
                }),
            },
        };
        let serialized = serde_json::to_value(&identity).unwrap();
        assert!(serialized.get("runtime").is_none());
        assert!(serialized["models"].get("observed_response").is_none());
        assert_eq!(
            serialized["models"]["configured"]["value"],
            "configured-model"
        );
        assert!(serialized["daemon"].get("source_revision").is_none());
        assert!(serialized["daemon"]["artifact"]
            .get("captured_at_ms")
            .is_none());
    }

    #[test]
    fn legacy_status_frame_without_identity_still_decodes() {
        let response = RpcResponse::success(
            "legacy-status".into(),
            RpcSuccess::SystemStatus {
                status: SystemStatusView {
                    mcp_version: MCP_VERSION.into(),
                    service_generation: "legacy-generation".into(),
                    components: BTreeMap::from([("daemon".into(), ComponentStateView::Ready)]),
                    capabilities: agent_capabilities(),
                    agents: Vec::new(),
                    identity: Some(DaemonIdentityView {
                        daemon: running_component_identity_from("daemon", "0.1.0", None),
                        models: ModelIdentityView { configured: None },
                    }),
                },
            },
        );
        let mut legacy = serde_json::to_value(response).unwrap();
        legacy["result"]["status"]
            .as_object_mut()
            .unwrap()
            .remove("identity");
        let decoded: RpcResponse = serde_json::from_value(legacy).unwrap();
        let RpcOutcome::Success { result } = decoded.outcome else {
            panic!("expected success")
        };
        let RpcSuccess::SystemStatus { status } = *result else {
            panic!("expected status")
        };
        assert_eq!(status.service_generation, "legacy-generation");
        assert_eq!(status.components["daemon"], ComponentStateView::Ready);
        assert_eq!(status.identity, None);
    }
}
