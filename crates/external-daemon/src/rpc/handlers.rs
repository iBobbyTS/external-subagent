//! The RPC service: frame decoding, dispatch, and per-method handlers.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use super::agents::{
    configured_agent_statuses, resolve_admission_with_instructions,
    unavailable_agent_statuses, unknown_agent_name_error, validate_agent_models_input,
    validate_agent_probe_input,
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
use external_store::{
    Store, StoredTaskResult, TaskOutcome, TaskPageFilter, TaskQueryScope, TaskRecord,
};
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
    AgentModelsInput, AgentModelsOutput, AgentProbeEvidence, AgentProbeInput, EvidenceState,
    ModelCatalogEvidence, ProbeScope, ScopeEvidence,
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
                if !config.subagents.contains_key(input.agent.as_str()) {
                    return Err(unknown_agent_name_error(&config));
                }
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
                if !config.subagents.contains_key(input.agent.as_str()) {
                    return Err(unknown_agent_name_error(&config));
                }
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
            RpcMethod::SubmitGeneral(mut input) => {
                // Mutex validation: profile cannot be combined with agent, model, effort, or manifest permission_mode.
                // This check MUST run before attempting to load or read any profile file.
                if input.profile.is_some()
                    && (input.agent.is_some()
                        || input.model.is_some()
                        || input.effort.is_some()
                        || input.manifest.permission_mode.is_some())
                {
                    return Err(RpcError::new_profile_error(
                        RpcErrorCode::Validation,
                        "profile cannot be combined with subagent, permission_mode, model, or effort; specify these in the profile TOML or omit profile",
                    ));
                }

                let developer_instructions = if let Some(profile_name) = input.profile.as_deref() {
                    let trimmed = profile_name.trim();
                    if trimmed.is_empty() || profile_name.len() > 128 || profile_name.contains('\0') {
                        let available = super::profiles::available_profile_names();
                        let message = super::profiles::format_profile_error_with_names(
                            "profile is invalid",
                            &available,
                        );
                        return Err(RpcError::new_profile_error(
                            RpcErrorCode::Validation,
                            message,
                        ));
                    }
                    let profile = super::profiles::load_profile(trimmed)?;
                    input.agent = profile.subagent;
                    input.model = profile.model;
                    input.effort = profile.effort;
                    input.manifest.permission_mode = Some(
                        profile
                            .permission_mode
                            .unwrap_or(external_core::PermissionMode::Build),
                    );
                    profile.developer_instructions
                } else {
                    None
                };

                let config = read_agent_config_snapshot()?;
                let effective_agent = input
                    .agent
                    .as_deref()
                    .or(config.default_subagent.as_deref());

                let admission = if effective_agent == Some("codex") {
                    // codex uses native developerInstructions on thread/start; prompt remains verbatim
                    resolve_admission_with_instructions(
                        &input,
                        &config,
                        developer_instructions.as_deref(),
                    )?
                } else {
                    // other subagents fallback to prompt splicing when developer_instructions is non-empty
                    if let Some(di) = developer_instructions.as_deref() {
                        if !di.is_empty() {
                            input.manifest.prompt = format!(
                                "Developer Instructions: {di}\n----------\n{}",
                                input.manifest.prompt
                            );
                        }
                    }
                    resolve_admission_with_instructions(&input, &config, None)?
                };

                let manifest = input.manifest;
                let submitted = self
                    .scheduler
                    .submit_and_start_general(&manifest, Some(admission), interrupted)
                    .map_err(map_scheduler)?;
                Ok(RpcSuccess::GeneralSubmitted {
                    task: task_view(submitted),
                })
            }
            RpcMethod::TaskList(query) => {
                if let Some(agent) = query.agent.as_deref() {
                    if !matches!(agent, "zcode" | "dsh" | "codex" | "agy") {
                        // The literal fast path keeps the happy read free of a
                        // config load; only the error path pays for the roster.
                        let config = read_agent_config_snapshot()?;
                        return Err(unknown_agent_name_error(&config));
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
                if !matches!(input.mode.as_str(), "queue" | "steer") {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "mode must be queue or steer",
                    ));
                }
                let task = self.require_task(&input.agent_id)?;
                if let Some(message_id) = input.message_id.as_deref() {
                    validate_id(message_id, "message_id")?;
                }
                validate_text(&input.content, "content", 16 * 1024)?;
                let message_id = input
                    .message_id
                    .unwrap_or_else(|| format!("subagent-message-{}", Uuid::new_v4()));
                let disposition = self
                    .scheduler
                    .send_message(&task.agent_id, &message_id, &input.mode, &input.content)
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
                let (task, stored, reason) = self.require_task_with_result(&agent_id)?;
                if limit == 0 || limit > MAX_RESULT_CHUNK_BYTES {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        format!(
                            "result limit must be between 1 and {MAX_RESULT_CHUNK_BYTES} bytes; received {limit}"
                        ),
                    ));
                }
                let result = stored
                    .map(|stored| {
                        let failure_message = failure_message_projection(&task, &stored);
                        self.task_result_view(stored, offset, limit, reason, failure_message)
                    })
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
                let adapter = crate::task_agent(&task);
                if !runtime_source_verified && adapter != "dsh" && adapter != "codex" && adapter != "agy"
                {
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
                        reasoning: (!matches!(adapter.as_str(), "codex" | "agy"))
                            .then_some(snapshot.reasoning),
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
        validate_task_record(task)
    }

    /// Snapshot the task row, its optional immutable result, and the latest
    /// terminal reason under one store lock so the two RPC exits can never pair
    /// a task state, a result, or a reason from a different round. The task
    /// still passes the existing access validation.
    #[allow(clippy::type_complexity)]
    pub(super) fn require_task_with_result(
        &self,
        agent_id: &str,
    ) -> Result<(TaskRecord, Option<StoredTaskResult>, Option<String>), RpcError> {
        validate_id(agent_id, "agent_id")?;
        let (task, result, reason) = self
            .store
            .task_with_result(agent_id)
            .map_err(map_store)?
            .ok_or_else(|| RpcError::new(RpcErrorCode::NotFound, "task was not found"))?;
        Ok((validate_task_record(task)?, result, reason))
    }

    pub(super) fn task_result_view(
        &self,
        stored: StoredTaskResult,
        offset: usize,
        limit: usize,
        reason_code: Option<String>,
        failure_message: Option<String>,
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
            reason_code,
            failure_message,
        })
    }
}

