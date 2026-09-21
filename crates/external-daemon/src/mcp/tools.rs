//! The public MCP server: tool catalog, RPC plumbing, and tool handlers.
//!
//! Extracted mechanically from the former private `mcp::server` module; the
//! facade at `crate::mcp` keeps every historical path importable.
use super::errors::{
    protocol_error, public_error, public_transport_error, validation_error, ToolError,
};
use super::schemas::{
    tool_output_schema, validate_text, AgentInput, AgentListInput, AgentRespondInput,
    AgentResultInput, AgentSendInput, AgentSpawnInput, AgentWaitInput, EmptyInput,
    MAX_MESSAGE_BYTES,
};
use super::types::{internal_task_id, public_task_id, PublicDecision, PublicResponseDisposition};
use super::views::{
    AgentListOutput, AgentObserveOutput, AgentRespondOutput, AgentResultOutput, AgentSendOutput,
    AgentSpawnOutput, AgentStateOutput, AgentWaitOutput, PublicMessageDisposition,
    PublicMessageReceipt, PublicResult, PublicTask, SystemStatusOutput,
};
use crate::rpc::{
    GeneralSubmitInput, MessageInput, RespondInput, ResponseDecision, ResponseOutcomeView,
    RpcClient, RpcMethod, RpcOutcome, RpcRequest, RpcService, RpcSuccess, TaskListQuery,
    TaskWaitQuery,
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
    timeout: Duration,
    service: Option<Arc<RpcService>>,
    next_request: Arc<AtomicU64>,
    tool_router: ToolRouter<Self>,
    #[cfg(test)]
    wait_handler_interrupted: Option<Arc<AtomicBool>>,
}

