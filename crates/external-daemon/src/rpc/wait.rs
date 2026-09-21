//! The bounded task_wait loop: wake decisions, instructions, and projection.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use super::errors::{map_store, RpcError, RpcErrorCode};
use super::handlers::RpcService;
use super::types::{
    RpcResponse, RpcSuccess, TaskWaitQuery, MAX_PENDING_REQUESTS, MAX_REQUEST_ID_BYTES,
    MAX_RESPONSE_FRAME_BYTES, MAX_RESULT_CHUNK_BYTES,
};
use super::views::{
    pending_request_view, result_page_bounds, task_activity_view, task_view,
    truncate_at_char_boundary, MessageReceiptView, TaskResultView, MAX_QUESTION_SUMMARY_BYTES,
};
use external_store::{PendingRequestState, TaskPhase};
use std::{
    thread,
    time::{Duration, Instant},
};

#[cfg(test)]
use super::types::{MessageInput, RpcMethod, RpcOutcome, RpcRequest};
#[cfg(test)]
use super::views::{agent_capabilities, MessageDispositionView, MAX_QUESTION_EMBED_BYTES};
#[cfg(all(test, unix))]
use super::{RpcClient, RpcServer, ServerOptions};
#[cfg(test)]
use crate::{Scheduler, SchedulerError};
#[cfg(test)]
use external_core::GeneralTaskManifest;
#[cfg(test)]
use external_store::{Store, TaskOutcome, TaskRecord};
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::sync::Arc;