fn validate_task_record(task: TaskRecord) -> Result<TaskRecord, RpcError> {
    let prepared = serde_json::from_str::<PreparedGeneralTask>(&task.prepared_launch_json)
        .map_err(|_| RpcError::new(RpcErrorCode::NotFound, "task was not found"))?;
    if prepared.repository.to_string_lossy() != task.repository {
        return Err(RpcError::new(RpcErrorCode::NotFound, "task was not found"));
    }
    Ok(task)
}

/// Expose the persisted failure detail only when the immutable stored result
/// is one of the three failure outcomes AND the task row's effective outcome
/// matches it. The matching check guards against mixing a task row with a
/// result from another round; same-round atomicity comes from
/// [`RpcService::require_task_with_result`]'s single-lock snapshot.
pub(super) fn failure_message_projection(
    task: &TaskRecord,
    result: &StoredTaskResult,
) -> Option<String> {
    let outcome = result.result.outcome;
    if !matches!(
        outcome,
        TaskOutcome::Failed | TaskOutcome::RuntimeLost | TaskOutcome::ResultInvalid
    ) {
        return None;
    }
    if task.outcome != Some(outcome) {
        return None;
    }
    task.failure_message.clone()
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
                runtime_path: None,
                state: EvidenceState::Ready,
                scope: input.scope.clone(),
                version: Some("3.8.1".into()),
                checked_at_ms: 123,
                reason: None,
            };
            let unknown_auth = ScopeEvidence {
                runtime_path: None,
                state: EvidenceState::Unknown,
                scope: input.scope.clone(),
                version: None,
                checked_at_ms: 123,
                reason: Some("auth_not_probed".into()),
            };
            let unknown_hi = ScopeEvidence {
                runtime_path: None,
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

        fn models(&self, input: &AgentModelsInput) -> AgentModelsOutput {
            // S03 catalog fixture: the RPC result must carry the projected
            // token list and the create-settings provenance unchanged.
            let supported = input.agent == "zcode";
            AgentModelsOutput {
                agent: input.agent.clone(),
                config_revision: 0,
                supported,
                models: if supported {
                    vec!["zai/GLM-5.3".into(), "deepseek/deepseek-flash".into()]
                } else {
                    Vec::new()
                },
                evidence: ModelCatalogEvidence {
                    source: "zcode_session_create_settings".into(),
                    version: Some("3.8.1".into()),
                    scope: input.scope.clone(),
                    checked_at_ms: 123,
                },
                reason: (!supported).then(|| "native_only".into()),
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
    fn probe_models_and_list_answer_unknown_agents_with_the_configured_roster() {
        let _config_guard = admission_fixtures::config_env_guard();
        let config_root = tempfile::tempdir().unwrap();
        let config_path = config_root.path().join("agents.json");
        std::fs::write(
            &config_path,
            r#"{"schema_version":2,"subagents":{"zcode":{"enabled":false,"spawn_supported":false},"dsh":{"enabled":true,"spawn_supported":false}}}"#,
        )
        .unwrap();
        let _config_scope = crate::rpc::admission_fixtures::ConfigEnvScope::install(&config_path);
        let (_directory, service) = service();
        // zcode is configured but disabled: probing it stays legal, so the
        // roster keeps it listable alongside enabled dsh and defaulted codex.
        let expected =
            "subagent is unknown, available subagents are [\"agy\", \"codex\", \"dsh\", \"zcode\"]";
        let probe = service
            .dispatch(RpcMethod::AgentProbe(AgentProbeInput {
                agent: "future-provider".into(),
                through: crate::agent_status::ProbeLayer::Local,
                scope: ProbeScope::default(),
            }))
            .unwrap_err();
        assert_eq!(probe.code, RpcErrorCode::AgentUnknown);
        assert_eq!(probe.message, expected);
        let models = service
            .dispatch(RpcMethod::AgentModels(AgentModelsInput {
                agent: "future-provider".into(),
                scope: ProbeScope::default(),
            }))
            .unwrap_err();
        assert_eq!(models.code, RpcErrorCode::AgentUnknown);
        assert_eq!(models.message, expected);
        let list = service
            .dispatch(RpcMethod::TaskList(crate::rpc::TaskListQuery {
                agent: Some("future-provider".into()),
                repository: Some("/repository".into()),
                phase: None,
                outcome: None,
                cursor: None,
                limit: 5,
            }))
            .unwrap_err();
        assert_eq!(list.code, RpcErrorCode::AgentUnknown);
        assert_eq!(list.message, expected);
    }

    #[test]
    fn agent_models_rpc_preserves_catalog_result_and_config_identity() {
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
        assert!(catalog.supported);
        assert_eq!(
            catalog.models,
            vec![
                "zai/GLM-5.3".to_owned(),
                "deepseek/deepseek-flash".to_owned()
            ]
        );
        assert_eq!(catalog.reason, None);
        assert_eq!(catalog.evidence.source, "zcode_session_create_settings");
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
            effort: None,
            developer_instructions: None,
        }
    }

    fn service_with_pinned_source(
        directory: &std::path::Path,
        adapter: &str,
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
            permission_mode: Some(external_core::PermissionMode::Plan),
            prompt: "observe gate fixture".into(),
            write_manifest: Vec::new(),
        };
        // The task was accepted for admission but never launched.
        let submitted = scheduler
            .enqueue_general_with_admission(&manifest, Some(admission(adapter)))
            .unwrap();
        let task = submitted;
        let service = Arc::new(RpcService::new(scheduler, store).unwrap());
        (service, task)
    }

    #[test]
    fn task_observe_active_trackers_publish_only_adapter_public_reasoning() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        for adapter in ["zcode", "dsh", "codex"] {
            let directory = tempfile::Builder::new()
                .prefix("s07-active-")
                .tempdir_in(&root)
                .unwrap();
            let (service, task) = service_with_pinned_source(directory.path(), adapter);
            // Supply the launch-scoped verdict; production obtains it from
            // the pinned file (zcode) or the public ACP contract (dsh).
            let tracker = Arc::new(crate::PassiveActivityTracker::for_adapter(
                adapter,
                adapter != "codex",
            ));
            tracker.observe(&crate::RuntimeEvent::Driver(external_runtime::Inbound::Message(
                external_contract::WireMessage::UnknownEvent {
                    method: "session/event".into(),
                    raw: serde_json::json!({"params":{
                        "type":"model.streaming", "eventId":"e1", "turnId":"t1",
                        "payload":{"kind":"reasoning_delta", "delta":"中🙂".repeat(110), "encrypted_content":"SECRET"}
                    }}),
                }
            )));
            service
                .scheduler
                .inner
                .state
                .lock()
                .unwrap()
                .activities
                .insert(task.agent_id.clone(), tracker);
            let RpcSuccess::TaskObserved { observation } = service
                .dispatch(RpcMethod::TaskObserve {
                    agent_id: task.agent_id,
                })
                .unwrap()
            else {
                panic!("expected observation");
            };
            assert!(observation.tools.is_empty());
            assert_eq!(
                observation.coverage.tool_history_complete,
                adapter == "zcode"
            );
            assert_eq!(observation.coverage.reasoning_complete, adapter != "codex");
            assert_eq!(observation.coverage.dropped_events, 0);
            let encoded = serde_json::to_value(observation).unwrap();
            assert!(!encoded.to_string().contains("SECRET"));
            if adapter == "codex" {
                assert!(encoded["reasoning"].is_null());
            } else {
                assert_eq!(encoded["reasoning"]["text"], "中🙂".repeat(100));
                assert_eq!(encoded["reasoning"]["truncated"], true);
            }
        }
    }

    #[test]
    fn task_observe_missing_activity_preserves_adapter_policy_and_coverage() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        for adapter in ["zcode", "dsh", "codex"] {
            let directory = tempfile::Builder::new()
                .prefix("s07-observe-")
                .tempdir_in(&root)
                .unwrap();
            let (service, task) = service_with_pinned_source(directory.path(), adapter);
            let result = service.dispatch(RpcMethod::TaskObserve {
                agent_id: task.agent_id,
            });
            if adapter == "zcode" {
                assert_eq!(result.unwrap_err().code, RpcErrorCode::Unavailable);
                continue;
            }
            let RpcSuccess::TaskObserved { observation } = result.unwrap() else {
                panic!("expected observation");
            };
            assert!(observation.tools.is_empty());
            assert!(!observation.coverage.tool_history_complete);
            assert!(!observation.coverage.reasoning_complete);
            assert_eq!(observation.coverage.dropped_events, 0);
            if adapter == "codex" {
                assert!(serde_json::to_value(&observation).unwrap()["reasoning"].is_null());
            } else {
                let reasoning = observation.reasoning.as_ref().unwrap();
                assert!(reasoning.text.is_empty());
                assert_eq!(reasoning.source, crate::observation::ReasoningSource::dsh());
            }
        }
    }

    #[cfg(test)]
    mod s01_profile_acceptance_tests {
        use super::*;
        use crate::rpc::agents::admission_fixtures::{admission_root, config_env_guard, ConfigEnvScope};
        use crate::rpc::profiles::{load_profile, load_profiles_from_dir, parse_profile_toml};
        use crate::rpc::types::GeneralSubmitInput;
        use crate::{Scheduler, SchedulerConfig};
        use external_agent_codex::session::{codex_posture, thread_start_params, CodexPermissionMode};
        use external_core::{
            GeneralTaskManifest, PermissionMode, PreparedGeneralTask, GENERAL_TASK_SCHEMA,
        };
        const MAX_PROMPT_BYTES: usize = 256 * 1024;
        use external_store::Store;
        use std::fs;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};

        struct TestEnvironment {
            _scope: ConfigEnvScope,
            _config_dir: tempfile::TempDir,
            profiles_dir: PathBuf,
            workspace_dir: tempfile::TempDir,
            service: Arc<RpcService>,
            store: Arc<Store>,
            _guard: crate::rpc::agents::admission_fixtures::ConfigEnvGuard,
        }

        fn setup_environment() -> TestEnvironment {
            let guard = config_env_guard();
            let config_dir = admission_root("s01-test-config-");
            let profiles_dir = config_dir.path().join("profiles");
            fs::create_dir_all(&profiles_dir).unwrap();
            let codex_home = config_dir.path().join("codex-home");
            fs::create_dir_all(&codex_home).unwrap();

            let dummy_bin = config_dir.path().join("dummy_bin");
            fs::write(&dummy_bin, b"#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                let mut perms = fs::metadata(&dummy_bin).unwrap().permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&dummy_bin, perms).unwrap();
            }

            let config_json = serde_json::json!({
                "schema_version": 2,
                "revision": 1,
                "default_subagent": "zcode",
                "subagents": {
                    "zcode": {
                        "enabled": true,
                        "spawn_supported": true,
                        "default_model": null,
                    },
                    "codex": {
                        "enabled": true,
                        "spawn_supported": true,
                        "default_model": "gpt-5",
                        "runtime_path": dummy_bin.to_string_lossy(),
                        "home": codex_home.to_string_lossy(),
                        "profile": "app-server",
                        "version": "test",
                    },
                    "dsh": {
                        "enabled": true,
                        "spawn_supported": true,
                        "default_model": "anthropic:claude-3-7-sonnet",
                        "runtime_path": dummy_bin.to_string_lossy(),
                        "home": null,
                        "profile": "acp",
                        "version": external_agent_dsh::profile::PINNED_DSH_VERSION,
                    },
                    "agy": {
                        "enabled": true,
                        "spawn_supported": true,
                        "default_model": "gemini-2.5-flash",
                        "runtime_path": dummy_bin.to_string_lossy(),
                        "home": null,
                        "profile": "cli",
                        "version": "test",
                    }
                }
            });
            let config_path = config_dir.path().join("agents.json");
            fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();
            let scope = ConfigEnvScope::install(&config_path);

            let workspace_dir = admission_root("s01-test-workspace-");
            let store = Arc::new(Store::open(workspace_dir.path().join("state.sqlite")).unwrap());
            let factory = Arc::new(crate::rpc::wait::wait_tests::FakeRunnableFactory);
            let scheduler = Scheduler::new(
                "s01-test",
                Arc::clone(&store),
                factory,
                SchedulerConfig::default(),
            )
            .unwrap();
            let service = Arc::new(RpcService::new(scheduler, Arc::clone(&store)).unwrap());

            TestEnvironment {
                _scope: scope,
                _config_dir: config_dir,
                profiles_dir,
                workspace_dir,
                service,
                store,
                _guard: guard,
            }
        }

        fn test_manifest(_workspace: &Path, prompt: &str) -> GeneralTaskManifest {
            let ws = admission_root("s01-ws-");
            let path = ws.path().canonicalize().unwrap();
            std::mem::forget(ws);
            GeneralTaskManifest {
                schema: GENERAL_TASK_SCHEMA.into(),
                agent_id: String::new(),
                repository: path,
                permission_mode: None,
                prompt: prompt.into(),
                write_manifest: Vec::new(),
            }
        }

        #[test]
        fn ac1_profile_loading_validation_and_errors() {
            let env = setup_environment();

            // 1. Valid profile with Unicode and multiline instructions
            let valid_toml = r#"
name = "full_worker"
subagent = "zcode"
permission_mode = "edit"
model = "zai/GLM-5.3"
effort = "high"
developer_instructions = """
Line 1: 遵循准则
Line 2: Be precise
"""
"#;
            fs::write(env.profiles_dir.join("worker.toml"), valid_toml).unwrap();
            let loaded = load_profile("full_worker").unwrap();
            assert_eq!(loaded.name, "full_worker");
            assert_eq!(loaded.subagent.as_deref(), Some("zcode"));
            assert_eq!(loaded.permission_mode, Some(PermissionMode::Edit));
            assert_eq!(loaded.model.as_deref(), Some("zai/GLM-5.3"));
            assert_eq!(loaded.effort.as_deref(), Some("high"));
            assert_eq!(
                loaded.developer_instructions.as_deref(),
                Some("Line 1: 遵循准则\nLine 2: Be precise\n")
            );

            // 2. Error reporting specifies file path and field name
            // 2a. Unknown field
            let unknown_field_toml = "name = \"bad\"\nunknown_key = 123\n";
            let err = parse_profile_toml(unknown_field_toml, Path::new("bad_field.toml")).unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Validation);
            assert!(err.message.contains("bad_field.toml"));
            assert!(err.message.contains("unknown_key"));

            // 2b. Missing name
            let missing_name_toml = "subagent = \"zcode\"\n";
            let err = parse_profile_toml(missing_name_toml, Path::new("missing_name.toml")).unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Validation);
            assert!(err.message.contains("missing_name.toml"));
            assert!(err.message.contains("name"));

            // 2c. Empty name
            let empty_name_toml = "name = \"  \"\n";
            let err = parse_profile_toml(empty_name_toml, Path::new("empty_name.toml")).unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Validation);
            assert!(err.message.contains("empty_name.toml"));
            assert!(err.message.contains("name"));

            // 2d. Name > 128 bytes
            let long_name = "x".repeat(129);
            let long_name_toml = format!("name = \"{long_name}\"\n");
            let err = parse_profile_toml(&long_name_toml, Path::new("long_name.toml")).unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Validation);
            assert!(err.message.contains("long_name.toml"));
            assert!(err.message.contains("128 bytes"));

            // 2e. Invalid type
            let bad_type_toml = "name = \"ok\"\nsubagent = 999\n";
            let err = parse_profile_toml(bad_type_toml, Path::new("bad_type.toml")).unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Validation);
            assert!(err.message.contains("bad_type.toml"));
            assert!(err.message.contains("subagent"));

            // 2f. Duplicate profile name across files
            let dup_dir = tempfile::tempdir().unwrap();
            fs::write(dup_dir.path().join("p1.toml"), "name = \"same_name\"\n").unwrap();
            fs::write(dup_dir.path().join("p2.toml"), "name = \"same_name\"\n").unwrap();
            let err = load_profiles_from_dir(dup_dir.path()).unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Validation);
            assert!(err.message.contains("same_name"));
            assert!(err.message.contains("duplicate profile name"));

            // 3. Missing or empty directory reports "available profiles: none"
            let empty_dir = tempfile::tempdir().unwrap();
            let profiles = load_profiles_from_dir(empty_dir.path()).unwrap();
            assert!(profiles.is_empty());
            let err_empty = load_profiles_from_dir(&empty_dir.path().join("not_found")).unwrap();
            assert!(err_empty.is_empty());
        }

        #[test]
        fn ac2_daemon_mutex_four_fields_and_precedence() {
            let env = setup_environment();

            // Create a valid profile
            fs::write(env.profiles_dir.join("worker.toml"), "name = \"worker\"\n").unwrap();

            // 1. Daemon mutex validation: profile cannot be combined with agent, model, effort, or permission_mode
            let test_cases = [
                (Some("zcode"), None, None, None),
                (None, Some("gpt-5"), None, None),
                (None, None, Some("high"), None),
                (None, None, None, Some(PermissionMode::Plan)),
            ];

            for (agent, model, effort, mode) in test_cases {
                let mut manifest = test_manifest(env.workspace_dir.path(), "test prompt");
                manifest.permission_mode = mode;
                let input = GeneralSubmitInput {
                    agent: agent.map(str::to_owned),
                    model: model.map(str::to_owned),
                    effort: effort.map(str::to_owned),
                    profile: Some("worker".into()),
                    manifest,
                };
                let err = env.service.dispatch(RpcMethod::SubmitGeneral(input)).unwrap_err();
                assert_eq!(err.code, RpcErrorCode::Validation);
                assert!(
                    err.message.contains("profile cannot be combined with subagent, permission_mode, model, or effort"),
                    "Error must contain user guidance: {}",
                    err.message
                );
            }

            // 2. Mutex validation runs BEFORE file read:
            // Non-existent profile + conflicting parameter returns MUTEX error, not "profile not found"!
            let input_non_existent = GeneralSubmitInput {
                agent: Some("codex".into()),
                model: None,
                effort: None,
                profile: Some("non_existent_profile_xyz".into()),
                manifest: test_manifest(env.workspace_dir.path(), "test prompt"),
            };
            let err = env.service.dispatch(RpcMethod::SubmitGeneral(input_non_existent)).unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Validation);
            assert!(
                err.message.contains("profile cannot be combined with"),
                "Mutex check must precede profile file read: {}",
                err.message
            );
            assert!(!err.message.contains("not found"));
        }

        #[test]
        fn ac2_direct_rpc_permission_mode_four_cases_and_wire_null() {
            let env = setup_environment();
            fs::write(env.profiles_dir.join("worker.toml"), "name = \"worker\"\n").unwrap();

            // Case 1: Direct RPC with explicit "permission_mode": "build" + profile -> rejected by daemon mutex
            let raw_case1 = serde_json::json!({
                "profile": "worker",
                "manifest": {
                    "schema": GENERAL_TASK_SCHEMA,
                    "agent_id": "",
                    "repository": env.workspace_dir.path().canonicalize().unwrap(),
                    "permission_mode": "build",
                    "prompt": "test prompt",
                    "write_manifest": []
                }
            });
            let input_case1: GeneralSubmitInput = serde_json::from_value(raw_case1).unwrap();
            let err_case1 = env.service.dispatch(RpcMethod::SubmitGeneral(input_case1)).unwrap_err();
            assert_eq!(err_case1.code, RpcErrorCode::Validation);
            assert!(err_case1.message.contains("profile cannot be combined with"));

            // Case 2: Direct RPC with explicit "permission_mode": "plan" + profile -> rejected by daemon mutex
            let raw_case2 = serde_json::json!({
                "profile": "worker",
                "manifest": {
                    "schema": GENERAL_TASK_SCHEMA,
                    "agent_id": "",
                    "repository": env.workspace_dir.path().canonicalize().unwrap(),
                    "permission_mode": "plan",
                    "prompt": "test prompt",
                    "write_manifest": []
                }
            });
            let input_case2: GeneralSubmitInput = serde_json::from_value(raw_case2).unwrap();
            let err_case2 = env.service.dispatch(RpcMethod::SubmitGeneral(input_case2)).unwrap_err();
            assert_eq!(err_case2.code, RpcErrorCode::Validation);
            assert!(err_case2.message.contains("profile cannot be combined with"));

            // Case 3: Direct RPC with explicit "permission_mode": null + profile -> rejected at wire deserialization
            let raw_case3 = serde_json::json!({
                "profile": "worker",
                "manifest": {
                    "schema": GENERAL_TASK_SCHEMA,
                    "agent_id": "",
                    "repository": env.workspace_dir.path().canonicalize().unwrap(),
                    "permission_mode": null,
                    "prompt": "test prompt",
                    "write_manifest": []
                }
            });
            assert!(
                serde_json::from_value::<GeneralSubmitInput>(raw_case3).is_err(),
                "Explicit null permission_mode must be rejected by optional_non_null deserializer"
            );

            // Case 4: Direct RPC with omitted permission_mode + profile -> passes deserialization and mutex, proceeds
            let raw_case4 = serde_json::json!({
                "profile": "worker",
                "manifest": {
                    "schema": GENERAL_TASK_SCHEMA,
                    "agent_id": "",
                    "repository": env.workspace_dir.path().canonicalize().unwrap(),
                    "prompt": "test prompt",
                    "write_manifest": []
                }
            });
            let input_case4: GeneralSubmitInput = serde_json::from_value(raw_case4).unwrap();
            assert_eq!(input_case4.manifest.permission_mode, None);
            let res_case4 = env.service.dispatch(RpcMethod::SubmitGeneral(input_case4));
            assert!(res_case4.is_ok());

            // Negative case: non-profile direct RPC with explicit "permission_mode": null -> rejected at wire deserialization
            let raw_non_profile_null = serde_json::json!({
                "manifest": {
                    "schema": GENERAL_TASK_SCHEMA,
                    "agent_id": "",
                    "repository": env.workspace_dir.path().canonicalize().unwrap(),
                    "permission_mode": null,
                    "prompt": "test prompt",
                    "write_manifest": []
                }
            });
            assert!(
                serde_json::from_value::<GeneralSubmitInput>(raw_non_profile_null).is_err(),
                "Explicit null permission_mode on non-profile request must be rejected"
            );
        }

        #[test]
        fn ac3_profile_four_fields_merge_and_defaults() {
            let env = setup_environment();

            // 1. Profile with all four fields explicit
            let full_toml = r#"
name = "full_profile"
subagent = "zcode"
permission_mode = "edit"
model = "zai/GLM-5.3"
effort = "high"
"#;
            fs::write(env.profiles_dir.join("full.toml"), full_toml).unwrap();

            let input = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("full_profile".into()),
                manifest: test_manifest(env.workspace_dir.path(), "merge test"),
            };
            let response = env.service.dispatch(RpcMethod::SubmitGeneral(input)).unwrap();
            let RpcSuccess::GeneralSubmitted { task } = response else { panic!() };

            let record = env.store.get_task(&task.agent_id).unwrap().unwrap();
            let prepared: PreparedGeneralTask = serde_json::from_str(&record.prepared_launch_json).unwrap();
            assert_eq!(prepared.permission_mode, PermissionMode::Edit);
            let admission = prepared.admission.unwrap();
            assert_eq!(admission.agent, "zcode");
            assert_eq!(admission.model.as_deref(), Some("zai/GLM-5.3"));
            assert_eq!(admission.model_source, "spawn_catalog");
            assert_eq!(admission.effort.as_deref(), Some("high"));

            // 2. Profile with omitted/empty fields falls back to defaults
            let minimal_toml = r#"
