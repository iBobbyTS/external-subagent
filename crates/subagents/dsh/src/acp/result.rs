//! Prompt-settlement folding into the shared task result.
//!
//! The final text is the committed assistant message identified by the
//! settlement's verified `messageId`, with its text blocks preserved in
//! order. Progress, thought, and tool updates are never merged into the
//! result, and an upstream version that does not carry a recognizable
//! `messageId` cannot fake this guarantee (P06). Each stop reason keeps its
//! own meaning: `end_turn` is protocol completion only, never business
//! acceptance; `max_tokens`, `refusal`, wire errors, and cancellation are
//! never folded into success.

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

/// Bounded aggregation of committed assistant messages by id.
#[derive(Default)]
pub struct MessageAggregation {
    messages: HashMap<String, String>,
}

impl MessageAggregation {
    /// Record one committed message. Appends are order-preserving within a
    /// message; the total stays byte-bounded.
    pub fn observe_committed(&mut self, message_id: &str, text: &str) {
        if message_id.is_empty()
            || message_id.len() > 512
            || message_id.contains('\0')
            || text.contains('\0')
        {
            return;
        }
        let entry = self.messages.entry(message_id.to_owned()).or_default();
        if entry.len() >= MAX_MESSAGE_BYTES {
            return;
        }
        entry.push_str(text);
        if entry.len() > MAX_MESSAGE_BYTES {
            let mut split = entry.len() - MAX_MESSAGE_BYTES;
            while split < entry.len() && !entry.is_char_boundary(split) {
                split += 1;
            }
            *entry = entry[split..].to_owned();
        }
    }

    pub fn message(&self, message_id: &str) -> Option<&str> {
        self.messages.get(message_id).map(String::as_str)
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
pub fn fold_settlement(
    settlement: &PromptSettlement,
    messages: &MessageAggregation,
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
        .map(str::to_owned);
    SettlementOutcome::Completed { final_text }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settlement(stop_reason: &str, message_id: Option<&str>) -> PromptSettlement {
        PromptSettlement {
            stop_reason: stop_reason.to_owned(),
            message_id: message_id.map(str::to_owned),
        }
    }

    #[test]
    fn end_turn_returns_only_the_verified_message() {
        let mut messages = MessageAggregation::default();
        messages.observe_committed("progress-1", "partial progress");
        messages.observe_committed("message-1", "final ");
        messages.observe_committed("message-1", "answer");
        let outcome = fold_settlement(&settlement("end_turn", Some("message-1")), &messages);
        assert_eq!(
            outcome,
            SettlementOutcome::Completed {
                final_text: Some("final answer".into())
            }
        );
    }

    #[test]
    fn missing_message_identity_never_fakes_a_result() {
        let mut messages = MessageAggregation::default();
        messages.observe_committed("message-1", "text");
        assert_eq!(
            fold_settlement(&settlement("end_turn", None), &messages),
            SettlementOutcome::Completed { final_text: None }
        );
        assert_eq!(
            fold_settlement(&settlement("end_turn", Some("unknown-id")), &messages),
            SettlementOutcome::Completed { final_text: None }
        );
    }

    #[test]
    fn non_end_turn_stop_reasons_keep_distinct_codes() {
        let messages = MessageAggregation::default();
        assert_eq!(
            fold_settlement(&settlement("max_tokens", Some("m")), &messages),
            SettlementOutcome::Failed {
                reason_code: "MAX_TOKENS".into()
            }
        );
        assert_eq!(
            fold_settlement(&settlement("refusal", None), &messages),
            SettlementOutcome::Failed {
                reason_code: "REFUSAL".into()
            }
        );
        assert_eq!(
            fold_settlement(&settlement("cancelled", None), &messages),
            SettlementOutcome::Failed {
                reason_code: "CANCELLED".into()
            }
        );
        assert_eq!(
            fold_settlement(&settlement("weird reason!", None), &messages),
            SettlementOutcome::Failed {
                reason_code: "UNKNOWN_STOP_REASON_weirdreason".into()
            }
        );
    }

    #[test]
    fn aggregation_is_bounded_and_rejects_bad_ids() {
        let mut messages = MessageAggregation::default();
        messages.observe_committed("", "x");
        messages.observe_committed(&"i".repeat(513), "x");
        messages.observe_committed("ok\0id", "x");
        assert!(messages.is_empty());
        messages.observe_committed("m", &"y".repeat(MAX_MESSAGE_BYTES + 4096));
        assert!(messages.message("m").unwrap().len() <= MAX_MESSAGE_BYTES + 3);
        assert!(messages.message("m").unwrap().chars().all(|c| c == 'y'));
    }
}
