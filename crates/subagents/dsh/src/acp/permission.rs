//! DSH ACP permission offers and single-shot responses.
//!
//! The public allow/deny decision can only select one option the server
//! actually offered (`allow_once` for allow, `reject_once`/`deny` for deny).
//! Durable grants like `allow_always` are never constructed. Two observed
//! option dialects are accepted: the upstream standard shape
//! (`options: [{optionId, name, kind}]` with `toolCall: {toolCallId}`) and
//! the S01-pinned fixture shape (`options: [{kind}]` with a flat
//! `toolCallId`); the response echoes an identifier the server itself
//! offered, so no value is invented.

use serde_json::{json, Value};
use std::collections::HashMap;

use external_contract::WireId;

pub const ALLOW_ONCE_KINDS: &[&str] = &["allow_once"];
pub const DENY_ONCE_KINDS: &[&str] = &["reject_once", "deny", "reject"];

const MAX_OPTIONS: usize = 16;
const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_OFFERED_REQUESTS: usize = 128;

/// One option the server offered for a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub option_id: Option<String>,
    pub kind: String,
}

/// A parsed `session/request_permission` offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOffer {
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub tool_call_id: Option<String>,
    /// Human-facing tool title carried by the request, if any.
    pub tool_title: Option<String>,
    pub options: Vec<PermissionOption>,
}

impl PermissionOffer {
    fn option_for(&self, kinds: &[&str]) -> Option<&PermissionOption> {
        self.options
            .iter()
            .find(|option| kinds.contains(&option.kind.as_str()))
    }

    /// Build the JSON-RPC response result for one public decision. The
    /// selected option identifier is only ever an echo of what the server
    /// offered (`optionId` when present, otherwise the offered `kind`).
    pub fn select(&self, decision: &str) -> Option<Value> {
        let kinds = match decision {
            "allow" => ALLOW_ONCE_KINDS,
            "deny" => DENY_ONCE_KINDS,
            _ => return None,
        };
        let option = self.option_for(kinds)?;
        let identifier = option
            .option_id
            .clone()
            .unwrap_or_else(|| option.kind.clone());
        Some(json!({
            "outcome": {
                "outcome": "selected",
                "optionId": identifier,
            }
        }))
    }

    /// Best-effort tool label for projections, bounded like the public view.
    pub fn tool_label(&self) -> Option<&str> {
        self.tool_title.as_deref().map(|title| {
            let mut end = title.len().min(MAX_TOOL_NAME_BYTES);
            while end < title.len() && !title.is_char_boundary(end) {
                end += 1;
            }
            &title[..end]
        })
    }
}