name = "minimal_profile"
"#;
            fs::write(env.profiles_dir.join("minimal.toml"), minimal_toml).unwrap();

            let input_min = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("minimal_profile".into()),
                manifest: test_manifest(env.workspace_dir.path(), "minimal test"),
            };
            let response_min = env.service.dispatch(RpcMethod::SubmitGeneral(input_min)).unwrap();
            let RpcSuccess::GeneralSubmitted { task: task_min } = response_min else { panic!() };

            let record_min = env.store.get_task(&task_min.agent_id).unwrap().unwrap();
            let prepared_min: PreparedGeneralTask = serde_json::from_str(&record_min.prepared_launch_json).unwrap();
            assert_eq!(prepared_min.permission_mode, PermissionMode::Build);
            let admission_min = prepared_min.admission.unwrap();
            assert_eq!(admission_min.agent, "zcode"); // defaulted to config.default_subagent
            assert_eq!(admission_min.model, None);
            assert_eq!(admission_min.effort, None);
        }

        #[test]
        fn ac3_model_token_conventions_for_subagents() {
            let env = setup_environment();

            // 1. dsh: provider:model format accepted
            let dsh_toml = r#"
name = "dsh_worker"
subagent = "dsh"
model = "anthropic:claude-3-7-sonnet"
"#;
            fs::write(env.profiles_dir.join("dsh.toml"), dsh_toml).unwrap();
            let input = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("dsh_worker".into()),
                manifest: test_manifest(env.workspace_dir.path(), "dsh prompt"),
            };
            assert!(env.service.dispatch(RpcMethod::SubmitGeneral(input)).is_ok());

            // 1b. dsh: invalid model format (no colon) rejected
            let dsh_bad_toml = r#"
