//! Codex `item/*`, `turn/*`, `error`, and MCP-startup notification
//! folding: pure extraction of the projection inputs and the bounded
//! aggregation helpers (item text, turn-failure detail, MCP diagnostic
//! tail, retired turns) the daemon applies to its shared state. Nothing
//! here holds daemon state, builds a wire payload or emits; the daemon
//! owns attribution against the live thread and turn and publishes typed
//! facts.

use std::collections::HashMap;

/// Hard bound on one aggregated agent-message item.
const MAX_ITEM_TEXT_BYTES: usize = 512 * 1024;
/// Hard bound on the number of tracked agent-message items.
const MAX_TRACKED_ITEMS: usize = 128;
/// Hard bound (Unicode chars) on the ES-composed turn-failure label carried by
/// diagnostics and by the boundary payload's `reason_code` fallback. The
/// native error object itself is never bounded.
const MAX_TURN_FAILURE_LABEL_CHARS: usize = 512;
/// Hard bound on the retained MCP startup-failure diagnostic tail.
const MAX_MCP_DIAGNOSTIC_BYTES: usize = 2 * 1024;
/// Hard bound on the retired-turn memory.
const MAX_RETIRED_TURNS: usize = 64;
/// Hard bound on one counted tool item id, mirroring the daemon's activity id
/// bound: an over-long id cannot become a stable dedupe identity.
const MAX_TOOL_ITEM_ID_BYTES: usize = 512;
/// Hard bound on the per-turn first-seen tool-item memory.
const MAX_TRACKED_TOOL_ITEMS: usize = 4096;

/// The Codex item types the adapter projects as tool calls. The whitelist is
/// adapter-owned: content items (agentMessage, reasoning, ...), mode switches,
/// and output-side records stay excluded.
pub const TOOL_ITEM_TYPES: [&str; 9] = [
    "commandExecution",
    "fileChange",
    "mcpToolCall",
    "dynamicToolCall",
    "webSearch",
    "collabAgentToolCall",
    "imageGeneration",
    "imageView",
    "sleep",
];

/// Whether an item type is on the adapter's tool whitelist.
pub fn is_tool_item(item_type: &str) -> bool {
    TOOL_ITEM_TYPES.contains(&item_type)
}

/// The counted tool item id of an `item/started`/`item/completed` frame: the
/// item type must be whitelisted and the id present, bounded, and NUL-free.
/// Any other item is not a count input.
pub fn tool_item_id(params: &serde_json::Value) -> Option<&str> {
    let item = params.get("item").unwrap_or(&serde_json::Value::Null);
    if !item
        .get("type")
        .and_then(|value| value.as_str())
        .is_some_and(is_tool_item)
    {
        return None;
    }
    item.get("id")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty() && id.len() <= MAX_TOOL_ITEM_ID_BYTES && !id.contains('\0'))
}

/// Bounded first-seen dedupe for tool item ids within one turn: a new id is
/// recorded and returns `true`; an already-seen id returns `false` without
/// changing the ring. At capacity the oldest entry is evicted first (the
/// [`retire_turn`] style), so a pathological single-turn stream trades the
/// oldest dedupe memory for bounded state.
pub fn observe_tool_item(seen: &mut Vec<String>, id: &str) -> bool {
    if seen.iter().any(|existing| existing == id) {
        return false;
    }
    if seen.len() >= MAX_TRACKED_TOOL_ITEMS {
        seen.remove(0);
    }
    seen.push(id.to_owned());
    true
}

/// The public identity and status of one whitelisted tool item. Returned only
/// for an `item/started` or `item/completed` frame whose item type is on the
/// adapter whitelist and whose id is a bounded, NUL-free identity.
pub struct ToolItem<'a> {
    pub id: &'a str,
    pub item_type: &'a str,
    /// Native tool name for named families, otherwise the public item type.
    pub name: &'a str,
    /// Native terminal status of the item, when the protocol type declares it.
    pub status: Option<&'a str>,
    /// Public arguments of the call, projected per item type. Never includes
    /// diffs, outputs, raw reasoning or encrypted content.
    pub arguments: Option<serde_json::Value>,
    /// The item's native error/failure object, when the completed item carries one.
    pub error: Option<serde_json::Value>,
}

