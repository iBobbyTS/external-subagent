//! Persisted agent configuration: snapshot types, parsing, and migration.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use super::errors::{RpcError, RpcErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, env, fs};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentConfigSnapshot {
    #[serde(default)]
    pub(super) schema_version: u32,
    #[serde(default)]
    pub(super) revision: u64,
    #[serde(default)]
    pub(super) default_subagent: Option<String>,
    #[serde(default)]
    pub(super) subagents: BTreeMap<String, AgentConfigEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentConfigEntry {
    pub(super) enabled: bool,
    pub(super) spawn_supported: bool,
    #[serde(default)]
    pub(super) default_model: Option<String>,
    #[serde(default)]
    pub(super) runtime_path: Option<String>,
    #[serde(default)]
    pub(super) home: Option<String>,
    #[serde(default)]
    pub(super) profile: Option<String>,
    #[serde(default)]
    pub(super) version: Option<String>,
}

impl Default for AgentConfigSnapshot {
    fn default() -> Self {
        Self {
            schema_version: 2,
            revision: 0,
            default_subagent: None,
            subagents: BTreeMap::from([
                (
                    "zcode".into(),
                    AgentConfigEntry {
                        enabled: false,
                        spawn_supported: false,
                        default_model: None,
                        runtime_path: None,
                        home: None,
                        profile: None,
                        version: None,
                    },
                ),
                (
                    "dsh".into(),
                    AgentConfigEntry {
                        enabled: false,
                        spawn_supported: false,
                        default_model: None,
                        runtime_path: None,
                        home: None,
                        profile: None,
                        version: None,
                    },
                ),
                (
                    "codex".into(),
                    AgentConfigEntry {
                        enabled: false,
                        spawn_supported: false,
                        default_model: None,
                        runtime_path: None,
                        home: None,
                        profile: None,
                        version: None,
                    },
                ),
            ]),
        }
    }
}

pub(super) fn read_agent_config_snapshot() -> Result<AgentConfigSnapshot, RpcError> {
    let Some(path) =
        env::var_os("EXTERNAL_SUBAGENT_CONFIG").or_else(|| env::var_os("ZCODE_AGENT_CONFIG"))
    else {
        return Ok(AgentConfigSnapshot::default());
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentConfigSnapshot::default())
        }
        Err(_) => {
            return Err(RpcError::new(
                RpcErrorCode::Validation,
                "agent config is unreadable",
            ))
        }
    };
    parse_agent_config_snapshot(&bytes)
}

/// Startup and RPC admission share the persisted config validation and migration.
pub fn parse_subagent_config(bytes: &[u8]) -> Result<Value, RpcError> {
    serde_json::to_value(parse_agent_config_snapshot(bytes)?)
        .map_err(|_| RpcError::new(RpcErrorCode::Validation, "agent config is invalid"))
}

fn parse_agent_config_snapshot(bytes: &[u8]) -> Result<AgentConfigSnapshot, RpcError> {
    let mut value: Value = serde_json::from_slice(bytes)
        .map_err(|_| RpcError::new(RpcErrorCode::Validation, "agent config is invalid"))?;
    normalize_agent_config_value(&mut value)?;
    let mut snapshot: AgentConfigSnapshot = serde_json::from_value(value)
        .map_err(|_| RpcError::new(RpcErrorCode::Validation, "agent config is invalid"))?;
    if snapshot.schema_version != 2 {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            "unsupported agent config schema version",
        ));
    }
    if snapshot
        .subagents
        .keys()
        .any(|agent| !matches!(agent.as_str(), "zcode" | "dsh" | "codex"))
    {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            "agent config contains an unknown agent",
        ));
    }
    let defaults = AgentConfigSnapshot::default();
    for (name, entry) in defaults.subagents {
        snapshot.subagents.entry(name).or_insert(entry);
    }
    if snapshot
        .default_subagent
        .as_deref()
        .is_some_and(|agent| !snapshot.subagents.contains_key(agent))
    {
        return Err(RpcError::new(
            RpcErrorCode::AgentUnknown,
            "default_subagent is unknown",
        ));
    }
    if snapshot
        .default_subagent
        .as_deref()
        .is_some_and(|agent| !snapshot.subagents[agent].enabled)
    {
        return Err(RpcError::new(
            RpcErrorCode::AgentDisabled,
            "default_subagent is disabled",
        ));
    }
    Ok(snapshot)
}