impl RpcService {
    pub(super) fn task_wait(
        &self,
        query: TaskWaitQuery,
        deadline: Instant,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<RpcSuccess, RpcError> {
        loop {
            if interrupted() {
                return Err(RpcError::new(RpcErrorCode::Unavailable, "wait interrupted"));
            }
            let task = self.require_task(&query.agent_id)?;
            let message_receipt = if let Some(id) = &query.message_id {
                self.store.message(id).map_err(map_store)?.and_then(|m| {
                    (m.agent_id == query.agent_id).then(|| MessageReceiptView {
                        message_id: m.message_id,
                        state: format!("{:?}", m.state).to_lowercase(),
                        failure_code: m.failure_code,
                    })
                })
            } else {
                None
            };
            // The wake decision scans the real pending owner so an actionable
            // request beyond the 100-record output projection still wakes this
            // wait; only ordinary progress, message receipts, and records the
            // caller cannot act on (already responded, unsupported kinds)
            // keep it blocked.
            let all_pending = self
                .store
                .pending_requests(&task.agent_id)
                .map_err(map_store)?;
            let actionable: Vec<_> = all_pending
                .iter()
                .filter(|request| wake_pending_request(request))
                .cloned()
                .collect();
            let wake_request = actionable.first().cloned();
            // The projection carries only actionable records and is capped;
            // the wake target is the first actionable record, so it is always
            // part of the returned projection.
            let pending_requests = actionable
                .iter()
                .take(MAX_PENDING_REQUESTS)
                .cloned()
                .map(pending_request_view)
                .collect::<Vec<_>>();
            let wake_respondable = wake_request.is_some();
            let stored_result = self.store.task_result(&task.agent_id).map_err(map_store)?;
            let result_available = stored_result.is_some();
            let activity = self.scheduler.passive_activity_snapshot(&task.agent_id);
            let terminal = task.phase == TaskPhase::Terminal;
            let now = Instant::now();
            if wake_respondable || terminal || now >= deadline {
                let timed_out = !wake_respondable && !terminal && now >= deadline;
                let result_page = stored_result
                    .filter(|_| terminal)
                    .map(|stored| self.task_result_view(stored, 0, MAX_RESULT_CHUNK_BYTES))
                    .transpose()?;
                let mut activity = task_activity_view(task.phase, activity);
                if let Some(page) = result_page.as_ref() {
                    dedup_terminal_tail(&mut activity.latest_text_tail, &page.final_text);
                }
                let instruction =
                    wait_instruction(terminal, result_page.as_ref(), wake_request.as_ref());
                let mut response = RpcSuccess::TaskWait {
                    activity,
                    task: task_view(task.clone()),
                    pending_requests,
                    result_available,
                    result: result_page,
                    instruction,
                    timed_out,
                    message_receipt,
                };
                bound_wait_result(&mut response)?;
                return Ok(response);
            }
            thread::sleep((deadline - now).min(Duration::from_millis(10)));
        }
    }
}

/// Instructions are advisory text for models that forget skill guidance
/// during long runs; they name the action the current state allows.
fn wait_instruction(
    terminal: bool,
    result_page: Option<&TaskResultView>,
    wake_request: Option<&external_store::StoredPendingRequest>,
) -> Option<String> {
    if terminal {
        return match result_page {
            Some(result) if result.complete => Some(
                "This is the final result. There is no need to call external_subagent_result again."
                    .to_owned(),
            ),
            Some(result) => Some(format!(
                "The final result is available but this bounded page is partial; continue reading it with external_subagent_result from offset {}.",
                result.next_offset.unwrap_or(result.offset)
            )),
            None => Some(
                "The task is finished; use external_subagent_result to read the stored result."
                    .to_owned(),
            ),
        };
    }
    match wake_request.map(|request| request.request_type.as_str()) {
        Some("user_input") => Some(
            "The subagent requested input; answer it now with external_subagent_respond using decision answer and non-empty content."
                .to_owned(),
        ),
        Some(_) => Some(
            "A permission request is pending; respond now with external_subagent_respond using decision allow or deny."
                .to_owned(),
        ),
        None => Some(
            "Not finished yet, call wait again; use observe only if latest_text_tail may indicate subagent runs into a meaningless loop"
                .to_owned(),
        ),
    }
}

/// The terminal wait embeds the final-result page next to the activity tail;
/// when one of the two repeats the other verbatim as a suffix (the observed
/// shapes: tail == final, preamble + final, or the tail being the final's
/// last window), strip the duplicate bytes so the same text is not shipped
/// twice. Unrelated tails — multi-page finals whose page covers the head
/// while the tail covers the end, or failure diagnostics — are kept intact.
fn dedup_terminal_tail(tail: &mut String, final_text: &str) {
    if tail.is_empty() || final_text.is_empty() {
        return;
    }
    if tail.ends_with(final_text) {
        tail.truncate(tail.len() - final_text.len());
    } else if final_text.ends_with(tail.as_str()) {
        tail.clear();
    }
}

/// A pending request wakes (and is projected) only when the caller can act
/// on it: still pending, and a known kind with a respond contract
/// (permission needs allow/deny, user_input needs answer with content).
fn wake_pending_request(request: &external_store::StoredPendingRequest) -> bool {
    request.state == PendingRequestState::Pending
        && matches!(request.request_type.as_str(), "permission" | "user_input")
}

// Measure the complete envelope with the largest valid request ID. Large result
// text falls back to the existing first-page contract; embedded questions
// degrade to a bounded prefix so actionable request_ids stay reachable. Only
// metadata that still exceeds the cap uses the transport's Oversized response.
fn bound_wait_result(response: &mut RpcSuccess) -> Result<(), RpcError> {
    let envelope = RpcResponse::success("\u{1}".repeat(MAX_REQUEST_ID_BYTES), response.clone());
    let fits = serde_json::to_vec(&envelope)
        .map_err(|_| RpcError::new(RpcErrorCode::Oversized, "response encoding failed"))?
        .len()
        .saturating_add(1)
        <= MAX_RESPONSE_FRAME_BYTES;
    if !fits {
        if let RpcSuccess::TaskWait {
            result,
            pending_requests,
            ..
        } = response
        {
            // Degrade each bounded page independently: a non-terminal wait has
            // no result to shrink, but its embedded questions still must.
            if let Some(result) = result.as_mut() {
                let (end, next_offset) =
                    result_page_bounds(&result.final_text, 0, MAX_RESULT_CHUNK_BYTES)?;
                result.final_text.truncate(end);
                result.next_offset = next_offset;
                result.complete = next_offset.is_none();
            }
            for request in pending_requests {
                if let Some(question) = request.question.as_mut() {
                    let full = std::mem::take(&mut question.text);
                    question.text = truncate_at_char_boundary(&full, MAX_QUESTION_SUMMARY_BYTES);
                    question.truncated |= question.text.len() < full.len();
                }
            }
        }
        let envelope = RpcResponse::success("\u{1}".repeat(MAX_REQUEST_ID_BYTES), response.clone());
        if serde_json::to_vec(&envelope)
            .map_or(true, |bytes| bytes.len() + 1 > MAX_RESPONSE_FRAME_BYTES)
        {
            return Err(RpcError::new(
                RpcErrorCode::Oversized,
                "response frame exceeds cap",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod wait_tests {
    use super::*;
    use crate::{CommandRuntimeFactory, SchedulerConfig};
    use external_store::TaskResult;
    use std::process::Command;

    #[test]
    fn draining_existing_task_message_is_idempotent_but_new_rejected() {
        let (_dir, service, id) = fixture();
        let msg = MessageInput {
            agent_id: id.clone(),
            message_id: Some("drain-msg".into()),
            content: "x".into(),
        };
        service
            .dispatch(RpcMethod::TaskMessage(msg.clone()))
            .unwrap();
        service
            .dispatch(RpcMethod::DaemonBeginDrain {
                cancel_active: false,
            })
            .unwrap();
        let err = service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                message_id: Some("new-msg".into()),
                ..msg.clone()
            }))
            .unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Unavailable);
        assert!(service.dispatch(RpcMethod::TaskMessage(msg)).is_ok());
    }

    #[test]
    fn abort_drain_reopens_admission_and_preserves_task_facts() {
        let (directory, service, id) = fixture();
        let original = MessageInput {
            agent_id: id.clone(),
            message_id: Some("before-abort".into()),
            content: "delivered before the drain".into(),
        };
        service
            .dispatch(RpcMethod::TaskMessage(original.clone()))
            .unwrap();
        service
            .dispatch(RpcMethod::DaemonBeginDrain {
                cancel_active: false,
            })
            .unwrap();
        let rejected = service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                message_id: Some("during-drain".into()),
                ..original.clone()
            }))
            .unwrap_err();
        assert_eq!(rejected.message, "daemon_draining");

        let RpcSuccess::DaemonDrainStatus {
            is_draining,
            ready_for_activation,
            updater_fired,
            activation_claim,
            ..
        } = service.dispatch(RpcMethod::DaemonAbortDrain).unwrap()
        else {
            panic!("abort must answer with the drain status")
        };
        assert!(!is_draining);
        assert!(!ready_for_activation);
        assert!(!updater_fired);
        assert_eq!(activation_claim, None);