/// Extract the tool item of an `item/started` or `item/completed` frame.
pub fn tool_item(params: &serde_json::Value) -> Option<ToolItem<'_>> {
    let item = params.get("item").unwrap_or(&serde_json::Value::Null);
    let id = tool_item_id(params)?;
    let item_type = item.get("type").and_then(|value| value.as_str())?;
    Some(ToolItem {
        id,
        item_type,
        name: match item_type {
            "mcpToolCall" | "dynamicToolCall" | "collabAgentToolCall" => item
                .get("tool")
                .and_then(|value| value.as_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(item_type),
            _ => item_type,
        },
        status: item.get("status").and_then(|value| value.as_str()),
        arguments: tool_arguments(item, item_type),
        error: (if item_type == "imageGeneration" {
            item.get("failure").filter(|value| !value.is_null())
        } else {
            None
        })
        .or_else(|| item.get("error").filter(|value| !value.is_null()))
        .cloned(),
    })
}

/// Project the public arguments of one tool item type. Only the fields the
/// public app-server item declares as call inputs are retained; diffs, command
/// output, results and opaque extension payloads never become arguments.
fn tool_arguments(item: &serde_json::Value, item_type: &str) -> Option<serde_json::Value> {
    let mut arguments = serde_json::Map::new();
    match item_type {
        "commandExecution" => {
            copy(item, "command", &mut arguments);
            copy(item, "cwd", &mut arguments);
        }
        "fileChange" => {
            if let Some(changes) = item.get("changes").and_then(|value| value.as_array()) {
                let changes: Vec<serde_json::Value> = changes
                    .iter()
                    .filter_map(|change| {
                        let path = change.get("path")?;
                        let mut projected = serde_json::Map::new();
                        projected.insert("path".into(), path.clone());
                        if let Some(kind) = change.get("kind") {
                            projected.insert("kind".into(), kind.clone());
                        }
                        Some(serde_json::Value::Object(projected))
                    })
                    .collect();
                arguments.insert("changes".into(), changes.into());
            }
        }
        "mcpToolCall" => {
            copy(item, "server", &mut arguments);
            copy(item, "tool", &mut arguments);
            copy(item, "arguments", &mut arguments);
        }
        "dynamicToolCall" => {
            copy(item, "namespace", &mut arguments);
            copy(item, "tool", &mut arguments);
            copy(item, "arguments", &mut arguments);
        }
        "webSearch" => {
            copy(item, "query", &mut arguments);
            if let Some(action) = item.get("action").filter(|value| value.is_object()) {
                let mut projected = serde_json::Map::new();
                copy(action, "type", &mut projected);
                match action.get("type").and_then(|value| value.as_str()) {
                    Some("search") => {
                        copy(action, "query", &mut projected);
                        copy(action, "queries", &mut projected);
                    }
                    Some("openPage") => copy(action, "url", &mut projected),
                    Some("findInPage") => {
                        copy(action, "url", &mut projected);
                        copy(action, "pattern", &mut projected);
                    }
                    _ => {}
                }
                arguments.insert("action".into(), projected.into());
            }
        }
        "collabAgentToolCall" => {
            copy(item, "tool", &mut arguments);
            copy(item, "prompt", &mut arguments);
        }
        "imageView" => copy(item, "path", &mut arguments),
        "sleep" => copy(item, "durationMs", &mut arguments),
        _ => {}
    }
    (!arguments.is_empty()).then_some(serde_json::Value::Object(arguments))
}

fn copy(
    item: &serde_json::Value,
    key: &str,
    into: &mut serde_json::Map<String, serde_json::Value>,
) {
    if let Some(value) = item.get(key).filter(|value| !value.is_null()) {
        into.insert(key.to_owned(), value.clone());
    }
}

