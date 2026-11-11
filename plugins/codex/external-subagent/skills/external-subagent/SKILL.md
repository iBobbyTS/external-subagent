---
name: external-subagent
description: Manage bounded repository-scoped tasks through external-subagent.
---

# external-subagent

Spawn with an absolute `repository`, a complete bounded `prompt` and a named
`profile` or explicit runtime settings. A profile supplies subagent, permissions,
model, effort and developer instructions; do not also pass these as overrides.
Use the tool parameter descriptions for target-specific values. Status exposes
runtime readiness when a capability needs checking; it is not a mandatory preflight.

Every successful spawn creates a new task and returns `agent_id`, `session_id`
and `status`. Preserve these actual identities. If the response fails, read the
complete error and any returned identity before deciding on another launch. Use
`list` with the required `repository` when locating existing tasks; never guess IDs.

Use `wait` in the current turn until the assignment completes or needs an answer;
do not schedule automatic wake-up and end the turn instead. `wait_time` is 0..600
seconds (default 290). Scalar `agent_id` returns one wait body; an array of 1..16
IDs returns a flat map keyed by ID. `wait_mode: "all"` waits for the batch;
`"any"` supports rolling replacement. Remove completed IDs from later waits.

Read the entire response. `result: null` means no final result yet;
`timed_out` describes the wait deadline, not task failure. A wait already
containing a result does not need another call to confirm completion. Consume
its `final_text`; call `result` with `next_offset` only for remaining pages
(or for a deliberate reread). Only `outcome: "COMPLETED"` is successful.
On native turn failure, `final_text` contains the original error as JSON with
credentials redacted; `failure_message` contains bounded ES diagnostics.
Resolve persistent quota/authentication failures before retrying. Handle the
full pending question or permission request before waiting again. Cancelling a
wait does not cancel the task.

Use `send` for a bounded follow-up to the actual actor. `queue` does not interrupt
and may wait for the current turn to end. For an urgent correction use supported
`steer` (codex/zcode), or cancel and collect termination before deciding on a
replacement. `delivered` means receipt, not completion. Keep the returned
`message_id` and use scalar wait to collect the receipt and result. Terminal
continuation uses resume where supported; dsh returns
`TERMINAL_SEND_UNSUPPORTED`. Preserve the assignment and caller-owned retry
count across continuations.

Use `observe` only to investigate suspected stalled or looping work. Activity
and coverage are evidence of liveness, not acceptance. Close the task when its
results are received and no further continuation is needed.