name = "dsh_bad"
subagent = "dsh"
model = "bare_model_without_colon"
"#;
            fs::write(env.profiles_dir.join("dsh_bad.toml"), dsh_bad_toml).unwrap();
            let input_bad = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("dsh_bad".into()),
                manifest: test_manifest(env.workspace_dir.path(), "dsh prompt"),
            };
            let err = env.service.dispatch(RpcMethod::SubmitGeneral(input_bad)).unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Validation);
            assert!(err.message.contains("{provider}:{model}"));

            // 2. zcode: provider/model or bare token accepted
            let zcode_slash_toml = r#"
name = "zcode_slash"
subagent = "zcode"
model = "provider/model-name"
"#;
            fs::write(env.profiles_dir.join("zcode_slash.toml"), zcode_slash_toml).unwrap();
            let input_z1 = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("zcode_slash".into()),
                manifest: test_manifest(env.workspace_dir.path(), "zcode prompt"),
            };
            assert!(env.service.dispatch(RpcMethod::SubmitGeneral(input_z1)).is_ok());

            let zcode_bare_toml = r#"
name = "zcode_bare"
subagent = "zcode"
model = "bare_model"
"#;
            fs::write(env.profiles_dir.join("zcode_bare.toml"), zcode_bare_toml).unwrap();
            let input_z2 = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("zcode_bare".into()),
                manifest: test_manifest(env.workspace_dir.path(), "zcode prompt"),
            };
            assert!(env.service.dispatch(RpcMethod::SubmitGeneral(input_z2)).is_ok());

            // 3. codex: bare slug accepted, slash rejected
            let codex_slug_toml = r#"
