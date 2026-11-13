//! Prompt-settlement folding into the shared task result.
//!
//! The public DSH ACP contract carries the turn's text as standard
//! `agent_message_chunk` updates and settles the prompt with a `stopReason`
//! only: it never promises a committed flag or a settlement `messageId`.
//! Text is therefore aggregated in two scopes at once — by native message
//! identity when the update carries one, and by the adapter's local prompt
//! turn, which is the only boundary the standard flow provides. Settlement
//! reads the native message when the response names one and otherwise the
//! prompt-scoped text; the prompt bucket is sealed once its settlement is
//! applied, so a late frame can never leak into a later turn.
//!
//! Progress, thought, and tool updates are never merged into the result.
//! Each stop reason keeps its own meaning: `end_turn` is protocol completion
//! only, never business acceptance; `max_tokens`, `refusal`, wire errors, and
//! cancellation are never folded into success.

use std::collections::HashMap;

use super::transport::PromptSettlement;

/// Hard bound on each prompt text bucket (and its duplicate native-ID text).
pub const MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;

/// Bound native identity overhead as well as duplicate text bytes.
const MAX_MESSAGE_IDS: usize = 128;

/// Classifications the daemon maps onto its terminal outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementOutcome {
    /// Protocol settlement reached. `final_text` is `None` when no verified
    /// message backs the turn; the shared scheduler then downgrades the
    /// result instead of guessing.
    Completed {
        final_text: Option<String>,
    },
    Failed {
        reason_code: String,
    },
}

/// Text aggregated for the prompt currently being settled.
#[derive(Default)]
struct TurnText {
    /// The adapter's local turn identity. `None` after the settlement sealed
    /// the bucket: text that arrives afterwards belongs to no open prompt.
    turn_id: Option<String>,
    text: String,
    /// A bounded observation must never become an authoritative full result.
    limited: bool,
}

/// Bounded aggregation of the current prompt's assistant text.
#[derive(Default)]
pub struct MessageAggregation {
    /// Text grouped by native message id, in arrival order within each id.
    messages: HashMap<String, String>,
    /// Ordered text of the open prompt, across message ids and for chunks
    /// that carry no id at all.
    turn: TurnText,
}

fn valid_identity(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.contains('\0')
}

/// Append a bounded prefix on a UTF-8 boundary, returning whether text was lost.
fn append_bounded(target: &mut String, text: &str) -> bool {
    let mut end = text
        .len()
        .min(MAX_MESSAGE_BYTES.saturating_sub(target.len()));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&text[..end]);
    end < text.len()
}

impl MessageAggregation {
    /// Open a fresh prompt bucket. The caller owns the local turn identity and
    /// passes the same one for every update of that prompt.
    pub fn begin_turn(&mut self, turn_id: &str) {
        self.messages.clear();
        self.turn = TurnText {
            turn_id: Some(turn_id.to_owned()),
            ..TurnText::default()
        };
    }

    /// Record one assistant text update.
    ///
    /// A native `message_id` groups the text by its native identity; the text
    /// also joins the prompt bucket while that prompt is open, which is what
    /// makes the standard chunk-only settlement readable. Malformed native
    /// identities are refused; legal JSON body text, including NUL, is retained.
    pub fn observe(&mut self, turn_id: &str, message_id: Option<&str>, text: &str) {
        if self.turn.turn_id.as_deref() != Some(turn_id)
            || self.turn.limited
            || text.is_empty()
        {
            return;
        }
        let message_id = message_id.filter(|id| valid_identity(id, 512));
        if message_id.is_some_and(|id| !self.messages.contains_key(id))
            && self.messages.len() >= MAX_MESSAGE_IDS
        {
            self.turn.limited = true;
            return;
        }
        let before = self.turn.text.len();
        self.turn.limited = append_bounded(&mut self.turn.text, text);
        // Copy only the admitted prefix. Across all IDs this retains at most
        // MAX_MESSAGE_BYTES, rather than that bound for every native message.
        let admitted = self.turn.text.len() - before;
        if let Some(id) = message_id.filter(|_| admitted > 0) {
            self.messages
                .entry(id.to_owned())
                .or_default()
                .push_str(&text[..admitted]);
        }
    }

