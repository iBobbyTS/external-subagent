//! ZCode native-protocol consistency fixture.
//!
//! This crate is the fake runtime (fake agent) exercised by the consistency
//! tests for the zcode native agent protocol (ZcodeStrict, with no jsonrpc
//! envelope); it is not a provider adapter. Production has no zcode
//! translation layer: the external-runtime `Driver` speaks the native protocol
//! directly, and this fixture only reproduces its wire shapes.

use serde_json::{json, Map, Value};
use std::io::{self, BufRead, Write};

fn write_value(out: &mut impl Write, value: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, &value)?;
    out.write_all(b"\n")?;
    out.flush()
}

fn response(id: Value, result: Value) -> Value {
    json!({"id": id, "result": result})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"id": id, "error": {"code": code, "message": message}})
}

fn event(sequence: &mut u64, session_id: &str, kind: &str, payload: Value) -> Value {
    *sequence = sequence.saturating_add(1);
    json!({
        "method": "session/event",
        "params": {
            "eventId": format!("fake-event-{sequence}"),
            "sessionId": session_id,
            "seq": sequence,
            "timestamp": sequence,
            "type": kind,
            "payload": payload,
        }
    })
}

/// Default create-time model catalog: a reasoning-capable `zai/GLM-5.3`
/// (levels low/high/max, default max) plus a non-`zai` provider so the
/// provider-qualified round trip can be exercised end to end.
fn default_catalog() -> Value {
    json!([
        {
            "ref": {"providerId": "zai", "modelId": "GLM-5.3"},
            "label": "GLM-5.3",
            "contextWindow": 1000000,
            "reasoning": {
                "levels": [
                    {"value": "low", "label": "low"},
                    {"value": "high", "label": "high"},
                    {"value": "max", "label": "max"}
                ],
                "defaultLevel": "max"
            }
        },
        {
            "ref": {"providerId": "deepseek", "modelId": "deepseek-flash"},
            "label": "deepseek-flash",
            "contextWindow": 200000,
            "reasoning": {
                "levels": [
                    {"value": "low", "label": "low"},
                    {"value": "high", "label": "high"}
                ],
                "defaultLevel": "low"
            }
        }
    ])
}

fn env_json(name: &str) -> Option<Value> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .and_then(|value| serde_json::from_str(&value).ok())
}

