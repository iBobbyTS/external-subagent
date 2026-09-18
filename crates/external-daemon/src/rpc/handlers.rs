//! The RPC service: frame decoding, dispatch, and per-method handlers.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use super::agents::{
    configured_agent_statuses, resolve_admission, unavailable_agent_statuses,
    validate_agent_models_input, validate_agent_probe_input,
};
use super::config::read_agent_config_snapshot;
use super::errors::{map_scheduler, map_store, RpcError, RpcErrorCode};
use super::types::{
    ResponseDecision, RpcMethod, RpcRequest, RpcResponse, RpcSuccess, MAX_LIST_TASKS,
    MAX_REQUEST_FRAME_BYTES, MAX_REQUEST_ID_BYTES, MAX_RESULT_CHUNK_BYTES, MAX_WAIT, MCP_VERSION,
};
use super::views::{
    agent_capabilities, result_page_bounds, running_component_identity, task_view,
    ComponentIdentityView, ComponentStateView, DaemonIdentityView, ModelIdentityView,
    SystemStatusView, TaskObservationView, TaskResultView,
};
use crate::{agent_status::AgentEvidenceStore, observation::OBSERVATION_SCHEMA, Scheduler};
use external_core::{canonical_general_repository, PreparedGeneralTask};
use external_store::{Store, StoredTaskResult, TaskPageFilter, TaskQueryScope, TaskRecord};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[cfg(test)]
use super::agents::admission_fixtures;
#[cfg(test)]
use super::config::AgentConfigSnapshot;
#[cfg(test)]
use super::views::{AgentModelSelectionModeView, AgentTransportView};
#[cfg(test)]
use crate::agent_status::{
    AgentModelsInput, AgentProbeEvidence, AgentProbeInput, EvidenceState, ProbeScope, ScopeEvidence,
};
#[cfg(test)]
use external_core::GeneralTaskManifest;
#[cfg(test)]
use std::path::PathBuf;

#[derive(Clone)]
pub struct RpcService {
    pub(super) scheduler: Scheduler,
    pub(super) store: Arc<Store>,
    pub(super) service_generation: String,
    pub(super) daemon_identity: ComponentIdentityView,
    pub(super) agent_evidence: AgentEvidenceStore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcServiceConfigError {
    MismatchedStore,
    GenerationUnavailable,
}

impl RpcService {
    #[cfg(test)]
    pub(crate) fn store_for_wait_test(&self) -> Arc<Store> {
        self.store.clone()
    }

    pub fn new(scheduler: Scheduler, store: Arc<Store>) -> Result<Self, RpcServiceConfigError> {
        let service_generation = opaque_generation()?;
        Self::new_with_service_generation(scheduler, store, service_generation)
    }

    pub(crate) fn new_with_service_generation(
        scheduler: Scheduler,
        store: Arc<Store>,
        service_generation: String,
    ) -> Result<Self, RpcServiceConfigError> {
        let evidence = AgentEvidenceStore::new(scheduler.configured_runtime_source());
        Self::new_with_service_generation_and_evidence(
            scheduler,
            store,
            service_generation,
            evidence,
        )
    }

    fn new_with_service_generation_and_evidence(
        scheduler: Scheduler,
        store: Arc<Store>,
        service_generation: String,
        agent_evidence: AgentEvidenceStore,
    ) -> Result<Self, RpcServiceConfigError> {
        if !Arc::ptr_eq(&scheduler.store(), &store) {
            return Err(RpcServiceConfigError::MismatchedStore);
        }
        Ok(Self {
            agent_evidence,
            scheduler,
            store,
            service_generation,
            daemon_identity: running_component_identity("daemon", env!("CARGO_PKG_VERSION")),
        })
    }

    pub fn handle_bytes(&self, frame: &[u8]) -> RpcResponse {
        self.handle_bytes_interruptible(frame, &|| false)
    }