    /// Close the prompt bucket: later frames cannot extend the settled turn.
    pub fn seal_turn(&mut self, turn_id: &str) {
        if self.turn.turn_id.as_deref() == Some(turn_id) {
            self.turn = TurnText::default();
            self.messages.clear();
        }
    }

    pub fn message(&self, message_id: &str) -> Option<&str> {
        self.messages.get(message_id).map(String::as_str)
    }

    /// The open prompt's ordered text, when `turn_id` names that prompt.
    pub fn turn_text(&self, turn_id: &str) -> Option<&str> {
        (self.turn.turn_id.as_deref() == Some(turn_id) && !self.turn.text.is_empty())
            .then_some(self.turn.text.as_str())
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

fn stop_reason_code(stop_reason: &str) -> String {
    match stop_reason {
        "max_tokens" => "MAX_TOKENS".to_owned(),
        "refusal" => "REFUSAL".to_owned(),
        "cancelled" => "CANCELLED".to_owned(),
        other => format!(
            "UNKNOWN_STOP_REASON_{}",
            other
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(48)
                .collect::<String>()
        ),
    }
}

/// Fold one settlement against the aggregated messages.
///
/// A `messageId` on the response is the native identity to prefer; the
/// prompt-scoped text is the standard-contract fallback when the settlement
/// names no message or the named message never contributed text.
pub fn fold_settlement(
    settlement: &PromptSettlement,
    messages: &MessageAggregation,
    turn_id: &str,
) -> SettlementOutcome {
    if settlement.stop_reason != "end_turn" {
        return SettlementOutcome::Failed {
            reason_code: stop_reason_code(&settlement.stop_reason),
        };
    }
    if messages.turn.turn_id.as_deref() != Some(turn_id) {
        return SettlementOutcome::Completed { final_text: None };
    }
    if messages.turn.limited {
        return SettlementOutcome::Failed {
            reason_code: "ACP_RESULT_TEXT_LIMIT".into(),
        };
    }
    let final_text = settlement
        .message_id
        .as_deref()
        .and_then(|id| messages.message(id))
        .map(str::to_owned)
        .or_else(|| messages.turn_text(turn_id).map(str::to_owned));
    SettlementOutcome::Completed { final_text }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: &str = "dsh-prompt-1";

    fn settlement(stop_reason: &str, message_id: Option<&str>) -> PromptSettlement {
        PromptSettlement {
            stop_reason: stop_reason.to_owned(),
            message_id: message_id.map(str::to_owned),
        }
    }

    fn aggregated() -> MessageAggregation {
        let mut messages = MessageAggregation::default();
        messages.begin_turn(TURN);
        messages
    }

    #[test]
    fn repair1_overflow_is_an_explicit_limit_not_a_completed_prefix() {
        let mut messages = aggregated();
        messages.observe(TURN, Some("first"), &"x".repeat(MAX_MESSAGE_BYTES));
        messages.observe(TURN, Some("last"), "done");
        assert_eq!(
            fold_settlement(&settlement("end_turn", None), &messages, TURN),
            SettlementOutcome::Failed {
                reason_code: "ACP_RESULT_TEXT_LIMIT".into()
            }
        );
        // Native stop reasons still take precedence over local text limits.
        assert_eq!(
            fold_settlement(&settlement("cancelled", None), &messages, TURN),
            SettlementOutcome::Failed {
                reason_code: "CANCELLED".into()
            }
        );
    }

    #[test]
    fn repair1_native_storage_is_prompt_scoped_and_refuses_out_of_scope_writes() {
        let mut messages = MessageAggregation::default();
        messages.observe("unopened", Some("orphan"), "orphan");
        assert!(messages.is_empty(), "unopened prompt retained native text");
        for i in 0..8 {
            let turn = format!("turn-{i}");
            let id = format!("native-{i}");
            messages.begin_turn(&turn);
            messages.observe("stale-turn", Some("stale"), "late");
            assert_eq!(messages.message("stale"), None);
            messages.observe(&turn, Some(&id), "named ");
            messages.observe(&turn, None, "tail");
            assert_eq!(
                fold_settlement(&settlement("end_turn", Some(&id)), &messages, &turn),
                SettlementOutcome::Completed {
                    final_text: Some("named ".into())
                }
            );
            assert_eq!(
                fold_settlement(&settlement("end_turn", None), &messages, &turn),
                SettlementOutcome::Completed {
                    final_text: Some("named tail".into())
                }
            );
            messages.seal_turn("wrong-turn");
            assert_eq!(messages.len(), 1);
            messages.seal_turn(&turn);
            assert!(messages.is_empty(), "settled native storage retained");
            messages.observe(&turn, Some(&id), "late");
            assert!(messages.is_empty(), "late frame repopulated sealed storage");
        }
        messages.begin_turn("abandoned");
        messages.observe("abandoned", Some("old"), "old");
        messages.begin_turn("new");
        assert!(
            messages.is_empty(),
            "new prompt inherited abandoned native storage"
        );
    }

    #[test]
    fn repair1_native_storage_has_an_aggregate_and_identity_bound() {
        let mut messages = aggregated();
        for i in 0..256 {
            messages.observe(TURN, Some(&format!("id-{i}")), &"x".repeat(16 * 1024));
        }
        let bytes: usize = messages.messages.values().map(String::len).sum();
        assert!(
            bytes <= MAX_MESSAGE_BYTES,
            "native text retained {bytes} bytes"
        );
        messages.begin_turn("tiny");
        for i in 0..1024 {
            messages.observe("tiny", Some(&format!("id-{i}")), "x");
        }
        assert!(
            messages.len() <= 128,
            "native identity count is not bounded"
        );
        assert!(matches!(
            fold_settlement(&settlement("end_turn", None), &messages, "tiny"),
            SettlementOutcome::Failed { .. }
        ));
    }

    #[test]
    fn repair4_nul_body_is_complete_in_prompt_and_native_scopes() {
        let expected = "prefix important\0suffix";
        let mut messages = aggregated();
        messages.observe(TURN, Some("native"), "prefix ");
        messages.observe(TURN, Some("native"), "important\0suffix");
        assert_eq!(messages.turn_text(TURN), Some(expected));
        assert_eq!(messages.message("native"), Some(expected));
        for message_id in [None, Some("native")] {
            assert_eq!(
                fold_settlement(&settlement("end_turn", message_id), &messages, TURN),
                SettlementOutcome::Completed {
                    final_text: Some(expected.into())
                }
            );
        }
        messages.seal_turn(TURN);
        messages.observe(TURN, Some("late"), "late\0text");
        assert_eq!(messages.message("late"), None);
        messages.begin_turn("next");
        messages.observe("next", None, expected);
        assert_eq!(
            fold_settlement(&settlement("end_turn", None), &messages, "next"),
            SettlementOutcome::Completed {
                final_text: Some(expected.into())
            }
        );
    }

    #[test]
    fn end_turn_returns_only_the_verified_message() {
        let mut messages = aggregated();
        messages.observe(TURN, Some("progress-1"), "partial progress");
        messages.observe(TURN, Some("message-1"), "final ");
        messages.observe(TURN, Some("message-1"), "answer");
        let outcome = fold_settlement(&settlement("end_turn", Some("message-1")), &messages, TURN);
        assert_eq!(
            outcome,
            SettlementOutcome::Completed {
                final_text: Some("final answer".into())
            }
        );
    }

    #[test]
    fn standard_chunk_only_settlement_folds_the_prompt_text() {
        // The upstream standard flow: ordered `agent_message_chunk` deltas and
        // a settlement that carries nothing but `stopReason`.
        let mut messages = aggregated();
        messages.observe(TURN, Some("message-1"), "build ");
        messages.observe(TURN, Some("message-1"), "answer");
        assert_eq!(
            fold_settlement(&settlement("end_turn", None), &messages, TURN),
            SettlementOutcome::Completed {
                final_text: Some("build answer".into())
            }
        );
    }

    #[test]
    fn chunk_without_message_identity_still_folds_in_prompt_order() {
        let mut messages = aggregated();
        messages.observe(TURN, None, "no ");
        messages.observe(TURN, None, "identity");
        assert_eq!(
            messages.turn_text(TURN),
            Some("no identity"),
            "the prompt bucket is the local correlation"
        );
        assert_eq!(
            fold_settlement(&settlement("end_turn", None), &messages, TURN),
            SettlementOutcome::Completed {
                final_text: Some("no identity".into())
            }
        );
    }

    #[test]
    fn settlement_identity_wins_over_prompt_scope() {
        let mut messages = aggregated();
        messages.observe(TURN, Some("message-1"), "first ");
        messages.observe(TURN, Some("message-2"), "second");
        assert_eq!(
            fold_settlement(&settlement("end_turn", Some("message-2")), &messages, TURN),
            SettlementOutcome::Completed {
                final_text: Some("second".into())
            }
        );
        // A settlement naming a message that never contributed text falls back
        // to the prompt's own ordered output instead of failing the turn.
        assert_eq!(
            fold_settlement(
                &settlement("end_turn", Some("unknown-id")),
                &messages,
                TURN
            ),
            SettlementOutcome::Completed {
                final_text: Some("first second".into())
            }
        );
    }

    #[test]
    fn sealed_turn_refuses_late_text_and_another_turn_cannot_read_it() {
        let mut messages = aggregated();
        messages.observe(TURN, None, "settled");
        messages.seal_turn(TURN);
        messages.observe(TURN, None, "late frame");
        assert_eq!(messages.turn_text(TURN), None);
        assert_eq!(
            fold_settlement(&settlement("end_turn", None), &messages, TURN),
            SettlementOutcome::Completed { final_text: None }
        );
        // A fresh prompt opens its own bucket; the previous turn is gone.
        messages.begin_turn("dsh-prompt-2");
        messages.observe("dsh-prompt-2", None, "next turn");
        assert_eq!(
            fold_settlement(&settlement("end_turn", None), &messages, "dsh-prompt-2"),
            SettlementOutcome::Completed {
                final_text: Some("next turn".into())
            }
        );
    }

    #[test]
    fn missing_message_identity_never_fakes_a_result() {
        let messages = aggregated();
        assert_eq!(
            fold_settlement(&settlement("end_turn", None), &messages, TURN),
            SettlementOutcome::Completed { final_text: None }
        );
        assert_eq!(
            fold_settlement(
                &settlement("end_turn", Some("unknown-id")),
                &messages,
                TURN
            ),
            SettlementOutcome::Completed { final_text: None }
        );
    }

    #[test]
    fn non_end_turn_stop_reasons_keep_distinct_codes() {
        let messages = aggregated();
        assert_eq!(
            fold_settlement(&settlement("max_tokens", Some("m")), &messages, TURN),
            SettlementOutcome::Failed {
                reason_code: "MAX_TOKENS".into()
            }
        );
        assert_eq!(
            fold_settlement(&settlement("refusal", None), &messages, TURN),
            SettlementOutcome::Failed {
                reason_code: "REFUSAL".into()
            }
        );
        assert_eq!(
            fold_settlement(&settlement("cancelled", None), &messages, TURN),
            SettlementOutcome::Failed {
                reason_code: "CANCELLED".into()
            }
        );
        assert_eq!(
            fold_settlement(&settlement("weird reason!", None), &messages, TURN),
            SettlementOutcome::Failed {
                reason_code: "UNKNOWN_STOP_REASON_weirdreason".into()
            }
        );
    }

    #[test]
    fn aggregation_is_bounded_and_rejects_bad_ids() {
        let mut messages = aggregated();
        messages.observe(TURN, Some(""), "x");
        messages.observe(TURN, Some(&"i".repeat(513)), "x");
        messages.observe(TURN, Some("ok\0id"), "x");
        // A malformed id never creates a native message entry, but the text
        // still belongs to the open prompt, which is the local correlation the
        // standard settlement reads.
        assert!(messages.is_empty());
        assert_eq!(messages.turn_text(TURN), Some("xxx"));

        // Legal NUL-bearing body text is preserved in both scopes.
        messages.begin_turn("dsh-prompt-2");
        messages.observe("dsh-prompt-2", Some("m"), "bad\0text");
        assert_eq!(messages.message("m"), Some("bad\0text"));
        assert_eq!(messages.turn_text("dsh-prompt-2"), Some("bad\0text"));

        messages.begin_turn(TURN);
        messages.observe(TURN, Some("m"), &"y".repeat(MAX_MESSAGE_BYTES + 4096));
        assert!(messages.message("m").unwrap().len() <= MAX_MESSAGE_BYTES + 3);
        assert!(messages.message("m").unwrap().chars().all(|c| c == 'y'));
        assert!(messages.turn_text(TURN).unwrap().len() <= MAX_MESSAGE_BYTES + 3);
    }
}
