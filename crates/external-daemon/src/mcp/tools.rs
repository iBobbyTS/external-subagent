//! The public MCP server: tool catalog, RPC plumbing, and tool handlers.
//!
//! Extracted mechanically from the former private `mcp::server` module; the
//! facade at `crate::mcp` keeps every historical path importable.
use super::errors::{
    protocol_error, public_error_for_op, public_transport_error, validation_error, ToolError,
};
use super::schemas::{
    tool_output_schema, validate_text, AgentInput, AgentListInput, AgentRespondInput,
    AgentIdOrList, AgentResultInput, AgentSendInput, AgentSpawnInput, AgentWaitInput, EmptyInput,
    PublicPermissionMode, StatusInput, MAX_MESSAGE_BYTES,
};
use super::types::{internal_task_id, public_task_id, PublicDecision, PublicResponseDisposition};
use super::views::{
    AgentListOutput, AgentObserveOutput, AgentRespondOutput, AgentResultOutput, AgentSendOutput,
    AgentSpawnOutput, AgentStateOutput, AgentWaitAnyOutput, AgentWaitElement, AgentWaitManyOutput,
    AgentWaitOutput, PublicMessageDisposition, PublicResult, PublicTask, SPAWN_INSTRUCTION,
    SystemStatusOutput,
};
use crate::rpc::{
    GeneralSubmitInput, MessageInput, RespondInput, ResponseDecision, ResponseOutcomeView,
    RpcClient, RpcMethod, RpcOutcome, RpcRequest, RpcService, RpcSuccess, TaskListQuery,
    TaskWaitQuery, WaitTarget,
};
use external_core::{GeneralTaskManifest, GENERAL_TASK_SCHEMA};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    service::RequestContext,
    tool, tool_handler, tool_router, Json, RoleServer, ServerHandler, ServiceExt,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

#[cfg(test)]
use super::schemas::default_result_limit;

/// Upper bound on the number of agent ids accepted by a list-mode wait;
/// the canonical constant lives in the RPC protocol limits.
pub use crate::rpc::MAX_WAIT_AGENTS;

pub const PUBLIC_TOOLS: [&str; 10] = [
    "external_subagent_cancel",
    "external_subagent_close",
    "external_subagent_list",
    "external_subagent_observe",
    "external_subagent_respond",
    "external_subagent_result",
    "external_subagent_send",
    "external_subagent_spawn",
    "external_subagent_status",
    "external_subagent_wait",
];

#[derive(Clone)]
pub struct SubagentMcp {
    socket: Option<PathBuf>,
    pub(crate) timeout: Duration,
    service: Option<Arc<RpcService>>,
    next_request: Arc<AtomicU64>,
    tool_router: ToolRouter<Self>,
    #[cfg(test)]
    wait_handler_interrupted: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    spawn_handler_interrupted: Option<Arc<AtomicBool>>,
}

impl SubagentMcp {
    pub fn new(socket: PathBuf, timeout: Duration) -> Self {
        Self {
            socket: Some(socket),
            timeout: timeout.max(Duration::from_secs(125)),
            service: None,
            next_request: Arc::new(AtomicU64::new(1)),
            tool_router: Self::tool_router(),
            #[cfg(test)]
            wait_handler_interrupted: None,
            #[cfg(test)]
            spawn_handler_interrupted: None,
        }
    }

    pub fn from_service(service: Arc<RpcService>) -> Self {
        Self {
            socket: None,
            timeout: Duration::from_secs(5),
            service: Some(service),
            next_request: Arc::new(AtomicU64::new(1)),
            tool_router: Self::tool_router(),
            #[cfg(test)]
            wait_handler_interrupted: None,
            #[cfg(test)]
            spawn_handler_interrupted: None,
        }
    }

    fn rpc(&self, method: RpcMethod) -> Result<RpcSuccess, ToolError> {
        self.rpc_interruptible(method, &|| false)
    }

    async fn rpc_wait(
        &self,
        query: TaskWaitQuery,
        request_cancelled: impl Fn() -> bool + Send + 'static,
    ) -> Result<RpcSuccess, ToolError> {
        struct InterruptOnDrop {
            interrupted: Arc<AtomicBool>,
            #[cfg(test)]
            handler_interrupted: Option<Arc<AtomicBool>>,
        }
        impl Drop for InterruptOnDrop {
            fn drop(&mut self) {
                self.interrupted.store(true, Ordering::Release);
                #[cfg(test)]
                if let Some(latch) = &self.handler_interrupted {
                    latch.store(true, Ordering::Release);
                }
            }
        }
        let interrupted = Arc::new(AtomicBool::new(false));
        let _guard = InterruptOnDrop {
            interrupted: Arc::clone(&interrupted),
            #[cfg(test)]
            handler_interrupted: self.wait_handler_interrupted.clone(),
        };
        let mut facade = self.clone();
        facade.timeout = facade
            .timeout
            .max(Duration::from_secs(query.wait_time.saturating_add(5)));
        tokio::task::spawn_blocking(move || {
            facade.rpc_interruptible(RpcMethod::TaskWait(query), &|| {
                interrupted.load(Ordering::Acquire) || request_cancelled()
            })
        })
        .await
        .map_err(|_| protocol_error().with_operation("wait"))?
    }

    async fn rpc_spawn(
        &self,
        input: GeneralSubmitInput,
        request_cancelled: impl Fn() -> bool + Send + 'static,
    ) -> Result<RpcSuccess, ToolError> {
        struct InterruptOnDrop {
            interrupted: Arc<AtomicBool>,
            #[cfg(test)]
            handler_interrupted: Option<Arc<AtomicBool>>,
        }
        impl Drop for InterruptOnDrop {
            fn drop(&mut self) {
                self.interrupted.store(true, Ordering::Release);
                #[cfg(test)]
                if let Some(latch) = &self.handler_interrupted {
                    latch.store(true, Ordering::Release);
                }
            }
        }
        let interrupted = Arc::new(AtomicBool::new(false));
        let _guard = InterruptOnDrop {
            interrupted: Arc::clone(&interrupted),
            #[cfg(test)]
            handler_interrupted: self.spawn_handler_interrupted.clone(),
        };
        let mut facade = self.clone();
        facade.timeout = facade.timeout.max(Duration::from_secs(125));
        tokio::task::spawn_blocking(move || {
            facade.rpc_interruptible(RpcMethod::SubmitGeneral(input), &|| {
                interrupted.load(Ordering::Acquire) || request_cancelled()
            })
        })
        .await
        .map_err(|_| protocol_error().with_operation("spawn"))?
    }

    fn rpc_interruptible(
        &self,
        method: RpcMethod,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<RpcSuccess, ToolError> {
        let (operation, agent_id) = rpc_context(&method);
        let request = RpcRequest {
            request_id: format!(
                "subagent-mcp-{}",
                self.next_request.fetch_add(1, Ordering::Relaxed)
            ),
            method,
        };
        let request_id = request.request_id.clone();
        let encoded = serde_json::to_vec(&request).map_err(|_| {
            validation_error("request encoding failed")
                .with_operation(operation)
                .with_request_id(request_id.clone())
                .with_agent_id(agent_id.clone())
        })?;
        if encoded.len() + 1 > 512 * 1024 {
            return Err(validation_error("encoded RPC request exceeds frame cap")
                .with_operation(operation)
                .with_request_id(request_id)
                .with_agent_id(agent_id));
        }
        let response = if let Some(service) = &self.service {
            service.handle_bytes_interruptible(&encoded, interrupted)
        } else {
            RpcClient::new(
                self.socket.as_ref().expect("socket configured"),
                self.timeout,
            )
            .call_interruptible(&request, interrupted)
            .map_err(|error| {
                public_transport_error(error)
                    .with_operation(operation)
                    .with_request_id(request.request_id.clone())
                    .with_agent_id(agent_id.clone())
            })?
        };
        if response.request_id.as_deref() != Some(request_id.as_str()) {
            return Err(ToolError::new(
                "protocol_error",
                "unexpected daemon response",
                "protocol_error: daemon answered a different request id",
                "daemon",
            )
            .with_operation(operation)
            .with_request_id(request_id)
            .with_agent_id(agent_id));
        }
        match response.outcome {
            RpcOutcome::Success { result } => Ok(*result),
            RpcOutcome::Error { error } => Err(public_error_for_op(error, operation)
                .with_operation(operation)
                .with_request_id(request_id)
                .with_agent_id(agent_id)),
        }
    }

    fn result(
        &self,
        agent_id: String,
        offset: usize,
        limit: usize,
    ) -> Result<(PublicTask, Option<PublicResult>, Option<String>), ToolError> {
        match self.rpc(RpcMethod::TaskResult {
            agent_id: agent_id.clone(),
            offset,
            limit,
        })? {
            RpcSuccess::TaskResult {
                task,
                result,
                instruction,
            } => Ok((
                task.try_into()?,
                result.map(TryInto::try_into).transpose()?,
                instruction,
            )),
            _ => Err(protocol_error()
                .with_operation("result")
                .with_agent_id(Some(agent_id))),
        }
    }
}

/// The wait context id: the scalar id when one agent is awaited, otherwise
/// the count hint keeps error payloads identifiable without echoing the full
/// list.
fn wait_context_id(target: &crate::rpc::WaitTarget) -> Option<String> {
    match target {
        crate::rpc::WaitTarget::One(id) => Some(id.clone()),
        crate::rpc::WaitTarget::Many(ids) => Some(format!("{} agents", ids.len())),
    }
}

fn rpc_context(method: &RpcMethod) -> (&'static str, Option<String>) {    match method {
        RpcMethod::SystemStatus | RpcMethod::SystemStatusRefresh { .. } => ("status", None),
        RpcMethod::DaemonBeginDrain { .. } => ("drain", None),
        RpcMethod::DaemonDrainStatus => ("drain_status", None),
        RpcMethod::DaemonAbortDrain => ("drain_abort", None),
        RpcMethod::DaemonActivateReady => ("activate_ready", None),
        RpcMethod::AgentProbe { .. } => ("agent_probe", None),
        RpcMethod::AgentModels { .. } => ("agent_models", None),
        RpcMethod::SubmitGeneral { .. } => ("spawn", None),
        RpcMethod::TaskList(_) => ("list", None),
        RpcMethod::TaskWait(input) => ("wait", wait_context_id(&input.agent_id)),
        RpcMethod::TaskMessage(input) => ("send", Some(input.agent_id.clone())),
        RpcMethod::TaskRespond(input) => ("respond", Some(input.agent_id.clone())),
        RpcMethod::TaskCancel { agent_id } => ("cancel", Some(agent_id.clone())),
        RpcMethod::TaskResult { agent_id, .. } => ("result", Some(agent_id.clone())),
        RpcMethod::TaskClose { agent_id } => ("close", Some(agent_id.clone())),
        RpcMethod::TaskObserve { agent_id } => ("observe", Some(agent_id.clone())),
    }
}