    pub(crate) fn handle_bytes_interruptible(
        &self,
        frame: &[u8],
        interrupted: &dyn Fn() -> bool,
    ) -> RpcResponse {
        if frame.len().saturating_add(1) > MAX_REQUEST_FRAME_BYTES {
            return RpcResponse::error(
                None,
                RpcError::new(RpcErrorCode::Oversized, "request frame exceeds the RPC cap"),
            );
        }
        let mut value = match serde_json::from_slice::<Value>(frame) {
            Ok(value) => value,
            Err(_) => {
                return RpcResponse::error(
                    None,
                    RpcError::new(RpcErrorCode::Malformed, "request is not valid JSON"),
                )
            }
        };
        let request_id = value
            .get("request_id")
            .and_then(Value::as_str)
            .filter(|request_id| valid_request_id(request_id))
            .map(str::to_owned);
        if value.as_object().is_none_or(|object| {
            object
                .keys()
                .any(|key| !matches!(key.as_str(), "request_id" | "method" | "params"))
        }) {
            return RpcResponse::error(
                request_id,
                RpcError::new(RpcErrorCode::Validation, "request fields are invalid"),
            );
        }
        let method = value.get("method").and_then(Value::as_str);
        if let Some(method) = method {
            if !RpcMethod::is_known(method) {
                return RpcResponse::error(
                    request_id,
                    RpcError::new(RpcErrorCode::UnknownMethod, "unknown RPC method"),
                );
            }
        }
        // Legacy drain requests omitted params; absence still means passive
        // draining. Explicit malformed params continue to fail validation.
        if method == Some("daemon_begin_drain") && value.get("params").is_none() {
            value["params"] = serde_json::json!({});
        }
        let request = match serde_json::from_value::<RpcRequest>(value) {
            Ok(request) => request,
            Err(_) => {
                return RpcResponse::error(
                    request_id,
                    RpcError::new(RpcErrorCode::Validation, "request fields are invalid"),
                )
            }
        };
        if !valid_request_id(&request.request_id) {
            return RpcResponse::error(
                None,
                RpcError::new(RpcErrorCode::Validation, "request_id is invalid"),
            );
        }
        let request_id = request.request_id;
        match self.dispatch_interruptible(request.method, interrupted) {
            Ok(result) => RpcResponse::success(request_id, result),
            Err(error) => RpcResponse::error(Some(request_id), error),
        }
    }

    pub fn dispatch(&self, method: RpcMethod) -> Result<RpcSuccess, RpcError> {
        self.dispatch_interruptible(method, &|| false)
    }