fn env_token(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Append one inbound frame to the wire log when `ZCODE_FAKE_LOG` is set.
fn append_log(path: &str, line: &str) {
    if let Ok(mut file) = std::fs::File::options()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn exact_keys(object: &Map<String, Value>, required: &[&str], optional: &[&str]) -> bool {
    required.iter().all(|key| object.contains_key(*key))
        && object
            .keys()
            .all(|key| required.contains(&key.as_str()) || optional.contains(&key.as_str()))
}

fn valid_wire_id(value: Option<&Value>) -> bool {
    value.is_some_and(|value| value.as_i64().is_some() || value.is_string())
}

fn valid_request_envelope(value: &Value) -> bool {
    value.get("jsonrpc").is_none()
        && value.as_object().is_some_and(|object| {
            exact_keys(object, &["id", "method", "params"], &[]) && valid_wire_id(object.get("id"))
        })
}

fn valid_response_envelope(value: &Value) -> bool {
    value.get("jsonrpc").is_none()
        && value.as_object().is_some_and(|object| {
            exact_keys(object, &["id", "result"], &[])
                && valid_wire_id(object.get("id"))
                && object.get("result").is_some_and(|result| !result.is_null())
        })
}

fn valid_params(method: &str, params: &Value, session_id: &str) -> bool {
    let Some(params) = params.as_object() else {
        return false;
    };
    match method {
        "workspace/readState" => {
            exact_keys(params, &["workspace"], &[])
                && params
                    .get("workspace")
                    .and_then(Value::as_object)
                    .is_some_and(|workspace| {
                        exact_keys(workspace, &["workspaceKey", "workspacePath"], &[])
                            && workspace.get("workspaceKey").is_some_and(Value::is_string)
                            && workspace.get("workspacePath").is_some_and(Value::is_string)
                    })
        }
        "session/create" => {
            // The official session/create schema is strict with optional
            // `mode` and `thoughtLevel` string inputs alongside the observed
            // workspace/mcpServers shape; anything else is a protocol drift.
            if !exact_keys(
                params,
                &["workspace"],
                &["mode", "thoughtLevel", "mcpServers"],
            ) {
                return false;
            }
            let Some(workspace) = params.get("workspace").and_then(Value::as_object) else {
                return false;
            };
            let workspace_valid = exact_keys(workspace, &["workspaceKey", "workspacePath"], &[])
                && workspace.get("workspaceKey").is_some_and(Value::is_string)
                && workspace.get("workspacePath").is_some_and(Value::is_string);
            let non_empty_string =
                |value: &Value| value.as_str().is_some_and(|token| !token.trim().is_empty());
            let mode_valid = params.get("mode").is_none_or(non_empty_string);
            let thought_level_valid = params.get("thoughtLevel").is_none_or(non_empty_string);
            let mcp_valid = params.get("mcpServers").is_none_or(|servers| {
                servers.as_array().is_some_and(|servers| {
                    !servers.is_empty()
                        && servers.iter().all(|server| {
                            server.as_object().is_some_and(|server| {
                                exact_keys(server, &["name", "command", "args", "env"], &[])
                                    && server.get("name").is_some_and(Value::is_string)
                                    && server.get("command").is_some_and(Value::is_string)
                                    && server.get("args").is_some_and(Value::is_array)
                                    && server.get("env").is_some_and(Value::is_array)
                            })
                        })
                })
            });
            workspace_valid && mode_valid && thought_level_valid && mcp_valid
        }
        "session/setModel" => {
            // Pinned switch shape: sessionId + model{providerId,modelId[,options
            // .reasoningLevel]} + persistAsWorkspaceLastUsed. Anything else is
            // protocol drift.
            if !exact_keys(
                params,
                &["sessionId", "model", "persistAsWorkspaceLastUsed"],
                &[],
            ) || params.get("sessionId").and_then(Value::as_str) != Some(session_id)
                || !params
                    .get("persistAsWorkspaceLastUsed")
                    .is_some_and(Value::is_boolean)
            {
                return false;
            }
            let Some(model) = params.get("model").and_then(Value::as_object) else {
                return false;
            };
            let non_empty_string =
                |value: &Value| value.as_str().is_some_and(|token| !token.trim().is_empty());
            exact_keys(model, &["providerId", "modelId"], &["options"])
                && model.get("providerId").is_some_and(non_empty_string)
                && model.get("modelId").is_some_and(non_empty_string)
                && model.get("options").is_none_or(|options| {
                    options.as_object().is_some_and(|options| {
                        exact_keys(options, &["reasoningLevel"], &[])
                            && options.get("reasoningLevel").is_some_and(non_empty_string)
                    })
                })
        }
        "session/subscribe" => {
            exact_keys(
                params,
                &["sessionId", "deliveryKind", "includeSnapshot"],
                &[],
            ) && params.get("sessionId").and_then(Value::as_str) == Some(session_id)
                && params.get("deliveryKind").and_then(Value::as_str) == Some("desktop-continuous")
                && params.get("includeSnapshot") == Some(&Value::Bool(true))
        }
        "session/send" => {
            exact_keys(params, &["sessionId", "content"], &[])
                && params.get("sessionId").and_then(Value::as_str) == Some(session_id)
                && params.get("content").is_some_and(Value::is_string)
        }
        "session/stop" | "session/close" => {
            exact_keys(params, &["sessionId"], &[])
                && params.get("sessionId").and_then(Value::as_str) == Some(session_id)
        }
        _ => false,
    }
}

fn valid_permission_response(result: &Value) -> bool {
    let Some(result) = result.as_object() else {
        return false;
    };
    exact_keys(
        result,
        &["decision"],
        &["reason", "modifiedInput", "permissionUpdates"],
    ) && matches!(
        result.get("decision").and_then(Value::as_str),
        Some("allow" | "deny")
    )
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "session".into());
    let stdin = io::stdin();
    let mut out = io::stdout();
    let mut active_turn = false;
    let mut next_turn = 1u64;
    let mut sequence = 0u64;
    let session_id =
        std::env::var("ZCODE_FAKE_SESSION_ID").unwrap_or_else(|_| "fake-session-7f3a".into());
    let catalog = env_json("ZCODE_FAKE_MODEL_CATALOG").unwrap_or_else(default_catalog);
    let create_current = env_json("ZCODE_FAKE_MODEL_CURRENT")
        .unwrap_or_else(|| json!({"providerId": "zai", "modelId": "fixture-model"}));
    let set_model_current = env_json("ZCODE_FAKE_SETMODEL_CURRENT");
    let set_model_effort = std::env::var("ZCODE_FAKE_SETMODEL_EFFORT").ok();
    let set_model_error_code = env_token("ZCODE_FAKE_SETMODEL_ERROR_CODE");
    let set_model_error_without_code = env_token("ZCODE_FAKE_SETMODEL_ERROR_WITHOUT_CODE");
    let log_path = env_token("ZCODE_FAKE_LOG");
    let mut pending_permission: Option<Value> = None;

    let mut lines = stdin.lock().lines();
    while let Some(line) = lines.next() {
        let Ok(line) = line else { break };
        if let Some(path) = &log_path {
            append_log(path, &line);
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if mode == "malformed" {
            let _ = out.write_all(b"{\n");
            let _ = write_value(
                &mut out,
                event(
                    &mut sequence,
                    &session_id,
                    "future.event",
                    json!({"raw": "sensitive"}),
                ),
            );
            break;
        }
        if mode == "out-of-order" {
            let _ = write_value(
                &mut out,
                event(&mut sequence, &session_id, "turn.completed", json!({})),
            );
            let _ = write_value(
                &mut out,
                event(&mut sequence, &session_id, "turn.started", json!({})),
            );
            break;
        }

        let id = value
            .get("id")
            .filter(|_| valid_wire_id(value.get("id")))
            .cloned()
            .unwrap_or_else(|| Value::String("invalid-id".into()));
        if value.get("jsonrpc").is_some() {
            let _ = write_value(&mut out, error(id, -32600, "jsonrpc is not accepted"));
            continue;
        }
        let Some(object) = value.as_object() else {
            continue;
        };
        if object.get("method").is_none() {
            if valid_response_envelope(&value)
                && pending_permission.as_ref() == Some(&id)
                && object.get("error").is_none()
                && object.get("result").is_some_and(valid_permission_response)
            {
                let expected_response = json!({"decision": "allow", "reason": "allowed once"});
                if object.get("result") != Some(&expected_response) {
                    continue;
                }
                pending_permission = None;
            }
            continue;
        }
        if !valid_request_envelope(&value) {
            let _ = write_value(&mut out, error(id, -32600, "invalid strict envelope"));
            continue;
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            let _ = write_value(&mut out, error(id, -32600, "method must be a string"));
            continue;
        };
        let params = object.get("params").unwrap_or(&Value::Null);
        if !valid_params(method, params, &session_id) {
            let _ = write_value(&mut out, error(id, -32602, "invalid method parameters"));
            continue;
        }
        match method {
            "workspace/readState" => {
                let _ = write_value(&mut out, response(id, json!({"ready": true})));
            }
            "session/create" => {
                let preference_id = Value::String("runtime-preferences-1".into());
                let _ = write_value(
                    &mut out,
                    json!({
                        "id": preference_id,
                        "method": "session/requestRuntimePreferences",
                        "params": {"scope":"session","sessionId":&session_id}
                    }),
                );
                let preference_line = lines.next().and_then(Result::ok);
                if let (Some(path), Some(line)) = (&log_path, preference_line.as_deref()) {
                    append_log(path, line);
                }
                let preference_response =
                    preference_line.and_then(|line| serde_json::from_str::<Value>(&line).ok());
                if preference_response.as_ref().is_none_or(|response| {
                    response.get("id") != Some(&preference_id)
                        || response.get("result")
                            != Some(&json!({
                                "nativeSearchEnhancementsEnabled":false,
                                "memoryEnabled":false,
                                "askUserQuestionAutoResolutionEnabled":false
                            }))
                }) {
                    std::process::exit(24);
                }
                // Controlled effective-thought echo: the official settings
                // state only projects `thoughtLevel.current` when the level is
                // supported, so an unset ZCODE_FAKE_EFFORT_ECHO keeps the
                // whole section absent (the UNKNOWN state), while a set value
                // pins the echoed level (equal or diverging).
                let mut settings = json!({
                    "model": {
                        "current": create_current.clone(),
                        "available": catalog.clone()
                    }
                });
                if let Some(echo) = std::env::var("ZCODE_FAKE_EFFORT_ECHO")
                    .ok()
                    .filter(|value| !value.is_empty())
                {
                    settings["thoughtLevel"] = json!({
                        "enabled": true,
                        "current": echo,
                        "available": [
                            {"value": "high", "label": "high"},
                            {"value": "low", "label": "low"}
                        ]
                    });
                }
                let _ = write_value(
                    &mut out,
                    response(
                        id,
                        json!({
                            "session": {"sessionId": &session_id},
                            "settings": settings
                        }),
                    ),
                );
            }
            "session/setModel" => {
                let requested_model = params.get("model").cloned().unwrap_or_else(|| json!({}));
                // Configurable remote rejection: the documented discriminators
                // live on error.data.code, the top-level code always -32603.
                // ZCODE_FAKE_SETMODEL_ERROR_WITHOUT_CODE drops the whole
                // `data` object to model an unrecognized remote failure.
                if let Some(code) = &set_model_error_code {
                    let error = if set_model_error_without_code.is_some() {
                        json!({
                            "id": id,
                            "error": {
                                "code": -32603,
                                "message": "fixture setModel rejected without a data code"
                            }
                        })
                    } else {
                        json!({
                            "id": id,
                            "error": {
                                "code": -32603,
                                "message": format!("fixture setModel rejected: {code}"),
                                "data": {"name": "ModelProtocolError", "code": code}
                            }
                        })
                    };
                    let _ = write_value(&mut out, error);
                    continue;
                }
                // Success echo: default is the requested reference (so a
                // correct round trip passes), overridable to force a
                // provider-qualified read-back mismatch.
                let current = set_model_current
                    .clone()
                    .unwrap_or_else(|| requested_model.clone());
                let requested_level = requested_model
                    .get("options")
                    .and_then(|options| options.get("reasoningLevel"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let thought_level = match &set_model_effort {
                    Some(value) => (!value.is_empty()).then(|| value.clone()),
                    None => requested_level,
                };
                // The real runtime emits the model_changed notification before
                // resolving the setModel response; the event pump must tolerate
                // an UnknownEvent interleaved into the pending request.
                let _ = write_value(
                    &mut out,
                    json!({
                        "method": "state.updated",
                        "params": {
                            "patch": {"model": {"current": current.clone()}},
                            "reason": "model_changed",
                            "revision": sequence.saturating_add(1),
                            "scope": "session",
                            "sessionId": &session_id,
                            "type": "state.updated"
                        }
                    }),
                );
                let mut settings = json!({"model": {"current": current.clone()}});
                if let Some(level) = thought_level {
                    settings["thoughtLevel"] = json!({
                        "enabled": true,
                        "current": level,
                        "available": [
                            {"value": "low", "label": "low"},
                            {"value": "high", "label": "high"},
                            {"value": "max", "label": "max"}
                        ]
                    });
                }
                let _ = write_value(
                    &mut out,
                    response(
                        id,
                        json!({
                            "session": {
                                "sessionId": &session_id,
                                "model": {
                                    "providerId": current.get("providerId").cloned().unwrap_or(Value::Null),
                                    "modelId": current.get("modelId").cloned().unwrap_or(Value::Null)
                                }
                            },
                            "settings": settings
                        }),
                    ),
                );
            }
            "session/subscribe" => {
                let _ = write_value(
                    &mut out,
                    response(
                        id,
                        json!({"sessionId": &session_id, "eventSeq": sequence, "events": []}),
                    ),
                );
            }
            "session/send" => {
                let content = params
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if active_turn {
                    let _ = write_value(&mut out, error(id, -32010, "PROMPT_ALREADY_RUNNING"));
                    continue;
                }
                let turn_id = format!("fake-turn-{next_turn}");
                next_turn += 1;
                active_turn = true;
                let _ = write_value(
                    &mut out,
                    response(id, json!({"sessionId": &session_id, "accepted": true})),
                );
                let _ = write_value(
                    &mut out,
                    event(
                        &mut sequence,
                        &session_id,
                        "turn.started",
                        json!({"turnId": turn_id, "turnNumber": next_turn - 1}),
                    ),
                );
                if content.contains("permission") {
                    let request_id = Value::String("server-1".into());
                    pending_permission = Some(request_id.clone());
                    let _ = write_value(
                        &mut out,
                        json!({
                            "id": request_id,
                            "method": "interaction/requestPermission",
                            "params": {
                                "toolCallId": "tool-1",
                                "toolName": "read",
                                "riskLevel": "low",
                                "reason": "fixture permission",
                                "input": json!({"path": "fixture.txt"}),
                                "options": [
                                    {"id": "allow_once", "kind": "allow_once", "label": "Allow once", "response": {"decision": "allow", "reason": "allowed once"}},
                                    {"id": "deny", "kind": "deny", "label": "Deny", "response": {"decision": "deny", "reason": "denied"}}
                                ]
                            }
                        }),
                    );
                }
                if content.contains("input") {
                    let _ = write_value(
                        &mut out,
                        json!({
                            "id": "server-input-1",
                            "method": "interaction/requestUserInput",
                            "params": {"question": "unsupported fixture input"}
                        }),
                    );
                }
                if content.contains("unknown_event") {
                    let _ = write_value(
                        &mut out,
                        event(
                            &mut sequence,
                            &session_id,
                            "future.event",
                            json!({"secret": "redacted"}),
                        ),
                    );
                }
                if content.contains("auto_complete") {
                    let _ = write_value(
                        &mut out,
                        event(
                            &mut sequence,
                            &session_id,
                            "turn.completed",
                            json!({"response": "fixture complete"}),
                        ),
                    );
                    active_turn = false;
                }
                if mode == "crash" {
                    std::process::exit(17);
                }
            }
            "session/stop" => {
                let _ = write_value(&mut out, response(id, json!({"stopped": true})));
                if active_turn {
                    active_turn = false;
                    let _ = write_value(
                        &mut out,
                        event(
                            &mut sequence,
                            &session_id,
                            "turn.completed",
                            json!({"stopped": true}),
                        ),
                    );
                }
            }
            "session/close" => {
                let _ = write_value(&mut out, response(id, json!({"closed": true})));
                break;
            }
            _ => unreachable!("valid_params rejects unsupported methods"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_request_keys_accept_observed_and_reject_unobserved_fields() {
        assert!(valid_params(
            "workspace/readState",
            &json!({
                "workspace": {
                    "workspaceKey": "workspace-key",
                    "workspacePath": "/workspace"
                }
            }),
            "fake-session-7f3a",
        ));
        assert!(!valid_params(
            "workspace/readState",
            &json!({
                "workspace": {
                    "workspaceKey": "workspace-key",
                    "workspacePath": "/workspace"
                },
                "invented": true
            }),
            "fake-session-7f3a",
        ));
        assert!(!valid_params(
            "session/send",
            &json!({"session_id":"fake-session-7f3a","message":"review"}),
            "fake-session-7f3a",
        ));
        assert!(valid_params(
            "session/send",
            &json!({"sessionId":"fake-session-7f3a","content":"review"}),
            "fake-session-7f3a",
        ));
        assert!(valid_params(
            "session/subscribe",
            &json!({
                "sessionId":"fake-session-7f3a",
                "deliveryKind":"desktop-continuous",
                "includeSnapshot":true
            }),
            "fake-session-7f3a",
        ));
        assert!(!valid_params(
            "session/subscribe",
            &json!({
                "sessionId":"fake-session-7f3a",
                "deliveryKind":"desktop-continuous",
                "includeSnapshot":true,
                "afterSeq":0
            }),
            "fake-session-7f3a",
        ));
        for key in ["inputId", "queryId"] {
            let mut params = json!({
                "sessionId":"fake-session-7f3a",
                "content":"review"
            });
            params
                .as_object_mut()
                .unwrap()
                .insert(key.into(), json!("invented"));
            assert!(!valid_params("session/send", &params, "fake-session-7f3a",));
        }
    }

    #[test]
    fn session_create_accepts_the_official_optional_mode_and_thought_level_keys() {
        let base = json!({
            "workspace": {
                "workspaceKey": "workspace-key",
                "workspacePath": "/workspace"
            }
        });
        let mut with_optional = base.clone();
        with_optional["mode"] = json!("build");
        with_optional["thoughtLevel"] = json!("high");
        assert!(valid_params(
            "session/create",
            &with_optional,
            "fake-session-7f3a",
        ));
        // A task without an admitted effort sends the byte-identical frame.
        assert!(valid_params("session/create", &base, "fake-session-7f3a",));
        for key in ["mode", "thoughtLevel"] {
            let mut wrong_type = with_optional.clone();
            wrong_type[key] = json!(7);
            assert!(
                !valid_params("session/create", &wrong_type, "fake-session-7f3a",),
                "{key} must stay a non-empty string"
            );
            let mut empty = with_optional.clone();
            empty[key] = json!("  ");
            assert!(
                !valid_params("session/create", &empty, "fake-session-7f3a",),
                "{key} must not be blank"
            );
        }
        let mut invented = with_optional.clone();
        invented["reasoningEffort"] = json!("high");
        assert!(!valid_params(
            "session/create",
            &invented,
            "fake-session-7f3a",
        ));
    }

    #[test]
    fn strict_envelope_rejects_jsonrpc_and_extra_fields() {
        assert!(!valid_request_envelope(
            &json!({"jsonrpc":"2.0","id":1,"method":"session/stop","params":{}})
        ));
        assert!(!valid_request_envelope(
            &json!({"id":1,"method":"session/stop","params":{},"legacy":true})
        ));
        assert!(valid_request_envelope(
            &json!({"id":1,"method":"session/stop","params":{"sessionId":"s1"}})
        ));
    }

    #[test]
    fn strict_envelope_rejects_non_wire_ids_and_null_response_outcomes() {
        for id in [json!(true), json!(null), json!({}), json!([]), json!(1.5)] {
            assert!(!valid_request_envelope(
                &json!({"id":id,"method":"session/stop","params":{}})
            ));
            assert!(!valid_response_envelope(&json!({"id":id,"result":{}})));
        }
        assert!(!valid_response_envelope(
            &json!({"id":"server-1","result":null})
        ));
        assert!(valid_response_envelope(
            &json!({"id":"server-1","result":{"decision":"allow"}})
        ));
    }

    #[test]
    fn session_set_model_accepts_only_the_pinned_switch_shape() {
        let base = json!({
            "sessionId": "fake-session-7f3a",
            "model": {"providerId": "zai", "modelId": "GLM-5.3"},
            "persistAsWorkspaceLastUsed": false
        });
        assert!(valid_params("session/setModel", &base, "fake-session-7f3a"));
        let mut with_options = base.clone();
        with_options["model"]["options"] = json!({"reasoningLevel": "high"});
        assert!(valid_params(
            "session/setModel",
            &with_options,
            "fake-session-7f3a"
        ));
        // Wrong session, missing flag, non-boolean flag and invented keys all
        // fail closed.
        for invalid in [
            json!({
                "sessionId": "other",
                "model": {"providerId": "zai", "modelId": "GLM-5.3"},
                "persistAsWorkspaceLastUsed": false
            }),
            json!({
                "sessionId": "fake-session-7f3a",
                "model": {"providerId": "zai", "modelId": "GLM-5.3"}
            }),
            json!({
                "sessionId": "fake-session-7f3a",
                "model": {"providerId": "zai", "modelId": "GLM-5.3"},
                "persistAsWorkspaceLastUsed": "false"
            }),
            json!({
                "sessionId": "fake-session-7f3a",
                "model": {"providerId": "zai", "modelId": "GLM-5.3"},
                "persistAsWorkspaceLastUsed": false,
                "reasoningEffort": "high"
            }),
            json!({
                "sessionId": "fake-session-7f3a",
                "model": {"providerId": "zai", "modelId": "GLM-5.3", "options": {}},
                "persistAsWorkspaceLastUsed": false
            }),
        ] {
            assert!(
                !valid_params("session/setModel", &invalid, "fake-session-7f3a"),
                "setModel drift must be rejected: {invalid}"
            );
        }
    }

    #[test]
    fn permission_response_rejects_invented_content_field() {
        assert!(valid_permission_response(&json!({"decision":"allow"})));
        assert!(!valid_permission_response(
            &json!({"decision":"answer","content":"guessed"})
        ));
    }
}
