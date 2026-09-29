//! Pure turn-terminal classification for an `agy` stream.
//!
//! `agy` may emit more than one `result` for a turn (the idle-period signal
//! trap, `docs/compatibility/antigravity.md` §4): the classifier settles a
//! turn on the **last** `result` it observed, so a later result overwrites an
//! earlier one. It never consults process liveness — the daemon's three-state
//! lifecycle (settle on result / stop on completed turn / cancel on signal) is
//! layered on top, and deciding that a post-stop extra `ERROR` is stream
//! noise is the daemon's policy, not this classifier's.
//!
//! Per session the classifier additionally tracks the `conversation_id`, the
//! largest `step_index` seen with a continuity flag (indices are global and
//! non-decreasing across turns, repeating within one step's `ACTIVE`/`DONE`
//! pair), and the session-cumulative `num_turns` reported by the latest
//! result.

use crate::event::{AgyEvent, DeniedAction, ResultStatus};

/// The literal `error` value that marks a mid-turn signal interruption as a
/// cancellation (with the truncated partial text retained) rather than a
/// failure.
pub const INTERRUPTED_ERROR: &str = "interrupted";

/// The terminal outcome of one settled turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The last result was `SUCCESS`; `text` is its response.
    Completed { text: String },
    /// The turn was interrupted/cancelled; the truncated partial text is
    /// retained alongside the reason.
    Cancelled { text: String, error: String },
    /// The turn failed; `error` is the reported reason or a status-derived
    /// default.
    Failed { error: String },
}

/// The settlement of one turn: the last `result` for it, normalized.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnSettlement {
    pub conversation_id: String,
    pub status: ResultStatus,
    /// The session-cumulative turn count reported by this result.
    pub num_turns: u64,
    pub outcome: TurnOutcome,
    /// Structured soft-deny entries the result carried (successful soft-deny
    /// turns keep them; the daemon projects them on failure/cancel tails only).
    pub denied_actions: Vec<DeniedAction>,
}

/// A pure per-session state machine fed parsed events in stream order.
#[derive(Debug)]
pub struct TurnClassifier {
    conversation_id: Option<String>,
    last_step_index: Option<u64>,
    step_index_continuous: bool,
    num_turns: u64,
    settled: Option<TurnSettlement>,
}

impl Default for TurnClassifier {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnClassifier {
    pub fn new() -> Self {
        Self {
            conversation_id: None,
            last_step_index: None,
            step_index_continuous: true,
            num_turns: 0,
            settled: None,
        }
    }

    /// Feed one parsed event. A `result` re-settles the current turn (the last
    /// result wins) and returns the settlement it produced; every other event
    /// only advances the session progress and returns `None`.
    pub fn observe(&mut self, event: &AgyEvent) -> Option<TurnSettlement> {
        match event {
            AgyEvent::Init {
                conversation_id, ..
            } => {
                self.note_conversation_id(conversation_id);
                None
            }
            AgyEvent::StepUpdate(step) => {
                self.note_conversation_id(&step.conversation_id);
                self.note_step_index(step.step_index);
                None
            }
            AgyEvent::Result(result) => {
                self.note_conversation_id(&result.conversation_id);
                self.num_turns = self.num_turns.max(result.num_turns);
                let settlement = settle(result);
                self.settled = Some(settlement.clone());
                Some(settlement)
            }
            AgyEvent::Unknown { .. } => None,
        }
    }

    /// The current settled turn, if any result has been observed.
    pub fn settled(&self) -> Option<&TurnSettlement> {
        self.settled.as_ref()
    }

    /// The conversation identity learned from `init` or a payload.
    pub fn conversation_id(&self) -> Option<&str> {
        self.conversation_id.as_deref()
    }

    /// The session-cumulative turn count of the latest result.
    pub fn num_turns(&self) -> u64 {
        self.num_turns
    }

    /// The largest `step_index` observed.
    pub fn last_step_index(&self) -> Option<u64> {
        self.last_step_index
    }