    fn dispatch_interruptible(
        &self,
        method: RpcMethod,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<RpcSuccess, RpcError> {
        match method {
            RpcMethod::SystemStatus => Ok(RpcSuccess::SystemStatus {
                status: self.system_status(),
            }),
            RpcMethod::DaemonBeginDrain { cancel_active } => {
                self.scheduler.begin_drain();
                if cancel_active {
                    self.scheduler
                        .cancel_draining_tasks()
                        .map_err(map_scheduler)?;
                }
                Ok(RpcSuccess::DaemonDrainStatus {
                    is_draining: true,
                    active_count: self.scheduler.active_count(),
                    resources_reaped: self.scheduler.resources_reaped(),
                    ready_for_activation: self.scheduler.ready_for_activation(),
                    updater_fired: self.scheduler.updater_fired(),
                    activation_claim: None,
                })
            }
            RpcMethod::DaemonDrainStatus => Ok(RpcSuccess::DaemonDrainStatus {
                is_draining: self.scheduler.is_draining(),
                active_count: self.scheduler.active_count(),
                resources_reaped: self.scheduler.resources_reaped(),
                ready_for_activation: self.scheduler.ready_for_activation(),
                updater_fired: self.scheduler.updater_fired(),
                activation_claim: None,
            }),
            RpcMethod::DaemonAbortDrain => {
                // Bounded recovery: reopen admission on a daemon whose update
                // failed before activation. Refusals (no drain active, cancel
                // worker in flight) surface as state errors, never a reset.
                self.scheduler.abort_drain().map_err(map_scheduler)?;
                Ok(RpcSuccess::DaemonDrainStatus {
                    is_draining: self.scheduler.is_draining(),
                    active_count: self.scheduler.active_count(),
                    resources_reaped: self.scheduler.resources_reaped(),
                    ready_for_activation: self.scheduler.ready_for_activation(),
                    updater_fired: self.scheduler.updater_fired(),
                    activation_claim: None,
                })
            }
            RpcMethod::DaemonActivateReady => {
                let claim = self.scheduler.claim_activation();
                Ok(RpcSuccess::DaemonDrainStatus {
                    is_draining: self.scheduler.is_draining(),
                    active_count: self.scheduler.active_count(),
                    resources_reaped: self.scheduler.resources_reaped(),
                    ready_for_activation: self.scheduler.ready_for_activation(),
                    updater_fired: self.scheduler.updater_fired(),
                    activation_claim: claim,
                })
            }
            RpcMethod::AgentProbe(mut input) => {
                validate_agent_probe_input(&input)?;
                let config = read_agent_config_snapshot()?;
                if input.agent == "dsh" {
                    let dsh = config.subagents.get("dsh");
                    input.scope.profile = input
                        .scope
                        .profile
                        .or_else(|| dsh.and_then(|entry| entry.profile.clone()));
                    input.scope.version = input
                        .scope
                        .version
                        .or_else(|| dsh.and_then(|entry| entry.version.clone()));
                }
                let evidence = self.agent_evidence.probe(&input, config.revision);
                let status = configured_agent_statuses(&config, &self.agent_evidence)
                    .into_iter()
                    .find(|status| status.agent == input.agent)
                    .ok_or_else(|| RpcError::new(RpcErrorCode::AgentUnknown, "agent is unknown"))?;
                Ok(RpcSuccess::AgentProbed { evidence, status })
            }
            RpcMethod::AgentModels(mut input) => {
                validate_agent_models_input(&input)?;
                let config = read_agent_config_snapshot()?;
                if input.agent == "dsh" {
                    let dsh = config.subagents.get("dsh");
                    input.scope.profile = input
                        .scope
                        .profile
                        .or_else(|| dsh.and_then(|entry| entry.profile.clone()));
                    input.scope.version = input
                        .scope
                        .version
                        .or_else(|| dsh.and_then(|entry| entry.version.clone()));
                }
                Ok(RpcSuccess::AgentModels {
                    catalog: self.agent_evidence.models(&input, config.revision),
                })
            }
            RpcMethod::SubmitGeneral(input) => {
                let config = read_agent_config_snapshot()?;
                let admission = resolve_admission(&input, &config)?;
                let manifest = input.manifest;
                let submitted = self
                    .scheduler
                    .enqueue_general_with_admission(&manifest, Some(admission))
                    .map_err(map_scheduler)?;
                Ok(RpcSuccess::GeneralSubmitted {
                    task: task_view(submitted),
                })
            }
            RpcMethod::TaskList(query) => {
                if let Some(agent) = query.agent.as_deref() {
                    if !matches!(agent, "zcode" | "dsh" | "codex") {
                        return Err(RpcError::new(
                            RpcErrorCode::AgentUnknown,
                            "agent is unknown",
                        ));
                    }
                }
                if query.limit == 0 || query.limit > MAX_LIST_TASKS {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "task list limit is outside the allowed range",
                    ));
                }
                for (field, value, cap) in [("repository", query.repository.as_deref(), 4096usize)]
                {
                    if let Some(value) = value {
                        validate_text(value, field, cap)?;
                    }
                }
                if let Some(cursor) = query.cursor.as_deref() {
                    validate_text(cursor, "cursor", 64)?;
                }
                if query.repository.is_none() {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "at least one task list scope is required",
                    ));
                }
                let canonical_repository = query
                    .repository
                    .as_deref()
                    .map(|repository| canonical_general_repository(Path::new(repository)))
                    .transpose()
                    .map_err(|_| {
                        RpcError::new(RpcErrorCode::Validation, "repository scope is invalid")
                    })?
                    .map(|repository| repository.to_string_lossy().into_owned());
                let page = self
                    .store
                    .list_task_page(
                        TaskQueryScope {
                            repository: canonical_repository.as_deref(),
                        },
                        TaskPageFilter {
                            agent: query.agent.clone(),
                            phase: query.phase.map(Into::into),
                            outcome: query.outcome,
                        },
                        query.cursor.as_deref().map(parse_task_cursor).transpose()?,
                        query.limit,
                    )
                    .map_err(map_store)?;
                let mut views = Vec::with_capacity(page.tasks.len());
                for task in page.tasks {
                    views.push(task_view(task));
                }
                Ok(RpcSuccess::TaskListed {
                    tasks: views,
                    next_cursor: page.next_cursor.map(format_task_cursor),
                })
            }
            RpcMethod::TaskWait(query) => {
                if query.wait_time > MAX_WAIT.as_secs() {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "wait_time must be between 0 and 299 seconds",
                    ));
                }
                let deadline = Instant::now() + Duration::from_secs(query.wait_time);
                self.task_wait(query, deadline, interrupted)
            }
            RpcMethod::TaskMessage(input) => {
                let task = self.require_task(&input.agent_id)?;
                if let Some(message_id) = input.message_id.as_deref() {
                    validate_id(message_id, "message_id")?;
                }
                // The generic control plane only queues clarification. A
                // terminal task may be resumed by the scheduler when the
                // persisted ZCode session accepts a restore; other
                // interrupt-and-continue paths remain private.
                validate_text(&input.content, "content", 16 * 1024)?;
                let message_id = input
                    .message_id
                    .unwrap_or_else(|| format!("subagent-message-{}", Uuid::new_v4()));
                let disposition = self
                    .scheduler
                    .queue_message(&task.agent_id, &message_id, &input.content)
                    .map_err(map_scheduler)?;
                let task = self.require_task(&input.agent_id)?;
                Ok(RpcSuccess::Message {
                    message_id,
                    disposition: disposition.into(),
                    task: task_view(task),
                })
            }
            RpcMethod::TaskRespond(input) => {
                let task = self.require_task(&input.agent_id)?;
                validate_id(&input.request_id, "request_id")?;
                if let Some(content) = input.content.as_deref() {
                    validate_text(content, "response content", 16 * 1024)?;
                }
                if input.decision == ResponseDecision::Answer
                    && input
                        .content
                        .as_deref()
                        .is_none_or(|content| content.trim().is_empty())
                {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "answer requires non-empty content",
                    ));
                }
                let outcome = self
                    .scheduler
                    .respond_request(
                        &task.agent_id,
                        &input.request_id,
                        input.decision.as_str(),
                        input.content.as_deref(),
                    )
                    .map_err(map_scheduler)?;
                let task = self.require_task(&input.agent_id)?;
                Ok(RpcSuccess::Respond {
                    outcome: outcome.into(),
                    task: task_view(task),
                })
            }
            RpcMethod::TaskCancel { agent_id } => {
                let task = self.require_task(&agent_id)?;
                self.scheduler
                    .cancel_task(&task.agent_id)
                    .map_err(map_scheduler)?;
                let task = self.require_task(&agent_id)?;
                Ok(RpcSuccess::Stopped {
                    task: task_view(task),
                })
            }
            RpcMethod::TaskResult {
                agent_id,
                offset,
                limit,
            } => {
                let task = self.require_task(&agent_id)?;
                if limit == 0 || limit > MAX_RESULT_CHUNK_BYTES {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        format!(
                            "result limit must be between 1 and {MAX_RESULT_CHUNK_BYTES} bytes; received {limit}"
                        ),
                    ));
                }
                let result = self
                    .store
                    .task_result(&task.agent_id)
                    .map_err(map_store)?
                    .map(|stored| self.task_result_view(stored, offset, limit))
                    .transpose()?;
                Ok(RpcSuccess::TaskResult {
                    task: task_view(task),
                    result,
                })
            }
            RpcMethod::TaskClose { agent_id } => {
                let task = self.require_task(&agent_id)?;
                self.scheduler
                    .close_task(&task.agent_id)
                    .map_err(map_scheduler)?;
                let task = self.require_task(&agent_id)?;
                Ok(RpcSuccess::Closed {
                    task: task_view(task),
                })
            }
            RpcMethod::TaskObserve { agent_id } => {
                let task = self.require_task(&agent_id)?;
                let (snapshot, runtime_source_verified) =
                    self.scheduler.observation_snapshot(&task.agent_id);
                if !runtime_source_verified {
                    return Err(RpcError::new(
                        RpcErrorCode::Unavailable,
                        "observation runtime source is not verified",
                    ));
                }
                Ok(RpcSuccess::TaskObserved {
                    observation: TaskObservationView {
                        schema: OBSERVATION_SCHEMA.into(),
                        agent_id: task.agent_id,
                        count_scope: "agent_lifetime".into(),
                        tools: snapshot.tools,
                        reasoning: snapshot.reasoning,
                        coverage: snapshot.coverage,
                    },
                })
            }
        }
    }

    fn system_status(&self) -> SystemStatusView {
        let mut components = BTreeMap::new();
        components.insert("facade".into(), ComponentStateView::Unknown);
        components.insert("daemon".into(), ComponentStateView::Ready);
        components.insert(
            "store".into(),
            match self.store.journal_mode() {
                Ok(mode) if mode.eq_ignore_ascii_case("wal") => ComponentStateView::Ready,
                Ok(_) => ComponentStateView::Degraded,
                Err(_) => ComponentStateView::Unavailable,
            },
        );
        components.insert("scheduler".into(), ComponentStateView::Ready);
        components.insert("driver".into(), ComponentStateView::Unknown);
        components.insert("runtime".into(), ComponentStateView::Unknown);
        components.insert("model_auth".into(), ComponentStateView::Unknown);
        SystemStatusView {
            mcp_version: MCP_VERSION.into(),
            service_generation: self.service_generation.clone(),
            components,
            capabilities: agent_capabilities(),
            agents: read_agent_config_snapshot()
                .map(|config| configured_agent_statuses(&config, &self.agent_evidence))
                .unwrap_or_else(|_| unavailable_agent_statuses()),
            identity: Some(DaemonIdentityView {
                daemon: self.daemon_identity.clone(),
                // Status has no Agent/session scope, and the current runtime
                // exposes no verified response-producer model identity.
                models: ModelIdentityView { configured: None },
            }),
        }
    }

    pub(super) fn require_task(&self, agent_id: &str) -> Result<TaskRecord, RpcError> {
        validate_id(agent_id, "agent_id")?;
        let task = self
            .store
            .get_task(agent_id)
            .map_err(map_store)?
            .ok_or_else(|| RpcError::new(RpcErrorCode::NotFound, "task was not found"))?;
        let prepared = serde_json::from_str::<PreparedGeneralTask>(&task.prepared_launch_json)
            .map_err(|_| RpcError::new(RpcErrorCode::NotFound, "task was not found"))?;
        if prepared.repository.to_string_lossy() != task.repository {
            return Err(RpcError::new(RpcErrorCode::NotFound, "task was not found"));
        }
        Ok(task)
    }

    pub(super) fn task_result_view(
        &self,
        stored: StoredTaskResult,
        offset: usize,
        limit: usize,
    ) -> Result<TaskResultView, RpcError> {
        let text = stored.result.final_text;
        let total_bytes = text.len();
        let (end, next_offset) = result_page_bounds(&text, offset, limit)?;
        Ok(TaskResultView {
            outcome: stored.result.outcome,
            final_text: text[offset..end].to_owned(),
            partial: stored.result.partial,
            offset,
            total_bytes,
            next_offset,
            complete: next_offset.is_none(),
        })
    }
}