name = "codex_slug"
subagent = "codex"
model = "gpt-5"
"#;
            fs::write(env.profiles_dir.join("codex_slug.toml"), codex_slug_toml).unwrap();
            let input_c1 = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("codex_slug".into()),
                manifest: test_manifest(env.workspace_dir.path(), "codex prompt"),
            };
            assert!(env.service.dispatch(RpcMethod::SubmitGeneral(input_c1)).is_ok());

            let codex_slash_toml = r#"
name = "codex_slash"
subagent = "codex"
model = "openai/gpt-5"
"#;
            fs::write(env.profiles_dir.join("codex_slash.toml"), codex_slash_toml).unwrap();
            let input_c2 = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("codex_slash".into()),
                manifest: test_manifest(env.workspace_dir.path(), "codex prompt"),
            };
            let err_c2 = env.service.dispatch(RpcMethod::SubmitGeneral(input_c2)).unwrap_err();
            assert_eq!(err_c2.code, RpcErrorCode::Validation);
        }

        #[test]
        fn ac4_developer_instructions_dispatch_channel() {
            let env = setup_environment();

            // 1. Codex: developer_instructions routed via native channel, initial_prompt is verbatim
            let codex_toml = r#"
name = "codex_worker"
subagent = "codex"
model = "gpt-5"
developer_instructions = "Custom instructions for codex"
"#;
            fs::write(env.profiles_dir.join("codex.toml"), codex_toml).unwrap();
            let input_codex = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("codex_worker".into()),
                manifest: test_manifest(env.workspace_dir.path(), "Verbatim codex prompt"),
            };
            let response_codex = env.service.dispatch(RpcMethod::SubmitGeneral(input_codex)).unwrap();
            let RpcSuccess::GeneralSubmitted { task: task_codex } = response_codex else { panic!() };

            let record_codex = env.store.get_task(&task_codex.agent_id).unwrap().unwrap();
            // initial_prompt is verbatim (NOT spliced!)
            assert_eq!(record_codex.initial_prompt, "Verbatim codex prompt");
            let prepared_codex: PreparedGeneralTask = serde_json::from_str(&record_codex.prepared_launch_json).unwrap();
            let admission_codex = prepared_codex.admission.unwrap();
            assert_eq!(
                admission_codex.developer_instructions.as_deref(),
                Some("Custom instructions for codex")
            );

            // Verify thread_start_params serialization with developerInstructions
            let posture = codex_posture(CodexPermissionMode::WorkspaceWrite);
            let params = thread_start_params(
                "gpt-5",
                "/tmp/ws",
                &posture,
                admission_codex.developer_instructions.as_deref(),
            );
            assert_eq!(
                params["developerInstructions"],
                "Custom instructions for codex"
            );

            // 2. Non-codex (zcode): prompt is spliced with Developer Instructions prefix
            let zcode_toml = r#"