fn normalize_agent_config_value(value: &mut Value) -> Result<(), RpcError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| RpcError::new(RpcErrorCode::Validation, "agent config is invalid"))?;
    if let Some(schema) = object.get("schema_version") {
        if !matches!(schema.as_u64(), Some(1) | Some(2)) {
            return Err(RpcError::new(
                RpcErrorCode::Validation,
                "unsupported agent config schema version",
            ));
        }
    }
    let legacy = object.contains_key("agents") || object.contains_key("default_agent");
    let canonical = object.contains_key("subagents") || object.contains_key("default_subagent");
    let version = object.get("schema_version").and_then(Value::as_u64);
    if (legacy && canonical) || (version == Some(1) && canonical) || (version == Some(2) && legacy)
    {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            "agent config fields do not match schema version",
        ));
    }
    if let Some(legacy) = object.remove("agents") {
        object.insert("subagents".into(), legacy);
    }
    if let Some(legacy) = object.remove("default_agent") {
        object.insert("default_subagent".into(), legacy);
    }
    object.insert("schema_version".into(), Value::from(2));
    let Some(agents) = object.get_mut("subagents") else {
        return Ok(());
    };
    let agents = agents.as_object_mut().ok_or_else(|| {
        RpcError::new(
            RpcErrorCode::Validation,
            "agent config subagents must be an object",
        )
    })?;
    for (name, entry) in agents.iter_mut() {
        let entry = entry.as_object_mut().ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::Validation,
                "agent config entry must be an object",
            )
        })?;
        for field in ["runtime_path", "home", "profile", "version"] {
            if let Some(value) = entry.get(field) {
                if !value.is_null() && !value.as_str().is_some_and(|value| !value.is_empty()) {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "agent config runtime fields must be non-empty strings or null",
                    ));
                }
            }
        }
        // AUD-005/D1 (S02): a zcode runtime_path has no consumer — startup
        // resolves the pinned packaged ZCode runtime from service arguments,
        // never from this field (see main.rs) — so accepting it here would let
        // a hand-written config pass startup/RPC admission while being
        // silently ignored at spawn.  Node rejects the same input with
        // `runtime_path_unsupported` (cli/config/schema.mjs); null stays
        // allowed, and malformed or empty values keep the generic rejection
        // above regardless of the agent.  dsh/codex stay configurable.
        if name == "zcode"
            && entry
                .get("runtime_path")
                .is_some_and(|value| !value.is_null())
        {
            return Err(RpcError::new(
                RpcErrorCode::Validation,
                "agent config runtime_path is unsupported for zcode",
            ));
        }
        let default_enabled = false;
        entry
            .entry("enabled")
            .or_insert(Value::Bool(default_enabled));
        entry
            .entry("spawn_supported")
            .or_insert(Value::Bool(default_enabled));
        entry.entry("default_model").or_insert(Value::Null);
        if !entry.get("enabled").is_some_and(Value::is_boolean)
            || !entry.get("spawn_supported").is_some_and(Value::is_boolean)
        {
            return Err(RpcError::new(
                RpcErrorCode::Validation,
                "agent config flags must be booleans",
            ));
        }
        if let Some(model) = entry.get("default_model") {
            if model.as_str().is_some_and(str::is_empty)
                || (name == "zcode" && !model.is_null())
                || (!model.is_null() && !model.is_string())
            {
                return Err(RpcError::new(
                    RpcErrorCode::Validation,
                    "agent config model selection is invalid",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod config_migration_tests {
    use super::*;

    #[test]
    fn omitted_flags_never_enable_a_subagent() {
        for input in [
            b"{}".as_slice(),
            br#"{"subagents":{"zcode":{},"dsh":{},"codex":{}}}"#,
        ] {
            let config = parse_agent_config_snapshot(input).unwrap();
            assert!(config
                .subagents
                .values()
                .all(|entry| !entry.enabled && !entry.spawn_supported));
        }
    }

    #[test]
    fn shared_node_rust_config_matrix() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/subagent-config-matrix.json"
        ))
        .unwrap();
        let all = cases.as_array().unwrap();
        let zcode_runtime_rejections = all
            .iter()
            .filter(|case| case["error_code"].as_str() == Some("runtime_path_unsupported"))
            .count();
        // AUD-005/S02: the shared fixture must keep exercising the zcode
        // runtime_path gate in both schema shapes plus the disabled-entry
        // variant; a matrix edit that drops them would otherwise let this
        // filtered test pass vacuously.
        assert!(
            zcode_runtime_rejections >= 3,
            "shared matrix lost its zcode runtime_path negatives"
        );
        eprintln!(
            "matrix: {} cases, {} zcode runtime_path rejections",
            all.len(),
            zcode_runtime_rejections
        );
        for case in all {
            let bytes = serde_json::to_vec(&case["input"]).unwrap();
            let startup = parse_subagent_config(&bytes);
            let rpc = parse_agent_config_snapshot(&bytes);
            let valid = case["valid"].as_bool().unwrap();
            assert_eq!(
                startup.is_ok(),
                valid,
                "startup {}: {:?}",
                case["name"],
                startup
            );
            assert_eq!(rpc.is_ok(), valid, "RPC {}: {:?}", case["name"], rpc);
            if !valid && case["error_code"].as_str() == Some("runtime_path_unsupported") {
                // Mirrors the Node `runtime_path_unsupported` contract: the
                // rejection must stay diagnosable as "zcode does not support
                // this field" through the existing RpcError message style.
                for error in [startup.as_ref().err(), rpc.as_ref().err()] {
                    let Some(error) = error else {
                        panic!("{} escaped rejection", case["name"]);
                    };
                    assert!(
                        error.message.contains("zcode") && error.message.contains("runtime_path"),
                        "{} rejection is not diagnosable: {}",
                        case["name"],
                        error.message
                    );
                }
            }
            if let Ok(value) = startup {
                assert_eq!(value["schema_version"], 2);
                assert!(value.get("agents").is_none());
                assert!(value.get("default_agent").is_none());
                let input = &case["input"];
                assert_eq!(
                    value["default_subagent"],
                    input
                        .get("default_agent")
                        .or_else(|| input.get("default_subagent"))
                        .unwrap_or(&Value::Null)
                        .clone()
                );
                if let Some(entries) = input
                    .get("agents")
                    .or_else(|| input.get("subagents"))
                    .and_then(Value::as_object)
                {
                    for (name, entry) in entries {
                        for (field, expected) in entry.as_object().unwrap() {
                            assert_eq!(
                                &value["subagents"][name][field], expected,
                                "{}",
                                case["name"]
                            );
                        }
                    }
                }
            }
        }
    }
}