    /// Whether every observed `step_index` stayed contiguous: a new maximum
    /// must be exactly one past the previous maximum, and a repeat of the
    /// current maximum (the `ACTIVE`/`DONE` pair) is allowed. A gap or an
    /// out-of-order index clears the flag for the session.
    pub fn step_index_continuous(&self) -> bool {
        self.step_index_continuous
    }

    fn note_conversation_id(&mut self, conversation_id: &str) {
        if !conversation_id.is_empty() && self.conversation_id.as_deref() != Some(conversation_id) {
            self.conversation_id = Some(conversation_id.to_owned());
        }
    }

    fn note_step_index(&mut self, index: u64) {
        match self.last_step_index {
            None => self.last_step_index = Some(index),
            Some(last) if index == last => {}
            Some(last) if index == last + 1 => self.last_step_index = Some(index),
            Some(last) if index > last => {
                self.step_index_continuous = false;
                self.last_step_index = Some(index);
            }
            Some(_) => self.step_index_continuous = false,
        }
    }
}

/// Normalize one result payload into a settlement. `SUCCESS` completes with
/// its response; an `ERROR` whose error is [`INTERRUPTED_ERROR`] is a
/// cancellation that keeps the partial text; any other failure is failed.
fn settle(result: &crate::event::ResultPayload) -> TurnSettlement {
    let outcome = match result.status {
        ResultStatus::Success => TurnOutcome::Completed {
            text: result.response.clone(),
        },
        ResultStatus::Interrupted | ResultStatus::Canceled => TurnOutcome::Cancelled {
            text: result.response.clone(),
            error: result
                .error
                .clone()
                .unwrap_or_else(|| result.status.as_str().to_lowercase()),
        },
        ResultStatus::Error => {
            let error = result.error.clone().unwrap_or_default();
            if error == INTERRUPTED_ERROR {
                TurnOutcome::Cancelled {
                    text: result.response.clone(),
                    error,
                }
            } else {
                TurnOutcome::Failed {
                    error: non_empty(error).unwrap_or_else(|| {
                        format!("agy result ended with status {}", result.status.as_str())
                    }),
                }
            }
        }
        _ => TurnOutcome::Failed {
            error: non_empty(result.error.clone().unwrap_or_default()).unwrap_or_else(|| {
                format!("agy result ended with status {}", result.status.as_str())
            }),
        },
    };
    TurnSettlement {
        conversation_id: result.conversation_id.clone(),
        status: result.status.clone(),
        num_turns: result.num_turns,
        outcome,
        denied_actions: result.denied_actions.clone(),
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::parse_line;

    const HAPPY: &str = include_str!("../tests/fixtures/happy.ndjson");
    const MULTI: &str = include_str!("../tests/fixtures/multi.ndjson");
    const DUAL_RESULT: &str = include_str!("../tests/fixtures/dual-result.ndjson");
    const INTERRUPTED: &str = include_str!("../tests/fixtures/interrupted.ndjson");
    const SOFT_DENY: &str = include_str!("../tests/fixtures/soft-deny.ndjson");
    const NEG_CONTROL: &str = include_str!("../tests/fixtures/neg-control.ndjson");

    fn classify(ndjson: &str) -> TurnClassifier {
        let mut classifier = TurnClassifier::new();
        for line in ndjson.lines() {
            classifier.observe(&parse_line(line).unwrap());
        }
        classifier
    }

    #[test]
    fn happy_turn_settles_completed_with_the_response() {
        // Boundary 1: init -> user_input -> agent_response(ACTIVE/DONE) ->
        // result{SUCCESS,response}.
        let classifier = classify(HAPPY);
        assert_eq!(
            classifier.conversation_id(),
            Some("e1c47663-9ef1-43ec-a333-dccd868220f1")
        );
        assert_eq!(classifier.num_turns(), 1);
        assert!(classifier.step_index_continuous());
        let settlement = classifier.settled().unwrap();
        assert_eq!(settlement.status, ResultStatus::Success);
        assert_eq!(
            settlement.outcome,
            TurnOutcome::Completed {
                text: "MANGO\n".into()
            }
        );
        assert!(settlement.denied_actions.is_empty());
    }

    #[test]
    fn mid_turn_interrupt_settles_cancelled_with_partial_text() {
        // Boundary 3: a single ERROR result with error="interrupted" keeps the
        // truncated response and cancels rather than fails.
        let classifier = classify(INTERRUPTED);
        let settlement = classifier.settled().unwrap();
        assert_eq!(settlement.status, ResultStatus::Error);
        match &settlement.outcome {
            TurnOutcome::Cancelled { text, error } => {
                assert_eq!(error, "interrupted");
                assert!(
                    text.starts_with("1\n2\n3\n"),
                    "partial text retained, got {text:?}"
                );
                assert!(text.ends_with("526"));
                assert!(!text.is_empty());
            }
            other => panic!("expected cancelled, got {other:?}"),
        }
        assert_eq!(classifier.num_turns(), 1);
    }

    #[test]
    fn idle_dual_result_last_result_wins() {
        // Boundary 4: a SUCCESS turn result followed by the idle-period stream
        // cancel ERROR. The turn settles on the last result.
        let classifier = classify(DUAL_RESULT);
        let settlement = classifier.settled().unwrap();
        assert_eq!(settlement.status, ResultStatus::Error);
        match &settlement.outcome {
            TurnOutcome::Failed { error } => {
                assert_eq!(error, "stream input cancelled: context canceled")
            }
            other => panic!("expected the later ERROR to win, got {other:?}"),
        }
        // The earlier SUCCESS settlement is fully overwritten: the response on
        // the winning settlement is the later result's text, not a merge.
        assert!(matches!(&settlement.outcome, TurnOutcome::Failed { .. }));
        assert_eq!(settlement.num_turns, 1);
        // Re-observing the two results in order reproduces last-wins.
        let mut fresh = TurnClassifier::new();
        let lines: Vec<_> = DUAL_RESULT.lines().collect();
        let first = fresh.observe(&parse_line(lines[0]).unwrap()).unwrap();
        assert!(matches!(first.outcome, TurnOutcome::Completed { .. }));
        let second = fresh.observe(&parse_line(lines[1]).unwrap()).unwrap();
        assert!(matches!(second.outcome, TurnOutcome::Failed { .. }));
        assert_eq!(fresh.settled(), Some(&second));
    }

    #[test]
    fn soft_deny_result_keeps_the_denied_actions() {
        // Boundary 5: default posture soft-denies a tool, the task still exits
        // SUCCESS, and the structured denied_actions ride on the result.
        let classifier = classify(SOFT_DENY);
        let settlement = classifier.settled().unwrap();
        assert_eq!(
            settlement.outcome,
            TurnOutcome::Completed {
                text: String::new()
            }
        );
        assert_eq!(
            settlement.denied_actions,
            vec![DeniedAction {
                action: "command".into(),
                display_name: "RunCommand".into(),
            }]
        );
    }

    #[test]
    fn negative_input_error_settles_failed() {
        // Boundary 6: a control_request is rejected with an ERROR result.
        let classifier = classify(NEG_CONTROL);
        let settlement = classifier.settled().unwrap();
        assert_eq!(settlement.status, ResultStatus::Error);
        match &settlement.outcome {
            TurnOutcome::Failed { error } => assert!(error.contains("control_request")),
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[test]
    fn admission_failure_shape_settles_failed() {
        // Boundary 7: unknown --model / invalid --effort fail admission with an
        // ERROR result whose conversation_id is empty and num_turns is 0
        // (docs/compatibility/antigravity.md §5).
        let line = concat!(
            r#"{"event":"result","result":{"conversation_id":"","status":"ERROR","#,
            r#""response":"","error":"error: invalid model selection \"nope\". "#,
            r#"Available models: gemini-3.8-flash-high, claude-sonnet-4-6","#,
            r#""duration_seconds":0,"num_turns":0,"usage":{"input_tokens":0,"#,
            r#""output_tokens":0,"thinking_tokens":0,"cache_read_tokens":0,"#,
            r#""total_tokens":0}}}"#
        );
        let classifier = classify(line);
        assert_eq!(classifier.conversation_id(), None);
        assert_eq!(classifier.num_turns(), 0);
        let settlement = classifier.settled().unwrap();
        match &settlement.outcome {
            TurnOutcome::Failed { error } => {
                assert!(error.contains("invalid model selection"))
            }
            other => panic!("expected failed, got {other:?}"),
        }
        assert_eq!(settlement.conversation_id, "");
    }

    #[test]
    fn multi_turn_tracks_num_turns_and_step_continuity() {
        let mut classifier = TurnClassifier::new();
        let mut settlements = Vec::new();
        for line in MULTI.lines() {
            if let Some(settlement) = classifier.observe(&parse_line(line).unwrap()) {
                settlements.push(settlement);
            }
        }
        assert_eq!(settlements.len(), 2);
        assert_eq!(
            settlements[0].outcome,
            TurnOutcome::Completed {
                text: "APPLE\n".into()
            }
        );
        assert_eq!(settlements[0].num_turns, 1);
        assert_eq!(
            settlements[1].outcome,
            TurnOutcome::Completed {
                text: "PEAR\n".into()
            }
        );
        assert_eq!(settlements[1].num_turns, 2);
        assert_eq!(classifier.num_turns(), 2);
        assert_eq!(classifier.last_step_index(), Some(3));
        assert!(classifier.step_index_continuous());
        assert_eq!(
            classifier.conversation_id(),
            Some("a48d42bb-a643-476b-b394-f4efac7fff21")
        );
    }

    #[test]
    fn step_index_gaps_clear_the_continuity_flag() {
        let mut classifier = TurnClassifier::new();
        for index in [0u64, 0, 1] {
            let line = format!(
                r#"{{"event":"step_update","step_update":{{"conversation_id":"c","step_index":{index},"state":"DONE","step_type":"user_input"}}}}"#
            );
            classifier.observe(&parse_line(&line).unwrap());
        }
        assert!(classifier.step_index_continuous());
        let gap = r#"{"event":"step_update","step_update":{"conversation_id":"c","step_index":5,"state":"DONE","step_type":"user_input"}}"#;
        classifier.observe(&parse_line(gap).unwrap());
        assert!(!classifier.step_index_continuous());
    }

    #[test]
    fn unknown_status_and_unsupported_failures_default_their_reason() {
        let line = concat!(
            r#"{"event":"result","result":{"conversation_id":"c","status":"WAITING","#,
            r#""response":"partial","num_turns":1}}"#
        );
        let settlement = classify(line).settled().unwrap().clone();
        assert_eq!(
            settlement.outcome,
            TurnOutcome::Failed {
                error: "agy result ended with status WAITING".into()
            }
        );

        // An ERROR with no error string still fails with a status default.
        let bare = concat!(
            r#"{"event":"result","result":{"conversation_id":"c","status":"ERROR","#,
            r#""response":"","num_turns":1}}"#
        );
        assert_eq!(
            classify(bare).settled().unwrap().outcome,
            TurnOutcome::Failed {
                error: "agy result ended with status ERROR".into()
            }
        );

        // CANCELED keeps the partial text like an interruption.
        let canceled = concat!(
            r#"{"event":"result","result":{"conversation_id":"c","status":"CANCELED","#,
            r#""response":"half","num_turns":1}}"#
        );
        assert_eq!(
            classify(canceled).settled().unwrap().outcome,
            TurnOutcome::Cancelled {
                text: "half".into(),
                error: "canceled".into()
            }
        );
    }
}