name = "zcode_worker"
subagent = "zcode"
developer_instructions = "Custom instructions for zcode"
"#;
            fs::write(env.profiles_dir.join("zcode.toml"), zcode_toml).unwrap();
            let input_zcode = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("zcode_worker".into()),
                manifest: test_manifest(env.workspace_dir.path(), "Original zcode prompt"),
            };
            let response_zcode = env.service.dispatch(RpcMethod::SubmitGeneral(input_zcode)).unwrap();
            let RpcSuccess::GeneralSubmitted { task: task_zcode } = response_zcode else { panic!() };

            let record_zcode = env.store.get_task(&task_zcode.agent_id).unwrap().unwrap();
            // initial_prompt is spliced
            assert_eq!(
                record_zcode.initial_prompt,
                "Developer Instructions: Custom instructions for zcode\n----------\nOriginal zcode prompt"
            );
            let prepared_zcode: PreparedGeneralTask = serde_json::from_str(&record_zcode.prepared_launch_json).unwrap();
            assert_eq!(
                prepared_zcode.admission.unwrap().developer_instructions,
                None
            );

            // 2b. Non-codex (dsh): prompt is spliced with Developer Instructions prefix
            let dsh_toml = r#"
name = "dsh_worker"
subagent = "dsh"
model = "anthropic:claude-3-7-sonnet"
developer_instructions = "Custom instructions for dsh"
"#;
            fs::write(env.profiles_dir.join("dsh.toml"), dsh_toml).unwrap();
            let input_dsh = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("dsh_worker".into()),
                manifest: test_manifest(env.workspace_dir.path(), "Original dsh prompt"),
            };
            let response_dsh = env.service.dispatch(RpcMethod::SubmitGeneral(input_dsh)).unwrap();
            let RpcSuccess::GeneralSubmitted { task: task_dsh } = response_dsh else { panic!() };
            let record_dsh = env.store.get_task(&task_dsh.agent_id).unwrap().unwrap();
            assert_eq!(
                record_dsh.initial_prompt,
                "Developer Instructions: Custom instructions for dsh\n----------\nOriginal dsh prompt"
            );
            let prepared_dsh: PreparedGeneralTask = serde_json::from_str(&record_dsh.prepared_launch_json).unwrap();
            assert_eq!(prepared_dsh.admission.unwrap().developer_instructions, None);

            // 2c. Non-codex (agy): prompt is spliced with Developer Instructions prefix
            let agy_toml = r#"