impl SubagentMcp {
    pub fn new(socket: PathBuf, timeout: Duration) -> Self {
        Self {
            socket: Some(socket),
            timeout,
            service: None,
            next_request: Arc::new(AtomicU64::new(1)),
            tool_router: Self::tool_router(),
            #[cfg(test)]
            wait_handler_interrupted: None,
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
            RpcOutcome::Error { error } => Err(public_error(error)
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
    ) -> Result<(PublicTask, Option<PublicResult>), ToolError> {
        match self.rpc(RpcMethod::TaskResult {
            agent_id: agent_id.clone(),
            offset,
            limit,
        })? {
            RpcSuccess::TaskResult { task, result } => {
                Ok((task.try_into()?, result.map(TryInto::try_into).transpose()?))
            }
            _ => Err(protocol_error()
                .with_operation("result")
                .with_agent_id(Some(agent_id))),
        }
    }
}

fn rpc_context(method: &RpcMethod) -> (&'static str, Option<String>) {
    match method {
        RpcMethod::SystemStatus => ("status", None),
        RpcMethod::DaemonBeginDrain { .. } => ("drain", None),
        RpcMethod::DaemonDrainStatus => ("drain_status", None),
        RpcMethod::DaemonAbortDrain => ("drain_abort", None),
        RpcMethod::DaemonActivateReady => ("activate_ready", None),
        RpcMethod::AgentProbe { .. } => ("agent_probe", None),
        RpcMethod::AgentModels { .. } => ("agent_models", None),
        RpcMethod::SubmitGeneral { .. } => ("spawn", None),
        RpcMethod::TaskList(_) => ("list", None),
        RpcMethod::TaskWait(input) => ("wait", Some(input.agent_id.clone())),
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
    Ok(GeneralTaskManifest {
        schema: GENERAL_TASK_SCHEMA.into(),
        agent_id: agent_id.clone(),
        repository,
        permission_mode: input.permission_mode.into(),
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
    description = "Read daemon/runtime readiness, the diagnostic-only mcp_version, component states, and capability limits. Read-only.",
    annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    )
)]
    async fn system_status(
        &self,
        Parameters(_): Parameters<EmptyInput>,
    ) -> Result<Json<SystemStatusOutput>, ToolError> {
        match self.rpc(RpcMethod::SystemStatus)? {
            RpcSuccess::SystemStatus { status } => Ok(Json(SystemStatusOutput::from_view(status))),
            _ => Err(protocol_error().with_operation("status")),
        }
    }

    #[tool(
    name = "external_subagent_spawn",
    output_schema = tool_output_schema::<AgentSpawnOutput>(),
    description = "Start one durable subagent in an absolute repository workspace. Specify subagent unless default_subagent is configured. ZCode uses its initialized native model and rejects model selection; dsh spawns when its enabled + spawn_supported + pinned-runtime configuration admits it. A dsh model is provider:model, split at the first colon (the model side may contain further colons; empty sides are rejected before any task). permission_mode defaults to build. Codex supports build/edit (workspace-write), plan (read-only), and yolo (danger-full-access), always with approvalPolicy=never; codex rejects non-empty write_manifest before task creation (codex_write_manifest_unsupported). dsh admits a non-empty write_manifest in build through the guarded manifest-build composition (workspace-write sandbox, only tool-fs writable, out-of-manifest paths rejected by the write-guard as FS_WRITE_MANIFEST_DENIED), bounded to 256 entries and 64 KiB serialized; an explicit [\".\"] keeps the legacy build composition and plan still requires an empty manifest. For other subagents an omitted write_manifest uses the protected workspace scope. The optional effort token (1..24 bytes of [a-z0-9_]) steers reasoning effort: codex admits only low, medium, high or xhigh, zcode and dsh pass a bounded token through to the runtime. Use wait with the returned agent_id for progress and terminal diagnostics.",
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
    ) -> Result<Json<AgentSpawnOutput>, ToolError> {
        let manifest = general_manifest(&input).map_err(|error| error.with_operation("spawn"))?;
        let task = match self.rpc(RpcMethod::SubmitGeneral(GeneralSubmitInput {
            agent: input.agent.clone(),
            model: input.model.clone(),
            effort: input.effort.clone(),
            manifest,
        }))? {
            RpcSuccess::GeneralSubmitted { task } => task,
            _ => return Err(protocol_error().with_operation("spawn")),
        };
        Ok(Json(AgentSpawnOutput {
            agent_id: public_task_id(&task.agent_id).map_err(|e| e.with_operation("spawn"))?,
            status: task.status,
        }))
    }

    #[tool(
    name = "external_subagent_wait",
    output_schema = tool_output_schema::<AgentWaitOutput>(),
    description = "Wait up to 290 seconds by default. Returns early only when an actionable pending request exists (permission requests need allow/deny; user-input requests carry the full embedded question and need answer with non-empty content) or the terminal result is available; ordinary progress, message receipts, already-responded and unsupported requests never wake it, and its timeout still returns timed_out=true. When the final result is embedded with complete=true no further result call is needed; a partial page directs you to external_subagent_result with next_offset. The pending_requests projection stays capped at 100 records while the wake decision scans the full pending set, and the request that woke the wait is always part of the returned projection so its request_id can be answered directly. activity carries the latest text tail (on the terminal response, bytes repeating the embedded result page verbatim are stripped so they are not shipped twice), a 200-char verified reasoning tail, tool calls started in the last 60 seconds, and telemetry status; use observe only when these suggest a meaningless loop.",
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
    ) -> Result<Json<AgentWaitOutput>, ToolError> {
        let agent_id =
            internal_task_id(input.agent_id).map_err(|error| error.with_operation("wait"))?;
        if input.wait_time > 299 {
            return Err(validation_error("wait_time must be between 0 and 299")
                .with_operation("wait")
                .with_agent_id(Some(agent_id)));
        }
        match self
            .rpc_wait(
                TaskWaitQuery {
                    agent_id,
                    wait_time: input.wait_time,
                    message_id: input.message_id,
                },
                move || context.ct.is_cancelled(),
            )
            .await?
        {
            RpcSuccess::TaskWait {
                task,
                pending_requests,
                result_available,
                activity,
                result,
                instruction,
                timed_out,
                message_receipt,
            } => Ok(Json(AgentWaitOutput {
                task: task.try_into()?,
                pending_requests: pending_requests.into_iter().map(Into::into).collect(),
                result_available,
                activity: activity.into(),
                result: result.map(TryInto::try_into).transpose()?,
                instruction,
                timed_out,
                message_receipt: message_receipt.map(|r| PublicMessageReceipt {
                    message_id: r.message_id,
                    state: r.state,
                    failure_code: r.failure_code,
                }),
            })),
            _ => Err(protocol_error().with_operation("wait")),
        }
    }