fn general_manifest(input: &AgentSpawnInput) -> Result<GeneralTaskManifest, ToolError> {
    let repository = PathBuf::from(&input.repository);
    let agent_id = "daemon-prepared".to_owned();
    let write_manifest = input.write_manifest.iter().map(PathBuf::from).collect();
    let permission_mode = match input.profile.as_ref() {
        Some(_) => None,
        None => Some(
            input
                .permission_mode
                .unwrap_or(PublicPermissionMode::Build)
                .into(),
        ),
    };
    Ok(GeneralTaskManifest {
        schema: GENERAL_TASK_SCHEMA.into(),
        agent_id: agent_id.clone(),
        repository,
        permission_mode,
        prompt: input.prompt.clone(),
        // Validate caller scope before the daemon applies its execution policy.
        write_manifest,
    })
}

fn public_decision(value: &str) -> Result<PublicDecision, ToolError> {
    match value {
        "allow" => Ok(PublicDecision::Allow),
        "deny" => Ok(PublicDecision::Deny),
        "answer" => Ok(PublicDecision::Answer),
        _ => Err(protocol_error()),
    }
}

fn project_response(value: ResponseOutcomeView) -> Result<AgentRespondOutput, ToolError> {
    Ok(AgentRespondOutput {
        disposition: match value.disposition {
            crate::rpc::ResponseDispositionView::Responded => PublicResponseDisposition::Responded,
            crate::rpc::ResponseDispositionView::AlreadyResponded => {
                PublicResponseDisposition::AlreadyResponded
            }
            crate::rpc::ResponseDispositionView::InFlight => PublicResponseDisposition::InFlight,
        },
        requested_decision: public_decision(&value.requested_decision)?,
        effective_decision: public_decision(&value.effective_decision)?,
        policy_overrode: value.policy_overrode,
        policy_reason_code: value.policy_reason_code,
    })
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SubagentMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions("Stateless public facade for durable local ZCode subagent tasks")
    }
}

