//! Read-only public projections of the daemon RPC views.
//!
//! Extracted mechanically from the former private `mcp::server` module; the
//! facade at `crate::mcp` keeps every historical path importable.
use super::errors::{protocol_error, ToolError};
use super::types::{
    public_task_id, PublicDecision, PublicPendingRequest, PublicResponseDisposition,
};
use crate::observation::OBSERVATION_SCHEMA;
use crate::rpc::{
    AgentCapabilitiesView, AgentEffortSelectionModeView, AgentModelSelectionModeView,
    AgentPermissionModeView, AgentScopeStatusView, AgentStatusView, CapabilityMaturityView,
    ComponentStateView, SystemStatusView, TaskActivityView, TaskObservationView, TaskResultView,
    TaskView, TelemetryStatusView,
};
use external_store::TaskOutcome;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PublicComponentState {
    Ready,
    Degraded,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicCapabilityMaturity {
    BetaReady,
    ExperimentalUnverifiedRuntime,
}

impl From<CapabilityMaturityView> for PublicCapabilityMaturity {
    fn from(value: CapabilityMaturityView) -> Self {
        match value {
            CapabilityMaturityView::BetaReady => Self::BetaReady,
            CapabilityMaturityView::ExperimentalUnverifiedRuntime => {
                Self::ExperimentalUnverifiedRuntime
            }
        }
    }
}

impl From<ComponentStateView> for PublicComponentState {
    fn from(value: ComponentStateView) -> Self {
        match value {
            ComponentStateView::Ready => Self::Ready,
            ComponentStateView::Degraded => Self::Degraded,
            ComponentStateView::Unavailable => Self::Unavailable,
            ComponentStateView::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicAgentCapabilities {
    pub max_rpc_request_frame_bytes: usize,
    pub max_rpc_response_frame_bytes: usize,
    pub max_wait_ms: u64,
    pub maturity: BTreeMap<String, PublicCapabilityMaturity>,
    pub observation: PublicObservationCapability,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicObservationCapability {
    pub public_reasoning_default: bool,
    pub defaults: PublicObservationDefaults,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicObservationDefaults {
    pub top_tools: usize,
    pub recent_calls_per_tool: usize,
    pub reasoning_chars: usize,
}

impl From<AgentCapabilitiesView> for PublicAgentCapabilities {
    fn from(value: AgentCapabilitiesView) -> Self {
        let maturity = value
            .maturity
            .into_iter()
            .map(|(name, maturity)| (name, maturity.into()))
            .collect();
        Self {
            max_rpc_request_frame_bytes: value.max_rpc_request_frame_bytes,
            max_rpc_response_frame_bytes: value.max_rpc_response_frame_bytes,
            max_wait_ms: value.max_wait_ms,
            maturity,
            observation: PublicObservationCapability {
                public_reasoning_default: value.observation.public_reasoning_default,
                defaults: PublicObservationDefaults {
                    top_tools: value.observation.defaults.top_tools,
                    recent_calls_per_tool: value.observation.defaults.recent_calls_per_tool,
                    reasoning_chars: value.observation.defaults.reasoning_chars,
                },
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentObserveOutput {
    pub tools: Vec<PublicObservedTool>,
    pub reasoning: PublicObservedReasoning,
    pub coverage: PublicObservationCoverage,
}

impl JsonSchema for AgentObserveOutput {
    fn schema_name() -> Cow<'static, str> {
        // Display name only: the serialized observation envelope is the
        // neutral `external-subagent-observation/1.1` wire identifier
        // (see docs/mcp-api.md).
        "external-subagent suspicion-only observation".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        let mut value: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../schema/observation.schema.json"
        ))
        .expect("packaged observation schema must be valid JSON");
        value
            .as_object_mut()
            .expect("packaged observation schema must be an object")
            .remove("$schema");
        value
            .try_into()
            .expect("packaged observation schema must be a JSON Schema object")
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicObservedTool {
    #[schemars(length(min = 1))]
    pub tool_name: String,
    #[schemars(range(min = 1))]
    pub call_count: u64,
    #[schemars(length(min = 1, max = 5))]
    pub recent_calls: Vec<PublicObservedCall>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicObservedCall {
    #[schemars(range(min = 1))]
    pub seq: u64,
    #[schemars(length(min = 1))]
    pub tool_call_id: String,
    pub arguments: serde_json::Map<String, serde_json::Value>,
    pub arguments_truncated: bool,
    pub redacted_fields: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicObservedReasoning {
    #[schemars(length(max = 200))]
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicObservationCoverage {
    pub tool_history_complete: bool,
    pub reasoning_complete: bool,
    pub dropped_events: u64,
}

impl TryFrom<TaskObservationView> for AgentObserveOutput {
    type Error = ToolError;

    fn try_from(value: TaskObservationView) -> Result<Self, Self::Error> {
        if value.schema != OBSERVATION_SCHEMA
            || value.count_scope != "agent_lifetime"
            || value.reasoning.source.status != "VERIFIED_RUNTIME_PUBLIC"
            || value.reasoning.source.runtime_version != "3.11.2"
            || value.reasoning.source.event_type != "model.streaming"
            || value.reasoning.source.delta_pointer != "/params/payload/delta"
        {
            return Err(protocol_error());
        }
        Ok(Self {
            tools: value
                .tools
                .into_iter()
                .map(|tool| PublicObservedTool {
                    tool_name: tool.tool_name,
                    call_count: tool.call_count,
                    recent_calls: tool
                        .recent_calls
                        .into_iter()
                        .map(|call| PublicObservedCall {
                            seq: call.seq,
                            tool_call_id: call.tool_call_id,
                            arguments: call.arguments,
                            arguments_truncated: call.arguments_truncated,
                            redacted_fields: call.redacted_fields,
                        })
                        .collect(),
                })
                .collect(),
            reasoning: PublicObservedReasoning {
                text: value.reasoning.text,
                truncated: value.reasoning.truncated,
            },
            coverage: PublicObservationCoverage {
                tool_history_complete: value.coverage.tool_history_complete,
                reasoning_complete: value.coverage.reasoning_complete,
                dropped_events: value.coverage.dropped_events,
            },
        })
    }
}

/// Model-facing status keeps route/capability/readiness only. Deployment
/// identity, config revisions, adapter transport detail and per-scope
/// probe evidence stay on the daemon RPC view, where the CLI diagnose
/// owner reads them.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SystemStatusOutput {
    pub mcp_version: String,
    pub components: BTreeMap<String, PublicComponentState>,
    pub capabilities: PublicAgentCapabilities,
    #[serde(rename = "subagents")]
    pub agents: Vec<PublicAgentStatus>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicAgentStatus {
    #[serde(rename = "subagent")]
    pub agent: String,
    pub configured: bool,
    pub enabled: bool,
    pub spawn_supported: bool,
    pub permission_modes: Vec<PublicAgentPermissionMode>,
    pub model_selection: PublicAgentModelSelectionCapability,
    pub effort_selection: PublicEffortSelectionCapability,
    pub local: PublicAgentScopeStatus,
    pub auth: PublicAgentScopeStatus,
    pub hi: PublicAgentScopeStatus,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicAgentPermissionMode {
    Build,
    Edit,
    Plan,
    Yolo,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicAgentModelSelectionMode {
    NativeOnly,
    CatalogToken,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicAgentModelSelectionCapability {
    pub supported: bool,
    pub mode: PublicAgentModelSelectionMode,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicEffortSelectionMode {
    ClosedSet,
    PassthroughToken,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicEffortSelectionCapability {
    pub supported: bool,
    pub mode: PublicEffortSelectionMode,
}

impl From<AgentPermissionModeView> for PublicAgentPermissionMode {
    fn from(value: AgentPermissionModeView) -> Self {
        match value {
            AgentPermissionModeView::Build => Self::Build,
            AgentPermissionModeView::Edit => Self::Edit,
            AgentPermissionModeView::Plan => Self::Plan,
            AgentPermissionModeView::Yolo => Self::Yolo,
        }
    }
}

impl From<AgentModelSelectionModeView> for PublicAgentModelSelectionMode {
    fn from(value: AgentModelSelectionModeView) -> Self {
        match value {
            AgentModelSelectionModeView::NativeOnly => Self::NativeOnly,
            AgentModelSelectionModeView::CatalogToken => Self::CatalogToken,
        }
    }
}

impl From<AgentEffortSelectionModeView> for PublicEffortSelectionMode {
    fn from(value: AgentEffortSelectionModeView) -> Self {
        match value {
            AgentEffortSelectionModeView::ClosedSet => Self::ClosedSet,
            AgentEffortSelectionModeView::PassthroughToken => Self::PassthroughToken,
        }
    }
}

/// Readiness conclusion only. The probe evidence behind it (scope paths,
/// version, checked_at_ms, reason) stays on the RPC view for CLI diagnose.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicAgentScopeStatus {
    pub state: PublicComponentState,
}

impl From<AgentScopeStatusView> for PublicAgentScopeStatus {
    fn from(value: AgentScopeStatusView) -> Self {
        Self {
            state: value.state.into(),
        }
    }
}

impl From<AgentStatusView> for PublicAgentStatus {
    fn from(value: AgentStatusView) -> Self {
        Self {
            agent: value.agent,
            configured: value.configured,
            enabled: value.enabled,
            spawn_supported: value.spawn_supported,
            permission_modes: value.permission_modes.into_iter().map(Into::into).collect(),
            model_selection: PublicAgentModelSelectionCapability {
                supported: value.model_selection.supported,
                mode: value.model_selection.mode.into(),
            },
            effort_selection: PublicEffortSelectionCapability {
                supported: value.effort_selection.supported,
                mode: value.effort_selection.mode.into(),
            },
            local: value.local.into(),
            auth: value.auth.into(),
            hi: value.hi.into(),
        }
    }
}

impl SystemStatusOutput {
    pub(super) fn from_view(value: SystemStatusView) -> Self {
        Self {
            mcp_version: value.mcp_version,
            components: value
                .components
                .into_iter()
                .map(|(name, state)| (name, state.into()))
                .collect(),
            capabilities: value.capabilities.into(),
            agents: value.agents.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentSpawnOutput {
    #[schemars(range(min = 10000000, max = 99999999))]
    pub agent_id: u64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicTask {
    #[schemars(range(min = 10000000, max = 99999999))]
    pub agent_id: u64,
    pub status: String,
    pub session_id: Option<String>,
    pub input_identity: PublicInputIdentity,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PublicInputIdentity {
    pub subagent: Option<String>,
    pub config_revision: Option<u64>,
    pub adapter_version: Option<String>,
    pub model: Option<String>,
    pub model_source: Option<String>,
    pub effort: Option<String>,
    pub workspace_path: Option<String>,
    pub permission_mode: Option<String>,
}

impl TryFrom<TaskView> for PublicTask {
    type Error = ToolError;

    fn try_from(value: TaskView) -> Result<Self, Self::Error> {
        Ok(Self {
            agent_id: public_task_id(&value.agent_id)?,
            status: value.status,
            session_id: value.session_id,
            input_identity: PublicInputIdentity {
                subagent: value.input_identity.subagent,
                config_revision: value.input_identity.config_revision,
                adapter_version: value.input_identity.adapter_version,
                model: value.input_identity.model,
                model_source: value.input_identity.model_source,
                effort: value.input_identity.effort,
                workspace_path: value.input_identity.workspace_path,
                permission_mode: value.input_identity.permission_mode,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PublicOutcome {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    RuntimeLost,
    ResultInvalid,
}

impl From<TaskOutcome> for PublicOutcome {
    fn from(value: TaskOutcome) -> Self {
        match value {
            TaskOutcome::Completed => Self::Completed,
            TaskOutcome::Failed => Self::Failed,
            TaskOutcome::Cancelled => Self::Cancelled,
            TaskOutcome::TimedOut => Self::TimedOut,
            TaskOutcome::RuntimeLost => Self::RuntimeLost,
            TaskOutcome::ResultInvalid => Self::ResultInvalid,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicResult {
    pub outcome: PublicOutcome,
    pub final_text: String,
    pub partial: bool,
    pub offset: usize,
    pub total_bytes: usize,
    pub next_offset: Option<usize>,
    pub complete: bool,
}

impl TryFrom<TaskResultView> for PublicResult {
    type Error = ToolError;

    fn try_from(value: TaskResultView) -> Result<Self, Self::Error> {
        Ok(Self {
            outcome: value.outcome.into(),
            final_text: value.final_text,
            partial: value.partial,
            offset: value.offset,
            total_bytes: value.total_bytes,
            next_offset: value.next_offset,
            complete: value.complete,
        })
    }
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentListOutput {
    pub tasks: Vec<PublicTask>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicTelemetryStatus {
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublicActivity {
    pub latest_text_tail: String,
    pub latest_text_truncated: bool,
    /// Verified-public reasoning tail (bounded to 200 Unicode
    /// characters); empty when the runtime source is not verified.
    pub latest_reasoning: String,
    /// Tool calls started in the last 60 seconds, across all tools.
    pub tool_calls_last_60s: u64,
    pub telemetry_status: PublicTelemetryStatus,
}

impl From<TaskActivityView> for PublicActivity {
    fn from(value: TaskActivityView) -> Self {
        Self {
            latest_text_tail: value.latest_text_tail,
            latest_text_truncated: value.latest_text_truncated,
            latest_reasoning: value.latest_reasoning,
            tool_calls_last_60s: value.tool_calls_last_60s,
            telemetry_status: match value.telemetry_status {
                TelemetryStatusView::Healthy => PublicTelemetryStatus::Healthy,
                TelemetryStatusView::Degraded => PublicTelemetryStatus::Degraded,
                TelemetryStatusView::Unavailable => PublicTelemetryStatus::Unavailable,
            },
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentWaitOutput {
    pub task: PublicTask,
    pub pending_requests: Vec<PublicPendingRequest>,
    pub result_available: bool,
    pub activity: PublicActivity,
    pub result: Option<PublicResult>,
    pub instruction: Option<String>,
    pub timed_out: bool,
    pub message_receipt: Option<PublicMessageReceipt>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PublicMessageReceipt {
    pub message_id: String,
    pub state: String,
    pub failure_code: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicMessageDisposition {
    Queued,
    Delivered,
    AlreadyDelivered,
    Failed,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentSendOutput {
    pub message_id: String,
    pub disposition: PublicMessageDisposition,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentRespondOutput {
    pub disposition: PublicResponseDisposition,
    pub requested_decision: PublicDecision,
    pub effective_decision: PublicDecision,
    pub policy_overrode: bool,
    pub policy_reason_code: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentStateOutput {
    pub task: PublicTask,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentResultOutput {
    pub task: PublicTask,
    pub result: Option<PublicResult>,
}

#[cfg(test)]
mod effort_projection_tests {
    use super::*;
    use crate::agent_status::ProbeScope;
    use crate::rpc::AgentEffortSelectionCapabilityView;

    fn scope_status() -> crate::rpc::AgentScopeStatusView {
        crate::rpc::AgentScopeStatusView {
            runtime_path: None,
            state: crate::rpc::ComponentStateView::Unknown,
            scope: ProbeScope::default(),
            version: None,
            checked_at_ms: None,
            reason: None,
        }
    }

    fn status(agent: &str, supported: bool) -> AgentStatusView {
        AgentStatusView {
            required_version: None,
            agent: agent.into(),
            config_revision: 1,
            configured: true,
            enabled: supported,
            spawn_supported: supported,
            transport_support: crate::rpc::AgentTransportSupportView {
                transport: crate::rpc::AgentTransportView::ZcodeAppServer,
                probe: true,
                spawn: supported,
            },
            permission_modes: Vec::new(),
            model_selection: crate::rpc::AgentModelSelectionCapabilityView {
                supported: false,
                mode: crate::rpc::AgentModelSelectionModeView::NativeOnly,
            },
            effort_selection: AgentEffortSelectionCapabilityView {
                supported,
                mode: crate::rpc::AgentEffortSelectionModeView::PassthroughToken,
            },
            local: scope_status(),
            auth: scope_status(),
            hi: scope_status(),
        }
    }

    #[test]
    fn public_status_mirrors_effort_selection_and_input_identity_effort() {
        let mut view = status("zcode", true);
        view.effort_selection.mode = crate::rpc::AgentEffortSelectionModeView::ClosedSet;
        let projected = PublicAgentStatus::from(view.clone());
        assert_eq!(projected.effort_selection.supported, true);
        assert!(matches!(
            projected.effort_selection.mode,
            PublicEffortSelectionMode::ClosedSet
        ));
        // Passthrough mode and the unsupported state both survive the mirror.
        view.effort_selection.mode = crate::rpc::AgentEffortSelectionModeView::PassthroughToken;
        view.effort_selection.supported = false;
        let projected = PublicAgentStatus::from(view);
        assert_eq!(projected.effort_selection.supported, false);
        assert!(matches!(
            projected.effort_selection.mode,
            PublicEffortSelectionMode::PassthroughToken
        ));

        // The public task projection carries the admitted effort slot.
        let task = TaskView {
            agent_id: "10000001".into(),
            status: "queued".into(),
            session_id: None,
            input_identity: crate::rpc::InputIdentityView {
                subagent: Some("zcode".into()),
                config_revision: Some(1),
                adapter_version: Some("test".into()),
                model: None,
                model_source: Some("native".into()),
                effort: Some("high".into()),
                workspace_path: Some("/tmp/repo".into()),
                permission_mode: Some("plan".into()),
            },
        };
        let public = PublicTask::try_from(task).unwrap();
        assert_eq!(public.input_identity.effort.as_deref(), Some("high"));
        let encoded = serde_json::to_value(&public).unwrap();
        assert_eq!(encoded["input_identity"]["effort"], "high");
    }
}