fn opaque_generation() -> Result<String, RpcServiceConfigError> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| RpcServiceConfigError::GenerationUnavailable)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn parse_task_cursor(cursor: &str) -> Result<u64, RpcError> {
    let value = cursor
        .strip_prefix("task:")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| RpcError::new(RpcErrorCode::Validation, "task cursor is invalid"))?;
    Ok(value)
}

fn format_task_cursor(cursor: u64) -> String {
    format!("task:{cursor}")
}

fn validate_id(value: &str, field: &str) -> Result<(), RpcError> {
    validate_text(value, field, 256)
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_REQUEST_ID_BYTES && !value.contains('\0')
}

pub(super) fn validate_text(value: &str, field: &str, max: usize) -> Result<(), RpcError> {
    if value.is_empty() || value.len() > max || value.contains('\0') {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            format!("{field} is invalid"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod agent_probe_tests {
    use super::*;
    use crate::{agent_status::AgentProbeBackend, CommandRuntimeFactory, SchedulerConfig};
    use std::process::Command;

    struct FixtureProbe;

    impl AgentProbeBackend for FixtureProbe {
        fn probe(&self, input: &AgentProbeInput) -> AgentProbeEvidence {
            let ready = ScopeEvidence {
                state: EvidenceState::Ready,
                scope: input.scope.clone(),
                version: Some("3.8.1".into()),
                checked_at_ms: 123,
                reason: None,
            };
            let unknown_auth = ScopeEvidence {
                state: EvidenceState::Unknown,
                scope: input.scope.clone(),
                version: None,
                checked_at_ms: 123,
                reason: Some("auth_not_probed".into()),
            };
            let unknown_hi = ScopeEvidence {
                state: EvidenceState::Unknown,
                scope: input.scope.clone(),
                version: None,
                checked_at_ms: 123,
                reason: Some("hi_not_probed".into()),
            };
            let (local, auth, hi) = match input.through {
                crate::agent_status::ProbeLayer::Local => (ready.clone(), unknown_auth, unknown_hi),
                crate::agent_status::ProbeLayer::Auth => (ready.clone(), ready.clone(), unknown_hi),
                crate::agent_status::ProbeLayer::Hi => (ready.clone(), ready.clone(), ready),
            };
            AgentProbeEvidence {
                agent: input.agent.clone(),
                config_revision: 0,
                local,
                auth,
                hi,
            }
        }
    }

    fn service() -> (tempfile::TempDir, RpcService) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| -> std::io::Result<Command> {
            panic!("probe fixture must use its dedicated backend")
        });
        let scheduler = Scheduler::new(
            "agent-probe-test",
            store.clone(),
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let evidence = AgentEvidenceStore::with_backend(Arc::new(FixtureProbe));
        let service = RpcService::new_with_service_generation_and_evidence(
            scheduler,
            store,
            "probe-generation".into(),
            evidence,
        )
        .unwrap();
        (directory, service)
    }

    #[test]
    fn status_is_passive_and_explicit_probe_records_scoped_evidence() {
        let _config_guard = admission_fixtures::config_env_guard();
        let (_directory, service) = service();
        let RpcSuccess::SystemStatus { status } =
            service.dispatch(RpcMethod::SystemStatus).unwrap()
        else {
            panic!("expected status")
        };
        assert_eq!(status.agents[0].local.state, ComponentStateView::Unknown);
        assert_eq!(status.agents[0].local.reason, None);
        assert_eq!(status.agents[0].local.checked_at_ms, None);

        let RpcSuccess::AgentProbed { status, .. } = service
            .dispatch(RpcMethod::AgentProbe(AgentProbeInput {
                agent: "zcode".into(),
                through: crate::agent_status::ProbeLayer::Local,
                scope: ProbeScope::default(),
            }))
            .unwrap()
        else {
            panic!("expected local probe")
        };
        assert_eq!(status.local.checked_at_ms, Some(123));
        assert_eq!(status.auth.checked_at_ms, None);
        assert_eq!(status.hi.checked_at_ms, None);

        let scope = ProbeScope {
            workspace: Some("/workspace-a".into()),
            home: Some("/home-a".into()),
            profile: None,
            version: None,
        };
        let RpcSuccess::AgentProbed { evidence, status } = service
            .dispatch(RpcMethod::AgentProbe(AgentProbeInput {
                agent: "zcode".into(),
                through: crate::agent_status::ProbeLayer::Hi,
                scope: scope.clone(),
            }))
            .unwrap()
        else {
            panic!("expected probe")
        };
        assert_eq!(evidence.hi.scope, scope);
        assert_eq!(evidence.config_revision, status.config_revision);
        assert_eq!(status.hi.state, ComponentStateView::Ready);
        assert_eq!(status.hi.checked_at_ms, Some(123));

        let RpcSuccess::SystemStatus { status } =
            service.dispatch(RpcMethod::SystemStatus).unwrap()
        else {
            panic!("expected status")
        };
        let zcode = status
            .agents
            .iter()
            .find(|status| status.agent == "zcode")
            .unwrap();
        assert_eq!(zcode.hi.scope.workspace.as_deref(), Some("/workspace-a"));
        let dsh = status
            .agents
            .iter()
            .find(|status| status.agent == "dsh")
            .unwrap();
        assert!(!dsh.spawn_supported);
        assert_eq!(dsh.hi.state, ComponentStateView::Unknown);
        assert!(dsh.configured);
        assert_eq!(dsh.transport_support.transport, AgentTransportView::DshAcp);
        assert!(!dsh.transport_support.spawn);
        assert!(dsh.permission_modes.is_empty());
        assert_eq!(
            dsh.model_selection.mode,
            AgentModelSelectionModeView::CatalogToken
        );
        assert!(!dsh.model_selection.supported);
    }

    #[test]
    fn config_revision_change_marks_previous_probe_evidence_stale() {
        let (_directory, service) = service();
        let input = AgentProbeInput {
            agent: "zcode".into(),
            through: crate::agent_status::ProbeLayer::Hi,
            scope: ProbeScope::default(),
        };
        let evidence = service.agent_evidence.probe(&input, 7);
        assert_eq!(evidence.config_revision, 7);
        let mut config = AgentConfigSnapshot::default();
        config.revision = 8;
        let status = configured_agent_statuses(&config, &service.agent_evidence)
            .into_iter()
            .find(|status| status.agent == "zcode")
            .unwrap();
        assert_eq!(status.config_revision, 8);
        for layer in [&status.local, &status.auth, &status.hi] {
            assert_eq!(layer.state, ComponentStateView::Unknown);
            assert_eq!(layer.reason.as_deref(), Some("stale_config_revision"));
        }
    }

    #[test]
    fn probe_rejects_unknown_agent_and_relative_scopes_before_execution() {
        let (_directory, service) = service();
        for input in [
            AgentProbeInput {
                agent: "other".into(),
                through: crate::agent_status::ProbeLayer::Local,
                scope: ProbeScope::default(),
            },
            AgentProbeInput {
                agent: "zcode".into(),
                through: crate::agent_status::ProbeLayer::Hi,
                scope: ProbeScope {
                    workspace: Some("relative".into()),
                    home: None,
                    profile: None,
                    version: None,
                },
            },
        ] {
            assert!(service.dispatch(RpcMethod::AgentProbe(input)).is_err());
        }
    }

    #[test]
    fn agent_models_rpc_preserves_native_only_result_and_config_identity() {
        let _config_guard = admission_fixtures::config_env_guard();
        let (_directory, service) = service();
        let RpcSuccess::AgentModels { catalog } = service
            .dispatch(RpcMethod::AgentModels(AgentModelsInput {
                agent: "zcode".into(),
                scope: ProbeScope::default(),
            }))
            .unwrap()
        else {
            panic!("expected models result")
        };
        assert!(!catalog.supported);
        assert!(catalog.models.is_empty());
        assert_eq!(catalog.reason.as_deref(), Some("native_only"));
        assert_eq!(catalog.evidence.source, "zcode_native_model");
        assert_eq!(
            catalog.config_revision,
            AgentConfigSnapshot::default().revision
        );
    }

    #[test]
    fn draining_management_methods_are_public_rpc_names() {
        assert!(RpcMethod::is_known("daemon_begin_drain"));
        assert!(RpcMethod::is_known("daemon_drain_status"));
        assert!(RpcMethod::is_known("daemon_abort_drain"));
    }

    #[test]
    fn draining_status_read_is_side_effect_free_and_activation_is_once() {
        let (_directory, service) = service();
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } =
            service.dispatch(RpcMethod::DaemonDrainStatus).unwrap()
        else {
            panic!("status")
        };
        assert!(!updater_fired);
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } = service
            .dispatch(RpcMethod::DaemonBeginDrain {
                cancel_active: false,
            })
            .unwrap()
        else {
            panic!("begin")
        };
        assert!(!updater_fired);
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } =
            service.dispatch(RpcMethod::DaemonDrainStatus).unwrap()
        else {
            panic!("status")
        };
        assert!(!updater_fired);
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } =
            service.dispatch(RpcMethod::DaemonActivateReady).unwrap()
        else {
            panic!("activate")
        };
        assert!(updater_fired);
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } =
            service.dispatch(RpcMethod::DaemonActivateReady).unwrap()
        else {
            panic!("activate")
        };
        assert!(updater_fired);
    }

    #[test]
    fn abort_drain_preserves_the_issued_claim_for_the_retry() {
        let (_directory, service) = service();
        service
            .dispatch(RpcMethod::DaemonBeginDrain {
                cancel_active: false,
            })
            .unwrap();
        let claim = match service.dispatch(RpcMethod::DaemonActivateReady).unwrap() {
            RpcSuccess::DaemonDrainStatus {
                activation_claim: Some(claim),
                updater_fired: true,
                ..
            } => claim,
            _ => panic!("expected the first activation claim"),
        };

        // The update the claim was issued for failed without activating:
        // reopen admission, but keep the claim as a recorded fact.
        let RpcSuccess::DaemonDrainStatus { is_draining, .. } =
            service.dispatch(RpcMethod::DaemonAbortDrain).unwrap()
        else {
            panic!("abort")
        };
        assert!(!is_draining);
        let RpcSuccess::DaemonDrainStatus {
            activation_claim, ..
        } = service.dispatch(RpcMethod::DaemonDrainStatus).unwrap()
        else {
            panic!("status")
        };
        assert_eq!(activation_claim, None);

        // The retry drains again and the SAME claim hands it its identity:
        // a reopened daemon never strands a retryable receipt.
        service
            .dispatch(RpcMethod::DaemonBeginDrain {
                cancel_active: false,
            })
            .unwrap();
        let RpcSuccess::DaemonDrainStatus {
            activation_claim: Some(retry),
            updater_fired,
            ..
        } = service.dispatch(RpcMethod::DaemonActivateReady).unwrap()
        else {
            panic!("activate")
        };
        assert_eq!(retry, claim);
        assert!(updater_fired);
    }
}