#[tool_router(router = tool_router)]
impl SubagentMcp {
    #[tool(
    name = "external_subagent_status",
    output_schema = tool_output_schema::<SystemStatusOutput>(),
    description = "Read daemon/runtime readiness, the diagnostic-only mcp_version, component states, and capability limits. Pass {\"probe\": true} to first refresh every configured subagent's model catalog through bounded, prompt-free discovery (no threads, no prompts, no model calls) and then return the same status shape with each subagent's optional models block projected from that fresh evidence; the default (probe omitted or false) stays a passive projection of previously recorded evidence. Not purely read-only in either mode: status always runs the no-model-call local layer probes.",
    annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    )
)]
    async fn system_status(
        &self,
        Parameters(input): Parameters<StatusInput>,
    ) -> Result<Json<SystemStatusOutput>, ToolError> {
        let method = if input.probe {
            RpcMethod::SystemStatusRefresh { probe: true }
        } else {
            RpcMethod::SystemStatus
        };
        match self.rpc(method)? {
            RpcSuccess::SystemStatus { status } => Ok(Json(SystemStatusOutput::from_view(status))),
            _ => Err(protocol_error().with_operation("status")),
        }
    }

    #[tool(
    name = "external_subagent_spawn",
    output_schema = tool_output_schema::<AgentSpawnOutput>(),
    description = "Start one durable subagent in an absolute repository workspace, returning after establishing the new session. Use profile to load reusable preset configurations (subagent, permission_mode, model, effort, developer_instructions) from global profiles; when profile is set, subagent, permission_mode, model, effort, and developer_instructions cannot be specified. developer_instructions (also settable per-spawn) routes to the codex-native developerInstructions channel on thread/start and is spliced as a Developer Instructions prefix block for every other subagent. Specify subagent unless default_subagent is configured. ZCode requires an explicit provider/model token from the spawn request or a named profile; a bare token, an omitted model, and agents.zcode.default_model are rejected at admission. dsh spawns when its enabled + spawn_supported + profile=acp + absolute executable runtime_path configuration admits it (the runtime version is advisory only: a difference never blocks a spawn and surfaces in status and failure instructions). A dsh model is provider:model, split at the first colon (the model side may contain further colons; empty sides are rejected before any task). permission_mode defaults to build. Codex supports build/edit (workspace-write), plan (read-only), and yolo (danger-full-access), always with approvalPolicy=never; codex rejects non-empty write_manifest before task creation (codex_write_manifest_unsupported). dsh admits a non-empty write_manifest in build through the guarded manifest-build composition (workspace-write sandbox, only tool-fs writable, out-of-manifest paths rejected by the write-guard as FS_WRITE_MANIFEST_DENIED), bounded to 256 entries and 64 KiB serialized; an explicit [\".\"] keeps the legacy build composition and plan still requires an empty manifest. agy spawns when its enabled + spawn_supported gate and an absolute AGY_RUNTIME_PATH executable admit it; it supports build (--mode accept-edits) and yolo (--dangerously-skip-permissions) only, takes a bare model slug whose shape admission validates (the daemon does not check catalog membership; an unknown slug fails loudly at session start when the agy CLI rejects it), admits effort only from low/medium/high/max, and rejects any non-empty write_manifest (agy_write_manifest_unsupported); agy has no permission-respond interaction, so tools are soft-denied and denied actions surface on failure diagnostics. For other subagents an omitted write_manifest uses the protected workspace scope. The optional effort token (1..24 bytes of [a-z0-9_]) steers reasoning effort: codex admits only low, medium, high, xhigh or max, zcode and dsh pass a bounded token through to the runtime. The returned session_id identifies the established subagent session. Use wait with the returned agent_id for progress and terminal diagnostics.",
    annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    )
)]
    async fn agent_spawn(
        &self,
        Parameters(input): Parameters<AgentSpawnInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentSpawnOutput>, ToolError> {
        self.agent_spawn_inner(input, move || context.ct.is_cancelled())
            .await
            .map(Json)
    }

    pub(crate) async fn agent_spawn_inner(
        &self,
        input: AgentSpawnInput,
        request_cancelled: impl Fn() -> bool + Send + 'static,
    ) -> Result<AgentSpawnOutput, ToolError> {
        if input.profile.is_some()
            && (input.agent.is_some()
                || input.permission_mode.is_some()
                || input.model.is_some()
                || input.effort.is_some()
                || input.developer_instructions.is_some())
        {
            return Err(validation_error(
                "profile cannot be combined with subagent, permission_mode, model, effort, or developer_instructions; specify these in the profile JSON or omit profile",
            )
            .with_operation("spawn"));
        }
        let manifest = general_manifest(&input).map_err(|error| error.with_operation("spawn"))?;
        let task = match self
            .rpc_spawn(
                GeneralSubmitInput {
                    profile: input.profile.clone(),
                    agent: input.agent.clone(),
                    model: input.model.clone(),
                    effort: input.effort.clone(),
                    developer_instructions: input.developer_instructions.clone(),
                    manifest,
                },
                request_cancelled,
            )
            .await?
        {
            RpcSuccess::GeneralSubmitted { task } => task,
            _ => return Err(protocol_error().with_operation("spawn")),
        };
        Ok(AgentSpawnOutput {
            agent_id: public_task_id(&task.agent_id).map_err(|e| e.with_operation("spawn"))?,
            status: task.status,
            session_id: task.session_id,
            instruction: SPAWN_INSTRUCTION.to_owned(),
        })
    }

    #[tool(
    name = "external_subagent_wait",
    output_schema = tool_output_schema::<AgentWaitAnyOutput>(),
    description = "Wait up to 290 seconds by default. agent_id accepts one agent id (number) or a list of 1..=16 ids. A single id returns the single wait object described below. A list blocks until every listed agent is terminal OR any one of them raises an actionable pending request, then returns a FLAT object: one key per distinct agent id carrying that agent's ordinary wait object (a repeated id is awaited once like any other), plus one \"error\" key mapping problematic ids to markers — {\"<id>\":\"duplicated\"} for ids repeated in the input and \"unknown_agent\" for ids that do not exist; the remaining ids are still awaited normally. Recommended parallel pattern: spawn every subagent first, then call wait once with all ids. In list mode message_id is not supported (pass it only with a scalar id), every element carries message_receipt=null, and each element's timed_out flag follows the same shared deadline. Returns early only when an actionable pending request exists (permission requests need allow/deny; user-input requests carry the full embedded question and need answer with non-empty content) or the terminal result is available; ordinary progress, message receipts, already-responded and unsupported requests never wake it, and its timeout still returns timed_out=true. When the embedded result page carries next_offset=null it is the whole final result and no further result call is needed; otherwise continue reading with external_subagent_result from next_offset. A failed terminal result also carries failure_message: usually the daemon's bounded single-line JSON failure detail, but an early fail_claim row may be plain text, so treat it as an opaque bounded string and attempt JSON parsing defensively (null when the task has no persisted detail or the outcome is not a failure). Read failure_message before falling back to daemon logs or sqlite. The pending_requests projection stays capped at 100 records while the wake decision scans the full pending set, and the request that woke the wait is always part of the returned projection so its request_id can be answered directly. activity carries the text tail incrementally: each wait returns only the text appended since the previous wait (an empty tail means nothing new), every byte is delivered by wait at most once, and latest_text_truncated flags the case where undelivered bytes rolled out of the bounded window so this response degraded to the whole window (on the terminal response, bytes repeating the embedded result page verbatim are stripped so they are not shipped twice), tool calls started in the last 60 seconds, and telemetry status; activity also exposes last_activity_age_ms (null before any activity) and model_request_active; additionally in_flight_tool_count counts tracked in-flight tool calls (scheduled or started without a matching result, error or permission resolution; 0 when there is no snapshot or the adapter has no per-call lifecycle tracking, such as codex) and oldest_in_flight_tool_age_ms is the age since the oldest tracked in-flight call first appeared (null when none is tracked); use external_subagent_observe only when these liveness signals suggest a meaningless loop or a stuck command, to inspect recent tool-call detail.",
    annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    )
)]
    async fn agent_wait(
        &self,
        Parameters(input): Parameters<AgentWaitInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<serde_json::Value>, ToolError> {
        let (target, context_id) = match &input.agent_id {
            AgentIdOrList::Single(id) => (
                WaitTarget::One(
                    internal_task_id(*id).map_err(|error| error.with_operation("wait"))?,
                ),
                *id,
            ),
            AgentIdOrList::Multiple(ids) => {
                if ids.is_empty() {
                    return Err(validation_error(
                        "agent_id list must contain at least one id",
                    )
                    .with_operation("wait"));
                }
                if ids.len() > MAX_WAIT_AGENTS {
                    return Err(validation_error(&format!(
                        "agent_id list supports at most {} ids",
                        MAX_WAIT_AGENTS
                    ))
                    .with_operation("wait"));
                }
                if input.message_id.is_some() {
                    return Err(validation_error(
                        "message_id is only supported with a single agent_id",
                    )
                    .with_operation("wait"));
                }
                let mut internal = Vec::with_capacity(ids.len());
                for id in ids {
                    internal
                        .push(internal_task_id(*id).map_err(|error| error.with_operation("wait"))?);
                }
                (WaitTarget::Many(internal), ids[0])
            }
        };
        if input.wait_time > 299 {
            return Err(validation_error("wait_time must be between 0 and 299")
                .with_operation("wait")
                .with_agent_id(Some(
                    internal_task_id(context_id).unwrap_or_else(|_| context_id.to_string()),
                )));
        }
        let success = self
            .rpc_wait(
                TaskWaitQuery {
                    agent_id: target,
                    wait_time: input.wait_time,
                    message_id: input.message_id,
                },
                move || context.ct.is_cancelled(),
            )
            .await?;
        match success {
            RpcSuccess::TaskWait(body) => {
                let output: AgentWaitOutput = body.try_into()?;
                Ok(Json(serde_json::to_value(output).map_err(|_| {
                    protocol_error().with_operation("wait")
                })?))
            }
            RpcSuccess::TaskWaitMany { waits } => {
                // Flat keyed projection: one key per distinct id carrying the
                // public wait body, plus the concentrated error map keyed by
                // public numeric ids.
                let mut projected = std::collections::BTreeMap::new();
                for (id, body) in waits.bodies {
                    let key = public_task_id(&id)
                        .map_err(|error| error.with_operation("wait"))?
                        .to_string();
                    let output: AgentWaitOutput = body.try_into()?;
                    projected.insert(key, AgentWaitElement::Wait(Box::new(output)));
                }
                let mut error = std::collections::BTreeMap::new();
                for (id, marker) in waits.error {
                    let key = public_task_id(&id)
                        .map_err(|error| error.with_operation("wait"))?
                        .to_string();
                    error.insert(key, marker);
                }
                let output = AgentWaitManyOutput {
                    waits: projected,
                    error: if error.is_empty() { None } else { Some(error) },
                };
                Ok(Json(serde_json::to_value(output).map_err(|_| {
                    protocol_error().with_operation("wait")
                })?))
            }
            _ => Err(protocol_error().with_operation("wait")),
        }
    }

    #[tool(
    name = "external_subagent_observe",
    output_schema = tool_output_schema::<AgentObserveOutput>(),
    description = "仅在怀疑 subagent 陷入无意义循环时才调用，检查最近公开推理和工具调用过程；不要用于健康任务的例行轮询。只读已捕获的本 Agent 数据，不启动模型或工具。默认按本 Agent 任务生命周期内的调用次数选最多的 3 类工具，每类返回最近最多 5 次调用（名称、ID、参数，不含结果）；工具详情来自 zcode/dsh 的公开事件以及 agy/codex 的受限描述符（codex 的 detail 位于 arguments.detail）；dsh/agy/codex 的工具历史均不完整并由 coverage.tool_history_complete=false 标注。zcode/dsh 返回已验证公开 reasoning delta 合并后的最新 200 个 Unicode 字符；codex 与 agy 不采集推理，整个 reasoning 字段为 null，coverage 如实标记未采集能力。encrypted_content 始终排除。本工具不判断循环、不返回进展标签、不自动取消。调用方结合当前任务与这些事实判断：PROGRESSING（新增事实或有效推进）；EXPECTED_WAIT（有目的的计算、权限或外部等待）；NEEDS_CLARIFICATION（具体输入或决定缺失）；NO_PROGRESS_LOOP（无新信息的等价行动循环，且合理重读、等待、状态变化等解释已排除）；INSUFFICIENT_OBSERVABILITY（截断、缺口或缺少上下文，不能断言循环）。相同文本、重复 read 或 true/echo 本身不是循环；没有工具结果也不能推断工具成功、文件未变化或任务失败。判断和取消由调用方独立决定。",
    annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    )
)]
    async fn agent_observe(
        &self,
        Parameters(input): Parameters<AgentInput>,
    ) -> Result<Json<AgentObserveOutput>, ToolError> {
        let agent_id =
            internal_task_id(input.agent_id).map_err(|error| error.with_operation("observe"))?;
        match self.rpc(RpcMethod::TaskObserve {
            agent_id: agent_id.clone(),
        })? {
            RpcSuccess::TaskObserved { observation } => Ok(Json(
                AgentObserveOutput::try_from(observation).map_err(|error| {
                    error
                        .with_operation("observe")
                        .with_agent_id(Some(agent_id.clone()))
                })?,
            )),
            _ => Err(protocol_error()
                .with_operation("observe")
                .with_agent_id(Some(agent_id))),
        }
    }

    #[tool(
    name = "external_subagent_list",
    output_schema = tool_output_schema::<AgentListOutput>(),
    description = "List tasks within an explicit daemon-enforced scope",
    annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    )
)]
    async fn agent_list(
        &self,
        Parameters(input): Parameters<AgentListInput>,
    ) -> Result<Json<AgentListOutput>, ToolError> {
        if !(1..=100).contains(&input.limit) {
            return Err(validation_error("limit must be between 1 and 100").with_operation("list"));
        }
        if input.repository.is_none() {
            return Err(
                validation_error("at least one list scope is required").with_operation("list")
            );
        }
        match self.rpc(RpcMethod::TaskList(TaskListQuery {
            agent: input.agent,
            repository: input.repository,
            phase: input.phase.map(Into::into),
            outcome: input.outcome.map(Into::into),
            cursor: input.cursor,
            limit: input.limit,
        }))? {
            RpcSuccess::TaskListed { tasks, next_cursor } => Ok(Json(AgentListOutput {
                tasks: tasks
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<Vec<_>, _>>()?,
                next_cursor,
            })),
            _ => Err(protocol_error().with_operation("list")),
        }
    }

    #[tool(
    name = "external_subagent_send",
    output_schema = tool_output_schema::<AgentSendOutput>(),
    description = "发送有界消息，必须指定 mode：queue 优先原生非破坏投递，必要时由 es 暂存；steer 中断后启动新 turn（仅 codex/zcode 支持）。省略 message_id 时 daemon 生成并返回有效 id",
    annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    )
)]
    async fn agent_send(
        &self,
        Parameters(input): Parameters<AgentSendInput>,
    ) -> Result<Json<AgentSendOutput>, ToolError> {
        let agent_id =
            internal_task_id(input.agent_id).map_err(|error| error.with_operation("send"))?;
        validate_text(&input.content, "content", MAX_MESSAGE_BYTES).map_err(|error| {
            error
                .with_operation("send")
                .with_agent_id(Some(agent_id.clone()))
        })?;
        match self.rpc(RpcMethod::TaskMessage(MessageInput {
            agent_id: agent_id.clone(),
            message_id: input.message_id,
            mode: input.mode.as_str().into(),
            content: input.content,
        }))? {
            RpcSuccess::Message {
                message_id,
                disposition,
                ..
            } => Ok(Json(AgentSendOutput {
                message_id,
                disposition: match disposition {
                    crate::rpc::MessageDispositionView::Queued => PublicMessageDisposition::Queued,
                    crate::rpc::MessageDispositionView::Delivered => {
                        PublicMessageDisposition::Delivered
                    }
                    crate::rpc::MessageDispositionView::AlreadyDelivered => {
                        PublicMessageDisposition::AlreadyDelivered
                    }
                    crate::rpc::MessageDispositionView::Failed => PublicMessageDisposition::Failed,
                },
            })),
            _ => Err(protocol_error()
                .with_operation("send")
                .with_agent_id(Some(agent_id))),
        }
    }

    #[tool(
    name = "external_subagent_respond",
    output_schema = tool_output_schema::<AgentRespondOutput>(),
    description = "Respond idempotently to a typed respondable pending request: allow or deny for permission requests, answer with non-empty content for answerable user-input requests",
    annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    )
)]
    async fn agent_respond(
        &self,
        Parameters(input): Parameters<AgentRespondInput>,
    ) -> Result<Json<AgentRespondOutput>, ToolError> {
        let agent_id =
            internal_task_id(input.agent_id).map_err(|error| error.with_operation("respond"))?;
        if let Some(content) = input.content.as_deref() {
            if content.is_empty() || content.len() > MAX_MESSAGE_BYTES || content.contains('\0') {
                return Err(validation_error("content is invalid")
                    .with_operation("respond")
                    .with_agent_id(Some(agent_id)));
            }
        }
        if matches!(input.decision, PublicDecision::Answer)
            && input
                .content
                .as_deref()
                .is_none_or(|content| content.trim().is_empty())
        {
            return Err(validation_error("answer requires non-empty content")
                .with_operation("respond")
                .with_agent_id(Some(agent_id)));
        }
        let decision = match input.decision {
            PublicDecision::Allow => ResponseDecision::Allow,
            PublicDecision::Deny => ResponseDecision::Deny,
            PublicDecision::Answer => ResponseDecision::Answer,
        };
        match self.rpc(RpcMethod::TaskRespond(RespondInput {
            agent_id: agent_id.clone(),
            request_id: input.request_id,
            decision,
            content: input.content,
        }))? {
            RpcSuccess::Respond { outcome, .. } => {
                Ok(Json(project_response(outcome).map_err(|error| {
                    error
                        .with_operation("respond")
                        .with_agent_id(Some(agent_id))
                })?))
            }
            _ => Err(protocol_error()
                .with_operation("respond")
                .with_agent_id(Some(agent_id))),
        }
    }

    #[tool(
    name = "external_subagent_cancel",
    output_schema = tool_output_schema::<AgentStateOutput>(),
    description = "Cancel a task without removing durable history",
    annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    )
)]
    async fn agent_cancel(
        &self,
        Parameters(input): Parameters<AgentInput>,
    ) -> Result<Json<AgentStateOutput>, ToolError> {
        let agent_id =
            internal_task_id(input.agent_id).map_err(|error| error.with_operation("cancel"))?;
        match self.rpc(RpcMethod::TaskCancel {
            agent_id: agent_id.clone(),
        })? {
            RpcSuccess::Stopped { task } => Ok(Json(AgentStateOutput {
                task: task.try_into()?,
            })),
            _ => Err(protocol_error()
                .with_operation("cancel")
                .with_agent_id(Some(agent_id))),
        }
    }

    #[tool(
    name = "external_subagent_result",
    output_schema = tool_output_schema::<AgentResultOutput>(),
    description = "Read a terminal task result with stable outcome, partial status, and bounded final-text segments. Returns null result while the task is non-terminal. A failed terminal result also carries failure_message: usually the daemon's bounded single-line JSON failure detail, but an early fail_claim row may be plain text, so treat it as an opaque bounded string and attempt JSON parsing defensively (null when the task has no persisted detail or the outcome is not a failure). Questions from user-input requests are embedded in full in external_subagent_wait projections, not paged here.",
    annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    )
)]
    async fn agent_result(
        &self,
        Parameters(input): Parameters<AgentResultInput>,
    ) -> Result<Json<AgentResultOutput>, ToolError> {
        let agent_id =
            internal_task_id(input.agent_id).map_err(|error| error.with_operation("result"))?;
        let (task, result, instruction) = self.result(agent_id, input.offset, input.limit)?;
        Ok(Json(AgentResultOutput {
            task,
            result,
            instruction,
        }))
    }

    #[tool(
    name = "external_subagent_close",
    output_schema = tool_output_schema::<AgentStateOutput>(),
    description = "Close a task and reap runtime resources while preserving durable history",
    annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    )
)]
    async fn agent_close(
        &self,
        Parameters(input): Parameters<AgentInput>,
    ) -> Result<Json<AgentStateOutput>, ToolError> {
        let agent_id =
            internal_task_id(input.agent_id).map_err(|error| error.with_operation("close"))?;
        match self.rpc(RpcMethod::TaskClose {
            agent_id: agent_id.clone(),
        })? {
            RpcSuccess::Closed { task } => Ok(Json(AgentStateOutput {
                task: task.try_into()?,
            })),
            _ => Err(protocol_error()
                .with_operation("close")
                .with_agent_id(Some(agent_id))),
        }
    }
}