/// Parse both observed offer dialects. Returns `None` when the frame does not
/// carry a bounded permission offer (the caller keeps it non-respondable).
pub fn parse_offer(params: &Value) -> Option<PermissionOffer> {
    let bounded = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty() && text.len() <= 512 && !text.contains('\0'))
            .map(str::to_owned)
    };
    let tool_call_id = bounded(params.get("toolCallId").or_else(|| {
        params
            .get("toolCall")
            .and_then(|call| call.get("toolCallId"))
    }));
    let options = params
        .get("options")
        .and_then(Value::as_array)?
        .iter()
        .map(|option| PermissionOption {
            option_id: option
                .get("optionId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            kind: option
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
        .filter(|option| !option.kind.is_empty())
        .take(MAX_OPTIONS)
        .collect::<Vec<_>>();
    if options.is_empty() {
        return None;
    }
    Some(PermissionOffer {
        request_id: bounded(params.get("requestId")),
        session_id: bounded(params.get("sessionId")),
        tool_call_id,
        tool_title: bounded(params.get("title")),
        options,
    })
}

/// Normalize an offer into the internal canonical permission-request params
/// consumed by the shared request projection. `correlated_tool_name` comes
/// from a previously observed tool update with the same `toolCallId`; when no
/// context exists the projection must show an unknown tool instead of
/// guessing a command (X06).
pub fn normalize_permission_request(
    offer: &PermissionOffer,
    correlated_tool_name: Option<&str>,
) -> Value {
    let tool_name = offer.tool_label().map(str::to_owned).or_else(|| {
        correlated_tool_name
            .filter(|name| !name.is_empty())
            .map(|name| name.chars().take(MAX_TOOL_NAME_BYTES).collect::<String>())
    });
    let options = offer
        .options
        .iter()
        .map(|option| {
            json!({
                "id": option.option_id.clone().unwrap_or_else(|| option.kind.clone()),
                "kind": option.kind,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "toolCallId": offer.tool_call_id,
        "toolName": tool_name.unwrap_or_else(|| "unknown".into()),
        "options": options,
        "origin": "dsh_acp",
    })
}

/// Bounded cache of outstanding offers keyed by the serialized JSON-RPC id of
/// the server request. Mirrors the ZCode offered-response cache discipline.
#[derive(Default)]
pub struct OfferCache {
    offers: HashMap<String, PermissionOffer>,
}

impl OfferCache {
    pub fn observe(&mut self, key: String, offer: PermissionOffer) {
        if self.offers.len() >= MAX_OFFERED_REQUESTS && !self.offers.contains_key(&key) {
            return;
        }
        self.offers.insert(key, offer);
    }

    pub fn take(&mut self, key: &str) -> Option<PermissionOffer> {
        self.offers.remove(key)
    }

    pub fn clear(&mut self) {
        self.offers.clear();
    }
}

/// Serialize a server request id the way the store persists correlations.
pub fn correlation_key(id: &WireId) -> String {
    serde_json::to_string(id).expect("wire id serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream_offer() -> Value {
        json!({
            "sessionId": "s1",
            "toolCall": {"toolCallId": "call-7"},
            "options": [
                {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"}
            ]
        })
    }

    fn s01_fixture_offer() -> Value {
        json!({
            "requestId": "permission-1",
            "toolCallId": "tool-1",
            "options": [{"kind": "allow_once"}, {"kind": "reject_once"}]
        })
    }

    #[test]
    fn both_observed_dialects_parse_with_bounded_ids() {
        let upstream = parse_offer(&upstream_offer()).unwrap();
        assert_eq!(upstream.tool_call_id.as_deref(), Some("call-7"));
        assert_eq!(upstream.options.len(), 2);
        let fixture = parse_offer(&s01_fixture_offer()).unwrap();
        assert_eq!(fixture.request_id.as_deref(), Some("permission-1"));
        assert_eq!(fixture.tool_call_id.as_deref(), Some("tool-1"));
        assert!(parse_offer(&json!({"options": []})).is_none());
        assert!(parse_offer(&json!({})).is_none());
    }

    #[test]
    fn selection_only_echoes_offered_single_shot_options() {
        let upstream = parse_offer(&upstream_offer()).unwrap();
        assert_eq!(
            upstream.select("allow").unwrap()["outcome"]["optionId"],
            "allow-once"
        );
        assert_eq!(
            upstream.select("deny").unwrap()["outcome"]["optionId"],
            "reject-once"
        );
        let fixture = parse_offer(&s01_fixture_offer()).unwrap();
        assert_eq!(
            fixture.select("allow").unwrap()["outcome"]["optionId"],
            "allow_once"
        );
        assert!(parse_offer(&s01_fixture_offer())
            .unwrap()
            .select("answer")
            .is_none());
        // allow_always is never selectable even if a server offers it.
        let durable = parse_offer(&json!({
            "toolCallId": "t",
            "options": [{"optionId": "allow-always", "kind": "allow_always"}]
        }))
        .unwrap();
        assert!(durable.select("allow").is_none());
        assert!(durable.select("deny").is_none());
    }

    #[test]
    fn normalization_marks_uncorrelated_tools_unknown() {
        let offer = parse_offer(&upstream_offer()).unwrap();
        let normalized = normalize_permission_request(&offer, Some("Edit"));
        assert_eq!(normalized["toolName"], "Edit");
        assert_eq!(normalized["toolCallId"], "call-7");
        assert_eq!(normalized["origin"], "dsh_acp");
        let uncorrelated = parse_offer(&s01_fixture_offer()).unwrap();
        let normalized = normalize_permission_request(&uncorrelated, None);
        assert_eq!(normalized["toolName"], "unknown");
    }

    #[test]
    fn offer_cache_is_bounded_and_take_is_once() {
        let mut cache = OfferCache::default();
        let offer = parse_offer(&s01_fixture_offer()).unwrap();
        let key = correlation_key(&WireId::String("srv-1".into()));
        cache.observe(key.clone(), offer);
        assert!(cache.take(&key).is_some());
        assert!(cache.take(&key).is_none());
        for index in 0i64..(MAX_OFFERED_REQUESTS as i64 + 8) {
            cache.observe(
                correlation_key(&WireId::Integer(index)),
                parse_offer(&s01_fixture_offer()).unwrap(),
            );
        }
        assert!(cache
            .take(&correlation_key(&WireId::Integer(200)))
            .is_none());
        cache.clear();
        assert!(cache.take(&correlation_key(&WireId::Integer(0))).is_none());
    }
}
