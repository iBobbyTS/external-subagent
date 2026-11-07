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

/// Hard bound on one aggregated message before it reaches the result path.
pub const MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;

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

/// Append `text`, keeping the aggregation byte-bounded on a char boundary.
fn append_bounded(target: &mut String, text: &str) {
    if target.len() >= MAX_MESSAGE_BYTES {
        return;
    }
    target.push_str(text);
    if target.len() > MAX_MESSAGE_BYTES {
        let mut split = target.len() - MAX_MESSAGE_BYTES;
        while split < target.len() && !target.is_char_boundary(split) {
            split += 1;
        }
        *target = target[split..].to_owned();
    }
}

impl MessageAggregation {
    /// Open a fresh prompt bucket. The caller owns the local turn identity and
    /// passes the same one for every update of that prompt.
    pub fn begin_turn(&mut self, turn_id: &str) {
        self.turn = TurnText {
            turn_id: Some(turn_id.to_owned()),
            text: String::new(),
        };
    }

    /// Record one assistant text update.
    ///
    /// A native `message_id` groups the text by its native identity; the text
    /// also joins the prompt bucket while that prompt is open, which is what
    /// makes the standard chunk-only settlement readable. Malformed identities
    /// and NUL-bearing text are refused rather than aggregated.
    pub fn observe(&mut self, turn_id: &str, message_id: Option<&str>, text: &str) {
        if text.contains('\0') {
            return;
        }
        if let Some(message_id) = message_id {
            if valid_identity(message_id, 512) && !text.is_empty() {
                append_bounded(self.messages.entry(message_id.to_owned()).or_default(), text);
            }
        }
        if self.turn.turn_id.as_deref() == Some(turn_id) && !text.is_empty() {
            append_bounded(&mut self.turn.text, text);
        }
    }

    /// Close the prompt bucket: later frames cannot extend the settled turn.
    pub fn seal_turn(&mut self, turn_id: &str) {
        if self.turn.turn_id.as_deref() == Some(turn_id) {
            self.turn = TurnText::default();
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

        // NUL-bearing text is refused in both scopes.
        messages.begin_turn("dsh-prompt-2");
        messages.observe("dsh-prompt-2", Some("m"), "bad\0text");
        assert_eq!(messages.message("m"), None);
        assert_eq!(messages.turn_text("dsh-prompt-2"), None);

        messages.begin_turn(TURN);
        messages.observe(TURN, Some("m"), &"y".repeat(MAX_MESSAGE_BYTES + 4096));
        assert!(messages.message("m").unwrap().len() <= MAX_MESSAGE_BYTES + 3);
        assert!(messages.message("m").unwrap().chars().all(|c| c == 'y'));
        assert!(messages.turn_text(TURN).unwrap().len() <= MAX_MESSAGE_BYTES + 3);
    }
}