        // Admission is open again: the message the drain rejected and a
        // brand-new task are both accepted, and the pre-drain delivery keeps
        // its idempotent fact.
        service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                message_id: Some("during-drain".into()),
                ..original.clone()
            }))
            .unwrap();
        service.dispatch(RpcMethod::TaskMessage(original)).unwrap();
        let workspace = directory.path().join("abort-readmission");
        std::fs::create_dir(&workspace).unwrap();
        service
            .scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: String::new(),
                repository: workspace.canonicalize().unwrap(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: "admitted after the aborted drain".into(),
                write_manifest: vec![],
            })
            .unwrap();
        // The drained task keeps its recorded facts.
        let RpcSuccess::TaskWait { task, .. } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("wait")
        };
        assert_eq!(task.agent_id, id);
        assert_ne!(task.status, "closed");
        // Bounded: aborting without an active drain is a state error, never
        // a second reset.
        let repeated = service.dispatch(RpcMethod::DaemonAbortDrain).unwrap_err();
        assert_eq!(repeated.message, "drain_not_active");
    }

    #[test]
    fn drain_transition_serializes_with_new_message_admission() {
        let (_dir, service, id) = fixture();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        service.scheduler.set_admission_hook({
            let barrier = Arc::clone(&barrier);
            Arc::new(move || {
                barrier.wait();
                barrier.wait();
            })
        });

        let message_service = Arc::clone(&service);
        let first_id = id.clone();
        let message = std::thread::spawn(move || {
            message_service.dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: first_id,
                message_id: Some("before-drain".into()),
                content: "accepted before the drain linearization point".into(),
            }))
        });

        barrier.wait();
        let drain_service = Arc::clone(&service);
        let drain = std::thread::spawn(move || {
            drain_service.dispatch(RpcMethod::DaemonBeginDrain {
                cancel_active: false,
            })
        });
        barrier.wait();

        assert!(message.join().unwrap().is_ok());
        assert!(drain.join().unwrap().is_ok());
        service.scheduler.set_admission_hook(Arc::new(|| {}));

        let error = service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: id,
                message_id: Some("after-drain".into()),
                content: "must be rejected".into(),
            }))
            .unwrap_err();
        assert_eq!(error.code, RpcErrorCode::Unavailable);
        assert_eq!(error.message, "daemon_draining");
        assert!(service.store.message("after-drain").unwrap().is_none());
    }

    #[test]
    fn drain_transition_serializes_with_new_task_admission() {
        let (directory, service, _) = fixture();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        service.scheduler.set_admission_hook({
            let barrier = Arc::clone(&barrier);
            Arc::new(move || {
                barrier.wait();
                barrier.wait();
            })
        });

        let task_workspace = directory.path().join("spawn-admission");
        std::fs::create_dir(&task_workspace).unwrap();
        let manifest = GeneralTaskManifest {
            schema: "zcode-general-task/v1".into(),
            agent_id: String::new(),
            repository: task_workspace.canonicalize().unwrap(),
            permission_mode: external_core::PermissionMode::Plan,
            prompt: "accepted before the drain linearization point".into(),
            write_manifest: vec![],
        };
        let enqueue_service = Arc::clone(&service);
        let first_manifest = manifest.clone();
        let enqueue =
            std::thread::spawn(move || enqueue_service.scheduler.enqueue_general(&first_manifest));

        barrier.wait();
        let drain_service = Arc::clone(&service);
        let drain = std::thread::spawn(move || {
            drain_service.dispatch(RpcMethod::DaemonBeginDrain {
                cancel_active: false,
            })
        });
        barrier.wait();

        assert!(enqueue.join().unwrap().is_ok());
        assert!(drain.join().unwrap().is_ok());
        service.scheduler.set_admission_hook(Arc::new(|| {}));

        let error = service.scheduler.enqueue_general(&manifest).unwrap_err();
        assert!(matches!(
            error,
            SchedulerError::InvalidConfig(ref message) if message == "daemon_draining"
        ));
    }

    #[test]
    fn draining_lifecycle_methods_are_not_gate_rejected() {
        let (_dir, service, id) = fixture();
        service
            .dispatch(RpcMethod::DaemonBeginDrain {
                cancel_active: false,
            })
            .unwrap();
        let methods = [
            RpcMethod::TaskWait(query(&id, 0)),
            RpcMethod::TaskCancel {
                agent_id: id.clone(),
            },
            RpcMethod::TaskResult {
                agent_id: id.clone(),
                offset: 0,
                limit: 10,
            },
            RpcMethod::TaskClose { agent_id: id },
        ];
        for method in methods {
            if let Err(error) = service.dispatch(method) {
                assert!(!error.message.contains("daemon_draining"));
            }
        }
    }

    pub(crate) fn fixture() -> (tempfile::TempDir, Arc<RpcService>, String) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("s01-wait-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| -> std::io::Result<Command> {
            panic!("wait fixture must never start a runtime")
        });
        let scheduler = Scheduler::new(
            "wait-test",
            store.clone(),
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: "".into(),
                repository: directory.path().canonicalize().unwrap(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: "wait fixture".into(),
                write_manifest: vec![],
            })
            .unwrap();
        let id = submitted.agent_id;
        let claim = store.claim_next("wait-test", 10, 10).unwrap().unwrap();
        store
            .mark_session_running(&id, claim.owner_epoch, "runtime", None, None, None)
            .unwrap();
        (
            directory,
            Arc::new(RpcService::new(scheduler, store).unwrap()),
            id,
        )
    }

    pub(crate) fn query(id: &str, wait_time: u64) -> TaskWaitQuery {
        TaskWaitQuery {
            agent_id: id.into(),
            wait_time,
            message_id: None,
        }
    }

    #[test]
    fn wait_defaults_limits_and_old_method_rejection() {
        let (_, service, id) = fixture();
        let parsed: TaskWaitQuery =
            serde_json::from_value(serde_json::json!({"agent_id":id})).unwrap();
        assert_eq!(parsed.wait_time, 290);
        assert_eq!(agent_capabilities().max_wait_ms, 299000);
        for value in [-1, 300] {
            let response = service.handle_bytes(
                &serde_json::to_vec(&serde_json::json!({
                    "request_id":"limits","method":"task_wait",
                    "params":{"agent_id":id,"wait_time":value}
                }))
                .unwrap(),
            );
            assert!(matches!(
                response.outcome,
                RpcOutcome::Error {
                    error: RpcError {
                        code: RpcErrorCode::Validation,
                        ..
                    }
                }
            ));
        }
        // The removed custom envelope version is rejected as an unknown field.
        for obsolete in [
            serde_json::json!({"request_id":"obsolete","method":"task_wait",
                "params":{"agent_id":id},"version":13}),
            serde_json::json!({"request_id":"obsolete","method":"task_wait",
                "params":{"agent_id":id,"after_revision":0}}),
        ] {
            let response = service.handle_bytes(&serde_json::to_vec(&obsolete).unwrap());
            assert!(
                matches!(
                    response.outcome,
                    RpcOutcome::Error {
                        error: RpcError {
                            code: RpcErrorCode::Validation,
                            ..
                        }
                    }
                ),
                "obsolete frame was accepted: {obsolete}"
            );
        }
        assert!(!RpcMethod::is_known("task_poll"));
        let before = service.store.get_task(&id).unwrap();
        let start = Instant::now();
        let RpcSuccess::TaskWait {
            timed_out: true,
            instruction,
            ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected unfinished wait response")
        };
        assert_eq!(
            instruction.as_deref(),
            Some("Not finished yet, call wait again; use observe only if latest_text_tail may indicate subagent runs into a meaningless loop")
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(before, service.store.get_task(&id).unwrap());
        // A terminal task accepts the maximum without actually sleeping.
        service
            .store
            .store_task_result(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "done".into(),
                    partial: false,
                },
            )
            .unwrap();
        let RpcSuccess::TaskWait {
            timed_out: false,
            instruction,
            result_available,
            result: Some(result),
            ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert!(result_available);
        assert!(result.complete);
        assert_eq!(
            instruction.as_deref(),
            Some("This is the final result. There is no need to call external_subagent_result again.")
        );
    }

    #[test]
    fn wait_ignores_message_receipt_and_ordinary_activity() {
        let (_, service, id) = fixture();
        service
            .store
            .insert_message("message-1", &id, "queue", "continue")
            .unwrap();
        let tracker = Arc::new(crate::PassiveActivityTracker::new(true));
        service
            .scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .insert(id.clone(), tracker.clone());
        let mut input = query(&id, 1);
        input.message_id = Some("message-1".into());
        let before = service.store.get_task(&id).unwrap();
        let start = Instant::now();
        let iterations = std::cell::Cell::new(0usize);
        let response = service
            .task_wait(input, start + Duration::from_millis(80), &|| {
                // Deterministic mid-wait snapshot changes, without timing a worker.
                iterations.set(iterations.get() + 1);
                if iterations.get() == 2 {
                    tracker.set_wait_fixture(Instant::now());
                }
                false
            })
            .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(80));
        let RpcSuccess::TaskWait {
            timed_out: true,
            activity,
            instruction,
            message_receipt: Some(_),
            ..
        } = response
        else {
            panic!("ordinary activity ended wait")
        };
        assert_eq!(activity.latest_text_tail, "ordinary text");
        assert_eq!(activity.tool_calls_last_60s, 1);
        assert_eq!(
            instruction.as_deref(),
            Some("Not finished yet, call wait again; use observe only if latest_text_tail may indicate subagent runs into a meaningless loop")
        );
        assert_eq!(before, service.store.get_task(&id).unwrap());
    }

    #[test]
    fn terminal_wait_strips_tail_bytes_repeating_the_result_page() {
        // codex shape: preamble + final; dsh shape: tail == final; unrelated
        // or empty inputs stay untouched. CJK content pins the char boundary.
        for (tail, final_text, expected) in [
            ("开场白。最终答复", "最终答复", "开场白。"),
            ("最终答复", "最终答复", ""),
            (
                "死前最后的文本",
                "Invalid params: unknown model",
                "死前最后的文本",
            ),
            ("", "最终答复", ""),
            ("最终答复", "", "最终答复"),
        ] {
            let mut tail = tail.to_owned();
            dedup_terminal_tail(&mut tail, final_text);
            assert_eq!(tail, expected, "final={final_text:?}");
        }
        // A final larger than the 8 KiB tail window: the tail is exactly the
        // final's last window and fully duplicated.
        let long_final = format!("{}结尾", "x".repeat(9 * 1024));
        let mut tail = long_final[long_final.len() - 4096..].to_owned();
        dedup_terminal_tail(&mut tail, &long_final);
        assert_eq!(tail, "");
    }

    #[test]
    fn terminal_wait_dedups_activity_tail_against_embedded_result() {
        let (_, service, id) = fixture();
        let tracker = Arc::new(crate::PassiveActivityTracker::new(true));
        tracker.set_wait_tail_fixture("我会按只读方式检查。HEAD 为 4cd6c57，总结……");
        service
            .scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .insert(id.clone(), tracker);
        service
            .store
            .store_task_result(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "HEAD 为 4cd6c57，总结……".into(),
                    partial: false,
                },
            )
            .unwrap();
        let RpcSuccess::TaskWait {
            activity,
            result: Some(result),
            ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert!(result.complete);
        assert_eq!(activity.latest_text_tail, "我会按只读方式检查。");
        assert!(!activity.latest_text_truncated);
    }

    fn stored_pending(
        request_type: &str,
        state: PendingRequestState,
    ) -> external_store::StoredPendingRequest {
        external_store::StoredPendingRequest {
            request_id: "r".into(),
            agent_id: "a".into(),
            correlation_id: "1".into(),
            request_type: request_type.into(),
            payload_json: r#"{"toolName":"Read"}"#.into(),
            state,
            response_decision: None,
            response_content: None,
            created_at: 0,
        }
    }

    #[test]
    fn wait_wake_predicate_matches_actionable_pending_requests() {
        for (request_type, state, wakes) in [
            ("permission", PendingRequestState::Pending, true),
            ("user_input", PendingRequestState::Pending, true),
            ("unsupported_input", PendingRequestState::Pending, false),
            ("permission", PendingRequestState::Sending, false),
            ("permission", PendingRequestState::Responded, false),
            ("user_input", PendingRequestState::Responded, false),
        ] {
            assert_eq!(
                wake_pending_request(&stored_pending(request_type, state)),
                wakes,
                "{request_type}/{state:?}"
            );
        }
    }

    #[test]
    fn wait_wakes_for_non_bash_respondable_permission() {
        let (_, service, id) = fixture();
        service
            .store
            .insert_pending_request(
                "request",
                &id,
                "correlation",
                "permission",
                r#"{"toolName":"Read"}"#,
            )
            .unwrap();
        let RpcSuccess::TaskWait {
            pending_requests,
            timed_out,
            instruction,
            ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        assert!(!timed_out);
        assert_eq!(pending_requests.len(), 1);
        assert_eq!(pending_requests[0].tool_name.as_deref(), Some("Read"));
        assert_eq!(
            instruction.as_deref(),
            Some("A permission request is pending; respond now with external_subagent_respond using decision allow or deny.")
        );
    }

    #[test]
    fn wait_wakes_for_user_input_with_the_full_embedded_question() {
        let (_, service, id) = fixture();
        service
            .store
            .insert_pending_request(
                "request",
                &id,
                "correlation",
                "user_input",
                r#"{"question":"which branch?"}"#,
            )
            .unwrap();
        // Every actionable request wakes without a capability handshake; the
        // question is embedded in full in the projection.
        let RpcSuccess::TaskWait {
            pending_requests,
            timed_out,
            instruction,
            ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        assert!(!timed_out);
        assert_eq!(pending_requests.len(), 1);
        assert_eq!(pending_requests[0].kind, "user_input");
        assert_eq!(pending_requests[0].summary, "question which branch?");
        let question = pending_requests[0].question.as_ref().expect("question");
        assert_eq!(question.text, "which branch?");
        assert!(!question.truncated);
        assert_eq!(
            instruction.as_deref(),
            Some("The subagent requested input; answer it now with external_subagent_respond using decision answer and non-empty content.")
        );
    }

    #[test]
    fn wait_never_wakes_for_or_projects_unsupported_input() {
        let (_, service, id) = fixture();
        service
            .store
            .insert_pending_request(
                "request",
                &id,
                "correlation",
                "unsupported_input",
                r#"{"origin":"dsh_acp","reason":"dsh server request is not a supported interaction"}"#,
            )
            .unwrap();
        // The unsupported sentinel is not actionable for any caller: it
        // neither wakes the wait nor appears in the projection.
        let start = Instant::now();
        let RpcSuccess::TaskWait {
            pending_requests,
            timed_out,
            instruction,
            ..
        } = service
            .task_wait(query(&id, 0), start + Duration::from_millis(30), &|| false)
            .unwrap()
        else {
            panic!("expected wait response")
        };
        assert!(timed_out);
        assert!(pending_requests.is_empty());
        assert_eq!(
            instruction.as_deref(),
            Some("Not finished yet, call wait again; use observe only if latest_text_tail may indicate subagent runs into a meaningless loop")
        );
    }

    #[test]
    fn long_answer_questions_expose_bounded_content_with_an_observable_cut() {
        let (_, service, id) = fixture();
        let question = "q".repeat(5000);
        service
            .store
            .insert_pending_request(
                "request",
                &id,
                "correlation",
                "user_input",
                &serde_json::to_string(&serde_json::json!({ "question": question })).unwrap(),
            )
            .unwrap();
        let RpcSuccess::TaskWait {
            pending_requests, ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        let summary = &pending_requests[0].summary;
        // The exposed prefix is bounded by bytes, the cut states exactly how
        // many question chars were omitted, and the full marker resolves.
        assert_eq!(
            summary,
            &format!(
                "question {} [+{} more chars]",
                "q".repeat(2048),
                5000 - 2048
            )
        );
        // A multibyte question cuts on a character boundary inside the cap.
        let wide = "語".repeat(1024);
        service
            .store
            .insert_pending_request(
                "wide",
                &id,
                "correlation-wide",
                "user_input",
                &serde_json::to_string(&serde_json::json!({ "question": wide })).unwrap(),
            )
            .unwrap();
        let RpcSuccess::TaskWait {
            pending_requests, ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        let wide_summary = &pending_requests
            .iter()
            .find(|request| request.request_id == "wide")
            .expect("wide request in projection")
            .summary;
        // 2048 bytes holds 682 three-byte chars (2 bytes remain, below the
        // next boundary), so 342 chars are omitted.
        assert_eq!(
            wide_summary.as_str(),
            format!("question {} [+{} more chars]", "語".repeat(682), 342)
        );
    }

    #[test]
    fn oversized_questions_embed_a_truncated_prefix_without_continuation() {
        let (_, service, id) = fixture();
        // The decisive option sits entirely beyond the 16 KiB embed cap.
        let option = "Deploy to which environment? options: [production, staging]";
        let question = format!("{}{}", "x".repeat(MAX_QUESTION_EMBED_BYTES + 500), option);
        service
            .store
            .insert_pending_request(
                "request",
                &id,
                "correlation",
                "user_input",
                &serde_json::to_string(&serde_json::json!({ "question": question })).unwrap(),
            )
            .unwrap();
        let RpcSuccess::TaskWait {
            pending_requests, ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        let view = &pending_requests[0];
        // wait is the only question surface: the embed is generous but
        // honestly truncated, and there is no continuation contract.
        let page = view.question.as_ref().expect("embedded question");
        assert_eq!(page.text.len(), MAX_QUESTION_EMBED_BYTES);
        assert_eq!(page.text, "x".repeat(MAX_QUESTION_EMBED_BYTES));
        assert!(page.truncated);
        assert!(!page.text.contains(option));
    }

    #[test]
    fn worst_case_wait_projection_with_question_pages_fits_the_frame() {
        let (_, service, id) = fixture();
        let question = format!("{}{}", "q".repeat(3000), "tail");
        for index in 0..MAX_PENDING_REQUESTS {
            service
                .store
                .insert_pending_request(
                    &format!("request-{index}"),
                    &id,
                    &format!("correlation-{index}"),
                    "user_input",
                    &serde_json::to_string(&serde_json::json!({ "question": question })).unwrap(),
                )
                .unwrap();
        }
        let response = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap();
        let envelope = RpcResponse::success("\u{1}".repeat(128), response);
        assert!(serde_json::to_vec(&envelope).unwrap().len() + 1 <= MAX_RESPONSE_FRAME_BYTES);
    }

    #[test]
    fn oversized_question_projections_degrade_to_bounded_prefixes_instead_of_erroring() {
        let (_, service, id) = fixture();
        // Escape-dense questions: each embeds 16 KiB of quotes, so a full
        // 100-record projection serializes far past the response frame cap.
        // Before the degradation path the wait itself returned Oversized,
        // hiding every request_id from the caller.
        let question = "\"".repeat(MAX_QUESTION_EMBED_BYTES);
        for index in 0..MAX_PENDING_REQUESTS {
            service
                .store
                .insert_pending_request(
                    &format!("request-{index}"),
                    &id,
                    &format!("correlation-{index}"),
                    "user_input",
                    &serde_json::to_string(&serde_json::json!({ "question": question })).unwrap(),
                )
                .unwrap();
        }
        let response = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap();
        let RpcSuccess::TaskWait {
            pending_requests, ..
        } = &response
        else {
            panic!("expected wait response")
        };
        assert_eq!(pending_requests.len(), MAX_PENDING_REQUESTS);
        for request in pending_requests {
            let degraded = request.question.as_ref().expect("embedded question");
            assert!(
                degraded.text.len() <= MAX_QUESTION_SUMMARY_BYTES,
                "degraded question stays bounded"
            );
            assert!(degraded.truncated);
            assert!(!request.request_id.is_empty());
        }
        let envelope = RpcResponse::success("\u{1}".repeat(128), response);
        assert!(serde_json::to_vec(&envelope).unwrap().len() + 1 <= MAX_RESPONSE_FRAME_BYTES);
    }

    #[test]
    fn result_rejects_the_removed_request_id_param_and_unknown_fields() {
        let (_, service, id) = fixture();
        // Question retrieval moved fully into wait: request_id is no longer
        // part of the task_result contract and the wire rejects it verbatim.
        let response = service.handle_bytes(
            &serde_json::to_vec(&serde_json::json!({
                "request_id":"wire",
                "method":"task_result",
                "params":{"agent_id":id,"request_id":"missing"}
            }))
            .unwrap(),
        );
        assert!(matches!(
            response.outcome,
            RpcOutcome::Error {
                error: RpcError {
                    code: RpcErrorCode::Validation,
                    ..
                }
            }
        ));
        let response = service.handle_bytes(
            &serde_json::to_vec(&serde_json::json!({
                "request_id":"wire",
                "method":"task_result",
                "params":{"agent_id":id,"bogus":true}
            }))
            .unwrap(),
        );
        assert!(matches!(
            response.outcome,
            RpcOutcome::Error {
                error: RpcError {
                    code: RpcErrorCode::Validation,
                    ..
                }
            }
        ));
    }

    #[test]
    fn wait_wakes_for_an_actionable_request_beyond_unsupported_noise() {
        let (_, service, id) = fixture();
        for index in 0..=MAX_PENDING_REQUESTS {
            let (request_type, payload) = if index < MAX_PENDING_REQUESTS {
                ("unsupported_input", r#"{}"#)
            } else {
                ("permission", r#"{"toolName":"Read"}"#)
            };
            service
                .store
                .insert_pending_request(
                    &format!("request-{index}"),
                    &id,
                    &format!("correlation-{index}"),
                    request_type,
                    payload,
                )
                .unwrap();
        }
        let start = Instant::now();
        let response = service
            .task_wait(query(&id, 1), start + Duration::from_millis(60), &|| false)
            .unwrap();
        let RpcSuccess::TaskWait {
            pending_requests,
            timed_out,
            ..
        } = response
        else {
            panic!("expected wait response")
        };
        // The wake decision scans the real pending owner while the projection
        // carries only actionable records: the 100 unsupported sentinels stay
        // invisible and the actionable 101st is the projected wake target.
        assert!(!timed_out);
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(pending_requests.len(), 1);
        let wake = &pending_requests[0];
        assert_eq!(wake.request_id, "request-100");
        assert_eq!(wake.kind, "permission");
        // The returned id addresses the real 101st store row, not a
        // projection-only copy.
        assert_eq!(
            service
                .store
                .pending_request(&id, &wake.request_id)
                .unwrap()
                .expect("wake request row")
                .request_type,
            "permission"
        );
        // User-input requests are actionable for every caller now: the same
        // layout wakes without a capability handshake and embeds the question.
        let (_, service, id) = fixture();
        for index in 0..=MAX_PENDING_REQUESTS {
            let (request_type, payload) = if index < MAX_PENDING_REQUESTS {
                ("unsupported_input", r#"{}"#)
            } else {
                ("user_input", r#"{"question":"continue?"}"#)
            };
            service
                .store
                .insert_pending_request(
                    &format!("request-{index}"),
                    &id,
                    &format!("correlation-{index}"),
                    request_type,
                    payload,
                )
                .unwrap();
        }
        let start = Instant::now();
        let RpcSuccess::TaskWait {
            pending_requests,
            timed_out,
            ..
        } = service
            .task_wait(query(&id, 1), start + Duration::from_millis(60), &|| false)
            .unwrap()
        else {
            panic!("expected wait response")
        };
        assert!(!timed_out);
        assert_eq!(pending_requests.len(), 1);
        assert_eq!(pending_requests[0].kind, "user_input");
        assert_eq!(
            pending_requests[0].question.as_ref().unwrap().text,
            "continue?"
        );
    }

    #[test]
    fn wait_bash_state_transition_stops_early_wake() {
        let (_, service, id) = fixture();
        service
            .store
            .insert_pending_request(
                "request",
                &id,
                "correlation",
                "permission",
                r#"{"toolName":"Bash"}"#,
            )
            .unwrap();
        assert!(matches!(
            service
                .dispatch(RpcMethod::TaskWait(query(&id, 299)))
                .unwrap(),
            RpcSuccess::TaskWait {
                timed_out: false,
                ..
            }
        ));
        service
            .store
            .claim_pending_response_if_accepting(&id, "request", "allow", None)
            .unwrap();
        assert!(matches!(
            service
                .task_wait(
                    query(&id, 1),
                    Instant::now() + Duration::from_millis(30),
                    &|| false
                )
                .unwrap(),
            RpcSuccess::TaskWait {
                timed_out: true,
                ..
            }
        ));
    }

    #[test]
    fn wait_terminal_results_preserve_outcomes_and_page_large_text() {
        for (outcome, text, fits_page) in [
            (
                TaskOutcome::Completed,
                "x".repeat(MAX_RESULT_CHUNK_BYTES + 10),
                false,
            ),
            (TaskOutcome::Failed, "failure".into(), true),
            (TaskOutcome::Cancelled, "cancelled".into(), true),
            (
                TaskOutcome::Completed,
                "\0".repeat(MAX_RESPONSE_FRAME_BYTES),
                false,
            ),
        ] {
            let (_, service, id) = fixture();
            service
                .store
                .store_task_result(
                    &id,
                    &TaskResult {
                        outcome,
                        final_text: text.clone(),
                        partial: outcome != TaskOutcome::Completed,
                    },
                )
                .unwrap();
            let response = service
                .dispatch(RpcMethod::TaskWait(query(&id, 299)))
                .unwrap();
            let RpcSuccess::TaskWait {
                result: Some(result),
                timed_out: false,
                instruction,
                ..
            } = &response
            else {
                panic!("terminal result missing")
            };
            assert_eq!(result.outcome, outcome);
            assert_eq!(result.total_bytes, text.len());
            // Wait inlines the same bounded first page as external_subagent_result.
            assert_eq!(result.offset, 0);
            assert_eq!(result.complete, fits_page);
            assert_eq!(
                result.next_offset,
                (!fits_page).then_some(MAX_RESULT_CHUNK_BYTES)
            );
            assert_eq!(result.final_text, text[..result.final_text.len()]);
            let instruction = instruction.as_deref().expect("terminal instruction");
            if fits_page {
                assert_eq!(
                    instruction,
                    "This is the final result. There is no need to call external_subagent_result again."
                );
            } else {
                assert_eq!(
                    instruction,
                    &format!(
                        "The final result is available but this bounded page is partial; continue reading it with external_subagent_result from offset {}.",
                        MAX_RESULT_CHUNK_BYTES
                    )
                );
            }
            assert!(
                serde_json::to_vec(&RpcResponse::success("q".repeat(128), response.clone()))
                    .unwrap()
                    .len()
                    + 1
                    <= MAX_RESPONSE_FRAME_BYTES
            );
            assert_eq!(
                service
                    .store
                    .task_result(&id)
                    .unwrap()
                    .unwrap()
                    .result
                    .final_text,
                text
            );
        }
    }

    #[test]
    fn wait_interruption_does_not_mutate_task() {
        let (_, service, id) = fixture();
        let before = service.store.get_task(&id).unwrap();
        let response = service.handle_bytes_interruptible(
            &serde_json::to_vec(&RpcRequest {
                request_id: "interrupt".into(),
                method: RpcMethod::TaskWait(query(&id, 299)),
            })
            .unwrap(),
            &|| true,
        );
        assert!(matches!(response.outcome, RpcOutcome::Error { .. }));
        assert_eq!(before, service.store.get_task(&id).unwrap());
    }

    #[test]
    fn assembled_service_queues_message_and_exposes_receipt() {
        let (_directory, service, id) = fixture();
        let response = service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: id.clone(),
                message_id: Some("assembled-message".into()),
                content: "continue with the requested work".into(),
            }))
            .unwrap();
        let RpcSuccess::Message { disposition, .. } = response else {
            panic!("expected message response");
        };
        assert_eq!(disposition, MessageDispositionView::Queued);
        let receipt = service
            .store
            .message("assembled-message")
            .unwrap()
            .expect("queued message receipt");
        assert_eq!(receipt.message_id, "assembled-message");
        assert_eq!(receipt.mode, "queue");
        assert_eq!(receipt.content, "continue with the requested work");
    }

    #[test]
    fn daemon_generates_missing_message_ids_and_returns_them() {
        let (_directory, service, id) = fixture();
        let generated = service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: id.clone(),
                message_id: None,
                content: "continue without an explicit id".into(),
            }))
            .unwrap();
        let RpcSuccess::Message {
            message_id,
            disposition,
            ..
        } = generated
        else {
            panic!("expected message response")
        };
        assert_eq!(disposition, MessageDispositionView::Queued);
        assert!(
            message_id.starts_with("subagent-message-"),
            "generated id must use the daemon prefix: {message_id}"
        );
        assert!(service.store.message(&message_id).unwrap().is_some());
        // An explicit id keeps its idempotency semantics; a different content
        // under the same id is still a conflict.
        let repeated = service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: id.clone(),
                message_id: Some(message_id.clone()),
                content: "continue without an explicit id".into(),
            }))
            .unwrap();
        let RpcSuccess::Message {
            message_id: echoed, ..
        } = repeated
        else {
            panic!("expected message response")
        };
        assert_eq!(echoed, message_id);
        assert!(matches!(
            service.dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: id.clone(),
                message_id: Some("explicit-conflict".into()),
                content: "first".into(),
            })),
            Ok(_)
        ));
        assert!(service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: id,
                message_id: Some("explicit-conflict".into()),
                content: "different".into(),
            }))
            .is_err());
    }

    #[test]
    fn terminal_task_rejects_message_without_deleting_result() {
        let (_directory, service, id) = fixture();
        service
            .store
            .store_task_result(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "terminal result".into(),
                    partial: false,
                },
            )
            .unwrap();
        let before = service.store.task_result(&id).unwrap().unwrap();
        let response = service.dispatch(RpcMethod::TaskMessage(MessageInput {
            agent_id: id.clone(),
            message_id: Some("terminal-message".into()),
            content: "must not resume".into(),
        }));
        assert!(matches!(
            response,
            Err(RpcError {
                code: RpcErrorCode::Validation,
                ..
            })
        ));
        assert_eq!(
            service.store.task_result(&id).unwrap().unwrap().result,
            before.result
        );
        assert!(service.store.message("terminal-message").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn wait_socket_disconnect_and_shutdown_release_workers_without_task_mutation() {
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixStream;
        let (directory, service, id) = fixture();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        // macOS sockaddr_un caps path length; a cwd-relative path still keeps
        // the fixture inside the repository's prescribed workspace directory.
        let absolute_socket = directory.path().join("rpc.sock");
        let socket = absolute_socket
            .strip_prefix(std::env::current_dir().unwrap())
            .unwrap()
            .to_path_buf();
        let server = RpcServer::bind(
            &socket,
            service.clone(),
            ServerOptions {
                max_connections: 1,
                ..ServerOptions::default()
            },
        )
        .unwrap();
        let before = service.store.get_task(&id).unwrap();
        let request = RpcRequest {
            request_id: "wait".into(),
            method: RpcMethod::TaskWait(query(&id, 299)),
        };
        let mut frame = serde_json::to_vec(&request).unwrap();
        frame.push(b'\n');
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(&frame).unwrap();
        // Let the bounded connection worker enter the request, then disconnect.
        thread::sleep(Duration::from_millis(30));
        drop(stream);
        let client = RpcClient::new(&socket, Duration::from_secs(1));
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let response = client
                .call(&RpcRequest {
                    request_id: "status".into(),
                    method: RpcMethod::SystemStatus,
                })
                .unwrap();
            if matches!(response.outcome, RpcOutcome::Success { .. }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "disconnected wait retained connection slot"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(20));
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream.write_all(&frame).unwrap();
        thread::sleep(Duration::from_millis(30));
        let start = Instant::now();
        server.shutdown();
        assert!(start.elapsed() < Duration::from_secs(1));
        let mut returned = String::new();
        stream.read_to_string(&mut returned).unwrap();
        assert!(returned.contains("wait interrupted"));
        assert_eq!(before, service.store.get_task(&id).unwrap());
    }
}