/// Extract the public summary delta of an `item/reasoning/summaryTextDelta`
/// notification: the delta text, its item identity and summary part index.
pub fn summary_delta(params: &serde_json::Value) -> Option<(&str, Option<&str>, Option<i64>)> {
    let delta = params.get("delta").and_then(|value| value.as_str())?;
    let item_id = params.get("itemId").and_then(|value| value.as_str());
    let part = params.get("summaryIndex").and_then(|value| value.as_i64());
    Some((delta, item_id, part))
}

/// Whether a notification method is turn-scoped: its traffic must carry
/// the active thread id and is only ever projected onto the current turn.
pub fn is_turn_scoped(method: &str) -> bool {
    matches!(
        method,
        "turn/started"
            | "item/started"
            | "item/agentMessage/delta"
            | "item/completed"
            | "item/plan/delta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/summaryPartAdded"
            | "item/reasoning/textDelta"
            | "turn/completed"
            | "error"
    )
}

/// The non-empty turn id a `turn/started` notification must name.
pub fn started_turn_id(params: &serde_json::Value) -> Option<&str> {
    params
        .pointer("/turn/id")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty())
}

/// The streaming delta (`delta`, `itemId`) an `item/agentMessage/delta`
/// notification must carry.
pub fn agent_message_delta(params: &serde_json::Value) -> Option<(&str, &str)> {
    let delta = params.get("delta").and_then(|value| value.as_str());
    let item_id = params.get("itemId").and_then(|value| value.as_str());
    Some((delta?, item_id?))
}

/// The completed agent-message item (`item_id`, optional `text`) an
/// `item/completed` notification must carry. Any other item type is not a
/// projection input.
pub fn completed_agent_message(params: &serde_json::Value) -> Option<(&str, Option<&str>)> {
    let item = params.get("item").unwrap_or(&serde_json::Value::Null);
    if item.get("type").and_then(|value| value.as_str()) != Some("agentMessage") {
        return None;
    }
    let item_id = item.get("id").and_then(|value| value.as_str())?;
    let text = item.get("text").and_then(|value| value.as_str());
    Some((item_id, text))
}

/// A `turn/completed` notification's terminal view of the turn.
pub struct CompletedTurn<'a> {
    pub turn: &'a serde_json::Value,
    pub status: Option<&'a str>,
    pub turn_id: Option<&'a str>,
}

/// Extract the `turn/completed` notification's turn object, status, and
/// id. Extraction never fails; attribution decides projection.
pub fn completed_turn(params: &serde_json::Value) -> CompletedTurn<'_> {
    let turn = params.get("turn").unwrap_or(&serde_json::Value::Null);
    let status = turn.get("status").and_then(|value| value.as_str());
    let turn_id = turn.get("id").and_then(|value| value.as_str());
    CompletedTurn {
        turn,
        status,
        turn_id,
    }
}