#[cfg(test)]
mod observe_gate_tests {
    use super::*;
    use crate::{CommandRuntimeFactory, Scheduler, SchedulerConfig};

    /// The literal pinned ZCode runtime path: the strongest deterministic
    /// "pinned ZCode installation present" fixture. The assertions hold
    /// whether or not the file exists or matches the pinned digest, because
    /// observe trust is bound to the launched adapter, never the global file.
    const PINNED_ZCODE_SOURCE: &str = "/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs";

    fn admission(agent: &str) -> external_core::AdmissionIdentity {
        external_core::AdmissionIdentity {
            agent: agent.into(),
            config_revision: 1,
            adapter_version: env!("CARGO_PKG_VERSION").into(),
            model: None,
            model_source: "catalog".into(),
        }
    }

    fn service_with_pinned_source(
        directory: &std::path::Path,
    ) -> (Arc<RpcService>, external_store::TaskRecord) {
        let store = Arc::new(Store::open(directory.join("state.sqlite")).unwrap());
        let factory = Arc::new(CommandRuntimeFactory::new(
            |_: &TaskRecord| -> std::io::Result<std::process::Command> {
                panic!("observe gate test must never start a runtime");
            },
        ));
        let scheduler = Scheduler::new(
            "observe-gate",
            Arc::clone(&store),
            factory,
            SchedulerConfig {
                runtime_source: Some(PathBuf::from(PINNED_ZCODE_SOURCE)),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let manifest = GeneralTaskManifest {
            schema: "zcode-general-task/v1".into(),
            agent_id: String::new(),
            repository: directory.canonicalize().unwrap(),
            permission_mode: external_core::PermissionMode::Plan,
            prompt: "observe gate fixture".into(),
            write_manifest: Vec::new(),
        };
        // A queued DSH task was accepted for admission but never launched.
        let submitted = scheduler
            .enqueue_general_with_admission(&manifest, Some(admission("dsh")))
            .unwrap();
        let task = submitted;
        let service = Arc::new(RpcService::new(scheduler, store).unwrap());
        (service, task)
    }

    #[test]
    fn task_observe_reports_unavailable_without_launch_scoped_evidence() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("s04-observe-gate-")
            .tempdir_in(root)
            .unwrap();
        let (service, task) = service_with_pinned_source(directory.path());
        // The never-launched DSH task has no activity; even the pinned ZCode
        // runtime configured globally must not satisfy the observe gate.
        let error = service
            .dispatch(RpcMethod::TaskObserve {
                agent_id: task.agent_id.clone(),
            })
            .unwrap_err();
        assert_eq!(error.code, RpcErrorCode::Unavailable);
    }
}