    #[tool(
    name = "external_subagent_observe",
    output_schema = tool_output_schema::<AgentObserveOutput>(),
    description = "仅在怀疑 subagent 陷入无意义循环时才调用，检查最近公开推理和工具调用过程；不要用于健康任务的例行轮询。只读已捕获的本 Agent 数据，不启动模型或工具。默认按本 Agent 任务生命周期内的调用次数选最多的 3 类工具，每类返回最近最多 5 次调用（名称、ID、参数，不含结果），zcode/dsh 返回已验证公开 reasoning delta 合并后的最新 200 个 Unicode 字符；codex 不采集推理，整个 reasoning 字段为 null，coverage 如实标记未采集能力。encrypted_content 始终排除。本工具不判断循环、不返回进展标签、不自动取消。调用方结合当前任务与这些事实判断：PROGRESSING（新增事实或有效推进）；EXPECTED_WAIT（有目的的计算、权限或外部等待）；NEEDS_CLARIFICATION（具体输入或决定缺失）；NO_PROGRESS_LOOP（无新信息的等价行动循环，且合理重读、等待、状态变化等解释已排除）；INSUFFICIENT_OBSERVABILITY（截断、缺口或缺少上下文，不能断言循环）。相同文本、重复 read 或 true/echo 本身不是循环；没有工具结果也不能推断工具成功、文件未变化或任务失败。判断和取消由调用方独立决定。",
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
    description = "Queue a bounded message for a running task; the daemon generates message_id when omitted and always returns the effective id",
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
    description = "Read a terminal task result with stable outcome, partial status, and bounded final-text segments. Returns null result while the task is non-terminal. Questions from user-input requests are embedded in full in external_subagent_wait projections, not paged here.",
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
        let (task, result) = self.result(agent_id, input.offset, input.limit)?;
        Ok(Json(AgentResultOutput { task, result }))
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
            "content": "continue"
        }))
        .unwrap();
        assert!(send.message_id.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_keeps_status_and_respond_serviceable_and_wakes_on_completion() {
        use crate::rpc::{RespondInput, ResponseDecision, RpcSuccess, TaskResultView};
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
            RpcSuccess::TaskWait {
                timed_out: false,
                result: Some(TaskResultView { complete: true, .. }),
                ..
            }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_wait_output_carries_the_model_rejection_reason() {
        use crate::mcp::views::PublicResult;
        use crate::rpc::RpcSuccess;
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
            )
            .unwrap();
        let facade = SubagentMcp::from_service(service);
        let response = facade
            .rpc_wait(crate::rpc::wait_tests::query(&id, 0), || false)
            .await
            .unwrap();
        let RpcSuccess::TaskWait {
            result: Some(result),
            ..
        } = response
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
        assert!(wait["instruction"]
            .as_str()
            .expect("respond instruction")
            .contains("external_subagent_respond"));
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
            }))
            .await
            .unwrap();
        assert_eq!(explicit.0.message_id, "explicit-id");
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
        let runtime = config_root.path().join("dsh-runtime");
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
                Some("chosen"),
                "model_selection_unsupported",
                "model selection is unsupported for zcode",
            ),
            (
                "dsh",
                Some("deepseek-flash"),
                "validation",
                "dsh model must be {provider}:{model}; the ':' separator is missing",
            ),
        ] {
            let input = AgentSpawnInput {
                agent: Some(agent.into()),
                model: model.map(str::to_owned),
                effort: None,
                repository: "invalid-relative-path".into(),
                prompt: "".into(),
                permission_mode: PublicPermissionMode::Plan,
                write_manifest: vec![],
            };
            let error = facade
                .agent_spawn(rmcp::handler::server::wrapper::Parameters(input))
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
        let (_directory, service, _id) = crate::rpc::wait_tests::fixture();
        let store = service.store_for_wait_test();
        let repository = tempfile::tempdir().unwrap();
        let facade = SubagentMcp::from_service(service);
        let input = AgentSpawnInput {
            agent: Some("zcode".into()),
            model: None,
            effort: Some("high".into()),
            repository: repository.path().to_string_lossy().into_owned(),
            prompt: "effort passthrough".into(),
            permission_mode: PublicPermissionMode::Plan,
            write_manifest: Vec::new(),
        };
        let output = facade
            .agent_spawn(rmcp::handler::server::wrapper::Parameters(input))
            .await
            .unwrap();
        let stored = store
            .get_task(&output.0.agent_id.to_string())
            .unwrap()
            .unwrap();
        assert!(
            stored.prepared_launch_json.contains("\"effort\":\"high\""),
            "spawn effort must reach the persisted admission identity: {}",
            stored.prepared_launch_json
        );
    }

    #[test]
    fn spawn_rpc_context_omits_the_preallocation_placeholder() {
        let method = RpcMethod::SubmitGeneral(GeneralSubmitInput {
            agent: Some("zcode".into()),
            model: None,
            effort: None,
            manifest: external_core::GeneralTaskManifest {
                schema: external_core::GENERAL_TASK_SCHEMA.into(),
                agent_id: "daemon-prepared".into(),
                repository: PathBuf::from("/tmp/repository"),
                permission_mode: external_core::PermissionMode::Plan,
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
            components: BTreeMap::from([("daemon".into(), ComponentStateView::Ready)]),
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
            "c78ea8e6c8677dc3f5e4ab9fbba376e65eb069459486951b467123aff58fdfd8"
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
        let activity = serde_json::json!({
            "latest_text_tail":"", "latest_text_truncated":false,
            "latest_reasoning":"", "tool_calls_last_60s":0,
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
            "task":task.clone(),"pending_requests":[],
            "result_available":false,"activity":activity,
            "result":null,"instruction":null,"timed_out":false,
            "message_receipt":{"message_id":"message-1","state":"queued","failure_code":null}
        });
        let successes = BTreeMap::from([
            ("external_subagent_status", status),
            (
                "external_subagent_spawn",
                serde_json::json!({"agent_id":10000001,"status":"queued"}),
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
                serde_json::json!({"task":task.clone(),"result":null}),
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
    }
}