/// The final text of a completed turn: the tracked text of the turn's last
/// completed agent message when it exists, otherwise the first
/// agentMessage item the completed turn itself carries.
pub fn final_text(turn: &serde_json::Value, tracked: Option<String>) -> Option<String> {
    if tracked.is_some() {
        return tracked;
    }
    turn.get("items")?
        .as_array()?
        .iter()
        .find(|item| item.get("type").and_then(|value| value.as_str()) == Some("agentMessage"))
        .and_then(|item| item.get("text"))
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

/// The failure of a failed or interrupted turn: the ES-composed label plus the
/// complete native error the turn carried. A recorded turn-scoped error wins
/// over the turn boundary's own error — a later async boundary or natural exit
/// must never overwrite the earlier native cause — and both are retained in
/// full instead of being summarized.
pub fn turn_failure(
    turn: &serde_json::Value,
    status: &str,
    recorded: Option<serde_json::Value>,
) -> (String, Option<serde_json::Value>) {
    let native = recorded.or_else(|| turn.get("error").filter(|value| !value.is_null()).cloned());
    let label = native.as_ref().map_or_else(
        || format!("codex turn ended with status {status}"),
        turn_failure_label,
    );
    (label, native)
}

/// The bounded ES label of one native error (`codexErrorInfo: message` with
/// protocol defaults). It is diagnostic only: the native error object itself
/// is delivered unmodified through the turn boundary payload.
pub fn turn_failure_label(error: &serde_json::Value) -> String {
    let info = error
        .get("codexErrorInfo")
        .map(codex_error_info_label)
        .unwrap_or_else(|| "codex_error".to_owned());
    let message = error
        .get("message")
        .and_then(|value| value.as_str())
        .unwrap_or("codex reported a turn error");
    let label = format!("{info}: {message}");
    label.chars().take(MAX_TURN_FAILURE_LABEL_CHARS).collect()
}

/// The wire name of one `codexErrorInfo`: a unit variant is its own name; an
/// externally tagged struct variant (`{"httpConnectionFailed":
/// {"httpStatusCode": 502}}`) keeps the HTTP status its classification already
/// carries.
fn codex_error_info_label(info: &serde_json::Value) -> String {
    match info {
        serde_json::Value::String(name) => name.clone(),
        serde_json::Value::Object(fields) => {
            let Some((name, body)) = fields.iter().next() else {
                return "codex_error".to_owned();
            };
            match body.get("httpStatusCode").and_then(|value| value.as_u64()) {
                Some(status) => format!("{name} http {status}"),
                None => name.clone(),
            }
        }
        _ => "codex_error".to_owned(),
    }
}

/// The complete native error of a turn-scoped `error` notification: the
/// `error` object exactly as received, with the notification's sibling
/// `willRetry` retained under its native wire name. Never summarized or
/// bounded here; only the shared delivery layer redacts credentials.
pub fn turn_error_native(params: &serde_json::Value) -> Option<serde_json::Value> {
    let error = params.get("error").filter(|value| !value.is_null())?;
    let mut native = error.clone();
    let will_retry = params.get("willRetry").filter(|value| !value.is_null());
    if let (Some(object), Some(will_retry)) = (native.as_object_mut(), will_retry) {
        // The notification-level retry flag travels beside the error. Never
        // shadow a field the server already sent.
        object
            .entry("willRetry")
            .or_insert_with(|| will_retry.clone());
    }
    Some(native)
}

/// A failed MCP server startup (`name`, `error`) with protocol defaults.
/// An unrelated MCP startup failure stays diagnostic: it never fails the
/// turn by itself.
pub fn mcp_startup_failure(params: &serde_json::Value) -> Option<(&str, &str)> {
    if params.get("status").and_then(|value| value.as_str()) != Some("failed") {
        return None;
    }
    let name = params
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let error = params
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("startup failed");
    Some((name, error))
}

/// Append one delta to an agent-message item's tracked text. A new item is
/// refused once the tracking cap is reached; an existing item keeps
/// appending, and the total stays byte-bounded on char boundaries.
pub fn observe_item_text(items: &mut HashMap<String, String>, item_id: &str, text: &str) {
    if !items.contains_key(item_id) && items.len() >= MAX_TRACKED_ITEMS {
        return;
    }
    let entry = items.entry(item_id.to_owned()).or_default();
    let bounded = text.len() + entry.len() <= MAX_ITEM_TEXT_BYTES;
    if bounded {
        entry.push_str(text);
    } else {
        let remaining = MAX_ITEM_TEXT_BYTES.saturating_sub(entry.len());
        if remaining > 0 {
            let mut end = remaining;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            entry.push_str(&text[..end]);
        }
    }
}

/// Record a completed agent-message item's full text. A new item is
/// refused once the tracking cap is reached; an existing item is always
/// refreshed.
pub fn record_completed_item(items: &mut HashMap<String, String>, item_id: &str, text: &str) {
    if items.contains_key(item_id) || items.len() < MAX_TRACKED_ITEMS {
        items.insert(item_id.to_owned(), text.to_owned());
    }
}

/// Append one MCP startup-failure line to the diagnostic tail and keep the
/// tail byte-bounded on char boundaries (the most recent lines survive).
pub fn record_mcp_failure(tail: &mut String, name: &str, error: &str) {
    let line = format!("mcp {name} failed: {error}\n");
    tail.push_str(&line);
    if tail.len() > MAX_MCP_DIAGNOSTIC_BYTES {
        let mut keep = tail.len() - MAX_MCP_DIAGNOSTIC_BYTES;
        while keep < tail.len() && !tail.is_char_boundary(keep) {
            keep += 1;
        }
        tail.drain(..keep);
    }
}

/// Retire one turn: its identity can never become current again. The ring
/// is deduplicated and bounded; the oldest entry is evicted first.
pub fn retire_turn(retired: &mut Vec<String>, turn_id: &str) {
    if !retired.iter().any(|id| id == turn_id) {
        if retired.len() >= MAX_RETIRED_TURNS {
            retired.remove(0);
        }
        retired.push(turn_id.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_scoped_methods_cover_content_tools_and_public_reasoning() {
        for method in [
            "turn/started",
            "item/started",
            "item/agentMessage/delta",
            "item/completed",
            "item/plan/delta",
            "item/reasoning/summaryTextDelta",
            "item/reasoning/summaryPartAdded",
            "item/reasoning/textDelta",
            "turn/completed",
            "error",
        ] {
            assert!(is_turn_scoped(method), "{method}");
        }
        assert!(!is_turn_scoped("mcpServer/startupStatus/updated"));
        assert!(!is_turn_scoped("thread/started"));
        assert!(!is_turn_scoped("account/rateLimits/updated"));
    }

    #[test]
    fn tool_item_whitelist_admits_calls_and_excludes_content() {
        for item_type in TOOL_ITEM_TYPES {
            assert!(is_tool_item(item_type), "{item_type}");
        }
        for item_type in [
            "agentMessage",
            "reasoning",
            "subAgentActivity",
            "functionCallOutput",
            "userMessage",
            "hookPrompt",
            "plan",
            "enteredReviewMode",
            "exitedReviewMode",
            "contextCompaction",
            "unknownItem",
        ] {
            assert!(!is_tool_item(item_type), "{item_type}");
        }
    }

    #[test]
    fn tool_item_id_requires_a_whitelisted_typed_bounded_id() {
        let valid = serde_json::json!({"item": {"type": "commandExecution", "id": "c-1"}});
        assert_eq!(tool_item_id(&valid), Some("c-1"));
        let empty = serde_json::json!({"item": {"type": "fileChange", "id": ""}});
        assert_eq!(tool_item_id(&empty), None);
        let nul = serde_json::json!({"item": {"type": "fileChange", "id": "a\0b"}});
        assert_eq!(tool_item_id(&nul), None);
        let long = serde_json::json!({
            "item": {"type": "fileChange", "id": "x".repeat(MAX_TOOL_ITEM_ID_BYTES + 1)}
        });
        assert_eq!(tool_item_id(&long), None);
        let no_id = serde_json::json!({"item": {"type": "fileChange"}});
        assert_eq!(tool_item_id(&no_id), None);
        let non_tool = serde_json::json!({"item": {"type": "agentMessage", "id": "m-1"}});
        assert_eq!(tool_item_id(&non_tool), None);
        assert_eq!(tool_item_id(&serde_json::json!({})), None);
    }

    #[test]
    fn tool_items_project_only_their_public_call_arguments() {
        let command = serde_json::json!({"item": {
            "type": "commandExecution", "id": "c-1",
            "command": "/bin/zsh -lc 'echo 中'", "cwd": "/workspace",
            "aggregatedOutput": "secret output", "status": "inProgress",
        }});
        let item = tool_item(&command).expect("tool item");
        assert_eq!(item.id, "c-1");
        assert_eq!(item.item_type, "commandExecution");
        assert_eq!(item.status, Some("inProgress"));
        assert_eq!(
            item.arguments,
            Some(serde_json::json!({
                "command": "/bin/zsh -lc 'echo 中'",
                "cwd": "/workspace",
            }))
        );
        assert_eq!(item.error, None);

        let change = serde_json::json!({"item": {
            "type": "fileChange", "id": "c-2",
            "changes": [
                {"path": "a.ts", "kind": {"type": "add"}, "diff": "+secret"},
                {"path": "b.ts", "kind": {"type": "delete"}, "diff": "-secret"},
            ],
            "status": "completed",
        }});
        assert_eq!(
            tool_item(&change).expect("tool item").arguments,
            Some(serde_json::json!({"changes": [
                {"path": "a.ts", "kind": {"type": "add"}},
                {"path": "b.ts", "kind": {"type": "delete"}},
            ]}))
        );

        let mcp = serde_json::json!({"item": {
            "type": "mcpToolCall", "id": "c-3", "server": "search", "tool": "query",
            "arguments": {"q": "rust"}, "result": {"content": "hidden"},
            "error": {"message": "tool failed"},
            "status": "failed",
        }});
        let item = tool_item(&mcp).expect("tool item");
        assert_eq!(
            item.arguments,
            Some(serde_json::json!({
                "server": "search", "tool": "query", "arguments": {"q": "rust"},
            }))
        );
        assert_eq!(
            item.error,
            Some(serde_json::json!({"message": "tool failed"}))
        );

        // Non-whitelisted items never become tool items.
        assert!(tool_item(&serde_json::json!({"item": {
            "type": "reasoning", "id": "r-1", "content": ["raw"],
        }}))
        .is_none());
    }

    #[test]
    fn summary_delta_requires_text_and_keeps_part_identity() {
        let params = serde_json::json!({
            "delta": "public summary", "itemId": "rs-1", "summaryIndex": 2,
        });
        assert_eq!(
            summary_delta(&params),
            Some(("public summary", Some("rs-1"), Some(2)))
        );
        assert_eq!(summary_delta(&serde_json::json!({"itemId": "rs-1"})), None);
        assert_eq!(
            summary_delta(&serde_json::json!({"delta": "x"})),
            Some(("x", None, None))
        );
    }

    #[test]
    fn observe_tool_item_is_first_seen_dedupe_with_fifo_eviction() {
        let mut seen = Vec::new();
        assert!(observe_tool_item(&mut seen, "seen-before"));
        assert!(!observe_tool_item(&mut seen, "seen-before"));
        assert_eq!(seen.len(), 1);
        for index in 0..MAX_TRACKED_TOOL_ITEMS {
            observe_tool_item(&mut seen, &format!("item-{index}"));
        }
        assert_eq!(seen.len(), MAX_TRACKED_TOOL_ITEMS);
        assert!(
            !seen.iter().any(|id| id == "seen-before"),
            "oldest evicted first"
        );
        observe_tool_item(&mut seen, "item-new");
        assert_eq!(seen.len(), MAX_TRACKED_TOOL_ITEMS);
        assert!(seen.iter().any(|id| id == "item-new"));
    }

    #[test]
    fn started_turn_id_requires_a_non_empty_id() {
        let params = serde_json::json!({"turn": {"id": "turn-1"}});
        assert_eq!(started_turn_id(&params), Some("turn-1"));
        assert_eq!(
            started_turn_id(&serde_json::json!({"turn": {"id": ""}})),
            None
        );
        assert_eq!(started_turn_id(&serde_json::json!({})), None);
    }

    #[test]
    fn agent_message_delta_requires_both_fields() {
        let params = serde_json::json!({"delta": "hello", "itemId": "item-1"});
        assert_eq!(agent_message_delta(&params), Some(("hello", "item-1")));
        assert_eq!(
            agent_message_delta(&serde_json::json!({"delta": "hello"})),
            None
        );
        assert_eq!(
            agent_message_delta(&serde_json::json!({"itemId": "item-1"})),
            None
        );
    }

    #[test]
    fn completed_agent_message_accepts_only_agent_messages_with_ids() {
        let params =
            serde_json::json!({"item": {"type": "agentMessage", "id": "item-1", "text": "hi"}});
        assert_eq!(
            completed_agent_message(&params),
            Some(("item-1", Some("hi")))
        );
        let no_text = serde_json::json!({"item": {"type": "agentMessage", "id": "item-1"}});
        assert_eq!(completed_agent_message(&no_text), Some(("item-1", None)));
        let other_type = serde_json::json!({"item": {"type": "commandExecution", "id": "c-1"}});
        assert_eq!(completed_agent_message(&other_type), None);
        let no_id = serde_json::json!({"item": {"type": "agentMessage"}});
        assert_eq!(completed_agent_message(&no_id), None);
    }

    #[test]
    fn final_text_prefers_the_tracked_item_then_scans_the_turn() {
        let turn = serde_json::json!({"items": [
            {"type": "agentMessage", "text": "from items"},
        ]});
        assert_eq!(
            final_text(&turn, Some("tracked".into())),
            Some("tracked".into())
        );
        assert_eq!(final_text(&turn, None), Some("from items".into()));
        assert_eq!(final_text(&serde_json::json!({}), None), None);
        let no_agent_message = serde_json::json!({"items": [
            {"type": "commandExecution", "text": "not a message"},
        ]});
        assert_eq!(final_text(&no_agent_message, None), None);
    }

    #[test]
    fn turn_failure_retains_the_complete_native_error() {
        let recorded = serde_json::json!({
            "message": "Selected model is at capacity.",
            "codexErrorInfo": "serverOverloaded",
            "additionalDetails": "please retry later",
            "willRetry": true,
        });
        let (label, native) =
            turn_failure(&serde_json::json!({}), "failed", Some(recorded.clone()));
        assert_eq!(native, Some(recorded));
        assert_eq!(label, "serverOverloaded: Selected model is at capacity.");

        let boundary = serde_json::json!({"error": {
            "message": "capacity exhausted",
            "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 502}},
        }});
        let (label, native) = turn_failure(&boundary, "failed", None);
        assert_eq!(native, boundary.get("error").cloned());
        assert_eq!(label, "httpConnectionFailed http 502: capacity exhausted");

        let (label, native) =
            turn_failure(&serde_json::json!({"error": null}), "interrupted", None);
        assert_eq!(native, None);
        assert_eq!(label, "codex turn ended with status interrupted");
    }

    #[test]
    fn the_failure_label_is_bounded_while_the_native_error_is_not() {
        let long = "é".repeat(MAX_TURN_FAILURE_LABEL_CHARS + 10);
        let turn = serde_json::json!({"error": {"message": long}});
        let (label, native) = turn_failure(&turn, "failed", None);
        assert_eq!(label.chars().count(), MAX_TURN_FAILURE_LABEL_CHARS);
        assert_eq!(native.expect("native error")["message"], long);
    }

    #[test]
    fn turn_error_native_retains_every_native_field() {
        let params = serde_json::json!({
            "error": {
                "message": "usage limit reached",
                "codexErrorInfo": "usageLimitExceeded",
                "additionalDetails": "resets at 12:00",
            },
            "willRetry": true,
            "threadId": "thread-1",
            "turnId": "turn-1",
        });
        assert_eq!(
            turn_error_native(&params),
            Some(serde_json::json!({
                "message": "usage limit reached",
                "codexErrorInfo": "usageLimitExceeded",
                "additionalDetails": "resets at 12:00",
                "willRetry": true,
            }))
        );
        // A scalar error is retained exactly as received.
        assert_eq!(
            turn_error_native(&serde_json::json!({"error": "boom"})),
            Some(serde_json::json!("boom"))
        );
        assert_eq!(turn_error_native(&serde_json::json!({"error": null})), None);
        assert_eq!(turn_error_native(&serde_json::json!({})), None);
        // A field the server already sent is never shadowed.
        let own = serde_json::json!({
            "error": {"message": "m", "willRetry": false},
            "willRetry": true,
        });
        assert_eq!(turn_error_native(&own).unwrap()["willRetry"], false);
    }

    #[test]
    fn turn_failure_label_applies_the_protocol_defaults() {
        assert_eq!(
            turn_failure_label(&serde_json::json!({
                "codexErrorInfo": "serverOverloaded", "message": "try again"
            })),
            "serverOverloaded: try again"
        );
        assert_eq!(
            turn_failure_label(&serde_json::json!({})),
            "codex_error: codex reported a turn error"
        );
        assert_eq!(
            turn_failure_label(&serde_json::json!({"message": "boom"})),
            "codex_error: boom"
        );
    }

    #[test]
    fn mcp_startup_failure_extracts_only_failed_startups() {
        let params =
            serde_json::json!({"status": "failed", "name": "search", "error": "no binary"});
        assert_eq!(mcp_startup_failure(&params), Some(("search", "no binary")));
        assert_eq!(
            mcp_startup_failure(&serde_json::json!({"status": "failed"})),
            Some(("unknown", "startup failed"))
        );
        assert_eq!(
            mcp_startup_failure(&serde_json::json!({"status": "ready"})),
            None
        );
    }

    #[test]
    fn item_text_aggregation_is_capped_and_bounded() {
        let mut items = HashMap::new();
        for index in 0..MAX_TRACKED_ITEMS {
            items.insert(format!("item-{index}"), String::new());
        }
        observe_item_text(&mut items, "item-new", "refused");
        assert!(!items.contains_key("item-new"));
        observe_item_text(&mut items, "item-0", "appended");
        assert_eq!(items.get("item-0").map(String::as_str), Some("appended"));

        let mut single = HashMap::new();
        // 512 KiB + 4 bytes of two-byte chars: the append trims back to the
        // bound on a char boundary.
        let overflow = "é".repeat(MAX_ITEM_TEXT_BYTES / 2 + 2);
        observe_item_text(&mut single, "big", &overflow);
        assert_eq!(single["big"].len(), MAX_ITEM_TEXT_BYTES);
        observe_item_text(&mut single, "big", "more");
        assert_eq!(single["big"].len(), MAX_ITEM_TEXT_BYTES);
    }

    #[test]
    fn completed_items_refresh_existing_entries_and_respect_the_cap() {
        let mut items = HashMap::new();
        record_completed_item(&mut items, "item-1", "text");
        assert_eq!(items.get("item-1").map(String::as_str), Some("text"));
        record_completed_item(&mut items, "item-1", "replaced");
        assert_eq!(items.get("item-1").map(String::as_str), Some("replaced"));
        for index in 0..MAX_TRACKED_ITEMS {
            items.insert(format!("existing-{index}"), String::new());
        }
        record_completed_item(&mut items, "item-new", "refused");
        assert!(!items.contains_key("item-new"));
    }

    #[test]
    fn mcp_tail_is_byte_bounded_and_keeps_recent_lines() {
        let mut tail = String::new();
        for index in 0..200 {
            record_mcp_failure(&mut tail, &format!("server-{index}"), "boom");
        }
        assert!(tail.len() <= MAX_MCP_DIAGNOSTIC_BYTES);
        record_mcp_failure(&mut tail, "newest", "still visible");
        assert!(
            tail.ends_with("mcp newest failed: still visible\n"),
            "{tail}"
        );
    }

    #[test]
    fn retired_turns_are_deduplicated_and_bounded() {
        let mut retired = Vec::new();
        retire_turn(&mut retired, "turn-1");
        retire_turn(&mut retired, "turn-1");
        assert_eq!(retired.len(), 1);
        for index in 0..MAX_RETIRED_TURNS {
            retire_turn(&mut retired, &format!("turn-{index}"));
        }
        assert_eq!(retired.len(), MAX_RETIRED_TURNS);
        retire_turn(&mut retired, "turn-new");
        assert_eq!(retired.len(), MAX_RETIRED_TURNS);
        assert!(!retired.iter().any(|id| id == "turn-1"));
        assert!(retired.iter().any(|id| id == "turn-new"));
    }
}