pub async fn serve_stdio(
    socket: PathBuf,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    SubagentMcp::new(socket, timeout)
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
mod contract_default_tests {
    use super::general_manifest;
    use super::{
        default_result_limit, public_task_id, rpc_context, AgentListInput, AgentObserveOutput,
        AgentResultInput, AgentSendInput, AgentSpawnInput, AgentWaitInput, SubagentMcp,
        SystemStatusOutput, PUBLIC_TOOLS,
    };
    use crate::mcp::schemas::PublicPermissionMode;
    use crate::mcp::views::SPAWN_INSTRUCTION;
    use crate::mcp::views::PublicComponentState;
    use crate::{
        observation::{ObservationSnapshot, OBSERVATION_SCHEMA},
        rpc::{
            AgentCapabilitiesView, CapabilityMaturityView, ComponentStateView, GeneralSubmitInput,
            ObservationCapabilityView, ObservationDefaultsView, RpcMethod, SystemStatusView,
            TaskObservationView,
        },
    };
    use rmcp::ServiceExt;
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };

    #[test]
    fn omitted_public_fields_use_the_frozen_defaults() {
        let list: AgentListInput = serde_json::from_value(serde_json::json!({
            "repository": "/tmp/repository"
        }))
        .unwrap();
        assert_eq!(list.limit, 100);
        assert!(list.phase.is_none());
        assert!(list.outcome.is_none());
        assert!(list.cursor.is_none());

        let wait: AgentWaitInput =
            serde_json::from_value(serde_json::json!({"agent_id": 10000000})).unwrap();
        assert_eq!(wait.wait_time, 290);
        assert!(wait.message_id.is_none());
        let immediate: AgentWaitInput =
            serde_json::from_value(serde_json::json!({"agent_id": 10000000, "wait_time": 0}))
                .unwrap();
        assert_eq!(immediate.wait_time, 0);
        for removed in ["after_revision", "supports_answer"] {
            assert!(
                serde_json::from_value::<AgentWaitInput>(serde_json::json!({
                    "agent_id": 10000000, removed: true
                }))
                .is_err(),
                "removed {removed} must be rejected"
            );
        }

        let result: AgentResultInput =
            serde_json::from_value(serde_json::json!({"agent_id": 10000000})).unwrap();
        assert_eq!(result.offset, 0);
        assert_eq!(result.limit, default_result_limit());
        assert!(
            serde_json::from_value::<AgentResultInput>(serde_json::json!({
                "agent_id": 10000000, "request_id": "r"
            }))
            .is_err(),
            "removed result request_id must be rejected"
        );

        let send: AgentSendInput = serde_json::from_value(serde_json::json!({
            "agent_id": 10000000,
            "mode": "queue",
            "content": "continue"
        }))
        .unwrap();
        assert!(send.message_id.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_keeps_status_and_respond_serviceable_and_wakes_on_completion() {
        use crate::rpc::{
            RespondInput, ResponseDecision, RpcSuccess, TaskResultView, TaskWaitBody,
        };
        use external_store::TaskOutcome;
        let (_directory, service, id) = crate::rpc::wait_tests::fixture();
        let store = service.store_for_wait_test();
        let facade = SubagentMcp::from_service(service);
        let start = std::time::Instant::now();
        let waiting = facade.rpc_wait(crate::rpc::wait_tests::query(&id, 1), || false);
        let other_work = async {
            tokio::task::yield_now().await;
            assert!(matches!(
                facade.rpc(RpcMethod::SystemStatus).unwrap(),
                RpcSuccess::SystemStatus { .. }
            ));
            // Unknown request keeps the normal error, but must be serviced
            // while the independent wait is still blocked.
            let _ = facade.rpc(RpcMethod::TaskRespond(RespondInput {
                agent_id: id.clone(),
                request_id: "missing".into(),
                decision: ResponseDecision::Allow,
                content: None,
            }));
            assert!(start.elapsed() < Duration::from_millis(500));
            store
                .store_task_result(
                    &id,
                    &external_store::TaskResult {
                        outcome: TaskOutcome::Completed,
                        final_text: "final answer".into(),
                        partial: false,
                    },
                )
                .unwrap();
        };
        let (response, _) = tokio::join!(waiting, other_work);
        assert!(matches!(
            response.unwrap(),
            RpcSuccess::TaskWait(TaskWaitBody {
                timed_out: false,
                result: Some(TaskResultView {
                    next_offset: None,
                    ..
                }),
                ..
            })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_wait_output_carries_the_model_rejection_reason() {
        use crate::mcp::views::PublicResult;
        use crate::rpc::{RpcSuccess, TaskWaitBody};
        use external_store::TaskOutcome;
        let (_directory, service, id) = crate::rpc::wait_tests::fixture();
        service
            .store_for_wait_test()
            .store_task_result_with_reason(
                &id,
                &external_store::TaskResult {
                    outcome: TaskOutcome::Failed,
                    final_text: "model selection was rejected: unknown model: foo".into(),
                    partial: true,
                },
                Some("MODEL_REJECTED"),
                None,
            )
            .unwrap();
        let facade = SubagentMcp::from_service(service);
        let response = facade
            .rpc_wait(crate::rpc::wait_tests::query(&id, 0), || false)
            .await
            .unwrap();
        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(result),
            ..
        }) = response
        else {
            panic!("expected terminal wait response")
        };
        // The public projection is the MCP wait output; it must expose the
        // machine reason verbatim.
        let public = PublicResult::try_from(result).unwrap();
        let encoded = serde_json::to_string(&public).unwrap();
        assert!(
            encoded.contains("\"reason_code\":\"MODEL_REJECTED\""),
            "{encoded}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_wait_wakes_for_respondable_pending_read_request() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (_directory, service, id) = crate::rpc::wait_tests::fixture();
        service
            .store_for_wait_test()
            .insert_pending_request(
                "request",
                &id,
                "correlation",
                "permission",
                r#"{"toolName":"Read"}"#,
            )
            .unwrap();
        let facade = SubagentMcp::from_service(service);
        let (client, transport) = tokio::io::duplex(64 * 1024);
        let serving = tokio::spawn(async move {
            let server = facade.serve(transport).await.unwrap();
            server.waiting().await.unwrap();
        });
        let (reader, mut writer) = tokio::io::split(client);
        let mut lines = BufReader::new(reader).lines();
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"wait-test\",\"version\":\"1\"}}}\n").await.unwrap();
        let initialized = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&initialized).unwrap()["id"],
            1
        );
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        let call = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "external_subagent_wait", "arguments": {
                "agent_id": public_task_id(&id).unwrap(), "wait_time": 299
            }}
        });
        writer
            .write_all(format!("{call}\n").as_bytes())
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .expect("respondable Read did not wake MCP wait")
            .unwrap()
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        let wait = &response["result"]["structuredContent"];
        assert_eq!(response["id"], 2);
        assert_eq!(wait["timed_out"], false);
        assert_eq!(wait["pending_requests"][0]["kind"], "permission");
        assert_eq!(wait["pending_requests"][0]["tool_name"], "Read");
        assert_eq!(wait["pending_requests"][0]["operation"], "read");
        // The handshake fields are gone from the public projection.
        assert_eq!(
            wait["pending_requests"][0]["state"],
            serde_json::Value::Null
        );
        assert_eq!(
            wait["pending_requests"][0]["respondable"],
            serde_json::Value::Null
        );
        // The advisory instruction is gone from the wait projection: the
        // pending record itself names the respond path.
        assert_eq!(wait["instruction"], serde_json::Value::Null);
        serving.abort();
        let _ = serving.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_embedded_wait_leaves_durable_task_unchanged() {
        let (_directory, service, id) = crate::rpc::wait_tests::fixture();
        let store = service.store_for_wait_test();
        let before = store.get_task(&id).unwrap();
        let facade = SubagentMcp::from_service(service);
        let waiting = tokio::spawn(async move {
            facade
                .rpc_wait(crate::rpc::wait_tests::query(&id, 299), || false)
                .await
        });
        tokio::task::yield_now().await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert_eq!(
            before.clone(),
            store.get_task(&before.unwrap().agent_id).unwrap()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_cancelled_notification_interrupts_wait_without_mutating_task() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (_directory, service, id) = crate::rpc::wait_tests::fixture();
        let store = service.store_for_wait_test();
        let before = store.get_task(&id).unwrap();
        let handler_interrupted = Arc::new(AtomicBool::new(false));
        let mut facade = SubagentMcp::from_service(service);
        facade.wait_handler_interrupted = Some(Arc::clone(&handler_interrupted));
        let requests = Arc::clone(&facade.next_request);
        let (client, transport) = tokio::io::duplex(64 * 1024);
        let serving = tokio::spawn(async move {
            let server = facade.serve(transport).await.unwrap();
            server.waiting().await.unwrap();
        });
        let (reader, mut writer) = tokio::io::split(client);
        let mut lines = BufReader::new(reader).lines();
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"cancel-test\",\"version\":\"1\"}}}\n").await.unwrap();
        let initialized = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&initialized).unwrap()["id"],
            1
        );
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        let call = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "external_subagent_wait", "arguments": {
                "agent_id": public_task_id(&id).unwrap(), "wait_time": 299
            }}
        });
        writer
            .write_all(format!("{call}\n").as_bytes())
            .await
            .unwrap();
        // Wait until the real tool handler has dispatched its blocking RPC.
        tokio::time::timeout(Duration::from_secs(2), async {
            while requests.load(Ordering::Relaxed) == 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), lines.next_line())
                .await
                .is_err()
        );
        let cancellation_started = std::time::Instant::now();
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":2,\"reason\":\"caller stopped waiting\"}}\n").await.unwrap();
        // rmcp drops the cancelled request's response after cancelling its
        // context token. The connection remains usable for subsequent calls.
        tokio::time::timeout(Duration::from_secs(1), async {
            while !handler_interrupted.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled request handler did not get interrupted");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), lines.next_line())
                .await
                .is_err()
        );
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\",\"params\":{}}\n")
            .await
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .expect("connection did not remain serviceable after cancellation")
            .unwrap()
            .unwrap();
        assert!(
            cancellation_started.elapsed() < Duration::from_secs(1),
            "cancelled wait did not end promptly: {:?}",
            cancellation_started.elapsed()
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&status).unwrap()["id"],
            3
        );
        assert_eq!(before, store.get_task(&id).unwrap());
        serving.abort();
        let _ = serving.await;
    }

    #[tokio::test]
    async fn send_without_message_id_returns_the_daemon_generated_id() {
        let (_directory, service, id) = crate::rpc::wait_tests::fixture();
        let facade = SubagentMcp::from_service(service);
        let output = facade
            .agent_send(rmcp::handler::server::wrapper::Parameters(AgentSendInput {
                agent_id: public_task_id(&id).unwrap(),
                message_id: None,
                content: "continue".into(),
                mode: crate::mcp::schemas::PublicSendMode::Queue,
            }))
            .await
            .unwrap();
        assert!(
            output.0.message_id.starts_with("subagent-message-"),
            "daemon must generate the id: {}",
            output.0.message_id
        );
        // An explicit id is echoed back and keeps idempotent retries.
        let explicit = facade
            .agent_send(rmcp::handler::server::wrapper::Parameters(AgentSendInput {
                agent_id: public_task_id(&id).unwrap(),
                message_id: Some("explicit-id".into()),
                content: "retry".into(),
                mode: crate::mcp::schemas::PublicSendMode::Queue,
            }))
            .await
            .unwrap();
        assert_eq!(explicit.0.message_id, "explicit-id");
    }

    #[tokio::test]
    async fn send_projects_native_delivered_and_wait_receipt_state() {
        use crate::{
            ManagedRuntime, RuntimeCommandError, RuntimeTerminal, SessionReady, TurnSnapshot,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        struct NativeRuntime {
            stopped: AtomicBool,
        }
        impl ManagedRuntime for NativeRuntime {
            fn identity(&self) -> Option<external_runtime::ProcessIdentity> {
                None
            }
            fn stop(&self, _: std::time::Duration) -> RuntimeTerminal {
                self.stopped.store(true, Ordering::Release);
                RuntimeTerminal::Stopped(external_runtime::StopOutcome::AlreadyExited(
                    external_runtime::ChildExit::Exited(Some(0)),
                ))
            }
            fn wait_terminal(&self, _: std::time::Duration) -> Option<RuntimeTerminal> {
                std::thread::sleep(std::time::Duration::from_millis(1));
                self.stopped
                    .load(Ordering::Acquire)
                    .then(|| self.stop(std::time::Duration::ZERO))
            }
            fn bootstrap_session(
                &self,
                _: &external_store::TaskRecord,
                _: std::time::Duration,
            ) -> Result<SessionReady, RuntimeCommandError> {
                Ok(SessionReady {
                    session_id: "native-session".into(),
                    initial_turn_id: Some("active-turn".into()),
                    configured_model: Some("fixture-model".into()),
                })
            }
            fn turn_snapshot(&self) -> TurnSnapshot {
                TurnSnapshot {
                    generation: 1,
                    active: true,
                    boundary: None,
                }
            }
            fn inject_turn(
                &self,
                _: &str,
                content: &str,
                _: std::time::Duration,
            ) -> Result<Option<String>, RuntimeCommandError> {
                assert_eq!(content, "follow-up");
                Ok(None)
            }
        }
        struct NativeFactory;
        impl crate::RuntimeFactory for NativeFactory {
            fn spawn(
                &self,
                _: &external_store::TaskRecord,
                _: Arc<dyn crate::LifecycleSink>,
            ) -> std::io::Result<Arc<dyn ManagedRuntime>> {
                Ok(Arc::new(NativeRuntime {
                    stopped: AtomicBool::new(false),
                }))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let store =
            Arc::new(external_store::Store::open(directory.path().join("state.sqlite")).unwrap());
        let scheduler = crate::Scheduler::new(
            "native-mcp",
            store.clone(),
            Arc::new(NativeFactory),
            crate::SchedulerConfig::default(),
        )
        .unwrap();
        let task = scheduler
            .enqueue_general_with_admission(
                &external_core::GeneralTaskManifest {
                    schema: external_core::GENERAL_TASK_SCHEMA.into(),
                    agent_id: String::new(),
                    repository: directory.path().canonicalize().unwrap(),
                    permission_mode: Some(external_core::PermissionMode::Plan),
                    prompt: "initial".into(),
                    write_manifest: Vec::new(),
                },
                Some(external_core::AdmissionIdentity {
                    agent: "codex".into(),
                    config_revision: 1,
                    adapter_version: "fixture".into(),
                    model: Some("fixture-model".into()),
                    model_source: "catalog_token".into(),
                    effort: None,
                    developer_instructions: None,
                }),
            )
            .unwrap();
        scheduler.start_ready().unwrap();
        let facade = SubagentMcp::from_service(Arc::new(
            crate::rpc::RpcService::new(scheduler.clone(), store).unwrap(),
        ));
        let input = || AgentSendInput {
            agent_id: public_task_id(&task.agent_id).unwrap(),
            message_id: Some("native-message".into()),
            mode: crate::mcp::schemas::PublicSendMode::Queue,
            content: "follow-up".into(),
        };
        let delivered = facade
            .agent_send(rmcp::handler::server::wrapper::Parameters(input()))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(delivered.0).unwrap()["disposition"],
            "delivered"
        );
        let repeated = facade
            .agent_send(rmcp::handler::server::wrapper::Parameters(input()))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(repeated.0).unwrap()["disposition"],
            "already_delivered"
        );
        let waited = facade
            .rpc_wait(
                crate::rpc::TaskWaitQuery {
                    agent_id: crate::rpc::WaitTarget::One(task.agent_id.clone()),
                    wait_time: 0,
                    message_id: Some("native-message".into()),
                },
                || false,
            )
            .await
            .unwrap();
        let receipt = serde_json::to_value(waited).unwrap()["message_receipt"].clone();
        assert_eq!(receipt["state"], "delivered");
        assert!(receipt.get("disposition").is_none());
        scheduler.cancel_task(&task.agent_id).unwrap();
    }

    #[test]
    fn send_mode_is_required_and_validated_by_mcp_and_rpc() {
        let (_directory, service, id) = crate::rpc::wait_tests::fixture();
        for mode in [
            None,
            Some(serde_json::json!("invalid")),
            Some(serde_json::Value::Null),
            Some(serde_json::json!(1)),
        ] {
            let mut input = serde_json::json!({"agent_id": public_task_id(&id).unwrap(), "content":"follow-up"});
            if let Some(mode) = mode {
                input["mode"] = mode;
            }
            assert!(serde_json::from_value::<AgentSendInput>(input.clone()).is_err());
            input["agent_id"] = id.clone().into();
            let response = service.handle_bytes(&serde_json::to_vec(&serde_json::json!({"request_id":"validation", "method":"task_message", "params":input})).unwrap());
            assert!(
                matches!(response.outcome, crate::rpc::RpcOutcome::Error { error } if error.code == crate::rpc::RpcErrorCode::Validation)
            );
        }
        for mode in ["queue", "steer"] {
            let input: AgentSendInput = serde_json::from_value(
                serde_json::json!({"agent_id":10000000,"mode":mode,"content":"follow-up"}),
            )
            .unwrap();
            assert_eq!(input.mode.as_str(), mode);
        }
    }

    #[test]
    fn spawn_agent_contract_is_explicit_and_fail_closed() {
        let base = serde_json::json!({
            "repository": "/tmp/repository",
            "prompt": "test"
        });
        let omitted: AgentSpawnInput = serde_json::from_value(base.clone()).unwrap();
        assert!(general_manifest(&omitted).is_ok());

        let dsh: AgentSpawnInput = serde_json::from_value(
            serde_json::json!({"subagent":"dsh","repository":"/tmp/repository","prompt":"test"}),
        )
        .unwrap();
        // Admission is owned by daemon, including disabled versus unsupported order.
        assert!(general_manifest(&dsh).is_ok());
        for field in ["subagent", "model", "effort"] {
            let mut null_input = base.clone();
            null_input[field] = serde_json::Value::Null;
            assert!(serde_json::from_value::<AgentSpawnInput>(null_input).is_err());
        }

        let zcode: AgentSpawnInput = serde_json::from_value(
            serde_json::json!({"subagent":"zcode","repository":"/tmp/repository","prompt":"test"}),
        )
        .unwrap();
        assert!(general_manifest(&zcode).is_ok());
        assert!(serde_json::from_value::<AgentSpawnInput>(
            serde_json::json!({"subagent":"zcode","repository":"/tmp/repository","prompt":"test","extra":true})
        ).is_err());
    }

    #[tokio::test]
    async fn spawn_routes_admission_errors_through_daemon_before_manifest_preparation() {
        // ZCode stays enabled for its model refusal, DSH is fully gated so its
        // illegal-model refusal is reachable at admission, and a configured
        // codex entry stays disabled for the disabled-agent projection.
        let _config_guard = crate::rpc::admission_fixtures::config_env_guard();
        let config_root = tempfile::tempdir().unwrap();
        let runtime = config_root.path().join("dsh-runtime.exe");
        std::fs::write(&runtime, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&runtime).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&runtime, permissions).unwrap();
        }
        let config_path = config_root.path().join("agents.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 2,
                "subagents": {
                    "zcode": {"enabled": true, "spawn_supported": true},
                    "dsh": {
                        "enabled": true,
                        "spawn_supported": true,
                        "runtime_path": runtime.to_string_lossy(),
                        "profile": "acp",
                        "version": external_agent_dsh::profile::PINNED_DSH_VERSION,
                    },
                    "codex": {"enabled": false, "spawn_supported": true},
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let _config_scope = crate::rpc::admission_fixtures::ConfigEnvScope::install(&config_path);
        let (_directory, service, id) = crate::rpc::wait_tests::fixture();
        let store = service.store_for_wait_test();
        let before = store.get_task(&id).unwrap();
        let facade = SubagentMcp::from_service(service);
        for (agent, model, expected, expected_message) in [
            (
                "unknown",
                None,
                "subagent_unknown",
                "subagent is unknown, available subagents are [\"dsh\", \"zcode\"]",
            ),
            ("codex", None, "agent_disabled", "agent is disabled"),
            (
                "zcode",
                Some("two/slashes/here"),
                "validation",
                "zcode model must be {provider}/{model}; the token contains more than one '/' separator",
            ),
            (
                "dsh",
                Some("deepseek-flash"),
                "validation",
                "dsh model must be {provider}:{model}; the ':' separator is missing",
            ),
        ] {
            let input = AgentSpawnInput {
                profile: None,
                agent: Some(agent.into()),
                model: model.map(str::to_owned),
                effort: None,
                developer_instructions: None,
                repository: "invalid-relative-path".into(),
                prompt: "".into(),
                permission_mode: Some(PublicPermissionMode::Plan),
                write_manifest: vec![],
            };
            let error = facade
                .agent_spawn_inner(input, || false)
                .await
                .err()
                .expect("admission must reject");
            assert_eq!(error.body.code, expected);
            assert_eq!(error.body.message, expected_message);
            assert_eq!(error.body.prompt_count, Some(0));
            if agent == "unknown" {
                // The composed roster message is the whole legacy text too:
                // the generic ": {detail}" tail must not repeat the roster.
                assert_eq!(
                    error.legacy_text,
                    "subagent_unknown: subagent is unknown, available subagents are [\"dsh\", \"zcode\"]"
                );
            }
            if agent == "dsh" {
                // The composed format detail passes through verbatim and the
                // legacy text drops the static middle sentence.
                assert_eq!(error.legacy_text, format!("validation: {expected_message}"));
            }
        }
        assert_eq!(before, store.get_task(&id).unwrap());
    }

    #[tokio::test]
    async fn spawn_passes_effort_through_the_submit_general_wire() {
        // Enable ZCode explicitly so this test reaches effort persistence.
        let _config_guard = crate::rpc::admission_fixtures::config_env_guard();
        let config_root = tempfile::tempdir().unwrap();
        let config_path = config_root.path().join("agents.json");
        std::fs::write(
            &config_path,
            r#"{"schema_version":2,"subagents":{"zcode":{"enabled":true,"spawn_supported":true}}}"#,
        )
        .unwrap();
        let _config_scope = crate::rpc::admission_fixtures::ConfigEnvScope::install(&config_path);
        let (_directory, service, _id) = crate::rpc::wait_tests::runnable_fixture();
        let store = service.store_for_wait_test();
        let repository = tempfile::tempdir().unwrap();
        let facade = SubagentMcp::from_service(service);
        let input = AgentSpawnInput {
            profile: None,
            agent: Some("zcode".into()),
            model: Some("provider/model".into()),
            effort: Some("high".into()),
            developer_instructions: None,
            repository: repository.path().to_string_lossy().into_owned(),
            prompt: "effort passthrough".into(),
            permission_mode: Some(PublicPermissionMode::Plan),
            write_manifest: Vec::new(),
        };
        let output = facade
            .agent_spawn_inner(input, || false)
            .await
            .unwrap();
        assert_eq!(output.status, "running");
        assert!(output.session_id.is_some());
        let stored = store
            .get_task(&output.agent_id.to_string())
            .unwrap()
            .unwrap();
        assert!(
            stored.prepared_launch_json.contains("\"effort\":\"high\""),
            "spawn effort must reach the persisted admission identity: {}",
            stored.prepared_launch_json
        );
    }

    #[tokio::test]
    async fn spawn_passes_developer_instructions_through_the_submit_general_wire() {
        // ZCode routes developer_instructions through prompt splicing, so the
        // persisted initial_prompt anchors the facade → daemon passthrough.
        let _config_guard = crate::rpc::admission_fixtures::config_env_guard();
        let config_root = tempfile::tempdir().unwrap();
        let config_path = config_root.path().join("agents.json");
        std::fs::write(
            &config_path,
            r#"{"schema_version":2,"subagents":{"zcode":{"enabled":true,"spawn_supported":true}}}"#,
        )
        .unwrap();
        let _config_scope = crate::rpc::admission_fixtures::ConfigEnvScope::install(&config_path);
        let (_directory, service, _id) = crate::rpc::wait_tests::runnable_fixture();
        let store = service.store_for_wait_test();
        let repository = tempfile::tempdir().unwrap();
        let facade = SubagentMcp::from_service(service);
        let input = AgentSpawnInput {
            profile: None,
            agent: Some("zcode".into()),
            model: Some("provider/model".into()),
            effort: None,
            developer_instructions: Some("Facade spawn instructions".into()),
            repository: repository.path().to_string_lossy().into_owned(),
            prompt: "di passthrough".into(),
            permission_mode: Some(PublicPermissionMode::Plan),
            write_manifest: Vec::new(),
        };
        let output = facade.agent_spawn_inner(input, || false).await.unwrap();
        assert_eq!(output.status, "running");
        let stored = store
            .get_task(&output.agent_id.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.initial_prompt,
            "Developer Instructions: Facade spawn instructions\n----------\ndi passthrough"
        );
    }

    #[test]
    fn spawn_rpc_context_omits_the_preallocation_placeholder() {
        let method = RpcMethod::SubmitGeneral(GeneralSubmitInput {
            profile: None,
            agent: Some("zcode".into()),
            model: None,
            effort: None,
            developer_instructions: None,
            manifest: external_core::GeneralTaskManifest {
                schema: external_core::GENERAL_TASK_SCHEMA.into(),
                agent_id: "daemon-prepared".into(),
                repository: PathBuf::from("/tmp/repository"),
                permission_mode: Some(external_core::PermissionMode::Plan),
                prompt: "test".into(),
                write_manifest: Vec::new(),
            },
        });
        assert_eq!(rpc_context(&method), ("spawn", None));
    }

    #[test]
    fn legacy_daemon_status_keeps_readiness_and_routes_identity_to_cli_diagnose() {
        let status = SystemStatusView {
            mcp_version: "0.1.0".into(),
            service_generation: "legacy-generation".into(),
            components: BTreeMap::from([
                ("daemon".into(), ComponentStateView::Ready),
                // The daemon never observes the on-demand facade process; the
                // projection under test runs inside that very facade.
                ("facade".into(), ComponentStateView::Unknown),
            ]),
            capabilities: AgentCapabilitiesView {
                max_rpc_request_frame_bytes: 512 * 1024,
                max_rpc_response_frame_bytes: 2 * 1024 * 1024,
                max_wait_ms: 299000,
                maturity: BTreeMap::from([("spawn".into(), CapabilityMaturityView::BetaReady)]),
                observation: ObservationCapabilityView {
                    public_reasoning_default: true,
                    defaults: ObservationDefaultsView {
                        top_tools: 3,
                        recent_calls_per_tool: 5,
                        reasoning_chars: 200,
                    },
                },
            },
            agents: Vec::new(),
            identity: None,
        };
        let output = SystemStatusOutput::from_view(status);
        assert_eq!(output.mcp_version, "0.1.0");
        assert!(matches!(
            output.components.get("daemon"),
            Some(PublicComponentState::Ready)
        ));
        // A status response the host can read was forwarded by a live facade,
        // so the facade-side projection upgrades the daemon's Unknown
        // observation to Ready.
        assert!(matches!(
            output.components.get("facade"),
            Some(PublicComponentState::Ready)
        ));
        let serialized = serde_json::to_value(output).unwrap();
        assert_eq!(serialized["mcp_version"], "0.1.0");
        for removed in [
            "protocol_version",
            "service_generation",
            // Deployment identity stays on the RPC view for CLI diagnose.
            "identity",
        ] {
            assert!(serialized.get(removed).is_none());
        }
    }

    #[test]
    fn spawn_description_states_the_config_gated_dsh_reality() {
        let facade = SubagentMcp::new(
            PathBuf::from("/tmp/spawn-desc.sock"),
            Duration::from_secs(1),
        );
        let spawn = facade
            .tool_router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "external_subagent_spawn")
            .unwrap();
        let description = spawn.description.as_deref().unwrap();
        assert!(
            !description.contains("remains unsupported"),
            "stale pre-admission dsh claim still exposed to MCP consumers: {description}"
        );
        assert!(
            description.contains("spawn_supported"),
            "the configured dsh admission gate is not documented: {description}"
        );
        assert!(
            !description.contains("pinned-runtime"),
            "the removed version gate is still exposed to MCP consumers: {description}"
        );
        assert!(
            description.contains("version is advisory only"),
            "the advisory version semantics are not documented: {description}"
        );
    }

    #[test]
    fn observation_tool_catalog_matches_the_frozen_description_and_bounds() {
        let facade = SubagentMcp::new(PathBuf::from("/tmp/observe.sock"), Duration::from_secs(1));
        let tools = facade.tool_router.list_all();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            PUBLIC_TOOLS
        );
        let observe = tools
            .iter()
            .find(|tool| tool.name == "external_subagent_observe")
            .unwrap();
        let description = observe.description.as_deref().unwrap();
        // Frozen description, re-derived for the three-adapter observation contract.
        assert_eq!(
            format!("{:x}", Sha256::digest(description.as_bytes())),
            "c4d0ccf3a9a2e1c437787e7880af087977b452d6bfd87bac6df75f60a4181f54"
        );
        let input = serde_json::to_value(&observe.input_schema).unwrap();
        assert_eq!(input["additionalProperties"], false);
        assert_eq!(input["required"], serde_json::json!(["agent_id"]));
        let output = serde_json::to_value(observe.output_schema.as_ref().unwrap()).unwrap();
        let serialized = output.to_string();
        assert!(serialized.contains("\"maxItems\":3"));
        assert!(serialized.contains("\"maxItems\":5"));
        assert!(serialized.contains("\"maxLength\":200"));
        assert!(jsonschema::validator_for(&output).is_ok());
    }

    #[test]
    fn observation_projection_rejects_source_contract_drift() {
        let snapshot = ObservationSnapshot::unavailable();
        let view = TaskObservationView {
            schema: OBSERVATION_SCHEMA.into(),
            agent_id: "10000000".into(),
            count_scope: "agent_lifetime".into(),
            tools: snapshot.tools,
            reasoning: Some(snapshot.reasoning),
            coverage: snapshot.coverage,
        };
        assert!(AgentObserveOutput::try_from(view.clone()).is_ok());
        let mut drifted = view;
        drifted.reasoning.as_mut().unwrap().source.delta_pointer = "/private/field".into();
        assert!(AgentObserveOutput::try_from(drifted).is_err());
    }

    #[test]
    fn every_tool_output_schema_compiles_and_accepts_error_and_legacy_success_shapes() {
        let facade = SubagentMcp::new(PathBuf::from("/tmp/schema.sock"), Duration::from_secs(1));
        let task = serde_json::json!({
            "agent_id":10000001, "status":"running", "session_id":null,
            "input_identity":{
                "subagent":null,"config_revision":null,"adapter_version":null,
                "model":null,"model_source":null,"effort":null,"workspace_path":"/tmp/repo",
                "permission_mode":"build"
            }
        });
        // The wait output carries the lifecycle header only; its schema must
        // reject the full task shape that list/cancel/result still accept.
        let wait_task = serde_json::json!({
            "agent_id":10000001, "status":"running"
        });
        let activity = serde_json::json!({
            "latest_text_tail":"", "latest_text_truncated":false,
            "tool_calls_last_60s":0,
            "in_flight_tool_count":0,
            "oldest_in_flight_tool_age_ms":null,
            "last_activity_age_ms":null, "model_request_active":false,
            "telemetry_status":"healthy"
        });
        let observation = serde_json::json!({
            "tools":[], "reasoning":{"text":"","truncated":false},
            "coverage":{"tool_history_complete":false,"reasoning_complete":false,"dropped_events":0}
        });
        let status = serde_json::json!({
            "mcp_version":"0.1.0",
            "components":{},"capabilities":{"max_rpc_request_frame_bytes":524288,"max_rpc_response_frame_bytes":2097152,"max_wait_ms":299000,
                "maturity":{},"observation":{"public_reasoning_default":true,
                    "defaults":{"top_tools":3,"recent_calls_per_tool":5,"reasoning_chars":200}}},
            "subagents":[]
        });
        let wait = serde_json::json!({
            "task":wait_task,"pending_requests":[],
            "activity":activity,
            "result":null,"timed_out":false,
            "message_receipt":{"message_id":"message-1","state":"queued","failure_code":null}
        });
        let successes = BTreeMap::from([
            ("external_subagent_status", status),
            (
                "external_subagent_spawn",
                serde_json::json!({"agent_id":10000001,"status":"running","session_id":null,"instruction":SPAWN_INSTRUCTION}),
            ),
            ("external_subagent_wait", wait),
            ("external_subagent_observe", observation),
            (
                "external_subagent_list",
                serde_json::json!({"tasks":[],"next_cursor":null}),
            ),
            (
                "external_subagent_send",
                serde_json::json!({"message_id":"message-1","disposition":"queued"}),
            ),
            (
                "external_subagent_respond",
                serde_json::json!({"disposition":"responded","requested_decision":"allow","effective_decision":"allow","policy_overrode":false,"policy_reason_code":null}),
            ),
            (
                "external_subagent_cancel",
                serde_json::json!({"task":task.clone()}),
            ),
            (
                "external_subagent_result",
                serde_json::json!({"task":task.clone(),"result":null,"instruction":null}),
            ),
            ("external_subagent_close", serde_json::json!({"task":task})),
        ]);
        let error = serde_json::json!({"error":{
            "code":"not_found","message":"agent task was not found","component":"daemon",
            "operation":"result","request_id":"request-1","agent_id":10000001
        }});
        for tool in facade.tool_router.list_all() {
            let schema = serde_json::to_value(tool.output_schema.as_ref().unwrap()).unwrap();
            let validator = jsonschema::validator_for(&schema)
                .unwrap_or_else(|failure| panic!("{} schema failed: {failure}", tool.name));
            let success = successes.get(tool.name.as_ref()).unwrap();
            assert!(
                validator.is_valid(success),
                "{} rejected success: {:?}",
                tool.name,
                validator.iter_errors(success).collect::<Vec<_>>()
            );
            assert!(
                validator.is_valid(&error),
                "{} rejected error: {:?}",
                tool.name,
                validator.iter_errors(&error).collect::<Vec<_>>()
            );
        }

        // The wait output narrows its task to the lifecycle header; its schema
        // must reject the full PublicTask shape list/cancel/result accept.
        let wait_tool = facade
            .tool_router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "external_subagent_wait")
            .unwrap();
        let wait_schema = serde_json::to_value(wait_tool.output_schema.as_ref().unwrap()).unwrap();
        let wait_validator = jsonschema::validator_for(&wait_schema).unwrap();
        let mut full_task_wait = successes.get("external_subagent_wait").unwrap().clone();
        full_task_wait["task"] = serde_json::json!({
            "agent_id":10000001, "status":"running", "session_id":null,
            "input_identity":{}
        });
        assert!(
            !wait_validator.is_valid(&full_task_wait),
            "wait schema must reject the full task shape"
        );

        // List-mode wait: the same schema must accept the FLAT keyed object —
        // id keys carrying ordinary wait bodies plus one concentrated error
        // key for duplicated/unknown ids.
        let scalar_wait = successes.get("external_subagent_wait").unwrap().clone();
        let many_wait = serde_json::json!({
            "10000001": scalar_wait,
            "error": {
                "10000002": "unknown_agent",
                "10000003": "duplicated"
            }
        });
        assert!(
            wait_validator.is_valid(&many_wait),
            "wait schema rejected list-mode success: {:?}",
            wait_validator.iter_errors(&many_wait).collect::<Vec<_>>()
        );
    }

    #[test]
    fn status_tool_schema_accepts_models_block_and_input_declares_probe() {
        let facade = SubagentMcp::new(PathBuf::from("/tmp/schema.sock"), Duration::from_secs(1));
        let status_tool = facade
            .tool_router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "external_subagent_status")
            .unwrap();
        let schema = serde_json::to_value(status_tool.output_schema.as_ref().unwrap()).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();

        // A subagent that never ran discovery omits `models` entirely.
        let passive = serde_json::json!({
            "mcp_version":"0.1.0",
            "components":{},
            "capabilities":{"max_rpc_request_frame_bytes":524288,"max_rpc_response_frame_bytes":2097152,"max_wait_ms":299000,
                "maturity":{},"observation":{"public_reasoning_default":true,
                    "defaults":{"top_tools":3,"recent_calls_per_tool":5,"reasoning_chars":200}}},
            "subagents":[{
                "subagent":"codex","configured":true,"enabled":true,"spawn_supported":true,
                "permission_modes":["build","edit","plan","yolo"],
                "model_selection":{"supported":true,"mode":"catalog_token"},
                "effort_selection":{"supported":true,"mode":"closed_set"},
                "local":{"state":"UNKNOWN"},"auth":{"state":"UNKNOWN"},"hi":{"state":"UNKNOWN"}
            }]
        });
        assert!(
            validator.is_valid(&passive),
            "passive status rejected: {:?}",
            validator.iter_errors(&passive).collect::<Vec<_>>()
        );

        // A discovered subagent carries the models block; a degraded one
        // keeps the block with a bounded reason.
        let with_models = serde_json::json!({
            "mcp_version":"0.1.0",
            "components":{},
            "capabilities":{"max_rpc_request_frame_bytes":524288,"max_rpc_response_frame_bytes":2097152,"max_wait_ms":299000,
                "maturity":{},"observation":{"public_reasoning_default":true,
                    "defaults":{"top_tools":3,"recent_calls_per_tool":5,"reasoning_chars":200}}},
            "subagents":[
                {
                    "subagent":"codex","configured":true,"enabled":true,"spawn_supported":true,
                    "permission_modes":["build","edit","plan","yolo"],
                    "model_selection":{"supported":true,"mode":"catalog_token"},
                    "effort_selection":{"supported":true,"mode":"closed_set"},
                    "models":{"supported":true,"tokens":["gpt-6-luna"],"source":"codex_app_server_model_list","version":"0.163.0","checked_at_ms":42},
                    "local":{"state":"UNKNOWN"},"auth":{"state":"UNKNOWN"},"hi":{"state":"UNKNOWN"}
                },
                {
                    "subagent":"dsh","configured":true,"enabled":true,"spawn_supported":true,
                    "permission_modes":["build","edit","plan","yolo"],
                    "model_selection":{"supported":true,"mode":"catalog_token"},
                    "effort_selection":{"supported":true,"mode":"passthrough_token"},
                    "models":{"supported":false,"tokens":[],"source":"dsh_acp_models_list","checked_at_ms":43,"reason":"stale_config_revision"},
                    "local":{"state":"UNKNOWN"},"auth":{"state":"UNKNOWN"},"hi":{"state":"UNKNOWN"}
                }
            ]
        });
        assert!(
            validator.is_valid(&with_models),
            "models block rejected: {:?}",
            validator.iter_errors(&with_models).collect::<Vec<_>>()
        );

        // The tool's declared input now names the probe flag.
        let input_schema = serde_json::to_value(&status_tool.input_schema).unwrap();
        let properties = input_schema["properties"].as_object().cloned().unwrap_or_default();
        assert!(
            properties.contains_key("probe"),
            "status input schema must declare probe: {input_schema}"
        );
    }

    #[test]
    fn failure_message_is_documented_and_optional_in_both_read_tools() {
        fn walk(node: &serde_json::Value, tool: &str, found: &mut bool) {
            match node {
                serde_json::Value::Object(map) => {
                    if let Some(properties) = map.get("properties").and_then(|value| value.as_object())
                    {
                        if let Some(property) = properties.get("failure_message") {
                            *found = true;
                            let required = map
                                .get("required")
                                .and_then(|value| value.as_array())
                                .is_some_and(|required| {
                                    required
                                        .iter()
                                        .any(|value| value.as_str() == Some("failure_message"))
                                });
                            assert!(!required, "{tool} must not require failure_message");
                            let encoded = property.to_string();
                            assert!(
                                encoded.contains("null"),
                                "{tool} failure_message must allow null: {encoded}"
                            );
                        }
                    }
                    for value in map.values() {
                        walk(value, tool, found);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, tool, found);
                    }
                }
                _ => {}
            }
        }

        let facade = SubagentMcp::new(
            PathBuf::from("/tmp/failure-schema.sock"),
            Duration::from_secs(1),
        );
        let tools = facade.tool_router.list_all();
        for name in ["external_subagent_wait", "external_subagent_result"] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} tool missing"));
            let description = tool.description.as_deref().unwrap();
            assert!(
                description.contains("failure_message"),
                "{name} description must document failure_message: {description}"
            );
            let schema = serde_json::to_value(tool.output_schema.as_ref().unwrap()).unwrap();
            let mut found = false;
            walk(&schema, name, &mut found);
            assert!(found, "{name} output schema must expose failure_message");
        }

        // The schema accepts the result shape both with and without an
        // explicit failure_message value.
        let tool = tools
            .iter()
            .find(|tool| tool.name == "external_subagent_result")
            .unwrap();
        let schema = serde_json::to_value(tool.output_schema.as_ref().unwrap()).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let task = serde_json::json!({
            "agent_id":10000001, "status":"failed", "session_id":null,
            "input_identity":{
                "subagent":null,"config_revision":null,"adapter_version":null,
                "model":null,"model_source":null,"effort":null,"workspace_path":"/tmp/repo",
                "permission_mode":"build"
            }
        });
        let base_result = serde_json::json!({
            "outcome":"FAILED","final_text":"RUNTIME_TERMINAL","partial":true,
            "offset":0,"total_bytes":16,"next_offset":null,
            "reason_code":"RUNTIME_TERMINAL"
        });
        for failure in [
            serde_json::Value::Null,
            serde_json::json!(r#"{"stage":"runtime_terminal","error_code":"RUNTIME_TERMINAL"}"#),
        ] {
            let mut result = base_result.clone();
            if !failure.is_null() {
                result["failure_message"] = failure;
            }
            let success = serde_json::json!({"task": task.clone(), "result": result});
            assert!(
                validator.is_valid(&success),
                "result schema rejected {success}: {:?}",
                validator.iter_errors(&success).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn socket_facade_timeout_lifted() {
        let facade = SubagentMcp::new(PathBuf::from("/tmp/socket.sock"), Duration::from_secs(1));
        assert_eq!(facade.timeout, Duration::from_secs(125));
        let custom = SubagentMcp::new(PathBuf::from("/tmp/socket.sock"), Duration::from_secs(200));
        assert_eq!(custom.timeout, Duration::from_secs(200));
    }

    #[tokio::test]
    async fn mcp_spawn_output_has_session_id() {
        let _config_guard = crate::rpc::admission_fixtures::config_env_guard();
        let config_root = tempfile::tempdir().unwrap();
        let config_path = config_root.path().join("agents.json");
        std::fs::write(
            &config_path,
            r#"{"schema_version":2,"subagents":{"zcode":{"enabled":true,"spawn_supported":true}}}"#,
        )
        .unwrap();
        let _config_scope = crate::rpc::admission_fixtures::ConfigEnvScope::install(&config_path);
        let (_directory, service, _id) = crate::rpc::wait_tests::runnable_fixture();
        let facade = SubagentMcp::from_service(service);
        let repository = tempfile::tempdir().unwrap();
        let input = AgentSpawnInput {
            profile: None,
            agent: Some("zcode".into()),
            model: Some("provider/model".into()),
            effort: None,
            developer_instructions: None,
            repository: repository.path().to_string_lossy().into_owned(),
            prompt: "spawn session test".into(),
            permission_mode: Some(PublicPermissionMode::Plan),
            write_manifest: Vec::new(),
        };
        let output = facade.agent_spawn_inner(input, || false).await.unwrap();
        assert_eq!(output.status, "running");
        assert!(output.session_id.is_some());
        assert_eq!(output.instruction, SPAWN_INSTRUCTION);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_cancelled_notification_interrupts_spawn_without_mutating_task() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use crate::{LifecycleSink, ManagedRuntime, RuntimeFactory, RuntimeTerminal, Scheduler, SchedulerConfig, TurnSnapshot};
        use crate::rpc::RpcService;
        use external_store::{Store, TaskPhase, TaskRecord};

        struct BlockingRuntime {
            entered: Arc<AtomicBool>,
            gate: Arc<AtomicBool>,
        }
        impl ManagedRuntime for BlockingRuntime {
            fn identity(&self) -> Option<external_runtime::ProcessIdentity> {
                None
            }
            fn stop(&self, _timeout: Duration) -> RuntimeTerminal {
                RuntimeTerminal::Stopped(external_runtime::StopOutcome::AlreadyExited(
                    external_runtime::ChildExit::Exited(Some(0)),
                ))
            }
            fn wait_terminal(&self, _timeout: Duration) -> Option<RuntimeTerminal> {
                None
            }
            fn bootstrap_session(
                &self,
                _job: &TaskRecord,
                _timeout: Duration,
            ) -> Result<crate::SessionReady, crate::RuntimeCommandError> {
                self.entered.store(true, Ordering::Release);
                while !self.gate.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(crate::SessionReady {
                    session_id: "mcp-blocked-session".into(),
                    configured_model: Some("provider/model".into()),
                    initial_turn_id: None,
                })
            }
            fn turn_snapshot(&self) -> TurnSnapshot {
                TurnSnapshot {
                    active: false,
                    boundary: None,
                    generation: 0,
                }
            }
            fn stop_boundary_count(&self) -> u64 {
                0
            }
        }

        struct BlockingFactory {
            entered: Arc<AtomicBool>,
            gate: Arc<AtomicBool>,
        }
        impl RuntimeFactory for BlockingFactory {
            fn spawn(
                &self,
                _task: &TaskRecord,
                _sink: Arc<dyn LifecycleSink>,
            ) -> Result<Arc<dyn ManagedRuntime>, std::io::Error> {
                Ok(Arc::new(BlockingRuntime {
                    entered: Arc::clone(&self.entered),
                    gate: Arc::clone(&self.gate),
                }))
            }
        }

        let _config_guard = crate::rpc::admission_fixtures::config_env_guard();
        let config_root = tempfile::tempdir().unwrap();
        let config_path = config_root.path().join("agents.json");
        std::fs::write(
            &config_path,
            r#"{"schema_version":2,"subagents":{"zcode":{"enabled":true,"spawn_supported":true}}}"#,
        )
        .unwrap();
        let _config_scope = crate::rpc::admission_fixtures::ConfigEnvScope::install(&config_path);

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("mcp-blocking-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let bootstrap_entered = Arc::new(AtomicBool::new(false));
        let bootstrap_gate = Arc::new(AtomicBool::new(false));
        let scheduler = Scheduler::new(
            "mcp-blocking-test",
            store.clone(),
            Arc::new(BlockingFactory {
                entered: Arc::clone(&bootstrap_entered),
                gate: Arc::clone(&bootstrap_gate),
            }),
            SchedulerConfig::default(),
        )
        .unwrap();
        let service = Arc::new(RpcService::new(scheduler, store.clone()).unwrap());

        let handler_interrupted = Arc::new(AtomicBool::new(false));
        let mut facade = SubagentMcp::from_service(service.clone());
        facade.spawn_handler_interrupted = Some(Arc::clone(&handler_interrupted));
        let (client, transport) = tokio::io::duplex(64 * 1024);
        let serving = tokio::spawn(async move {
            let server = facade.serve(transport).await.unwrap();
            server.waiting().await.unwrap();
        });
        let (reader, mut writer) = tokio::io::split(client);
        let mut lines = BufReader::new(reader).lines();
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"cancel-test\",\"version\":\"1\"}}}\n").await.unwrap();
        let initialized = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&initialized).unwrap()["id"],
            1
        );
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        let repository = tempfile::tempdir().unwrap();
        let call = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "external_subagent_spawn", "arguments": {
                "subagent": "zcode",
                "model": "provider/model",
                "repository": repository.path().to_string_lossy(),
                "prompt": "interrupt test",
                "permission_mode": "plan"
            }}
        });
        writer
            .write_all(format!("{call}\n").as_bytes())
            .await
            .unwrap();

        // 1. Wait until bootstrap is actively entered and blocked
        tokio::time::timeout(Duration::from_secs(2), async {
            while !bootstrap_entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bootstrap must be entered and blocking");

        let cancellation_started = std::time::Instant::now();
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":2,\"reason\":\"caller stopped waiting\"}}\n").await.unwrap();

        // 2. Cancellation ends promptly in bounded time
        tokio::time::timeout(Duration::from_secs(1), async {
            while !handler_interrupted.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled spawn handler did not get interrupted");

        assert!(
            cancellation_started.elapsed() < Duration::from_secs(2),
            "cancellation must end in bounded time"
        );

        // rmcp drops response for cancelled request #2
        assert!(
            tokio::time::timeout(Duration::from_millis(300), lines.next_line())
                .await
                .is_err()
        );

        // Connection remains serviceable for subsequent tools/list request
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\",\"params\":{}}\n")
            .await
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .expect("connection did not remain serviceable after cancellation")
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&status).unwrap()["id"],
            3
        );

        // 3. Test agent_spawn_inner error variant directly with interrupted token:
        // must return timeout/interrupted variant carrying active_agent_id
        let repo2 = tempfile::tempdir().unwrap();
        let direct_facade = SubagentMcp::from_service(service.clone());
        let spawn_input = AgentSpawnInput {
            profile: None,
            agent: Some("zcode".into()),
            model: Some("provider/model".into()),
            effort: None,
            developer_instructions: None,
            repository: repo2.path().to_string_lossy().into_owned(),
            permission_mode: Some(PublicPermissionMode::Plan),
            prompt: "direct interrupt test".into(),
            write_manifest: vec![],
        };
        let err = direct_facade
            .agent_spawn_inner(spawn_input, || true)
            .await
            .unwrap_err();
        assert_eq!(err.body.code, "timeout");
        assert!(err.body.message.contains("spawn interrupted while session establishment continues"));
        assert!(err.body.agent_id.is_some(), "must carry active_agent_id");
        assert!(err.legacy_text.contains("active_agent_id="));

        // 4. Assert task in store for request #2: while blocked in bootstrap, stop_requested was not written
        let task_id = "10000000";
        let task = store.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.phase, TaskPhase::Preparing);
        assert!(!task.stop_requested, "interrupted spawn must not write stop_requested");

        // 5. Unblock bootstrap: task subsequently transitions to RUNNING without stop
        bootstrap_gate.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut reached_running = false;
        while std::time::Instant::now() < deadline {
            if let Some(t) = store.get_task(task_id).unwrap() {
                if t.phase == TaskPhase::Running {
                    assert_eq!(t.session_id.as_deref(), Some("mcp-blocked-session"));
                    assert!(!t.stop_requested);
                    reached_running = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(reached_running, "task must subsequently reach RUNNING as normal");

        serving.abort();
        let _ = serving.await;
    }

    #[test]
    fn mcp_spawn_error_fidelity_preserves_reason_id_and_distinguishable_messages() {
        use super::public_error_for_op;
        use crate::rpc::{RpcError, RpcErrorCode};

        // 1. StartFailed: preserves reason code, active_agent_id
        let mut failed = RpcError::new(RpcErrorCode::RuntimeLost, "SESSION_START_FAILED: adapter failed");
        failed.active_agent_id = Some("10000001".into());
        let tool_err = public_error_for_op(failed, "spawn");
        assert_eq!(tool_err.body.code, "runtime_lost");
        assert!(tool_err.body.message.contains("SESSION_START_FAILED"));
        assert_eq!(tool_err.body.agent_id, Some(10000001));

        // 2. StartTimeout (ordinary): preserves message and active_agent_id
        let mut timeout = RpcError::new(RpcErrorCode::Timeout, "spawn start timed out: session establishment exceeded deadline and was cancelled");
        timeout.active_agent_id = Some("10000002".into());
        let tool_err = public_error_for_op(timeout, "spawn");
        assert_eq!(tool_err.body.code, "timeout");
        assert!(tool_err.body.message.contains("exceeded deadline and was cancelled"));
        assert_eq!(tool_err.body.agent_id, Some(10000002));

        // 3. Interrupted: distinguishes from timeout, preserves message and active_agent_id
        let mut interrupted = RpcError::new(RpcErrorCode::Timeout, "spawn interrupted while session establishment continues for 10000003");
        interrupted.active_agent_id = Some("10000003".into());
        let tool_err = public_error_for_op(interrupted, "spawn");
        assert_eq!(tool_err.body.code, "timeout");
        assert!(tool_err.body.message.contains("spawn interrupted while session establishment continues"));
        assert_eq!(tool_err.body.agent_id, Some(10000003));
    }

    #[tokio::test]
    async fn facade_spawn_profile_mutex_and_json_shape() {
        // 1. General manifest for profile request omits permission_mode key from JSON
        let profile_input: AgentSpawnInput = serde_json::from_value(serde_json::json!({
            "profile": "fast-worker",
            "repository": "/tmp/repo",
            "prompt": "inspect",
        }))
        .unwrap();
        let manifest = general_manifest(&profile_input).unwrap();
        assert_eq!(manifest.permission_mode, None);
        let manifest_json = serde_json::to_value(&manifest).unwrap();
        assert!(
            manifest_json.get("permission_mode").is_none(),
            "profile request must omit permission_mode key: {manifest_json}"
        );

        // 2. Facade mutex validation rejects profile with any of subagent, model, effort, permission_mode
        let facade = SubagentMcp::new(PathBuf::from("/tmp/facade-test.sock"), Duration::from_secs(125));
        let base = serde_json::json!({
            "profile": "fast-worker",
            "repository": "/tmp/repo",
            "prompt": "inspect",
        });

        // 2a. With subagent
        let mut with_subagent = base.clone();
        with_subagent["subagent"] = serde_json::json!("codex");
        let err = facade
            .agent_spawn_inner(serde_json::from_value(with_subagent).unwrap(), || false)
            .await
            .unwrap_err();
        assert_eq!(err.body.code, "validation");
        assert!(err.body.message.contains("profile cannot be combined with"));

        // 2b. With model
        let mut with_model = base.clone();
        with_model["model"] = serde_json::json!("gpt-5");
        let err = facade
            .agent_spawn_inner(serde_json::from_value(with_model).unwrap(), || false)
            .await
            .unwrap_err();
        assert_eq!(err.body.code, "validation");
        assert!(err.body.message.contains("profile cannot be combined with"));

        // 2c. With effort
        let mut with_effort = base.clone();
        with_effort["effort"] = serde_json::json!("high");
        let err = facade
            .agent_spawn_inner(serde_json::from_value(with_effort).unwrap(), || false)
            .await
            .unwrap_err();
        assert_eq!(err.body.code, "validation");
        assert!(err.body.message.contains("profile cannot be combined with"));

        // 2d. With permission_mode
        let mut with_mode = base.clone();
        with_mode["permission_mode"] = serde_json::json!("plan");
        let err = facade
            .agent_spawn_inner(serde_json::from_value(with_mode).unwrap(), || false)
            .await
            .unwrap_err();
        assert_eq!(err.body.code, "validation");
        assert!(err.body.message.contains("profile cannot be combined with"));

        // 2e. With developer_instructions
        let mut with_di = base.clone();
        with_di["developer_instructions"] = serde_json::json!("Be concise");
        let err = facade
            .agent_spawn_inner(serde_json::from_value(with_di).unwrap(), || false)
            .await
            .unwrap_err();
        assert_eq!(err.body.code, "validation");
        assert!(err.body.message.contains("profile cannot be combined with"));

        // 3. Wire three-state: explicit null rejected via optional_non_null
        let mut null_profile = base.clone();
        null_profile["profile"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<AgentSpawnInput>(null_profile).is_err());

        let mut null_mode = base.clone();
        null_mode["permission_mode"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<AgentSpawnInput>(null_mode).is_err());

        // developer_instructions shares the same three-state wire rule.
        let mut null_di = base.clone();
        null_di["developer_instructions"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<AgentSpawnInput>(null_di).is_err());
    }
}
