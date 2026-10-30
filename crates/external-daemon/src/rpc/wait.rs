//! The bounded task_wait loop: wake decisions and projection.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use super::errors::{map_store, RpcError, RpcErrorCode};
use super::handlers::{failure_message_projection, RpcService};
use super::types::{
    RpcResponse, RpcSuccess, TaskWaitBody, TaskWaitManyBody, TaskWaitQuery, WaitTarget,
    MAX_PENDING_REQUESTS, MAX_REQUEST_ID_BYTES, MAX_RESPONSE_FRAME_BYTES, MAX_RESULT_CHUNK_BYTES,
};
use super::views::{
    pending_request_view, task_activity_view, task_header_view, truncate_at_char_boundary,
    MessageReceiptView, MAX_QUESTION_SUMMARY_BYTES,
};
use crate::scheduler::shrink_persistable_record;
use external_store::{PendingRequestState, TaskPhase};
use std::{
    thread,
    time::{Duration, Instant},
};

#[cfg(test)]
use super::types::{MessageInput, RpcMethod, RpcOutcome, RpcRequest};
#[cfg(test)]
use super::views::{
    agent_capabilities, MessageDispositionView, TaskResultView, MAX_QUESTION_EMBED_BYTES,
};
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
        match &query.agent_id {
            WaitTarget::One(agent_id) => self.task_wait_one(agent_id, &query, deadline, interrupted),
            WaitTarget::Many(agent_ids) => {
                self.task_wait_many(agent_ids, deadline, interrupted)
            }
        }
    }

    fn task_wait_one(
        &self,
        agent_id: &str,
        query: &TaskWaitQuery,
        deadline: Instant,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<RpcSuccess, RpcError> {
        loop {
            if interrupted() {
                return Err(RpcError::new(RpcErrorCode::Unavailable, "wait interrupted"));
            }
            // One snapshot per round: the task row, its optional immutable
            // result, and the terminal reason are read under a single store
            // lock, so a concurrent terminal write can never be interleaved
            // between them.
            let (task, stored_result, reason) = self.require_task_with_result(agent_id)?;
            let message_receipt = if let Some(id) = &query.message_id {
                self.store.message(id).map_err(map_store)?.and_then(|m| {
                    (m.agent_id == *agent_id).then(|| MessageReceiptView {
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
            let activity = self.scheduler.passive_activity_snapshot(&task.agent_id);
            let terminal = task.phase == TaskPhase::Terminal;
            let now = Instant::now();
            if wake_respondable || terminal || now >= deadline {
                let timed_out = !wake_respondable && !terminal && now >= deadline;
                let result_page = stored_result
                    .filter(|_| terminal)
                    .map(|stored| {
                        let failure_message = failure_message_projection(&task, &stored);
                        self.task_result_view(
                            stored,
                            0,
                            MAX_RESULT_CHUNK_BYTES,
                            reason,
                            failure_message,
                        )
                    })
                    .transpose()?;
                let mut activity = task_activity_view(task.phase, activity);
                // Take-on-delivery: only bytes appended since the previous
                // wait are shipped, and the cursor advances in the same lock.
                // The take happens here, at the single response construction
                // point, never mid-loop.
                if let Some(wait_tail) = self.scheduler.take_wait_tail(&task.agent_id) {
                    activity.latest_text_tail = wait_tail.text;
                    activity.latest_text_truncated = wait_tail.truncated;
                }
                if let Some(page) = result_page.as_ref() {
                    dedup_terminal_tail(&mut activity.latest_text_tail, &page.final_text);
                }
                let mut response = RpcSuccess::TaskWait(TaskWaitBody {
                    activity,
                    task: task_header_view(&task),
                    pending_requests,
                    result: result_page,
                    timed_out,
                    message_receipt,
                });
                bound_wait_result(&mut response)?;
                return Ok(response);
            }
            thread::sleep((deadline - now).min(Duration::from_millis(10)));
        }
    }

    /// List-mode wait: blocks until every distinct id is terminal, any one of
    /// them raises an actionable pending request, or the deadline expires.
    /// Response shape is FLAT: one key per distinct id carrying that agent's
    /// wait body — a duplicated id is awaited once like any other — plus a
    /// single `error` key collecting problematic ids (`duplicated`,
    /// `unknown_agent`). Unknown ids resolve on the first snapshot and stop
    /// participating without blocking the rest.
    fn task_wait_many(
        &self,
        agent_ids: &[String],
        deadline: Instant,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<RpcSuccess, RpcError> {
        // Distinct ids in caller order; repetitions are only flagged in the
        // error map, the id itself is still awaited and returned.
        let mut distinct: Vec<String> = Vec::with_capacity(agent_ids.len());
        let mut seen = std::collections::HashSet::new();
        let mut duplicated = std::collections::HashSet::new();
        for id in agent_ids {
            if seen.insert(id.clone()) {
                distinct.push(id.clone());
            } else {
                duplicated.insert(id.clone());
            }
        }
        let mut unknown = std::collections::HashSet::new();
        // Diagnostic attribution anchor for wait-path failures: a stable id
        // for the log line (record_failure keys its in-memory latest map by
        // agent id; a fleet failure belongs to the wait, not one member).
        let primary_id_hint = agent_ids
            .first()
            .cloned()
            .unwrap_or_else(|| "wait-many".to_owned());
        // Latest per-agent snapshot: Some((body, terminal, epoch)) once the
        // agent is terminal (candidate final) or actionable (candidate early
        // return); None while it is plain running. `epoch` is the task row's
        // owner_epoch at snapshot time: every claim increments it, so a
        // terminal → resume → terminal cycle between two poll rounds is
        // detectable even though the phase reads terminal again — the stale
        // snapshot (with the previous round's deleted result) is dropped.
        // Snapshots PEEK the wait tail (cursor untouched); the delivery
        // cursor is only committed at the single response-construction
        // point below after the frame passes its size check.
        let mut bodies: Vec<(String, Option<(TaskWaitBody, bool, u64)>)> =
            Vec::with_capacity(distinct.len());
        for id in &distinct {
            bodies.push((id.clone(), None));
        }
        loop {
            if interrupted() {
                self.scheduler.record_failure(
                    &primary_id_hint,
                    format!(
                        "task_wait interrupted: agents={} duplicated={} unknown={}",
                        agent_ids.len(),
                        duplicated.len(),
                        unknown.len()
                    ),
                );
                return Err(RpcError::new(RpcErrorCode::Unavailable, "wait interrupted"));
            }
            let now = Instant::now();
            let mut any_actionable = false;
            let mut all_settled = true;
            for (id, slot) in bodies.iter_mut() {
                if unknown.contains(id) {
                    continue;
                }
                let terminal = match slot {
                    Some((_, true, cached_epoch)) => {
                        // A terminal snapshot is only final while the row is
                        // still terminal AND still the same execution
                        // generation: a concurrent send can resume a terminal
                        // task (the requeue clears the result row and a new
                        // claim bumps the epoch), and the resumed run may
                        // reach terminal again between two of our poll
                        // rounds. Comparing the epoch catches that loop;
                        // comparing only the phase would not.
                        match self.store.get_task(id).map_err(map_store)? {
                            Some(task)
                                if task.phase == TaskPhase::Terminal
                                    && task.owner_epoch == *cached_epoch =>
                            {
                                true
                            }
                            Some(task) => {
                                // Phase still terminal but the epoch moved
                                // (resume + new claim), or the phase left
                                // terminal: either way the cached snapshot
                                // is from a previous generation — drop it
                                // and re-snapshot. Record the drop so a
                                // "why did the result change" question is
                                // answerable from the diagnostic log.
                                self.scheduler.record_failure(
                                    id,
                                    format!(
                                        "task_wait dropped stale terminal snapshot: phase={:?} cached_epoch={} current_epoch={}",
                                        task.phase, cached_epoch, task.owner_epoch
                                    ),
                                );
                                *slot = None;
                                all_settled = false;
                                continue;
                            }
                            None => {
                                // The task row disappeared (unknown).
                                unknown.insert(id.clone());
                                continue;
                            }
                        }
                    }
                    Some((_, false, _)) => false,
                    None => match self.wait_body(id, false) {
                        Err(error) if matches!(error.code, RpcErrorCode::NotFound) => {
                            unknown.insert(id.clone());
                            continue;
                        }
                        Err(error) => return Err(error),
                        Ok(None) => {
                            all_settled = false;
                            continue;
                        }
                        Ok(Some((body, actionable, terminal, epoch))) => {
                            if actionable {
                                any_actionable = true;
                            }
                            *slot = Some((body, terminal, epoch));
                            terminal
                        }
                    },
                };
                if !terminal {
                    all_settled = false;
                }
            }
            if all_settled || any_actionable || now >= deadline {
                let deadline_expired = !all_settled && !any_actionable && now >= deadline;
                let mut projected: Vec<(String, TaskWaitBody)> = Vec::with_capacity(distinct.len());
                let mut terminal_ids = std::collections::HashSet::new();
                for (id, slot) in &bodies {
                    // Unknown ids carry no body; they are only reported in
                    // the error map below.
                    if unknown.contains(id) {
                        continue;
                    }
                    match slot {
                        Some((body, terminal, _)) => {
                            if *terminal {
                                terminal_ids.insert(id.clone());
                            }
                            projected.push((id.clone(), body.clone()));
                        }
                        None => {
                            // Still running with no actionable request and
                            // no captured snapshot: project a fresh forced
                            // snapshot. timed_out is finalized below.
                            let (body, _, _, _) = self
                                .wait_body(id, true)?
                                .expect("forced snapshot always projects");
                            projected.push((id.clone(), body));
                        }
                    }
                }
                // Per-agent timed_out: claimed only when the deadline
                // actually expired AND this agent is not terminal (a
                // terminal element embeds its result page).
                if deadline_expired {
                    for (id, body) in projected.iter_mut() {
                        if !terminal_ids.contains(id) {
                            body.timed_out = true;
                        }
                    }
                }
                // The concentrated error map: duplicated repetitions and
                // unknown ids, each id once.
                let mut error = std::collections::BTreeMap::new();
                for id in &duplicated {
                    error.insert(id.clone(), "duplicated".to_owned());
                }
                for id in &unknown {
                    error.insert(id.clone(), "unknown_agent".to_owned());
                }
                let mut response = RpcSuccess::TaskWaitMany {
                    waits: TaskWaitManyBody {
                        bodies: projected,
                        error,
                    },
                };
                // Size-check the frame BEFORE committing any delivery
                // cursor: if the envelope cannot fit even after degradation,
                // the error return must leave every cursor untouched so a
                // later wait still sees the undelivered activity text.
                if let Err(error) = bound_wait_many_result(&mut response) {
                    // Preserve the degradation dead-end as a diagnostic
                    // record: the composition (body sizes, question counts,
                    // page totals after full degradation) is exactly the
                    // evidence needed to answer "whose 2 MiB was this".
                    let bodies = &response;
                    let composition = match bodies {
                        RpcSuccess::TaskWaitMany { waits } => {
                            let pages: Vec<String> = waits.bodies.iter().map(|(id, body)| {
                                let result = body.result.as_ref();
                                format!(
                                    "{}:page={}b/{}b",
                                    id,
                                    result.map(|view| view.final_text.len()).unwrap_or(0),
                                    result.map(|view| view.total_bytes).unwrap_or(0)
                                )
                            }).collect();
                            format!(
                                "task_wait_many oversized: agents={} error_ids={} questions={} pages=[{}]",
                                waits.bodies.len(),
                                waits.error.len(),
                                waits.bodies.iter().map(|(_, body)| body.pending_requests.len()).sum::<usize>(),
                                pages.join(",")
                            )
                        }
                        _ => "task_wait_many oversized".to_owned(),
                    };
                    self.scheduler.record_failure(&primary_id_hint, composition);
                    return Err(error);
                }
                // The response is now certain to be delivered: commit each
                // agent's activity-text delivery cursor and apply the
                // terminal-tail dedup in the same pass (mirrors the scalar
                // wait's take-on-delivery semantics; peeked text is
                // overwritten by the taken text, which is at least as
                // fresh). The cursors may have advanced past what the
                // response body carries only in the rolled-out-window
                // degradation case, which the tracker itself flags.
                let RpcSuccess::TaskWaitMany { waits } = &mut response else {
                    unreachable!("response kind was just constructed");
                };
                for (id, body) in waits.bodies.iter_mut() {
                    if let Some(wait_tail) = self.scheduler.take_wait_tail(id) {
                        body.activity.latest_text_tail = wait_tail.text;
                        body.activity.latest_text_truncated = wait_tail.truncated;
                    }
                    if let Some(page) = body.result.as_ref() {
                        dedup_terminal_tail(&mut body.activity.latest_text_tail, &page.final_text);
                    }
                }
                return Ok(response);
            }
            thread::sleep((deadline - now).min(Duration::from_millis(10)));
        }
    }

    /// One snapshot for one agent inside the list-mode loop. Returns
    /// `Ok(None)` while the agent is neither terminal nor actionable, and
    /// `Ok(Some((body, actionable, terminal, owner_epoch)))` when its
    /// current state should be projected (terminal, or actionable pending
    /// present). With `force == true` a plain running agent is projected
    /// too — used at response construction when the deadline expired. The
    /// actionable and terminal flags are reported separately: a terminal
    /// body may also carry actionable records, but only the actionable flag
    /// wakes the loop early and only the terminal flag settles it. The
    /// returned owner_epoch is the execution generation the snapshot was
    /// taken under (every claim increments it), letting the caller detect a
    /// terminal → resume → terminal cycle that a phase check alone misses.
    /// The wait tail is only PEEKED here; the caller commits the delivery
    /// cursor at the response-construction point.
    fn wait_body(
        &self,
        agent_id: &str,
        force: bool,
    ) -> Result<Option<(TaskWaitBody, bool, bool, u64)>, RpcError> {
        let (task, stored_result, reason) = self.require_task_with_result(agent_id)?;
        let all_pending = self
            .store
            .pending_requests(&task.agent_id)
            .map_err(map_store)?;
        let actionable: Vec<_> = all_pending
            .iter()
            .filter(|request| wake_pending_request(request))
            .cloned()
            .collect();
        let pending_requests = actionable
            .iter()
            .take(MAX_PENDING_REQUESTS)
            .cloned()
            .map(pending_request_view)
            .collect::<Vec<_>>();
        let actionable_now = !actionable.is_empty();
        let activity = self.scheduler.passive_activity_snapshot(&task.agent_id);
        let terminal = task.phase == TaskPhase::Terminal;
        if !terminal && !actionable_now && !force {
            return Ok(None);
        }
        let result_page = stored_result
            .filter(|_| terminal)
            .map(|stored| {
                let failure_message = failure_message_projection(&task, &stored);
                self.task_result_view(stored, 0, MAX_RESULT_CHUNK_BYTES, reason, failure_message)
            })
            .transpose()?;
        let mut activity = task_activity_view(task.phase, activity);
        // PEEK only: the delivery cursor stays untouched so an interrupted
        // list-mode wait loses nothing; the response-construction point in
        // task_wait_many performs the take (and terminal-tail dedup).
        if let Some(wait_tail) = self.scheduler.peek_wait_tail(&task.agent_id) {
            activity.latest_text_tail = wait_tail.text;
            activity.latest_text_truncated = wait_tail.truncated;
        }
        Ok(Some((
            TaskWaitBody {
                activity,
                task: task_header_view(&task),
                pending_requests,
                result: result_page,
                // The timed_out flag is finalized by the caller at response
                // construction; snapshots here never claim it.
                timed_out: false,
                message_receipt: None,
            },
            actionable_now,
            terminal,
            task.owner_epoch,
        )))
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

// Measure the complete envelope with the largest valid request ID. Degradation
// is staged: first the structured failure detail is re-shrunk (field-level, so
// it stays parseable with a latest-suffix tail), then embedded questions fall
// back to a bounded prefix so actionable request_ids stay reachable, and only
// as a last resort is the result page itself shortened. The last step derives
// the cursor from the original total/offset, so it never falsely reports a
// shortened page as the end of the text or points at a stale offset. Metadata
// that still exceeds the cap uses the transport's Oversized response.
fn bound_wait_result(response: &mut RpcSuccess) -> Result<(), RpcError> {
    let RpcSuccess::TaskWait(body) = response else {
        return Ok(());
    };
    bound_wait_body(body, &|body| {
        wait_envelope_fits(&RpcSuccess::TaskWait(body.clone()))
    })
}

/// The staged single-body degradation shared by scalar and list waits.
/// `fits` measures the complete envelope this body will be embedded in.
fn bound_wait_body(
    body: &mut TaskWaitBody,
    fits: &dyn Fn(&TaskWaitBody) -> Result<bool, RpcError>,
) -> Result<(), RpcError> {
    if fits(body)? {
        return Ok(());
    }
    let mut candidate = body.clone();
    // 1. Failure detail only: the result page, its cursor, and the questions
    //    stay byte-identical.
    if let Some(view) = candidate.result.as_ref() {
        if let Some(original) = view.failure_message.clone() {
            let fitted = fit_failure_message(&candidate, &original, fits);
            if let Some(view) = candidate.result.as_mut() {
                view.failure_message = Some(fitted);
            }
        }
    }
    if fits(&candidate)? {
        *body = candidate;
        return Ok(());
    }
    // 2. Embedded questions (existing bounded-prefix contract).
    for request in &mut candidate.pending_requests {
        if let Some(question) = request.question.as_mut() {
            let full = std::mem::take(&mut question.text);
            question.text = truncate_at_char_boundary(&full, MAX_QUESTION_SUMMARY_BYTES);
            question.truncated |= question.text.len() < full.len();
        }
    }
    if fits(&candidate)? {
        *body = candidate;
        return Ok(());
    }
    // 3. Last resort: shorten the result page. The cursor comes from the
    //    original total/offset.
    if let Some((end, next_offset)) = shrunk_result_page(&candidate, fits) {
        if let Some(view) = candidate.result.as_mut() {
            view.final_text.truncate(end);
            view.next_offset = next_offset;
        }
    }
    if !fits(&candidate)? {
        return Err(RpcError::new(
            RpcErrorCode::Oversized,
            "response frame exceeds cap",
        ));
    }
    *body = candidate;
    Ok(())
}

/// List-mode envelope degradation. Unlike the scalar path, NO single body
/// must fit the whole frame on its own: N agents each carrying a full-size
/// result page only fit together, so degradation is cooperative —
///
/// 1. every body runs the structured stages (failure-detail shrink, then
///    bounded question prefixes) measured against the FULL multi-wait
///    envelope;
/// 2. repeated stage-3 passes then shorten the body currently carrying the
///    longest result page — round-robin across bodies, since shrinking one
///    alone may never suffice — until the whole frame fits;
/// 3. only when no body can shrink further is the response rejected.
fn bound_wait_many_result(response: &mut RpcSuccess) -> Result<(), RpcError> {
    let RpcSuccess::TaskWaitMany { waits } = response else {
        return Ok(());
    };
    let error = waits.error.clone();
    let fits_all =
        |bodies: &[(String, TaskWaitBody)]| -> Result<bool, RpcError> {
            wait_envelope_fits(&RpcSuccess::TaskWaitMany {
                waits: TaskWaitManyBody {
                    bodies: bodies.to_vec(),
                    error: error.clone(),
                },
            })
        };
    let probe_with = |index: usize, replacement: &TaskWaitBody, bodies: &[(String, TaskWaitBody)]| {
        bodies
            .iter()
            .enumerate()
            .map(|(probe_index, (probe_id, probe_body))| {
                if probe_index == index {
                    (probe_id.clone(), replacement.clone())
                } else {
                    (probe_id.clone(), probe_body.clone())
                }
            })
            .collect::<Vec<_>>()
    };
    if fits_all(&waits.bodies)? {
        return Ok(());
    }
    // Stage 1+2 per body, best-effort: apply the structured shrinks when
    // they help the shared envelope, but never abort — the next body's
    // shrink may be the one that makes the frame fit.
    for index in 0..waits.bodies.len() {
        let mut candidate = waits.bodies[index].1.clone();
        let fits = |candidate: &TaskWaitBody| -> Result<bool, RpcError> {
            fits_all(&probe_with(index, candidate, &waits.bodies))
        };
        if fits(&candidate)? {
            continue;
        }
        // 1. Failure detail only.
        if let Some(view) = candidate.result.as_ref() {
            if let Some(original) = view.failure_message.clone() {
                let fitted = fit_failure_message(&candidate, &original, &fits);
                if let Some(view) = candidate.result.as_mut() {
                    view.failure_message = Some(fitted);
                }
            }
        }
        if fits(&candidate)? {
            waits.bodies[index].1 = candidate;
            continue;
        }
        // 2. Embedded questions (bounded-prefix contract).
        for request in &mut candidate.pending_requests {
            if let Some(question) = request.question.as_mut() {
                let full = std::mem::take(&mut question.text);
                question.text = truncate_at_char_boundary(&full, MAX_QUESTION_SUMMARY_BYTES);
                question.truncated |= question.text.len() < full.len();
            }
        }
        // Best-effort: keep the shrunk candidate regardless; the verdict
        // comes after every body had its chance.
        waits.bodies[index].1 = candidate;
    }
    if fits_all(&waits.bodies)? {
        return Ok(());
    }
    // Cooperative stage 3. The per-body binary search measures the FULL
    // envelope, so when several bodies are oversized together no single
    // shrink can make the frame fit and the search would find nothing.
    // Instead, first give every body an equal byte budget derived from the
    // frame cap (accounting for worst-case JSON escaping) and truncate to
    // it directly — a coarse pass that is guaranteed to fit for any
    // realistic body count — then fine-tune with the envelope-measured
    // search, then round-robin any residual.
    let body_count = waits.bodies.len().max(1);
    // Six bytes of worst-case JSON escape per input byte, plus envelope
    // overhead; the budget is intentionally conservative.
    let budget = (MAX_RESPONSE_FRAME_BYTES / 6) / body_count;
    for (_, body) in waits.bodies.iter_mut() {
        if let Some(view) = body.result.as_mut() {
            if view.final_text.len() > budget {
                let truncated =
                    truncate_at_char_boundary(&view.final_text, budget).len();
                let next = view.offset + truncated;
                view.final_text.truncate(truncated);
                view.next_offset = (next < view.total_bytes).then_some(next);
            }
        }
    }
    if fits_all(&waits.bodies)? {
        return Ok(());
    }
    // Fine-tune. The per-body binary search (`shrunk_result_page`) requires
    // that a SINGLE page's shrink can make the whole frame fit; with many
    // large bodies (e.g. a fleet of 2 KiB-question carriers plus several
    // 256 KiB result pages) no single page is ever enough, so that search
    // returns None and the old loop bailed out with Oversized even though a
    // joint shrink fits. Instead: repeatedly halve the longest remaining
    // page — a guaranteed monotone shrink that commits PARTIAL reductions —
    // until the frame fits or every page is down to one character.
    // Cursors stay consistent (derived from the original total/offset), so
    // no shortened page is ever presented as the end.
    while !fits_all(&waits.bodies)? {
        let mut target_index = None;
        let mut target_len = 0usize;
        for (index, (_, body)) in waits.bodies.iter().enumerate() {
            if let Some(view) = &body.result {
                if view.final_text.len() > target_len {
                    target_len = view.final_text.len();
                    target_index = Some(index);
                }
            }
        }
        let Some(index) = target_index else {
            break;
        };
        let (_, body) = &mut waits.bodies[index];
        let Some(view) = body.result.as_mut() else {
            break;
        };
        let current = view.final_text.len();
        if current <= 1 {
            // Every page is already at the floor and the frame still does
            // not fit: genuinely unrepresentable.
            break;
        }
        // Halve (at a char boundary), never below one character.
        let target = (current / 2).max(1);
        let mut end = target.min(current - 1);
        while !view.final_text.is_char_boundary(end) {
            end -= 1;
        }
        let next = view.offset + end;
        view.final_text.truncate(end);
        view.next_offset = (next < view.total_bytes).then_some(next);
    }
    if !fits_all(&waits.bodies)? {
        return Err(RpcError::new(
            RpcErrorCode::Oversized,
            "response frame exceeds cap",
        ));
    }
    Ok(())
}

/// Measure the whole wait envelope with the largest valid request ID plus the
/// newline the transport appends.
fn wait_envelope_fits(response: &RpcSuccess) -> Result<bool, RpcError> {
    let envelope = RpcResponse::success("\u{1}".repeat(MAX_REQUEST_ID_BYTES), response.clone());
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|_| RpcError::new(RpcErrorCode::Oversized, "response encoding failed"))?;
    Ok(bytes.len().saturating_add(1) <= MAX_RESPONSE_FRAME_BYTES)
}

/// Binary-search the largest structured shrink of `original` whose envelope
/// still fits. Envelope size is monotone in the shrunk record length, so the
/// feasible budgets form a prefix. When even the floor record cannot fit, the
/// caller falls through to the other degradation stages with the floor record.
fn fit_failure_message(
    base: &TaskWaitBody,
    original: &str,
    fits: &dyn Fn(&TaskWaitBody) -> Result<bool, RpcError>,
) -> String {
    let mut lo = 0usize;
    let mut hi = original.len();
    let mut best: Option<String> = None;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        match shrink_persistable_record(original, mid) {
            Some(candidate) if message_body_fits(base, &candidate, fits) => {
                best = Some(candidate);
                if mid == hi {
                    break;
                }
                lo = mid + 1;
            }
            _ => {
                if mid == 0 {
                    break;
                }
                hi = mid - 1;
            }
        }
    }
    best.or_else(|| shrink_persistable_record(original, 0))
        .unwrap_or_else(|| original.to_owned())
}

fn message_body_fits(
    base: &TaskWaitBody,
    message: &str,
    fits: &dyn Fn(&TaskWaitBody) -> Result<bool, RpcError>,
) -> bool {
    let mut probe = base.clone();
    if let Some(view) = probe.result.as_mut() {
        view.failure_message = Some(message.to_owned());
    }
    fits(&probe).unwrap_or(false)
}

/// The largest prefix of the embedded result page that keeps the envelope in
/// bounds, with `next_offset` derived from the ORIGINAL `total_bytes`/`offset`
/// rather than by re-paging the already-paged text. A `None` next offset
/// therefore still means "this page reaches the end of the text". Returns
/// `None` when there is no page to shorten or even one character does not
/// fit.
fn shrunk_result_page(
    base: &TaskWaitBody,
    fits: &dyn Fn(&TaskWaitBody) -> Result<bool, RpcError>,
) -> Option<(usize, Option<usize>)> {
    let view = base.result.as_ref()?;
    let page = &view.final_text;
    if page.is_empty() {
        return None;
    }
    let offset = view.offset;
    let total = view.total_bytes;
    // Candidate byte ends inside the page, always at least one character
    // (`char_indices` yields 0 first, which is skipped).
    let ends: Vec<usize> = page
        .char_indices()
        .map(|(index, _)| index)
        .skip(1)
        .collect();
    if ends.is_empty() {
        return None;
    }
    let fits_end = |end: usize| {
        let mut probe = base.clone();
        if let Some(view) = probe.result.as_mut() {
            let next = offset + end;
            view.final_text.truncate(end);
            view.next_offset = (next < total).then_some(next);
        }
        fits(&probe).unwrap_or(false)
    };
    if !fits_end(ends[0]) {
        return None;
    }
    let mut lo = 0usize;
    let mut hi = ends.len() - 1;
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        if fits_end(ends[mid]) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let end = ends[lo];
    let next = offset + end;
    let next_offset = (next < total).then_some(next);
    Some((end, next_offset))
}

#[cfg(test)]
pub(crate) mod wait_tests {
    use super::*;
    use crate::rpc::{
        PendingRequestView, QuestionView, TaskActivityView, TaskHeaderView, TelemetryStatusView,
    };
    use crate::{
        CommandRuntimeFactory, LifecycleSink, ManagedRuntime, RuntimeFactory, RuntimeTerminal,
        SchedulerConfig, TurnSnapshot,
    };
    use external_runtime::ProcessIdentity;
    use external_store::TaskResult;
    use std::process::Command;

    #[test]
    fn draining_existing_task_message_is_idempotent_but_new_rejected() {
        let (_dir, service, id) = fixture();
        let msg = MessageInput {
            agent_id: id.clone(),
            message_id: Some("drain-msg".into()),
            content: "x".into(),
            mode: "queue".into(),
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
            mode: "queue".into(),
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
                permission_mode: Some(external_core::PermissionMode::Plan),
                prompt: "admitted after the aborted drain".into(),
                write_manifest: vec![],
            })
            .unwrap();
        // The drained task keeps its recorded facts.
        let RpcSuccess::TaskWait(TaskWaitBody { task, .. }) = service
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
                mode: "queue".into(),
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
                mode: "queue".into(),
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
            permission_mode: Some(external_core::PermissionMode::Plan),
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

    /// A probe backend that reports a fixed local version on every probe. It
    /// deliberately starts with an empty evidence store: the result path's
    /// version advisory must invoke the bounded probe itself instead of
    /// reading cached input. Shared with the status-derivation tests in
    /// `rpc::agents`.
    pub(crate) struct FixedLocalVersion {
        pub(crate) version: Option<String>,
        pub(crate) probes: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::agent_status::AgentProbeBackend for FixedLocalVersion {
        fn probe(
            &self,
            input: &crate::agent_status::AgentProbeInput,
        ) -> crate::agent_status::AgentProbeEvidence {
            self.probes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let scope = input.scope.clone();
            let status = |reason: &str| crate::agent_status::ScopeEvidence {
                runtime_path: None,
                state: if self.version.is_some() {
                    crate::agent_status::EvidenceState::Ready
                } else {
                    crate::agent_status::EvidenceState::Unavailable
                },
                scope: scope.clone(),
                version: self.version.clone(),
                checked_at_ms: 0,
                reason: self.version.is_none().then(|| reason.to_owned()),
            };
            crate::agent_status::AgentProbeEvidence {
                agent: input.agent.clone(),
                config_revision: 0,
                local: status("missing"),
                auth: crate::agent_status::ScopeEvidence {
                    runtime_path: None,
                    state: crate::agent_status::EvidenceState::Unknown,
                    scope: scope.clone(),
                    version: None,
                    checked_at_ms: 0,
                    reason: Some("auth_not_probed".into()),
                },
                hi: crate::agent_status::ScopeEvidence {
                    runtime_path: None,
                    state: crate::agent_status::EvidenceState::Unknown,
                    scope,
                    version: None,
                    checked_at_ms: 0,
                    reason: Some("hi_not_probed".into()),
                },
            }
        }
    }

    /// A fixture whose task carries a dsh admission identity and whose evidence
    /// store probes a fixed observed version on demand.
    fn version_fixture(
        observed: Option<&str>,
    ) -> (
        tempfile::TempDir,
        Arc<RpcService>,
        String,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("version-wait-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| -> std::io::Result<Command> {
            panic!("version fixture must never start a runtime")
        });
        let scheduler = Scheduler::new(
            "version-wait-test",
            store.clone(),
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let manifest = GeneralTaskManifest {
            schema: external_core::GENERAL_TASK_SCHEMA.into(),
            agent_id: String::new(),
            repository: directory.path().canonicalize().unwrap(),
            permission_mode: Some(external_core::PermissionMode::Build),
            prompt: "version wait fixture".into(),
            write_manifest: vec![],
        };
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest,
                Some(external_core::AdmissionIdentity {
                    agent: "dsh".into(),
                    config_revision: 0,
                    adapter_version: "fixture".into(),
                    model: None,
                    model_source: "native".into(),
                    effort: None,
                    developer_instructions: None,
                }),
            )
            .unwrap();
        let id = submitted.agent_id;
        let claim = store.claim_next("version-wait-test", 10, 10).unwrap().unwrap();
        store
            .mark_session_running(&id, claim.owner_epoch, "runtime", None, Some("session"), None)
            .unwrap();
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let evidence = crate::agent_status::AgentEvidenceStore::with_backend(Arc::new(
            FixedLocalVersion {
                version: observed.map(str::to_owned),
                probes: Arc::clone(&probes),
            },
        ));
        (
            directory,
            Arc::new(
                RpcService::new_with_service_generation_and_evidence(
                    scheduler,
                    store,
                    "version-wait-fixture".into(),
                    evidence,
                )
                .unwrap(),
            ),
            id,
            probes,
        )
    }

    #[test]
    fn failed_terminal_wait_returns_the_result_without_a_version_probe() {
        use std::sync::atomic::Ordering;
        let (_directory, service, id, probes) = version_fixture(Some("0.1.5-rc.1"));
        service
            .store
            .store_task_result(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Failed,
                    final_text: "boom".into(),
                    partial: false,
                },
            )
            .unwrap();
        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(result),
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert_eq!(result.outcome, TaskOutcome::Failed);
        // Wait carries no advisory, so it never runs the version probe.
        assert_eq!(probes.load(Ordering::SeqCst), 0, "wait must not probe");
        assert!(
            service
                .agent_evidence
                .latest("dsh")
                .and_then(|evidence| evidence.local.version)
                .is_none()
        );

        // The independent result path keeps the advisory semantics.
        let RpcSuccess::TaskResult {
            instruction,
            result: Some(_),
            ..
        } = service
            .dispatch(RpcMethod::TaskResult {
                agent_id: id,
                offset: 0,
                limit: MAX_RESULT_CHUNK_BYTES,
            })
            .unwrap()
        else {
            panic!("expected terminal result response")
        };
        assert_eq!(
            instruction.as_deref(),
            Some(crate::agent_status::VERSION_MISMATCH_INSTRUCTION)
        );
        assert_eq!(probes.load(Ordering::SeqCst), 1, "one probe per response");
        assert_eq!(
            service
                .agent_evidence
                .latest("dsh")
                .and_then(|evidence| evidence.local.version)
                .as_deref(),
            Some("0.1.5-rc.1")
        );
    }

    #[test]
    fn successful_or_matched_results_never_carry_the_version_mismatch_instruction() {
        use std::sync::atomic::Ordering;
        for (outcome, observed, expected_probes) in [
            (TaskOutcome::Completed, Some("0.1.5-rc.1"), 0),
            (TaskOutcome::Failed, Some(external_agent_dsh::profile::PINNED_DSH_VERSION), 1),
            (TaskOutcome::Failed, None, 1),
        ] {
            let (_directory, service, id, probes) = version_fixture(observed);
            service
                .store
                .store_task_result(
                    &id,
                    &TaskResult {
                        outcome,
                        final_text: "done".into(),
                        partial: false,
                    },
                )
                .unwrap();
            // Wait never probes for a version advisory.
            let RpcSuccess::TaskWait(TaskWaitBody { .. }) = service
                .dispatch(RpcMethod::TaskWait(query(&id, 0)))
                .unwrap()
            else {
                panic!("expected terminal wait response")
            };
            assert_eq!(probes.load(Ordering::SeqCst), 0);
            let RpcSuccess::TaskResult { instruction, .. } = service
                .dispatch(RpcMethod::TaskResult {
                    agent_id: id,
                    offset: 0,
                    limit: MAX_RESULT_CHUNK_BYTES,
                })
                .unwrap()
            else {
                panic!("expected terminal result response")
            };
            assert_eq!(instruction, None);
            assert_eq!(probes.load(Ordering::SeqCst), expected_probes);
        }
    }

    pub(crate) fn fixture() -> (tempfile::TempDir, Arc<RpcService>, String) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("wait-")
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
                permission_mode: Some(external_core::PermissionMode::Plan),
                prompt: "wait fixture".into(),
                write_manifest: vec![],
            })
            .unwrap();
        let id = submitted.agent_id;
        let claim = store.claim_next("wait-test", 10, 10).unwrap().unwrap();
        store
            .mark_session_running(&id, claim.owner_epoch, "runtime", None, Some("session"), None)
            .unwrap();
        (
            directory,
            // SystemStatus refreshes every agent's local probe evidence, so
            // this fixture must inject the hermetic backend: the real one
            // would discover dsh/codex/agy on PATH and execute their version
            // commands inside timing-sensitive wait tests.
            Arc::new(
                RpcService::new_with_service_generation_and_evidence(
                    scheduler,
                    store,
                    "wait-fixture".into(),
                    crate::agent_status::AgentEvidenceStore::unprobed(),
                )
                .unwrap(),
            ),
            id,
        )
    }

    pub(crate) struct FakeRunnableRuntime;
    impl ManagedRuntime for FakeRunnableRuntime {
        fn identity(&self) -> Option<ProcessIdentity> {
            None
        }
        fn stop(&self, _grace: Duration) -> RuntimeTerminal {
            RuntimeTerminal::Stopped(external_runtime::StopOutcome::AlreadyExited(
                external_runtime::ChildExit::Unknown,
            ))
        }
        fn wait_terminal(&self, _timeout: Duration) -> Option<RuntimeTerminal> {
            None
        }
        fn bootstrap_session(
            &self,
            job: &TaskRecord,
            _timeout: Duration,
        ) -> Result<crate::SessionReady, crate::RuntimeCommandError> {
            let configured_model = serde_json::from_str::<serde_json::Value>(&job.prepared_launch_json)
                .ok()
                .and_then(|v| v.get("admission")?.get("model")?.as_str().map(str::to_owned));
            Ok(crate::SessionReady {
                session_id: "fake-session".into(),
                configured_model,
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

    pub(crate) struct FakeRunnableFactory;
    impl RuntimeFactory for FakeRunnableFactory {
        fn spawn(
            &self,
            _task: &TaskRecord,
            _sink: Arc<dyn LifecycleSink>,
        ) -> Result<Arc<dyn ManagedRuntime>, std::io::Error> {
            Ok(Arc::new(FakeRunnableRuntime))
        }
    }

    pub(crate) fn runnable_fixture() -> (tempfile::TempDir, Arc<RpcService>, String) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("runnable-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let scheduler = Scheduler::new(
            "runnable-test",
            store.clone(),
            Arc::new(FakeRunnableFactory),
            SchedulerConfig::default(),
        )
        .unwrap();
        (
            directory,
            Arc::new(RpcService::new(scheduler, store).unwrap()),
            String::new(),
        )
    }

    pub(crate) fn query(id: &str, wait_time: u64) -> TaskWaitQuery {
        TaskWaitQuery {
            agent_id: WaitTarget::One(id.into()),
            wait_time,
            message_id: None,
        }
    }

    #[test]
    fn wait_task_serializes_only_the_lifecycle_header() {
        let (_directory, service, id) = fixture();
        let RpcSuccess::TaskWait(TaskWaitBody { task, .. }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("wait")
        };
        let encoded = serde_json::to_value(&task).unwrap();
        let mut keys = encoded
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(keys, ["agent_id", "status"]);
        assert_eq!(encoded["agent_id"], id);
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
        let RpcSuccess::TaskWait(TaskWaitBody { timed_out: true, .. }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected unfinished wait response")
        };
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            timed_out: false,
            result: Some(result),
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert_eq!(result.next_offset, None);
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            timed_out: true,
            activity,
            message_receipt: Some(_),
            ..
        }) = response
        else {
            panic!("ordinary activity ended wait")
        };
        assert_eq!(activity.latest_text_tail, "ordinary text");
        assert_eq!(activity.tool_calls_last_60s, 1);
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            activity,
            result: Some(result),
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert_eq!(result.next_offset, None);
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            pending_requests,
            timed_out,
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        assert!(!timed_out);
        assert_eq!(pending_requests.len(), 1);
        assert_eq!(pending_requests[0].tool_name.as_deref(), Some("Read"));
        assert_eq!(pending_requests[0].summary.as_deref(), Some("permission request"));
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            pending_requests,
            timed_out,
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        assert!(!timed_out);
        assert_eq!(pending_requests.len(), 1);
        assert_eq!(pending_requests[0].kind, "user_input");
        // The question is the single text surface: no summary duplicate.
        assert_eq!(pending_requests[0].summary, None);
        let question = pending_requests[0].question.as_ref().expect("question");
        assert_eq!(question.text, "which branch?");
        assert!(!question.truncated);
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            pending_requests,
            timed_out,
            ..
        }) = service
            .task_wait(query(&id, 0), start + Duration::from_millis(30), &|| false)
            .unwrap()
        else {
            panic!("expected wait response")
        };
        assert!(timed_out);
        assert!(pending_requests.is_empty());
    }

    #[test]
    fn user_input_questions_below_the_embed_cap_ship_in_full_without_a_summary() {
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            pending_requests, ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        // The embedded question is the only text surface: the summary never
        // duplicates its head, and anything below the embed cap ships whole.
        assert_eq!(pending_requests[0].summary, None);
        let page = pending_requests[0].question.as_ref().expect("question");
        assert_eq!(page.text.len(), 5000);
        assert!(!page.truncated);
        // A multibyte question below the cap ships whole as well.
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            pending_requests, ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        let wide_view = pending_requests
            .iter()
            .find(|request| request.request_id == "wide")
            .expect("wide request in projection");
        assert_eq!(wide_view.summary, None);
        let wide_page = wide_view.question.as_ref().expect("question");
        assert_eq!(wide_page.text, "語".repeat(1024));
        assert!(!wide_page.truncated);
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            pending_requests, ..
        }) = service
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            pending_requests, ..
        }) = &response
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            pending_requests,
            timed_out,
            ..
        }) = response
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            pending_requests,
            timed_out,
            ..
        }) = service
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
            RpcSuccess::TaskWait(TaskWaitBody {
                timed_out: false,
                ..
            })
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
            RpcSuccess::TaskWait(TaskWaitBody {
                timed_out: true,
                ..
            })
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
            let RpcSuccess::TaskWait(TaskWaitBody {
                result: Some(result),
                timed_out: false,
                ..
            }) = &response
            else {
                panic!("terminal result missing")
            };
            assert_eq!(result.outcome, outcome);
            assert_eq!(result.total_bytes, text.len());
            // Wait inlines the same bounded first page as external_subagent_result.
            assert_eq!(result.offset, 0);
            assert_eq!(
                result.next_offset,
                (!fits_page).then_some(MAX_RESULT_CHUNK_BYTES)
            );
            assert_eq!(result.final_text, text[..result.final_text.len()]);
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
                mode: "queue".into(),
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
                mode: "queue".into(),
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
                mode: "queue".into(),
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
                mode: "queue".into(),
            })),
            Ok(_)
        ));
        assert!(service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: id,
                message_id: Some("explicit-conflict".into()),
                content: "different".into(),
                mode: "queue".into(),
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
        // The fixture task is a resume-eligible terminal zcode task (agent +
        // persisted session id), which terminal send now requeues by design.
        // The rejection-without-deletion invariant belongs to the ineligible
        // rows: request a close so the durable requeue must refuse.
        service.store.request_close(&id).unwrap();
        let before = service.store.task_result(&id).unwrap().unwrap();
        let response = service.dispatch(RpcMethod::TaskMessage(MessageInput {
            agent_id: id.clone(),
            message_id: Some("terminal-message".into()),
            content: "must not resume".into(),
            mode: "queue".into(),
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

    #[test]
    fn task_result_and_wait_surface_the_terminal_reason_code() {
        let (_directory, service, id) = fixture();
        service
            .store
            .store_task_result_with_reason(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Failed,
                    final_text: "model selection was rejected: unknown model: foo".into(),
                    partial: true,
                },
                Some("MODEL_REJECTED"),
                None,
            )
            .unwrap();

        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(result),
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert_eq!(result.reason_code.as_deref(), Some("MODEL_REJECTED"));
        assert_eq!(
            result.final_text,
            "model selection was rejected: unknown model: foo"
        );

        let RpcSuccess::TaskResult {
            result: Some(result),
            ..
        } = service
            .dispatch(RpcMethod::TaskResult {
                agent_id: id,
                offset: 0,
                limit: MAX_RESULT_CHUNK_BYTES,
            })
            .unwrap()
        else {
            panic!("expected terminal result response")
        };
        assert_eq!(result.reason_code.as_deref(), Some("MODEL_REJECTED"));
    }

    #[test]
    fn completed_task_result_reports_a_null_reason_code() {
        let (_directory, service, id) = fixture();
        service
            .store
            .store_task_result(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "all done".into(),
                    partial: false,
                },
            )
            .unwrap();
        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(result),
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert_eq!(result.reason_code, None);
    }

    fn perspective_task() -> TaskHeaderView {
        TaskHeaderView {
            agent_id: "10000001".into(),
            status: "failed".into(),
        }
    }

    fn empty_activity() -> TaskActivityView {
        TaskActivityView {
            latest_text_tail: String::new(),
            latest_text_truncated: false,
            tool_calls_last_60s: 0,
            in_flight_tool_count: 0,
            oldest_in_flight_tool_age_ms: None,
            last_activity_age_ms: None,
            model_request_active: false,
            telemetry_status: TelemetryStatusView::Healthy,
        }
    }

    fn user_question(request_id: &str, text: String) -> PendingRequestView {
        PendingRequestView {
            request_id: request_id.into(),
            kind: "user_input".into(),
            tool_name: None,
            operation: "user_input".into(),
            summary: None,
            question: Some(QuestionView {
                truncated: false,
                text,
            }),
        }
    }

    fn synthesized_wait(
        final_text: String,
        total_bytes: usize,
        next_offset: Option<usize>,
        failure_message: Option<String>,
        pending_requests: Vec<PendingRequestView>,
    ) -> RpcSuccess {
        RpcSuccess::TaskWait(TaskWaitBody {
            task: perspective_task(),
            pending_requests,
            activity: empty_activity(),
            result: Some(TaskResultView {
                outcome: TaskOutcome::Failed,
                final_text,
                partial: true,
                offset: 0,
                total_bytes,
                next_offset,
                reason_code: Some("RUNTIME_TERMINAL".into()),
                failure_message,
            }),
            timed_out: false,
            message_receipt: None,
        })
    }

    fn envelope_bytes(response: &RpcSuccess) -> usize {
        let envelope = RpcResponse::success("\u{1}".repeat(MAX_REQUEST_ID_BYTES), response.clone());
        serde_json::to_vec(&envelope).unwrap().len() + 1
    }

    fn realistic_failure_detail() -> String {
        crate::scheduler::persistable_failure_record(
            "10000007",
            Some("session-401"),
            "runtime_terminal",
            "RUNTIME_TERMINAL",
            r#"{"message":"request failed with status 401"}"#,
            &format!("{}401", "x".repeat(9 * 1024)),
        )
    }

    fn failed_result_for(service: &Arc<RpcService>, id: &str, reason: &str, detail: &str) {
        service
            .store
            .store_task_result_with_reason(
                id,
                &TaskResult {
                    outcome: TaskOutcome::Failed,
                    final_text: "RUNTIME_TERMINAL".into(),
                    partial: true,
                },
                Some(reason),
                Some(detail),
            )
            .unwrap();
    }

    #[test]
    fn failed_terminal_result_surfaces_the_persisted_failure_message_on_both_exits() {
        let (_directory, service, id) = fixture();
        let detail = realistic_failure_detail();
        failed_result_for(&service, &id, "RUNTIME_TERMINAL", &detail);
        // The RPC projection reads the stored library value verbatim.
        assert_eq!(
            service
                .store
                .get_task(&id)
                .unwrap()
                .unwrap()
                .failure_message
                .as_deref(),
            Some(detail.as_str())
        );

        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(result),
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert_eq!(result.failure_message.as_deref(), Some(detail.as_str()));
        assert_eq!(result.reason_code.as_deref(), Some("RUNTIME_TERMINAL"));
        assert_eq!(result.outcome, TaskOutcome::Failed);
        assert_eq!(result.final_text, "RUNTIME_TERMINAL");
        // The detail is parseable single-line JSON carrying the 401.
        let encoded: serde_json::Value =
            serde_json::from_str(result.failure_message.as_deref().unwrap()).unwrap();
        assert!(encoded["stderr_tail"].as_str().unwrap().contains("401"));

        let RpcSuccess::TaskResult {
            result: Some(result),
            ..
        } = service
            .dispatch(RpcMethod::TaskResult {
                agent_id: id,
                offset: 0,
                limit: MAX_RESULT_CHUNK_BYTES,
            })
            .unwrap()
        else {
            panic!("expected terminal result response")
        };
        // Both exits carry the same un-degraded detail.
        assert_eq!(result.failure_message.as_deref(), Some(detail.as_str()));
    }

    #[test]
    fn non_failure_outcomes_never_expose_a_failure_message() {
        let detail = r#"{"stage":"runtime_terminal","error_code":"RUNTIME_TERMINAL"}"#;
        for outcome in [
            TaskOutcome::Completed,
            TaskOutcome::Cancelled,
            TaskOutcome::TimedOut,
        ] {
            let (_directory, service, id) = fixture();
            service
                .store
                .store_task_result_with_reason(
                    &id,
                    &TaskResult {
                        outcome,
                        final_text: "terminal".into(),
                        partial: outcome != TaskOutcome::Completed,
                    },
                    None,
                    Some(detail),
                )
                .unwrap();
            // The store gate cleared the explicit detail.
            assert_eq!(
                service
                    .store
                    .get_task(&id)
                    .unwrap()
                    .unwrap()
                    .failure_message,
                None,
                "{outcome:?}"
            );
            let RpcSuccess::TaskWait(TaskWaitBody {
                result: Some(result),
                ..
            }) = service
                .dispatch(RpcMethod::TaskWait(query(&id, 0)))
                .unwrap()
            else {
                panic!("expected terminal wait response")
            };
            assert_eq!(result.outcome, outcome, "{outcome:?}");
            assert_eq!(result.failure_message, None, "{outcome:?}");
            let encoded = serde_json::to_value(&result).unwrap();
            assert!(encoded.get("failure_message").is_some(), "{outcome:?}");
            assert!(encoded["failure_message"].is_null(), "{outcome:?}");
        }
    }

    #[test]
    fn historical_failed_result_without_detail_serializes_a_null_failure_message() {
        let (_directory, service, id) = fixture();
        service
            .store
            .store_task_result(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Failed,
                    final_text: "RUNTIME_TERMINAL".into(),
                    partial: true,
                },
            )
            .unwrap();
        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(result),
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert_eq!(result.failure_message, None);
        assert!(serde_json::to_value(&result).unwrap()["failure_message"].is_null());
    }

    #[test]
    fn cross_round_same_outcome_failures_pair_the_current_detail_and_reason() {
        let (_directory, service, id) = fixture();
        let first = r#"{"stage":"runtime_terminal","error_code":"RUNTIME_TERMINAL","message":"round one"}"#;
        let second = r#"{"stage":"runtime_terminal","error_code":"RUNTIME_TERMINAL","message":"round two"}"#;
        // Round one: same final_text/digest as round two, different detail.
        failed_result_for(&service, &id, "RUNTIME_TERMINAL", first);
        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(result),
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected round-one terminal wait")
        };
        assert_eq!(result.failure_message.as_deref(), Some(first));
        assert_eq!(result.reason_code.as_deref(), Some("RUNTIME_TERMINAL"));

        // Resume clears the detail and deletes the immutable result.
        assert!(service
            .store
            .requeue_task_for_resume_with_message(&id, "resume-detail", "queue", "continue")
            .unwrap());
        let claim = service.store.claim_next("wait-test", 10, 10).unwrap().unwrap();
        service
            .store
            .mark_session_running(&id, claim.owner_epoch, "runtime", None, Some("session"), None)
            .unwrap();
        // Round two writes the same final_text (same digest) with a new detail
        // and a different reason.
        failed_result_for(&service, &id, "MODEL_REJECTED", second);

        for read in [
            service.dispatch(RpcMethod::TaskWait(query(&id, 0))).unwrap(),
            service
                .dispatch(RpcMethod::TaskResult {
                    agent_id: id.clone(),
                    offset: 0,
                    limit: MAX_RESULT_CHUNK_BYTES,
                })
                .unwrap(),
        ] {
            let result = match read {
                RpcSuccess::TaskWait(TaskWaitBody {
                    result: Some(result),
                    ..
                })
                | RpcSuccess::TaskResult {
                    result: Some(result),
                    ..
                } => result,
                other => panic!("expected terminal result: {other:?}"),
            };
            assert_eq!(result.final_text, "RUNTIME_TERMINAL");
            assert_eq!(
                result.failure_message.as_deref(),
                Some(second),
                "the old round's detail must not be mixed with the current result"
            );
            assert_eq!(
                result.reason_code.as_deref(),
                Some("MODEL_REJECTED"),
                "the old round's reason must not be mixed with the current result"
            );
        }
    }

    #[test]
    fn cross_round_reads_never_mix_detail_or_reason_during_a_concurrent_second_failure() {
        let (_directory, service, id) = fixture();
        let first = r#"{"stage":"runtime_terminal","error_code":"RUNTIME_TERMINAL","message":"round one"}"#;
        let second = r#"{"stage":"runtime_terminal","error_code":"RUNTIME_TERMINAL","message":"round two"}"#;
        failed_result_for(&service, &id, "RUNTIME_TERMINAL", first);

        // A real interleave: the writer resumes and commits a second
        // same-outcome, same-digest failure while the reader keeps calling both
        // exits. Detail and reason must always move together with the result
        // they belong to (the single-lock snapshot includes the reason).
        let writer_service = Arc::clone(&service);
        let writer_id = id.clone();
        let writer = std::thread::spawn(move || {
            assert!(writer_service
                .store
                .requeue_task_for_resume_with_message(&writer_id, "resume-concurrent", "queue", "continue")
                .unwrap());
            let claim = writer_service
                .store
                .claim_next("wait-test", 10, 10)
                .unwrap()
                .unwrap();
            writer_service
                .store
                .mark_session_running(
                    &writer_id,
                    claim.owner_epoch,
                    "runtime",
                    None,
                    Some("session"),
                    None,
                )
                .unwrap();
            writer_service
                .store
                .store_task_result_with_reason(
                    &writer_id,
                    &TaskResult {
                        outcome: TaskOutcome::Failed,
                        final_text: "RUNTIME_TERMINAL".into(),
                        partial: true,
                    },
                    Some("MODEL_REJECTED"),
                    Some(second),
                )
                .unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_round_two = false;
        while Instant::now() < deadline && !saw_round_two {
            for read in [
                service.dispatch(RpcMethod::TaskWait(query(&id, 0))).unwrap(),
                service
                    .dispatch(RpcMethod::TaskResult {
                        agent_id: id.clone(),
                        offset: 0,
                        limit: MAX_RESULT_CHUNK_BYTES,
                    })
                    .unwrap(),
            ] {
                let Some(result) = (match read {
                    RpcSuccess::TaskWait(TaskWaitBody { result, .. })
                    | RpcSuccess::TaskResult { result, .. } => result,
                    _ => None,
                }) else {
                    continue;
                };
                assert_eq!(result.final_text, "RUNTIME_TERMINAL");
                match result.failure_message.as_deref() {
                    Some(detail) if detail == first => assert_eq!(
                        result.reason_code.as_deref(),
                        Some("RUNTIME_TERMINAL"),
                        "round-one detail mixed with another round's reason"
                    ),
                    Some(detail) if detail == second => {
                        assert_eq!(
                            result.reason_code.as_deref(),
                            Some("MODEL_REJECTED"),
                            "round-two detail mixed with another round's reason"
                        );
                        saw_round_two = true;
                    }
                    other => panic!("unexpected failure detail {other:?}"),
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        writer.join().unwrap();
        assert!(
            saw_round_two,
            "the concurrent second round was never observed"
        );
    }

    #[test]
    fn wait_projection_shrinks_only_the_failure_detail_when_that_is_enough() {
        let detail = realistic_failure_detail();
        let build = |message: Option<String>, question: String| {
            synthesized_wait(
                "RUNTIME_TERMINAL".into(),
                "RUNTIME_TERMINAL".len() + 100,
                Some("RUNTIME_TERMINAL".len()),
                message,
                vec![user_question("q", question)],
            )
        };
        let with_detail = build(Some(detail.clone()), String::new());
        let without_detail = build(None, String::new());
        let contribution = envelope_bytes(&with_detail) - envelope_bytes(&without_detail);
        let deficit = MAX_RESPONSE_FRAME_BYTES - envelope_bytes(&with_detail);
        assert!(contribution > 12 && deficit > 0);
        // An escape-dense question grows the envelope by exactly six bytes per
        // NUL. Size it to push the envelope over the cap while leaving it under
        // the cap when the detail is absent, so only the detail stage runs.
        let question_len = (deficit + contribution / 2).div_ceil(6) + 1;
        let question = "\u{0}".repeat(question_len);
        let mut base = build(Some(detail.clone()), question.clone());
        assert!(envelope_bytes(&base) > MAX_RESPONSE_FRAME_BYTES);
        assert!(envelope_bytes(&build(None, question.clone())) <= MAX_RESPONSE_FRAME_BYTES);
        let original_final = match &base {
            RpcSuccess::TaskWait(TaskWaitBody {
                result: Some(view),
                ..
            }) => view.final_text.clone(),
            _ => unreachable!(),
        };
        bound_wait_result(&mut base).unwrap();
        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(view),
            pending_requests,
            ..
        }) = &base
        else {
            panic!("expected wait response")
        };
        assert!(envelope_bytes(&base) <= MAX_RESPONSE_FRAME_BYTES);
        // Detail shrunk, but still structured JSON with an honest marker.
        let shrunk = view.failure_message.as_deref().expect("shrunk detail");
        assert_ne!(shrunk, detail);
        assert!(shrunk.len() <= detail.len());
        serde_json::from_str::<serde_json::Value>(shrunk).unwrap();
        // The result page, its cursor, and the question are byte-identical:
        // this branch never touches them.
        assert_eq!(view.final_text, original_final);
        assert_eq!(view.next_offset, Some("RUNTIME_TERMINAL".len()));
        assert_eq!(pending_requests.len(), 1);
        assert_eq!(
            pending_requests[0].question.as_ref().unwrap().text,
            question
        );
        assert!(!pending_requests[0].question.as_ref().unwrap().truncated);
    }

    #[test]
    fn wait_projection_shrinks_questions_before_touching_a_paged_result() {
        let detail = realistic_failure_detail();
        let build = |message: Option<String>, question: String| {
            synthesized_wait(
                "RUNTIME_TERMINAL".into(),
                "RUNTIME_TERMINAL".len() + 100,
                Some("RUNTIME_TERMINAL".len()),
                message,
                vec![user_question("q", question)],
            )
        };
        // Size the question so that even the smallest structured detail cannot
        // fit, forcing the question stage; the question stage itself then has
        // ample room (2048 bytes per embed vs a multi-hundred-KiB oversized
        // embed).
        let floor =
            envelope_bytes(&build(Some("{}".into()), String::new()));
        let deficit = MAX_RESPONSE_FRAME_BYTES - floor;
        assert!(deficit > 0);
        let question_len = (deficit + 1024).div_ceil(6) + 1;
        let mut base = build(Some(detail.clone()), "\u{0}".repeat(question_len));
        let mut floor_probe = base.clone();
        if let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(view), ..
        }) = &mut floor_probe
        {
            view.failure_message = Some("{}".into());
        }
        assert!(envelope_bytes(&base) > MAX_RESPONSE_FRAME_BYTES);
        assert!(envelope_bytes(&floor_probe) > MAX_RESPONSE_FRAME_BYTES);
        bound_wait_result(&mut base).unwrap();
        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(view),
            pending_requests,
            ..
        }) = &base
        else {
            panic!("expected wait response")
        };
        assert!(envelope_bytes(&base) <= MAX_RESPONSE_FRAME_BYTES);
        let question = pending_requests[0].question.as_ref().unwrap();
        assert!(
            question.text.len() <= MAX_QUESTION_SUMMARY_BYTES,
            "questions stage must bound the embed"
        );
        assert!(question.truncated);
        // The paged result keeps its original cursor.
        assert_eq!(view.final_text, "RUNTIME_TERMINAL");
        assert_eq!(view.next_offset, Some("RUNTIME_TERMINAL".len()));
        assert_eq!(view.total_bytes, "RUNTIME_TERMINAL".len() + 100);
    }

    #[test]
    fn wait_projection_shortens_a_result_page_with_a_consistent_cursor() {
        let page = "\u{0}".repeat(MAX_RESULT_CHUNK_BYTES);
        let total = MAX_RESULT_CHUNK_BYTES + 4096;
        let mut base = synthesized_wait(
            page.clone(),
            total,
            Some(page.len()),
            None,
            (0..MAX_PENDING_REQUESTS)
                .map(|index| user_question(&format!("q{index}"), "\u{0}".repeat(16 * 1024)))
                .collect(),
        );
        assert!(envelope_bytes(&base) > MAX_RESPONSE_FRAME_BYTES);
        bound_wait_result(&mut base).unwrap();
        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(view),
            pending_requests,
            ..
        }) = &base
        else {
            panic!("expected wait response")
        };
        assert!(envelope_bytes(&base) <= MAX_RESPONSE_FRAME_BYTES);
        // The last-resort page is shorter than the original 256 KiB page.
        assert!(view.final_text.len() < page.len());
        assert!(!view.final_text.is_empty());
        // The cursor advances from the ORIGINAL total, not by re-paging the
        // shortened page (which would falsely report the end of the text).
        let end = view.final_text.len();
        assert_eq!(view.total_bytes, total);
        assert_eq!(view.next_offset, Some(end));
        // Pagination still advances and the questions were bounded too.
        assert!(view.next_offset.unwrap() > view.offset);
        assert!(pending_requests
            .iter()
            .all(|request| request.question.as_ref().unwrap().text.len()
                <= MAX_QUESTION_SUMMARY_BYTES));
    }

    #[test]
    fn wait_projection_shortens_a_complete_page_to_a_consistent_partial_cursor() {
        // A complete page that must shrink becomes partial: the cursor comes
        // from the original total, never from re-paging the shortened text.
        // The chosen prefix is maximal: one more byte cannot fit.
        let page = "\u{0}".repeat(MAX_RESULT_CHUNK_BYTES);
        let mut base = synthesized_wait(
            page.clone(),
            page.len(),
            None,
            None,
            (0..MAX_PENDING_REQUESTS)
                .map(|index| user_question(&format!("q{index}"), "\u{0}".repeat(16 * 1024)))
                .collect(),
        );
        assert!(matches!(
            &base,
            RpcSuccess::TaskWait(TaskWaitBody {
                result: Some(view),
                ..
            }) if view.next_offset.is_none()
        ));
        assert!(envelope_bytes(&base) > MAX_RESPONSE_FRAME_BYTES);
        bound_wait_result(&mut base).unwrap();
        let RpcSuccess::TaskWait(TaskWaitBody {
            result: Some(view), ..
        }) = &base
        else {
            panic!("expected wait response")
        };
        assert!(envelope_bytes(&base) <= MAX_RESPONSE_FRAME_BYTES);
        assert!(view.final_text.len() < page.len());
        let end = view.final_text.len();
        assert_eq!(view.total_bytes, page.len());
        assert_eq!(view.next_offset, Some(end));
        // The chosen prefix is maximal for the real envelope: one more byte
        // cannot fit.
        if end + 1 <= page.len() {
            let mut probe = base.clone();
            if let RpcSuccess::TaskWait(TaskWaitBody {
                result: Some(view), ..
            }) = &mut probe
            {
                view.final_text = page[..end + 1].to_owned();
                view.next_offset = Some(end + 1);
            }
            assert!(envelope_bytes(&probe) > MAX_RESPONSE_FRAME_BYTES);
        }
    }

    fn inject_wait_delta(tracker: &crate::PassiveActivityTracker, delta: &str) {
        tracker.observe(&crate::RuntimeEvent::Driver(external_runtime::Inbound::Message(
            external_contract::WireMessage::Event(external_contract::EventEnvelope {
                method: "session/event".into(),
                params: serde_json::json!({
                    "type": "model.streaming",
                    "eventId": format!("delta-{delta}"),
                    "payload": {
                        "kind": "text_delta",
                        "delta": delta,
                        "assistantMessageId": "m1"
                    }
                }),
            }),
        )));
    }

    #[test]
    fn wait_delivers_activity_tail_incrementally_across_every_exit() {
        // Timeout and actionable-wake exits over one tracker; the terminal exit
        // uses a second fixture because a pending request blocks completion.
        let (_, service, id) = fixture();
        let tracker = Arc::new(crate::PassiveActivityTracker::new(true));
        service
            .scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .insert(id.clone(), Arc::clone(&tracker));

        // First wait: the whole current window (AC1), delivered over the
        // timeout exit.
        inject_wait_delta(&tracker, "alpha");
        let RpcSuccess::TaskWait(TaskWaitBody {
            activity,
            timed_out: true,
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected the timeout exit")
        };
        assert_eq!(activity.latest_text_tail, "alpha");
        assert!(!activity.latest_text_truncated);

        // Actionable wake exit: only the bytes appended since the last wait.
        inject_wait_delta(&tracker, "beta");
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
        let RpcSuccess::TaskWait(TaskWaitBody {
            activity,
            timed_out: false,
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected the wake exit")
        };
        assert_eq!(activity.latest_text_tail, "beta");

        // The wake exit advanced the cursor: a second wait on the same task
        // ships an empty tail for the already-sent bytes.
        let RpcSuccess::TaskWait(TaskWaitBody { activity, .. }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected a wait response")
        };
        assert_eq!(activity.latest_text_tail, "");

        // Terminal exit: a fresh fixture (a pending request blocks completion)
        // proves the terminal response also advances the cursor.
        let (_, service, id) = fixture();
        let tracker = Arc::new(crate::PassiveActivityTracker::new(true));
        service
            .scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .insert(id.clone(), Arc::clone(&tracker));
        inject_wait_delta(&tracker, "gamma");
        service
            .store
            .store_task_result(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "the final answer".into(),
                    partial: false,
                },
            )
            .unwrap();
        let RpcSuccess::TaskWait(TaskWaitBody {
            activity,
            result: Some(_),
            ..
        }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected the terminal exit")
        };
        assert_eq!(activity.latest_text_tail, "gamma");
        // A repeated terminal wait does not re-send the consumed bytes.
        let RpcSuccess::TaskWait(TaskWaitBody { activity, .. }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected a wait response")
        };
        assert_eq!(activity.latest_text_tail, "");

        // Nothing appended is an empty tail, verified on the non-terminal path
        // (a second wait over the same tracker).
        let (_, service, id) = fixture();
        let tracker = Arc::new(crate::PassiveActivityTracker::new(true));
        service
            .scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .insert(id.clone(), Arc::clone(&tracker));
        inject_wait_delta(&tracker, "delta");
        let RpcSuccess::TaskWait(TaskWaitBody { activity, .. }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected a wait response")
        };
        assert_eq!(activity.latest_text_tail, "delta");
        let RpcSuccess::TaskWait(TaskWaitBody { activity, .. }) = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected a wait response")
        };
        assert_eq!(activity.latest_text_tail, "");
    }
    // ---- List-mode (multi-agent) wait contract ----

    fn many_query(ids: &[&str], wait_time: u64) -> TaskWaitQuery {
        TaskWaitQuery {
            agent_id: WaitTarget::Many(ids.iter().map(|id| (*id).to_owned()).collect()),
            wait_time,
            message_id: None,
        }
    }

    fn waits_of(success: RpcSuccess) -> TaskWaitManyBody {
        let RpcSuccess::TaskWaitMany { waits } = success else {
            panic!("expected task_wait_many response")
        };
        waits
    }

    fn body_map(waits: &TaskWaitManyBody) -> std::collections::HashMap<String, &TaskWaitBody> {
        waits.bodies.iter().map(|(id, body)| (id.clone(), body)).collect()
    }

    /// Two running tasks inside one store: the fixture's own task plus a
    /// freshly enqueued one, both claimed and marked running like the
    /// single-task fixture does.
    fn two_running_tasks() -> (tempfile::TempDir, Arc<RpcService>, String, String) {
        let (directory, service, id1) = fixture();
        // A workspace admits one active task at a time, so the second task
        // gets its own repository directory inside the same store.
        let second_repo = tempfile::Builder::new()
            .prefix("wait-second-")
            .tempdir_in(directory.path())
            .unwrap();
        let submitted = service
            .scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: "".into(),
                repository: second_repo.path().canonicalize().unwrap(),
                permission_mode: Some(external_core::PermissionMode::Plan),
                prompt: "second fixture".into(),
                write_manifest: vec![],
            })
            .unwrap();
        let id2 = submitted.agent_id;
        let claim = service.store.claim_next("wait-test", 10, 10).unwrap().unwrap();
        assert_eq!(claim.task.agent_id, id2, "the fixture's own task is already claimed");
        service
            .store
            .mark_session_running(&id2, claim.owner_epoch, "runtime", None, Some("session"), None)
            .unwrap();
        (directory, service, id1, id2)
    }

    fn terminalize(service: &Arc<RpcService>, id: &str, text: &str) {
        service
            .store
            .store_task_result(
                id,
                &TaskResult { outcome: TaskOutcome::Completed, final_text: text.into(), partial: false },
            )
            .unwrap();
    }

    #[test]
    fn many_wait_returns_every_terminal_agent_with_embedded_results() {
        let (_d, service, id1, id2) = two_running_tasks();
        terminalize(&service, &id1, "first");
        terminalize(&service, &id2, "second");
        let waits = waits_of(
            service
                .dispatch(RpcMethod::TaskWait(many_query(&[&id1, &id2], 299)))
                .unwrap(),
        );
        assert_eq!(waits.bodies.len(), 2);
        assert!(waits.error.is_empty());
        for (id, body) in &waits.bodies {
            assert_eq!(body.task.status, "completed");
            assert!(!body.timed_out);
            let result = body.result.as_ref().unwrap();
            assert_eq!(result.next_offset, None);
            assert_eq!(result.final_text, if *id == id1 { "first" } else { "second" });
        }
        // The envelope serializes as a FLAT object keyed by the ids, with no
        // waits wrapper and no error key when nothing was problematic.
        let encoded =
            serde_json::to_value(&RpcSuccess::TaskWaitMany { waits: waits.clone() }).unwrap();
        assert!(encoded.get(&id1).is_some(), "flat id key missing: {encoded}");
        assert!(encoded.get(&id2).is_some());
        assert!(encoded.get("error").is_none());
        assert!(encoded.get("waits").is_none());
    }

    #[test]
    fn many_wait_wakes_when_any_agent_raises_an_actionable_request() {
        let (_d, service, id1, id2) = two_running_tasks();
        // id2 stays plain running; id1 raises a question. The shared wait
        // must return early with id1's actionable projection and id2's
        // running snapshot carrying timed_out=false (an actionable wake is
        // not a timeout).
        service
            .store
            .insert_pending_request(
                "request-1",
                &id1,
                "correlation",
                "user_input",
                r#"{"question":"which branch?"}"#,
            )
            .unwrap();
        let start = Instant::now();
        let waits = waits_of(
            service
                .task_wait(
                    many_query(&[&id1, &id2], 60),
                    start + Duration::from_secs(5),
                    &|| false,
                )
                .unwrap(),
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "actionable wake must be immediate"
        );
        let map = body_map(&waits);
        let body1 = map[&id1];
        assert_eq!(body1.pending_requests.len(), 1);
        assert!(!body1.timed_out);
        let body2 = map[&id2];
        assert_eq!(body2.task.status, "running");
        assert!(!body2.timed_out);
    }

    #[test]
    fn many_wait_marks_only_unfinished_elements_timed_out_at_the_deadline() {
        let (_d, service, id1, id2) = two_running_tasks();
        terminalize(&service, &id1, "first");
        let start = Instant::now();
        let waits = waits_of(
            service
                .task_wait(
                    many_query(&[&id1, &id2], 0),
                    start + Duration::from_millis(30),
                    &|| false,
                )
                .unwrap(),
        );
        let map = body_map(&waits);
        let body1 = map[&id1];
        // Terminal elements embed the result page and never claim timeout.
        assert_eq!(body1.task.status, "completed");
        assert!(!body1.timed_out);
        assert!(body1.result.is_some());
        let body2 = map[&id2];
        assert_eq!(body2.task.status, "running");
        assert!(body2.timed_out);
        assert!(body2.result.is_none());
    }

    #[test]
    fn many_wait_flags_duplicated_and_unknown_ids_in_a_concentrated_error_map() {
        let (_d, service, id1) = fixture();
        terminalize(&service, &id1, "done");
        let unknown = "12345678".to_owned();
        let waits = waits_of(
            service
                .dispatch(RpcMethod::TaskWait(many_query(&[&id1, &id1, &unknown, &id1], 0)))
                .unwrap(),
        );
        // A duplicated id is STILL awaited and returned once under its own
        // key; only its repetition is flagged in the error map.
        assert_eq!(waits.bodies.len(), 1, "only the known id carries a body");
        let map = body_map(&waits);
        let body1 = map[&id1];
        assert_eq!(body1.task.status, "completed");
        assert_eq!(body1.result.as_ref().unwrap().final_text, "done");
        assert!(!map.contains_key(&unknown), "unknown ids carry no body");
        assert_eq!(
            waits.error.get(&id1).map(String::as_str),
            Some("duplicated")
        );
        assert_eq!(
            waits.error.get(&unknown).map(String::as_str),
            Some("unknown_agent")
        );
        // The wire form is FLAT: id keys plus one error key, no wrapper.
        let encoded = serde_json::to_value(&RpcSuccess::TaskWaitMany { waits }).unwrap();
        assert!(encoded.get(&id1).is_some());
        assert!(
            encoded.get("error").unwrap().get(&id1).is_some(),
            "error map missing duplicated id: {encoded}"
        );
        assert_eq!(encoded["error"][&unknown], "unknown_agent");
        assert!(encoded.get("waits").is_none());
    }

    #[test]
    fn many_wait_rejects_message_id_and_oversized_lists() {
        let (_d, service, id) = fixture();
        // message_id is scalar-only: rejected at the RPC layer.
        let mut input = many_query(&[&id], 0);
        input.message_id = Some("message-1".into());
        let error = service.dispatch(RpcMethod::TaskWait(input)).unwrap_err();
        assert!(matches!(error.code, RpcErrorCode::Validation));
        // More than 16 ids is rejected.
        let ids: Vec<String> = (0..17).map(|i| format!("{:08}", 10_000_000 + i)).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let error = service
            .dispatch(RpcMethod::TaskWait(many_query(&refs, 0)))
            .unwrap_err();
        assert!(matches!(error.code, RpcErrorCode::Validation));
    }

    #[test]
    fn many_wait_degrades_cooperatively_across_large_result_pages() {
        // Ten terminal agents each carrying a near-cap result page cannot
        // fit by shrinking any single one; the degradation must cooperate
        // across bodies instead of erroring on the first element.
        let (_d, service, first_id) = fixture();
        let mut ids = vec![first_id.clone()];
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.agent-work/tmp")
            .join("wait-many-large");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let mut repos = vec![std::env::temp_dir()];
        for index in 0..9 {
            let repo = base.join(format!("repo-{index}"));
            std::fs::create_dir_all(&repo).unwrap();
            repos.push(repo);
        }
        // The fixture's own task used the temp dir; enqueue the rest.
        for repo in &repos[1..] {
            let submitted = service
                .scheduler
                .enqueue_general(&GeneralTaskManifest {
                    schema: "zcode-general-task/v1".into(),
                    agent_id: "".into(),
                    repository: repo.canonicalize().unwrap(),
                    permission_mode: Some(external_core::PermissionMode::Plan),
                    prompt: "large fixture".into(),
                    write_manifest: vec![],
                })
                .unwrap();
            let id = submitted.agent_id;
            let claim = service.store.claim_next("wait-test", 10, 10).unwrap().unwrap();
            assert_eq!(claim.task.agent_id, id);
            service
                .store
                .mark_session_running(
                    &id,
                    claim.owner_epoch,
                    "runtime",
                    None,
                    Some("session"),
                    None,
                )
                .unwrap();
            ids.push(id);
        }
        // ~256 KiB ASCII payload per agent — ten of them far exceed the
        // 2 MiB frame cap even after any single-body shrink.
        let payload = "a".repeat(256 * 1024);
        for id in &ids {
            terminalize(&service, id, &payload);
        }
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let waits = waits_of(
            service
                .dispatch(RpcMethod::TaskWait(many_query(&refs, 0)))
                .expect("cooperative degradation must fit the frame"),
        );
        assert_eq!(waits.bodies.len(), ids.len());
        // Every body carries a real (shortened) page with a consistent
        // cursor: no page claims to be the end while text remains.
        let mut total = 0usize;
        for (_, body) in &waits.bodies {
            let view = body.result.as_ref().expect("terminal body embeds a page");
            assert!(!view.final_text.is_empty(), "page must keep at least one char");
            let consumed = view.offset + view.final_text.len();
            assert!(consumed < view.total_bytes, "shortened page must not claim the end");
            assert_eq!(view.next_offset, Some(consumed));
            total += view.final_text.len();
        }
        // The shared budget was actually distributed: each page shrank far
        // below its original 256 KiB.
        assert!(total < 10 * 256 * 1024);
        assert!(total > 0);
    }

    #[test]
    fn many_wait_drops_the_terminal_cache_when_send_resumes_the_task() {
        let (_d, service, id1, id2) = two_running_tasks();
        terminalize(&service, &id1, "first result");
        // First wait caches id1 as terminal with the old result embedded.
        let first = waits_of(
            service
                .dispatch(RpcMethod::TaskWait(many_query(&[&id1], 0)))
                .unwrap(),
        );
        assert_eq!(body_map(&first)[&id1].task.status, "completed");
        // id2 keeps a later wait blocked; meanwhile id1 is requeued for a
        // codex-style resume (the scheduler's send path performs exactly
        // this transition). The cached terminal snapshot must NOT shadow
        // the resumed state: the next wait must re-read the row.
        service
            .store
            .requeue_task_for_resume_with_message(&id1, "resume-message", "queue", "continue")
            .unwrap();
        // A zero-timeout wait projects the resumed state immediately: not
        // terminal, no stale result, timed_out=true only because this call
        // itself expired (id2 alone would also expire; check per element).
        let second = waits_of(
            service
                .dispatch(RpcMethod::TaskWait(many_query(&[&id1], 0)))
                .unwrap(),
        );
        let map = body_map(&second);
        let body1 = map[&id1];
        assert_ne!(
            body1.task.status, "completed",
            "the resumed state must replace the stale terminal snapshot"
        );
        assert_eq!(body1.task.status, "queued");
        assert!(
            body1.result.is_none(),
            "the stale terminal result must not be returned for the resumed task"
        );
        // And the resumed agent keeps the wait unsettled: a list wait over
        // [id1, id2] with a short deadline reports id1 (and id2) timed out
        // rather than returning early as all-terminal.
        let start = Instant::now();
        let third = waits_of(
            service
                .task_wait(
                    many_query(&[&id1, &id2], 0),
                    start + Duration::from_millis(30),
                    &|| false,
                )
                .unwrap(),
        );
        let map3 = body_map(&third);
        assert!(map3[&id1].timed_out, "resumed id1 is no longer settled");
        assert!(map3[&id2].timed_out);
        assert!(map3[&id1].result.is_none());
    }

    #[test]
    fn many_wait_interruption_does_not_consume_undelivered_activity_text() {
        let (_d, service, id1, id2) = two_running_tasks();
        terminalize(&service, &id1, "first");
        // Both agents have undelivered activity text. The interrupted
        // list-mode wait must leave the delivery cursors untouched, so a
        // later wait still sees the text.
        let tracker1 = Arc::new(crate::PassiveActivityTracker::new(true));
        service
            .scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .insert(id1.clone(), tracker1.clone());
        let tracker2 = Arc::new(crate::PassiveActivityTracker::new(true));
        service
            .scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .insert(id2.clone(), tracker2.clone());
        inject_wait_delta(&tracker1, "alpha-from-one");
        inject_wait_delta(&tracker2, "beta-from-two");
        let start = Instant::now();
        let interrupted = service.task_wait(
            many_query(&[&id1, &id2], 60),
            start + Duration::from_secs(5),
            &|| true, // cancelled immediately: no response is ever delivered
        );
        assert!(interrupted.is_err(), "the interrupted wait must fail");
        // The interruption itself is recorded with the fleet shape.
        let record = service
            .scheduler
            .last_error(&id1)
            .expect("interruption must be recorded");
        assert!(record.contains("task_wait interrupted"), "{record}");
        assert!(record.contains("agents=2"), "{record}");
        // The cursors were never consumed: a following wait still sees the
        // full undelivered tails.
        let waits = waits_of(
            service
                .dispatch(RpcMethod::TaskWait(many_query(&[&id1, &id2], 0)))
                .unwrap(),
        );
        let map = body_map(&waits);
        // id1 is terminal with an embedded result: its tail dedups against
        // the page, but the delivery itself only happened in THIS response.
        let body1 = map[&id1];
        assert_eq!(body1.task.status, "completed");
        let body2 = map[&id2];
        assert_eq!(
            body2.activity.latest_text_tail, "beta-from-two",
            "the interrupted wait must not have consumed id2's tail"
        );
        // And a second wait returns nothing new for id2 (cursor advanced by
        // the delivered response only).
        let second = waits_of(
            service
                .dispatch(RpcMethod::TaskWait(many_query(&[&id1, &id2], 0)))
                .unwrap(),
        );
        let map2 = body_map(&second);
        assert_eq!(map2[&id2].activity.latest_text_tail, "");
    }

    #[test]
    fn many_wait_drops_the_terminal_cache_across_a_resume_recompletion_cycle() {
        let (_d, service, id1, id2) = two_running_tasks();
        terminalize(&service, &id1, "old result");
        // id2 stays running so the wait below keeps polling. The sequence
        // exercised INSIDE that single wait call:
        //   round 1: id1 snapshot cached as terminal (generation N);
        //   between rounds: id1 is resumed (requeue -> claim bumps the
        //     epoch to N+1 -> message drained) and RE-completed with a new
        //     result — the phase reads terminal again, so only the epoch
        //     comparison can detect the stale cache;
        //   later round: the re-check drops the cache and re-snapshots,
        //     and the final response carries the NEW result.
        let trigger = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let service_outer = service.clone();
        let service_inner = service.clone();
        let id1_clone = id1.clone();
        let trigger_clone = trigger.clone();
        let start = Instant::now();
        // Run the wait on a worker thread: its poll rounds call `interrupted`,
        // which performs the resume cycle exactly once after the first round
        // has cached the terminal snapshot.
        let handle = std::thread::spawn(move || {
            service_outer.task_wait(
                many_query(&[&id1_clone, &id2], 60),
                start + Duration::from_secs(10),
                &move || {
                    if trigger_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 3 {
                        // Round 1 (and a few sleeps) have passed; the cache
                        // exists. Perform the resume + re-completion cycle.
                        let service_arc = &service_inner;
                        service_arc
                            .store
                            .requeue_task_for_resume_with_message(
                                &id1_clone,
                                "resume-message",
                                "queue",
                                "again",
                            )
                            .unwrap();
                        let claim = service_arc
                            .store
                            .claim_next("wait-test", 10, 10)
                            .unwrap()
                            .unwrap();
                        assert_eq!(claim.task.agent_id, id1_clone);
                        service_arc
                            .store
                            .mark_session_running(
                                &id1_clone,
                                claim.owner_epoch,
                                "runtime",
                                None,
                                Some("session"),
                                None,
                            )
                            .unwrap();
                        let queued = service_arc
                            .store
                            .claim_next_message(&id1_clone)
                            .unwrap()
                            .unwrap();
                        assert_eq!(queued.message_id, "resume-message");
                        service_arc
                            .store
                            .complete_message(&queued.message_id, None)
                            .unwrap();
                        // Mark the second generation terminal with a new
                        // result so the phase alone looks identical.
                        service_arc
                            .store
                            .store_task_result(
                                &id1_clone,
                                &TaskResult {
                                    outcome: TaskOutcome::Completed,
                                    final_text: "new result".into(),
                                    partial: false,
                                },
                            )
                            .unwrap();
                    }
                    false
                },
            )
        });
        let response = handle.join().unwrap().unwrap();
        let waits = waits_of(response);
        let map = body_map(&waits);
        let body1 = map[&id1];
        assert_eq!(body1.task.status, "completed");
        assert_eq!(
            body1.result.as_ref().unwrap().final_text,
            "new result",
            "the recompleted generation's result must replace the stale one"
        );
        // The stale-cache drop is visible as a diagnostic record: phase,
        // cached epoch, and the current epoch all preserved.
        let record = service
            .scheduler
            .last_error(&id1)
            .expect("stale snapshot drop must be recorded");
        assert!(
            record.contains("dropped stale terminal snapshot"),
            "{record}"
        );
        assert!(record.contains("cached_epoch="), "{record}");
        assert!(record.contains("current_epoch="), "{record}");
    }

    #[test]
    fn many_wait_fits_a_mixed_fleet_of_large_questions_and_result_pages() {
        // Reproduction shape: 3 terminal agents with 256 KiB result pages
        // plus 13 running agents each holding 72 pending user_input
        // questions of 2048-byte text. Questions degrade only to a 2048-byte
        // prefix (the embed cap), so the question fleet alone is ~1.9 MiB
        // before the result pages; no SINGLE page shrink can fit the frame,
        // only a joint one can.
        let (_d, service, first_id) = fixture();
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.agent-work/tmp")
            .join("wait-many-mixed");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let mut ids = vec![first_id.clone()];
        for index in 0..15 {
            let repo = base.join(format!("repo-{index}"));
            std::fs::create_dir_all(&repo).unwrap();
            let submitted = service
                .scheduler
                .enqueue_general(&GeneralTaskManifest {
                    schema: "zcode-general-task/v1".into(),
                    agent_id: "".into(),
                    repository: repo.canonicalize().unwrap(),
                    permission_mode: Some(external_core::PermissionMode::Plan),
                    prompt: "mixed fixture".into(),
                    write_manifest: vec![],
                })
                .unwrap();
            let id = submitted.agent_id;
            let claim = service.store.claim_next("wait-test", 10, 10).unwrap().unwrap();
            assert_eq!(claim.task.agent_id, id);
            service
                .store
                .mark_session_running(&id, claim.owner_epoch, "runtime", None, Some("session"), None)
                .unwrap();
            ids.push(id);
        }
        // Three terminal agents with large pages.
        let payload = "p".repeat(256 * 1024);
        for id in &ids[..3] {
            terminalize(&service, id, &payload);
        }
        // Thirteen running agents each with 72 actionable questions (the
        // actionable set wakes the wait immediately).
        let question = "q".repeat(2048);
        for id in &ids[3..] {
            for request in 0..72 {
                service
                    .store
                    .insert_pending_request(
                        &format!("q-{id}-{request}"),
                        id,
                        &format!("correlation-{id}-{request}"),
                        "user_input",
                        &serde_json::to_string(&serde_json::json!({ "question": question }))
                            .unwrap(),
                    )
                    .unwrap();
            }
        }
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let waits = waits_of(
            service
                .dispatch(RpcMethod::TaskWait(many_query(&refs, 0)))
                .expect("joint degradation must fit the mixed fleet"),
        );
        assert_eq!(waits.bodies.len(), ids.len());
        // Terminal pages were shortened consistently (never claiming the
        // end) and the question carriers kept their actionable records.
        for (id, body) in &waits.bodies {
            if ids[..3].contains(id) {
                let view = body.result.as_ref().unwrap();
                assert!(!view.final_text.is_empty());
                let consumed = view.offset + view.final_text.len();
                assert!(consumed < view.total_bytes, "shortened page must not claim the end");
                assert_eq!(view.next_offset, Some(consumed));
            } else {
                assert!(
                    !body.pending_requests.is_empty(),
                    "actionable questions stay reachable"
                );
            }
        }
    }

    #[test]
    fn many_wait_oversized_failure_leaves_delivery_cursors_untouched() {
        // A frame that cannot fit even after full degradation (16 agents
        // each with 100 questions of 2048-byte text ≈ 3.2 MiB after the
        // embed cap) must return Oversized WITHOUT committing any activity
        // delivery cursor — a later wait still sees the undelivered text.
        let (_d, service, first_id) = fixture();
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.agent-work/tmp")
            .join("wait-many-oversized");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let mut ids = vec![first_id.clone()];
        for index in 0..15 {
            let repo = base.join(format!("repo-{index}"));
            std::fs::create_dir_all(&repo).unwrap();
            let submitted = service
                .scheduler
                .enqueue_general(&GeneralTaskManifest {
                    schema: "zcode-general-task/v1".into(),
                    agent_id: "".into(),
                    repository: repo.canonicalize().unwrap(),
                    permission_mode: Some(external_core::PermissionMode::Plan),
                    prompt: "oversized fixture".into(),
                    write_manifest: vec![],
                })
                .unwrap();
            let id = submitted.agent_id;
            let claim = service.store.claim_next("wait-test", 10, 10).unwrap().unwrap();
            assert_eq!(claim.task.agent_id, id);
            service
                .store
                .mark_session_running(&id, claim.owner_epoch, "runtime", None, Some("session"), None)
                .unwrap();
            ids.push(id);
        }
        let question = "w".repeat(2048);
        for id in &ids {
            for request in 0..100 {
                service
                    .store
                    .insert_pending_request(
                        &format!("q-{id}-{request}"),
                        id,
                        &format!("correlation-{id}-{request}"),
                        "user_input",
                        &serde_json::to_string(&serde_json::json!({ "question": question }))
                            .unwrap(),
                    )
                    .unwrap();
            }
        }
        // Undelivered activity text on every agent.
        let trackers: Vec<_> = ids
            .iter()
            .map(|id| {
                let tracker = Arc::new(crate::PassiveActivityTracker::new(true));
                service
                    .scheduler
                    .inner
                    .state
                    .lock()
                    .unwrap()
                    .activities
                    .insert(id.clone(), tracker.clone());
                tracker
            })
            .collect();
        for (index, tracker) in trackers.iter().enumerate() {
            inject_wait_delta(tracker, &format!("tail-{index}"));
        }
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let failure = service.dispatch(RpcMethod::TaskWait(many_query(&refs, 0)));
        let error = failure.expect_err("this frame genuinely cannot fit");
        assert!(matches!(error.code, RpcErrorCode::Oversized));
        // The degradation dead-end is preserved as a diagnostic record keyed
        // by the wait's primary id, with the full body composition.
        let record = service
            .scheduler
            .last_error(&ids[0])
            .expect("oversized failure must be recorded");
        assert!(record.contains("task_wait_many oversized"), "{record}");
        assert!(record.contains("questions=1600"), "{record}");
        assert!(record.contains("agents=16"), "{record}");
        // No cursor moved: the single-agent wait for one member still sees
        // its full undelivered tail.
        let probe = &ids[7];
        let body = {
            let response = service
                .dispatch(RpcMethod::TaskWait(query(probe, 0)))
                .unwrap();
            let RpcSuccess::TaskWait(body) = response else {
                panic!("scalar wait");
            };
            body
        };
        assert_eq!(
            body.activity.latest_text_tail, "tail-7",
            "the Oversized failure must not have consumed any delivery cursor"
        );
    }

    #[test]
    fn scalar_wait_response_shape_is_unchanged() {
        let (_d, service, id) = fixture();
        service
            .store
            .store_task_result(
                &id,
                &TaskResult { outcome: TaskOutcome::Completed, final_text: "done".into(), partial: false },
            )
            .unwrap();
        let response = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap();
        // The scalar response is the flat six-field object: exactly the keys
        // of the pre-union contract, no waits wrapper.
        let encoded = serde_json::to_value(&response).unwrap();
        let mut keys = encoded.as_object().unwrap().keys().collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["activity", "kind", "message_receipt", "pending_requests", "result", "task", "timed_out"]
        );
        assert_eq!(encoded["kind"], "task_wait");
    }

}