name = "agy_worker"
subagent = "agy"
model = "gemini-2.5-flash"
effort = "high"
developer_instructions = "Custom instructions for agy"
"#;
            fs::write(env.profiles_dir.join("agy.toml"), agy_toml).unwrap();
            let input_agy = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("agy_worker".into()),
                manifest: test_manifest(env.workspace_dir.path(), "Original agy prompt"),
            };
            let response_agy = env.service.dispatch(RpcMethod::SubmitGeneral(input_agy)).unwrap();
            let RpcSuccess::GeneralSubmitted { task: task_agy } = response_agy else { panic!() };
            let record_agy = env.store.get_task(&task_agy.agent_id).unwrap().unwrap();
            assert_eq!(
                record_agy.initial_prompt,
                "Developer Instructions: Custom instructions for agy\n----------\nOriginal agy prompt"
            );
            let prepared_agy: PreparedGeneralTask = serde_json::from_str(&record_agy.prepared_launch_json).unwrap();
            assert_eq!(prepared_agy.admission.unwrap().developer_instructions, None);

            // 3. Empty developer_instructions: prompt unchanged on codex and zcode
            let empty_toml = r#"
name = "empty_di_worker"
subagent = "zcode"
developer_instructions = ""
"#;
            fs::write(env.profiles_dir.join("empty.toml"), empty_toml).unwrap();
            let input_empty = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("empty_di_worker".into()),
                manifest: test_manifest(env.workspace_dir.path(), "Unchanged prompt"),
            };
            let response_empty = env.service.dispatch(RpcMethod::SubmitGeneral(input_empty)).unwrap();
            let RpcSuccess::GeneralSubmitted { task: task_empty } = response_empty else { panic!() };
            let record_empty = env.store.get_task(&task_empty.agent_id).unwrap().unwrap();
            assert_eq!(record_empty.initial_prompt, "Unchanged prompt");

            let codex_empty_toml = r#"
name = "codex_empty_worker"
subagent = "codex"
developer_instructions = ""
"#;
            fs::write(env.profiles_dir.join("codex_empty.toml"), codex_empty_toml).unwrap();
            let input_codex_empty = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("codex_empty_worker".into()),
                manifest: test_manifest(env.workspace_dir.path(), "Unchanged codex prompt"),
            };
            let response_codex_empty = env.service.dispatch(RpcMethod::SubmitGeneral(input_codex_empty)).unwrap();
            let RpcSuccess::GeneralSubmitted { task: task_codex_empty } = response_codex_empty else { panic!() };
            let record_codex_empty = env.store.get_task(&task_codex_empty.agent_id).unwrap().unwrap();
            assert_eq!(record_codex_empty.initial_prompt, "Unchanged codex prompt");
            let prepared_codex_empty: PreparedGeneralTask = serde_json::from_str(&record_codex_empty.prepared_launch_json).unwrap();
            assert_eq!(prepared_codex_empty.admission.unwrap().developer_instructions, None);

            // 4. Spliced prompt exceeding MAX_PROMPT_BYTES (256 KiB) is rejected
            let di_text = "Instructions: ".repeat(20); // ~280 bytes
            let big_di_toml = format!(
                "name = \"big_di\"\nsubagent = \"zcode\"\ndeveloper_instructions = \"{di_text}\"\n"
            );
            fs::write(env.profiles_dir.join("big_di.toml"), big_di_toml).unwrap();
            // Make base prompt just 50 bytes under MAX_PROMPT_BYTES so splicing exceeds the limit
            let base_prompt = "a".repeat(MAX_PROMPT_BYTES - 50);
            let input_oversized = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("big_di".into()),
                manifest: test_manifest(env.workspace_dir.path(), &base_prompt),
            };
            let err_oversized = env.service.dispatch(RpcMethod::SubmitGeneral(input_oversized)).unwrap_err();
            assert_eq!(err_oversized.code, RpcErrorCode::Validation);
        }

        #[test]
        fn b02_invalid_profile_reference_outputs_diagnostic_and_available_list() {
            let env = setup_environment();

            // Create good.toml and bad.toml
            let good_toml = r#"
name = "good"
subagent = "zcode"
permission_mode = "edit"
"#;
            let bad_toml = r#"
name = "bad"
permission_mode = "superuser"
"#;
            fs::write(env.profiles_dir.join("good.toml"), good_toml).unwrap();
            fs::write(env.profiles_dir.join("bad.toml"), bad_toml).unwrap();

            // 1. Reference bad profile -> returns error containing bad diagnostic AND available profiles: [good]
            let input_bad = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("bad".into()),
                manifest: test_manifest(env.workspace_dir.path(), "test prompt"),
            };
            let err_bad = env.service.dispatch(RpcMethod::SubmitGeneral(input_bad)).unwrap_err();
            assert_eq!(err_bad.code, RpcErrorCode::Validation);
            assert!(err_bad.message.contains("bad.toml"));
            assert!(err_bad.message.contains("superuser"));
            assert!(err_bad.message.contains("available profiles: [good]"));

            // 2. Reference good profile -> succeeds
            let input_good = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("good".into()),
                manifest: test_manifest(env.workspace_dir.path(), "test prompt"),
            };
            let res_good = env.service.dispatch(RpcMethod::SubmitGeneral(input_good));
            assert!(res_good.is_ok());

            // 3. Reference invalid profile parameter (e.g. empty) -> returns error containing available profiles: [good]
            let input_invalid_param = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("".into()),
                manifest: test_manifest(env.workspace_dir.path(), "test prompt"),
            };
            let err_inv = env.service.dispatch(RpcMethod::SubmitGeneral(input_invalid_param)).unwrap_err();
            assert_eq!(err_inv.code, RpcErrorCode::Validation);
            assert!(err_inv.message.contains("profile is invalid"));
            assert!(err_inv.message.contains("available profiles: [good]"));

            // 4. Reference unknown profile -> returns error containing available profiles: [good]
            let input_missing = GeneralSubmitInput {
                agent: None,
                model: None,
                effort: None,
                profile: Some("missing_profile".into()),
                manifest: test_manifest(env.workspace_dir.path(), "test prompt"),
            };
            let err_missing = env.service.dispatch(RpcMethod::SubmitGeneral(input_missing)).unwrap_err();
            assert_eq!(err_missing.code, RpcErrorCode::Validation);
            assert!(err_missing.message.contains("profile 'missing_profile' not found"));
            assert!(err_missing.message.contains("available profiles: [good]"));
        }

        #[test]
        fn ac5_discovery_and_truncation_fidelity() {
            let env = setup_environment();

            // 1. Profiles listed in stable alphabetical sort
            fs::write(env.profiles_dir.join("c.toml"), "name = \"charlie\"\n").unwrap();
            fs::write(env.profiles_dir.join("a.toml"), "name = \"alpha\"\n").unwrap();
            fs::write(env.profiles_dir.join("b.toml"), "name = \"bravo\"\n").unwrap();

            let err = load_profile("missing").unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Validation);
            assert_eq!(
                err.message,
                "profile 'missing' not found; available profiles: [alpha, bravo, charlie]"
            );

            // 2. Large ASCII list > 512 bytes preserved without truncation in RPC and MCP
            let large_dir = tempfile::tempdir().unwrap();
            for i in 0..40 {
                let name = format!("profile_worker_long_name_{:02}", i);
                fs::write(
                    large_dir.path().join(format!("p_{:02}.toml", i)),
                    format!("name = \"{name}\"\n"),
                )
                .unwrap();
            }
            let profiles = load_profiles_from_dir(large_dir.path()).unwrap();
            let mut names: Vec<String> = profiles.into_keys().collect();
            names.sort();
            let err_msg = crate::rpc::profiles::format_unknown_profile_error("missing", &names);
            assert!(
                err_msg.len() > 512,
                "List must exceed 512 bytes: len={}",
                err_msg.len()
            );
            assert!(err_msg.contains("profile_worker_long_name_39"));

            let rpc_err = RpcError::new_profile_error(RpcErrorCode::Validation, &err_msg);
            assert_eq!(rpc_err.message, err_msg, "RPC message must not be truncated to 512 bytes");

            // 3. Large Unicode list > 512 bytes character boundary safe without panic
            let mut unicode_names = Vec::new();
            for i in 0..30 {
                unicode_names.push(format!("中文配置预设名称_{:02}", i));
            }
            let unicode_err_msg = crate::rpc::profiles::format_unknown_profile_error("missing", &unicode_names);
            assert!(unicode_err_msg.len() > 512);
            assert!(unicode_err_msg.contains("中文配置预设名称_29"));

            let unicode_rpc_err = RpcError::new_profile_error(RpcErrorCode::Validation, &unicode_err_msg);
            assert_eq!(unicode_rpc_err.message, unicode_err_msg);
        }

        #[test]
        fn ac6_non_profile_regression() {
            let env = setup_environment();

            // Request without profile: explicit subagent, model, effort, permission_mode
            let mut manifest = test_manifest(env.workspace_dir.path(), "regression prompt");
            manifest.permission_mode = Some(PermissionMode::Plan);
            let input = GeneralSubmitInput {
                agent: Some("zcode".into()),
                model: Some("zai/GLM-5.3".into()),
                effort: Some("high".into()),
                profile: None,
                manifest,
            };
            let response = env.service.dispatch(RpcMethod::SubmitGeneral(input)).unwrap();
            let RpcSuccess::GeneralSubmitted { task } = response else { panic!() };

            let record = env.store.get_task(&task.agent_id).unwrap().unwrap();
            let prepared: PreparedGeneralTask = serde_json::from_str(&record.prepared_launch_json).unwrap();
            assert_eq!(prepared.permission_mode, PermissionMode::Plan);
            let admission = prepared.admission.unwrap();
            assert_eq!(admission.agent, "zcode");
            assert_eq!(admission.model.as_deref(), Some("zai/GLM-5.3"));
            assert_eq!(admission.effort.as_deref(), Some("high"));
            assert_eq!(admission.developer_instructions, None);
            assert_eq!(record.initial_prompt, "regression prompt");

            // Request without profile and omitted permission_mode defaults to Build
            let mut manifest_default = test_manifest(env.workspace_dir.path(), "default mode prompt");
            manifest_default.permission_mode = None;
            let input_default = GeneralSubmitInput {
                agent: Some("zcode".into()),
                model: None,
                effort: None,
                profile: None,
                manifest: manifest_default,
            };
            let response_default = env.service.dispatch(RpcMethod::SubmitGeneral(input_default)).unwrap();
            let RpcSuccess::GeneralSubmitted { task: task_def } = response_default else { panic!() };
            let record_def = env.store.get_task(&task_def.agent_id).unwrap().unwrap();
            let prepared_def: PreparedGeneralTask = serde_json::from_str(&record_def.prepared_launch_json).unwrap();
            assert_eq!(prepared_def.permission_mode, PermissionMode::Build);
        }
    }
}
