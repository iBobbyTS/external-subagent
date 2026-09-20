//! Agent admission policy and status derivation from config plus evidence.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use super::config::{AgentConfigEntry, AgentConfigSnapshot};
use super::errors::{RpcError, RpcErrorCode};
use super::handlers::validate_text;
use super::types::GeneralSubmitInput;
use super::views::{
    AgentEffortSelectionCapabilityView, AgentEffortSelectionModeView,
    AgentModelSelectionCapabilityView, AgentModelSelectionModeView, AgentPermissionModeView,
    AgentScopeStatusView, AgentStatusView, AgentTransportSupportView, AgentTransportView,
    ComponentStateView,
};
use crate::agent_status::{
    AgentEvidenceStore, AgentModelsInput, AgentProbeEvidence, AgentProbeInput, EvidenceState,
    ProbeScope, ScopeEvidence,
};
use std::{env, fs, path::Path};

#[cfg(test)]
use super::views::{flat_identity, task_view};
#[cfg(test)]
use crate::rpc::wait_tests;
#[cfg(test)]
use crate::rpc::{
    RpcMethod, RpcOutcome, RpcResponse, RpcService, RpcSuccess, TaskListQuery, MAX_LIST_TASKS,
};
#[cfg(test)]
use external_core::GeneralTaskManifest;
#[cfg(test)]
use external_store::{Store, TaskOutcome, TaskPageFilter, TaskQueryScope};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

pub(super) fn configured_agent_statuses(
    config: &AgentConfigSnapshot,
    evidence: &AgentEvidenceStore,
) -> Vec<AgentStatusView> {
    ["zcode", "dsh", "codex"]
        .into_iter()
        .map(|agent| {
            let entry = &config.subagents[agent];
            let observed = evidence.latest(agent);
            AgentStatusView {
                agent: agent.into(),
                required_version: required_version(agent),
                config_revision: config.revision,
                configured: true,
                enabled: entry.enabled,
                spawn_supported: effective_spawn_supported(agent, entry),
                transport_support: transport_support(agent, entry),
                permission_modes: permission_modes(agent, entry),
                model_selection: model_selection(agent, entry),
                effort_selection: effort_selection(agent, entry),
                local: current_scope(&observed, config.revision, |evidence| &evidence.local),
                auth: current_scope(&observed, config.revision, |evidence| &evidence.auth),
                hi: current_scope(&observed, config.revision, |evidence| &evidence.hi),
            }
        })
        .collect()
}

pub(super) fn unavailable_agent_statuses() -> Vec<AgentStatusView> {
    ["zcode", "dsh", "codex"]
        .into_iter()
        .map(|agent| AgentStatusView {
            agent: agent.into(),
            required_version: required_version(agent),
            config_revision: 0,
            configured: false,
            enabled: false,
            spawn_supported: false,
            transport_support: transport_support(
                agent,
                &AgentConfigEntry {
                    enabled: false,
                    spawn_supported: false,
                    default_model: None,
                    runtime_path: None,
                    home: None,
                    profile: None,
                    version: None,
                },
            ),
            permission_modes: Vec::new(),
            model_selection: model_selection(
                agent,
                &AgentConfigEntry {
                    enabled: false,
                    spawn_supported: false,
                    default_model: None,
                    runtime_path: None,
                    home: None,
                    profile: None,
                    version: None,
                },
            ),
            effort_selection: effort_selection(
                agent,
                &AgentConfigEntry {
                    enabled: false,
                    spawn_supported: false,
                    default_model: None,
                    runtime_path: None,
                    home: None,
                    profile: None,
                    version: None,
                },
            ),
            local: unprobed_scope(),
            auth: unprobed_scope(),
            hi: unprobed_scope(),
        })
        .collect()
}

fn required_version(agent: &str) -> Option<String> {
    (agent == "dsh").then(|| external_agent_dsh::profile::PINNED_DSH_VERSION.into())
}

fn effective_spawn_supported(agent: &str, entry: &AgentConfigEntry) -> bool {
    if agent == "dsh" {
        return entry.enabled
            && entry.spawn_supported
            && entry.profile.as_deref() == Some("acp")
            && entry.version.as_deref() == Some(external_agent_dsh::profile::PINNED_DSH_VERSION)
            && entry
                .runtime_path
                .as_deref()
                .map(Path::new)
                .is_some_and(|path| {
                    path.is_absolute()
                        && fs::metadata(path).is_ok_and(|m| {
                            m.is_file() && {
                                #[cfg(unix)]
                                {
                                    use std::os::unix::fs::PermissionsExt;
                                    m.permissions().mode() & 0o111 != 0
                                }
                                #[cfg(not(unix))]
                                {
                                    true
                                }
                            }
                        })
                });
    }
    if agent == "codex" {
        // The Codex gate pins the persisted runtime executable only; the home
        // precedence (agents.codex.home over inherited CODEX_HOME) is enforced
        // at admission and spawn so no launch ever falls back to ~/.codex.
        return entry.enabled
            && entry.spawn_supported
            && entry
                .runtime_path
                .as_deref()
                .map(Path::new)
                .is_some_and(|path| {
                    path.is_absolute()
                        && fs::metadata(path).is_ok_and(|m| {
                            m.is_file() && {
                                #[cfg(unix)]
                                {
                                    use std::os::unix::fs::PermissionsExt;
                                    m.permissions().mode() & 0o111 != 0
                                }
                                #[cfg(not(unix))]
                                {
                                    true
                                }
                            }
                        })
                });
    }
    entry.enabled && entry.spawn_supported
}

fn transport_support(agent: &str, entry: &AgentConfigEntry) -> AgentTransportSupportView {
    AgentTransportSupportView {
        transport: match agent {
            "zcode" => AgentTransportView::ZcodeAppServer,
            "codex" => AgentTransportView::CodexAppServer,
            _ => AgentTransportView::DshAcp,
        },
        probe: true,
        spawn: effective_spawn_supported(agent, entry),
    }
}

fn permission_modes(agent: &str, entry: &AgentConfigEntry) -> Vec<AgentPermissionModeView> {
    if !effective_spawn_supported(agent, entry) {
        return Vec::new();
    }
    if agent == "dsh" {
        // First-launch dsh admission proves only the read-bounded build and
        // strict plan scopes; edit/yolo are refused before the prompt.
        return vec![
            AgentPermissionModeView::Build,
            AgentPermissionModeView::Plan,
        ];
    }
    if agent == "codex" {
        // Codex admission is posture-pinned with approvalPolicy=never:
        // plan maps to sandbox=read-only and yolo maps to
        // sandbox=danger-full-access. build/edit stay refused before the
        // prompt until a workspace-write confinement is proven equivalent.
        return vec![AgentPermissionModeView::Plan, AgentPermissionModeView::Yolo];
    }
    vec![
        AgentPermissionModeView::Build,
        AgentPermissionModeView::Edit,
        AgentPermissionModeView::Plan,
        AgentPermissionModeView::Yolo,
    ]
}

fn model_selection(agent: &str, entry: &AgentConfigEntry) -> AgentModelSelectionCapabilityView {
    if agent == "zcode" {
        AgentModelSelectionCapabilityView {
            supported: false,
            mode: AgentModelSelectionModeView::NativeOnly,
        }
    } else {
        AgentModelSelectionCapabilityView {
            supported: matches!(agent, "dsh" | "codex") && effective_spawn_supported(agent, entry),
            mode: AgentModelSelectionModeView::CatalogToken,
        }
    }
}

fn effort_selection(agent: &str, entry: &AgentConfigEntry) -> AgentEffortSelectionCapabilityView {
    AgentEffortSelectionCapabilityView {
        supported: effective_spawn_supported(agent, entry),
        mode: if agent == "codex" {
            AgentEffortSelectionModeView::ClosedSet
        } else {
            AgentEffortSelectionModeView::PassthroughToken
        },
    }
}

fn current_scope(
    evidence: &Option<AgentProbeEvidence>,
    config_revision: u64,
    select: impl FnOnce(&AgentProbeEvidence) -> &ScopeEvidence,
) -> AgentScopeStatusView {
    match evidence {
        Some(evidence) if evidence.config_revision == config_revision => {
            scope_status_view(select(evidence))
        }
        Some(evidence) => stale_scope(select(evidence)),
        None => unprobed_scope(),
    }
}

fn stale_scope(value: &ScopeEvidence) -> AgentScopeStatusView {
    let was_checked = value
        .reason
        .as_deref()
        .map_or(true, |reason| !reason.contains("not_probed"));
    AgentScopeStatusView {
        state: ComponentStateView::Unknown,
        runtime_path: value.runtime_path.clone(),
        scope: value.scope.clone(),
        version: value.version.clone(),
        checked_at_ms: was_checked.then_some(value.checked_at_ms),
        reason: Some("stale_config_revision".into()),
    }
}

fn unprobed_scope() -> AgentScopeStatusView {
    AgentScopeStatusView {
        state: ComponentStateView::Unknown,
        runtime_path: None,
        scope: ProbeScope::default(),
        version: None,
        checked_at_ms: None,
        reason: None,
    }
}

fn scope_status_view(value: &ScopeEvidence) -> AgentScopeStatusView {
    let was_checked = value
        .reason
        .as_deref()
        .map_or(true, |reason| !reason.contains("not_probed"));
    AgentScopeStatusView {
        state: match value.state {
            EvidenceState::Ready => ComponentStateView::Ready,
            EvidenceState::Degraded => ComponentStateView::Degraded,
            EvidenceState::Unavailable => ComponentStateView::Unavailable,
            EvidenceState::Unknown => ComponentStateView::Unknown,
        },
        runtime_path: value.runtime_path.clone(),
        scope: value.scope.clone(),
        version: value.version.clone(),
        checked_at_ms: was_checked.then_some(value.checked_at_ms),
        // `*_not_probed` is an internal derivation detail. Public status
        // conveys whether a scope was checked via state + timestamp instead.
        reason: value
            .reason
            .as_deref()
            .filter(|reason| !reason.contains("not_probed"))
            .map(str::to_owned),
    }
}

pub(super) fn validate_agent_probe_input(input: &AgentProbeInput) -> Result<(), RpcError> {
    if !matches!(input.agent.as_str(), "zcode" | "dsh" | "codex") {
        return Err(RpcError::new(
            RpcErrorCode::AgentUnknown,
            "agent is unknown",
        ));
    }
    for (name, value) in [
        ("workspace", input.scope.workspace.as_deref()),
        ("home", input.scope.home.as_deref()),
    ] {
        if let Some(value) = value {
            validate_text(value, name, 4096)?;
            if !Path::new(value).is_absolute() {
                return Err(RpcError::new(
                    RpcErrorCode::Validation,
                    format!("{name} must be absolute"),
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_agent_models_input(input: &AgentModelsInput) -> Result<(), RpcError> {
    validate_agent_probe_input(&AgentProbeInput {
        agent: input.agent.clone(),
        through: crate::agent_status::ProbeLayer::Local,
        scope: input.scope.clone(),
    })
}

pub(super) fn resolve_admission(
    input: &GeneralSubmitInput,
    config: &AgentConfigSnapshot,
) -> Result<external_core::AdmissionIdentity, RpcError> {
    for (field, value) in [
        ("agent", input.agent.as_deref()),
        ("model", input.model.as_deref()),
    ] {
        if let Some(value) = value {
            validate_text(value.trim(), field, 4096)?;
        }
    }
    let agent = input
        .agent
        .as_deref()
        .or(config.default_subagent.as_deref())
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::AgentRequired,
                "agent is required when no default_subagent is configured",
            )
        })?;
    let configured = config
        .subagents
        .get(agent)
        .ok_or_else(|| RpcError::new(RpcErrorCode::AgentUnknown, "agent is unknown"))?;
    if !configured.enabled {
        return Err(RpcError::new(
            RpcErrorCode::AgentDisabled,
            "agent is disabled",
        ));
    }
    if agent == "zcode" && (input.model.is_some() || configured.default_model.is_some()) {
        return Err(RpcError::new(
            RpcErrorCode::ModelSelectionUnsupported,
            "model selection is unsupported for zcode; prompt_count=0",
        ));
    }
    if !effective_spawn_supported(agent, configured) {
        return Err(RpcError::new(
            RpcErrorCode::AgentUnsupported,
            format!("agent {agent} is unsupported; prompt_count=0"),
        ));
    }
    let effort = resolve_effort_selection(agent, input)?;
    let (model, model_source) = if agent == "dsh" {
        // Explicit spawn token, then the configured default, then the
        // provider-native model. The configured default is a selection too:
        // an unusable token is refused here rather than silently downgraded
        // to native. The dsh model is `{provider}:{model}` (split at the first
        // colon); the ACP session re-serializes it to the byte-exact wire
        // tuple immediately before the prompt gate.
        let token = input
            .model
            .as_deref()
            .map(str::trim)
            .or_else(|| configured.default_model.as_deref().map(str::trim));
        match token {
            Some(token) => {
                if external_agent_dsh::acp::model::parse_colon_token(token).is_err() {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "dsh model must be provider:model, split at the first colon, with non-empty sides, at most 512 bytes, no NUL; prompt_count=0",
                    ));
                }
                if input.model.is_some() {
                    (Some(token.to_owned()), "spawn_catalog")
                } else {
                    (Some(token.to_owned()), "configured_default")
                }
            }
            None => (None, "native"),
        }
    } else if agent == "codex" {
        // Explicit submit model, then the configured default. Absence is
        // rejected: the daemon never silently picks a provider-default model.
        let token = input
            .model
            .as_deref()
            .map(str::trim)
            .or_else(|| configured.default_model.as_deref().map(str::trim));
        let Some(token) = token else {
            return Err(RpcError::new(
                RpcErrorCode::Validation,
                "codex requires an explicit model or agents.codex.default_model; prompt_count=0",
            ));
        };
        if token.is_empty() || token.len() > 128 || token.contains('\0') || token.contains('/') {
            return Err(RpcError::new(
                RpcErrorCode::Validation,
                "model token is not a bounded non-empty string",
            ));
        }
        if input.model.is_some() {
            (Some(token.to_owned()), "spawn_catalog")
        } else {
            (Some(token.to_owned()), "configured_default")
        }
    } else {
        (None, "native")
    };
    if agent == "dsh" {
        if !matches!(
            input.manifest.permission_mode,
            external_core::PermissionMode::Build | external_core::PermissionMode::Plan
        ) {
            return Err(RpcError::new(
                RpcErrorCode::AgentUnsupported,
                "dsh first-launch admission supports only the build and plan permission modes; prompt_count=0",
            ));
        }
        if !input.manifest.write_manifest.is_empty() {
            return Err(RpcError::new(
                RpcErrorCode::AgentUnsupported,
                "dsh first-launch admission requires the caller-empty write manifest; prompt_count=0",
            ));
        }
    }
    if agent == "codex" {
        // Codex admission is posture-pinned with approvalPolicy=never:
        // plan maps to sandbox=read-only and yolo maps to
        // sandbox=danger-full-access. build/edit are refused before the
        // prompt because the native workspace-write policy is not proven
        // equivalent to this daemon's protected workspace confinement.
        if !matches!(
            input.manifest.permission_mode,
            external_core::PermissionMode::Plan | external_core::PermissionMode::Yolo
        ) {
            return Err(RpcError::new(
                RpcErrorCode::AgentUnsupported,
                "codex admission supports only the plan and yolo permission modes; prompt_count=0",
            ));
        }
        // Home precedence is agents.codex.home over the inherited CODEX_HOME;
        // neither being present rejects before the prompt, never ~/.codex.
        if configured.home.is_none() && env::var_os("CODEX_HOME").is_none() {
            return Err(RpcError::new(
                RpcErrorCode::AgentUnsupported,
                "codex home is unconfigured; prompt_count=0",
            ));
        }
    }
    Ok(external_core::AdmissionIdentity {
        agent: agent.to_owned(),
        config_revision: config.revision,
        adapter_version: env!("CARGO_PKG_VERSION").into(),
        model,
        model_source: model_source.into(),
        effort,
    })
}

/// The reasoning-effort admission bound: 1..24 bytes of `[a-z0-9_]` with no
/// NUL. Codex additionally admits only its closed effort set {low, medium,
/// high, xhigh}. Evidence notes for the values left OUT (S02 handoff): the
/// wire enum on codex-cli 0.154.0 also serializes `minimal` (binary strings
/// adjacency), but every model in the OBSERVED probe catalog
/// (.agent-work/tmp/codex-app-server-probe/result-20260915-persistent.json,
/// models/result/data[*]/supportedReasoningEfforts) lists only low..xhigh
/// (some add the catalog tokens `max`/`ultra`, whose mapping onto the wire
/// enum is unverified), so minimal/max/ultra stay out until a live run
/// proves a model accepts them; zcode/dsh are bounded passthrough tokens
/// because their supported sets are only known at runtime and admission
/// must not fabricate a catalog.
fn resolve_effort_selection(
    agent: &str,
    input: &GeneralSubmitInput,
) -> Result<Option<String>, RpcError> {
    let Some(token) = input.effort.as_deref().map(str::trim) else {
        return Ok(None);
    };
    if token.is_empty()
        || token.len() > 24
        || token.contains('\0')
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            "effort token must be 1..24 bytes of lowercase [a-z0-9_] with no NUL; prompt_count=0",
        ));
    }
    if agent == "codex" && !matches!(token, "low" | "medium" | "high" | "xhigh") {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            "codex effort must be one of low, medium, high, xhigh; prompt_count=0",
        ));
    }
    Ok(Some(token.to_owned()))
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    fn input(repository: &Path) -> GeneralSubmitInput {
        GeneralSubmitInput {
            agent: Some("zcode".into()),
            model: None,
            effort: None,
            manifest: GeneralTaskManifest {
                schema: external_core::GENERAL_TASK_SCHEMA.into(),
                agent_id: "daemon-prepared".into(),
                repository: repository.into(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: "identity fixture".into(),
                write_manifest: vec![],
            },
        }
    }

    #[test]
    fn null_is_rejected_but_omission_round_trips() {
        let mut value = serde_json::to_value(input(Path::new("/repository"))).unwrap();
        value.as_object_mut().unwrap().remove("agent");
        assert!(value.get("model").is_none());
        let omitted: GeneralSubmitInput = serde_json::from_value(value.clone()).unwrap();
        assert!(omitted.agent.is_none() && omitted.model.is_none());
        assert_eq!(serde_json::to_value(omitted).unwrap(), value);
        for field in ["agent", "model"] {
            let mut invalid = value.clone();
            invalid[field] = serde_json::Value::Null;
            assert!(serde_json::from_value::<GeneralSubmitInput>(invalid).is_err());
        }
    }

    fn static_env_guard() -> &'static Mutex<()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn canonical_admission_precedence_and_dsh_gate() {
        let mut input = input(Path::new("/repository"));
        let mut config = AgentConfigSnapshot::default();
        input.agent = None;
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::AgentRequired
        );
        input.agent = Some("unknown".into());
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::AgentUnknown
        );
        input.agent = Some("dsh".into());
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::AgentDisabled
        );
        config.subagents.get_mut("dsh").unwrap().enabled = true;
        config.subagents.get_mut("dsh").unwrap().spawn_supported = true;
        config.subagents.get_mut("dsh").unwrap().profile = Some("acp".into());
        config.subagents.get_mut("dsh").unwrap().version =
            Some(external_agent_dsh::profile::PINNED_DSH_VERSION.into());
        input.model = Some("opaque-provider:opaque-token".into());
        let env_guard = static_env_guard().lock().unwrap();
        let previous_runtime = env::var_os("DSH_RUNTIME_PATH");
        env::set_var("DSH_RUNTIME_PATH", "relative/runtime");
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::AgentUnsupported
        );
        let runtime = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = runtime.as_file().metadata().unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(runtime.path(), permissions).unwrap();
        }
        env::set_var("DSH_RUNTIME_PATH", runtime.path());
        config.subagents.get_mut("dsh").unwrap().runtime_path =
            Some(runtime.path().to_string_lossy().into_owned());
        let identity = resolve_admission(&input, &config).unwrap();
        assert_eq!(identity.agent, "dsh");
        assert_eq!(
            identity.model.as_deref(),
            Some("opaque-provider:opaque-token")
        );
        assert_eq!(identity.model_source, "spawn_catalog");
        match previous_runtime {
            Some(value) => env::set_var("DSH_RUNTIME_PATH", value),
            None => env::remove_var("DSH_RUNTIME_PATH"),
        }
        drop(env_guard);
        input.agent = Some("zcode".into());
        config.subagents.get_mut("zcode").unwrap().enabled = true;
        input.model = Some("model".into());
        config.subagents.get_mut("zcode").unwrap().spawn_supported = false;
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::ModelSelectionUnsupported
        );
        input.model = Some(" ".into());
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::Validation
        );
    }

    #[test]
    fn dsh_admission_rejects_incomplete_persisted_identity() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("runtime");
        std::fs::write(&runtime, b"runtime").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = std::fs::metadata(&runtime).unwrap().permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(&runtime, mode).unwrap();
        }
        let mut config = AgentConfigSnapshot::default();
        let dsh = config.subagents.get_mut("dsh").unwrap();
        dsh.enabled = true;
        dsh.spawn_supported = true;
        dsh.runtime_path = Some(runtime.to_string_lossy().into_owned());
        dsh.profile = Some("acp".into());
        dsh.version = Some(external_agent_dsh::profile::PINNED_DSH_VERSION.into());
        let input = GeneralSubmitInput {
            agent: Some("dsh".into()),
            model: None,
            effort: None,
            manifest: GeneralTaskManifest {
                schema: external_core::GENERAL_TASK_SCHEMA.into(),
                agent_id: "gate-test".into(),
                repository: directory.path().into(),
                permission_mode: external_core::PermissionMode::Build,
                prompt: "test".into(),
                write_manifest: vec![],
            },
        };
        assert_eq!(resolve_admission(&input, &config).unwrap().agent, "dsh");
        config.subagents.get_mut("dsh").unwrap().version = None;
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::AgentUnsupported
        );
    }

    fn codex_gate_config(directory: &std::path::Path) -> AgentConfigSnapshot {
        let runtime = directory.join("codex-runtime");
        std::fs::write(&runtime, b"runtime").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = std::fs::metadata(&runtime).unwrap().permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(&runtime, mode).unwrap();
        }
        let mut config = AgentConfigSnapshot::default();
        let codex = config.subagents.get_mut("codex").unwrap();
        codex.enabled = true;
        codex.spawn_supported = true;
        codex.runtime_path = Some(runtime.to_string_lossy().into_owned());
        codex.home = Some(directory.join("codex-home").to_string_lossy().into_owned());
        config
    }

    fn codex_input(
        directory: &std::path::Path,
        mode: external_core::PermissionMode,
    ) -> GeneralSubmitInput {
        GeneralSubmitInput {
            agent: Some("codex".into()),
            model: Some("gpt-5.6-terra".into()),
            effort: None,
            manifest: GeneralTaskManifest {
                schema: external_core::GENERAL_TASK_SCHEMA.into(),
                agent_id: "codex-gate-test".into(),
                repository: directory.into(),
                permission_mode: mode,
                prompt: "test".into(),
                write_manifest: vec![],
            },
        }
    }

    #[test]
    fn codex_admission_accepts_plan_and_yolo_with_a_model_and_configured_home() {
        let directory = tempfile::tempdir().unwrap();
        let config = codex_gate_config(directory.path());
        let input = codex_input(directory.path(), external_core::PermissionMode::Plan);
        let identity = resolve_admission(&input, &config).unwrap();
        assert_eq!(identity.agent, "codex");
        assert_eq!(identity.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(identity.model_source, "spawn_catalog");

        // Yolo admits too: it pins sandbox=danger-full-access with
        // approvalPolicy=never at the thread boundary.
        assert_eq!(
            resolve_admission(
                &codex_input(directory.path(), external_core::PermissionMode::Yolo),
                &config
            )
            .unwrap()
            .agent,
            "codex"
        );

        // The configured default is an equally valid selection.
        let mut default_model_input =
            codex_input(directory.path(), external_core::PermissionMode::Plan);
        default_model_input.model = None;
        let mut defaulted = config.clone();
        defaulted.subagents.get_mut("codex").unwrap().default_model = Some("gpt-5.6-terra".into());
        let identity = resolve_admission(&default_model_input, &defaulted).unwrap();
        assert_eq!(identity.model_source, "configured_default");

        // Absent model is rejected instead of silently choosing a provider default.
        let unconfigured = config.clone();
        let error = resolve_admission(&default_model_input, &unconfigured).unwrap_err();
        assert_eq!(error.code, RpcErrorCode::Validation);
        assert!(error.message.contains("agents.codex.default_model"));

        // The unproven write modes are refused before the prompt.
        for mode in [
            external_core::PermissionMode::Build,
            external_core::PermissionMode::Edit,
        ] {
            let error =
                resolve_admission(&codex_input(directory.path(), mode), &config).unwrap_err();
            assert_eq!(error.code, RpcErrorCode::AgentUnsupported);
            assert!(error
                .message
                .contains("only the plan and yolo permission modes"));
        }

        // No home from either source rejects before the prompt.
        let env_guard = static_env_guard().lock().unwrap();
        let previous_home = env::var_os("CODEX_HOME");
        env::remove_var("CODEX_HOME");
        let mut homeless = config.clone();
        homeless.subagents.get_mut("codex").unwrap().home = None;
        let error = resolve_admission(
            &codex_input(directory.path(), external_core::PermissionMode::Plan),
            &homeless,
        )
        .unwrap_err();
        assert_eq!(error.code, RpcErrorCode::AgentUnsupported);
        assert!(error.message.contains("codex home is unconfigured"));
        // An inherited CODEX_HOME is the accepted second-priority source.
        env::set_var("CODEX_HOME", "/inherited/codex-home");
        assert!(resolve_admission(
            &codex_input(directory.path(), external_core::PermissionMode::Plan),
            &homeless
        )
        .is_ok());
        match previous_home {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        drop(env_guard);

        // The runtime-path gate still fails closed.
        let mut ungated = config.clone();
        ungated.subagents.get_mut("codex").unwrap().runtime_path = None;
        assert_eq!(
            resolve_admission(
                &codex_input(directory.path(), external_core::PermissionMode::Plan),
                &ungated
            )
            .unwrap_err()
            .code,
            RpcErrorCode::AgentUnsupported
        );

        // Capability projection is plan+yolo over the codex transport.
        let evidence = AgentEvidenceStore::new(None);
        let status = configured_agent_statuses(&config, &evidence)
            .into_iter()
            .find(|status| status.agent == "codex")
            .unwrap();
        assert_eq!(
            status.transport_support.transport,
            AgentTransportView::CodexAppServer
        );
        assert!(status.transport_support.spawn);
        assert_eq!(
            status.permission_modes,
            vec![AgentPermissionModeView::Plan, AgentPermissionModeView::Yolo]
        );
        assert_eq!(
            status.model_selection,
            AgentModelSelectionCapabilityView {
                supported: true,
                mode: AgentModelSelectionModeView::CatalogToken,
            }
        );
    }

    #[test]
    fn effort_admission_bounds_tokens_and_pins_the_codex_closed_set() {
        let directory = tempfile::tempdir().unwrap();
        let config = codex_gate_config(directory.path());
        let mut input = codex_input(directory.path(), external_core::PermissionMode::Plan);
        for admitted in ["low", "medium", "high", "xhigh"] {
            input.effort = Some(admitted.into());
            assert_eq!(
                resolve_admission(&input, &config)
                    .unwrap()
                    .effort
                    .as_deref(),
                Some(admitted),
                "codex effort {admitted} must admit"
            );
        }
        input.effort = None;
        assert_eq!(resolve_admission(&input, &config).unwrap().effort, None);
        // Tokens trim exactly like the model selection: surrounding blanks
        // never reject an otherwise bounded closed-set value.
        input.effort = Some(" high ".into());
        assert_eq!(
            resolve_admission(&input, &config)
                .unwrap()
                .effort
                .as_deref(),
            Some("high")
        );
        // `minimal` serializes on the codex wire enum but no OBSERVED model
        // catalog lists it, and `max` is a catalog token whose wire mapping
        // is unverified — both stay outside the admitted closed set.
        for invalid in [
            "minimal",
            "max",
            "ultra",
            "HIGH",
            "hi gh",
            "",
            "t".repeat(25).as_str(),
            "high\0",
        ] {
            input.effort = Some(invalid.into());
            let error = resolve_admission(&input, &config).unwrap_err();
            assert_eq!(
                error.code,
                RpcErrorCode::Validation,
                "codex effort {invalid:?}"
            );
            assert!(error.message.contains("prompt_count=0"));
        }
    }

    #[test]
    fn zcode_and_dsh_effort_admits_bounded_passthrough_tokens() {
        // zcode keeps its default admission: an unknown but well-formed value
        // passes through instead of being checked against a fabricated catalog.
        let mut input = input(Path::new("/repository"));
        let mut config = AgentConfigSnapshot::default();
        config.subagents.get_mut("zcode").unwrap().enabled = true;
        config.subagents.get_mut("zcode").unwrap().spawn_supported = true;
        for passthrough in ["high", "turbo_deep", "v9_max", "t".repeat(24).as_str()] {
            input.effort = Some(passthrough.into());
            assert_eq!(
                resolve_admission(&input, &config)
                    .unwrap()
                    .effort
                    .as_deref(),
                Some(passthrough),
                "zcode effort {passthrough} must pass through"
            );
        }
        for invalid in [
            "High",
            "hi gh",
            "",
            "t".repeat(25).as_str(),
            "max-effort",
            "effort\0",
        ] {
            input.effort = Some(invalid.into());
            assert_eq!(
                resolve_admission(&input, &config).unwrap_err().code,
                RpcErrorCode::Validation,
                "zcode effort {invalid:?}"
            );
        }
        input.effort = None;
        assert_eq!(resolve_admission(&input, &config).unwrap().effort, None);
        // dsh passes through the same non-codex branch once its production
        // gate admits the task, so the test name carries a real dsh case.
        let root = admission_fixtures::gated_dsh_config(None);
        let dsh_config = admission_fixtures::gated_dsh_snapshot(&root);
        let mut dsh_input = input.clone();
        dsh_input.agent = Some("dsh".into());
        dsh_input.effort = Some("high".into());
        assert_eq!(
            resolve_admission(&dsh_input, &dsh_config)
                .unwrap()
                .effort
                .as_deref(),
            Some("high"),
            "dsh effort must pass through the gated config"
        );
    }

    #[test]
    fn effort_selection_capability_follows_the_spawn_gate_per_agent() {
        let directory = tempfile::tempdir().unwrap();
        let config = codex_gate_config(directory.path());
        let evidence = AgentEvidenceStore::new(None);
        let statuses = configured_agent_statuses(&config, &evidence);
        let codex = statuses.iter().find(|s| s.agent == "codex").unwrap();
        assert_eq!(
            codex.effort_selection,
            AgentEffortSelectionCapabilityView {
                supported: true,
                mode: AgentEffortSelectionModeView::ClosedSet,
            }
        );
        let zcode = statuses.iter().find(|s| s.agent == "zcode").unwrap();
        assert_eq!(
            zcode.effort_selection,
            AgentEffortSelectionCapabilityView {
                supported: false,
                mode: AgentEffortSelectionModeView::PassthroughToken,
            }
        );
        // dsh stays passthrough too, but unsupported until the spawn gate opens.
        let dsh = statuses.iter().find(|s| s.agent == "dsh").unwrap();
        assert_eq!(
            dsh.effort_selection,
            AgentEffortSelectionCapabilityView {
                supported: false,
                mode: AgentEffortSelectionModeView::PassthroughToken,
            }
        );
        // The unavailable projection keeps the same modes with support off.
        for status in unavailable_agent_statuses() {
            assert!(!status.effort_selection.supported, "{}", status.agent);
        }
    }

    #[test]
    fn admitted_effort_is_persisted_and_projected_into_input_identity() {
        let (directory, service, previous_id) = wait_tests::fixture();
        service
            .store
            .store_task_result(
                &previous_id,
                &external_store::TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "done".into(),
                    partial: false,
                },
            )
            .unwrap();
        let mut input = input(directory.path());
        input.effort = Some("high".into());
        let mut config = AgentConfigSnapshot::default();
        config.revision = 41;
        config.subagents.get_mut("zcode").unwrap().enabled = true;
        config.subagents.get_mut("zcode").unwrap().spawn_supported = true;
        let identity = resolve_admission(&input, &config).unwrap();
        assert_eq!(identity.effort.as_deref(), Some("high"));
        let task = service
            .scheduler
            .enqueue_general_with_admission(&input.manifest, Some(identity.clone()))
            .unwrap();
        let reopened = Store::open(directory.path().join("state.sqlite")).unwrap();
        let stored = reopened.get_task(&task.agent_id).unwrap().unwrap();
        let prepared: external_core::PreparedGeneralTask =
            serde_json::from_str(&stored.prepared_launch_json).unwrap();
        prepared.validate_digest().unwrap();
        assert_eq!(prepared.admission.as_ref(), Some(&identity));
        assert!(
            stored.prepared_launch_json.contains("\"effort\":\"high\""),
            "effort must be persisted inside admission: {}",
            stored.prepared_launch_json
        );
        assert_eq!(
            flat_identity(&task_view(stored).input_identity),
            Some(identity)
        );
    }

    #[test]
    fn admission_snapshot_survives_config_change_reopen_and_filtered_pagination() {
        let (directory, service, previous_id) = wait_tests::fixture();
        service
            .store
            .store_task_result(
                &previous_id,
                &external_store::TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "done".into(),
                    partial: false,
                },
            )
            .unwrap();
        let input = input(directory.path());
        let mut config = AgentConfigSnapshot::default();
        config.revision = 41;
        config.subagents.get_mut("zcode").unwrap().enabled = true;
        config.subagents.get_mut("zcode").unwrap().spawn_supported = true;
        let identity = resolve_admission(&input, &config).unwrap();
        let task = service
            .scheduler
            .enqueue_general_with_admission(&input.manifest, Some(identity.clone()))
            .unwrap();
        config.revision = 42;
        config.subagents.get_mut("zcode").unwrap().enabled = false;
        assert!(resolve_admission(&input, &config).is_err());
        let reopened = Store::open(directory.path().join("state.sqlite")).unwrap();
        let stored = reopened.get_task(&task.agent_id).unwrap().unwrap();
        let prepared: external_core::PreparedGeneralTask =
            serde_json::from_str(&stored.prepared_launch_json).unwrap();
        prepared.validate_digest().unwrap();
        assert_eq!(prepared.admission.as_ref(), Some(&identity));
        assert_eq!(
            flat_identity(&task_view(stored).input_identity),
            Some(identity)
        );
        let query = |agent: &str| TaskListQuery {
            agent: Some(agent.into()),
            repository: Some(directory.path().to_string_lossy().into_owned()),
            phase: None,
            outcome: None,
            cursor: None,
            limit: 1,
        };
        let RpcSuccess::TaskListed { tasks, next_cursor } = service
            .dispatch(RpcMethod::TaskList(query("zcode")))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].agent_id, task.agent_id);
        assert!(next_cursor.is_none()); // Unclassified older task does not occupy a filtered page.
        let RpcSuccess::TaskListed { tasks, .. } =
            service.dispatch(RpcMethod::TaskList(query("dsh"))).unwrap()
        else {
            panic!()
        };
        assert!(tasks.is_empty());
    }
}

/// Shared fixtures for the admission policy oracles. The environment guard
/// serializes tests that install a process-wide agent-config file with every
/// dispatch-based test that reads the configuration through the service.
#[cfg(test)]
pub(crate) mod admission_fixtures {
    use super::*;

    pub(crate) fn config_env_guard() -> std::sync::MutexGuard<'static, ()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Installs `EXTERNAL_SUBAGENT_CONFIG` for the guard-held scope and
    /// restores the previous process environment on drop.
    pub(crate) struct ConfigEnvScope {
        previous: Option<std::ffi::OsString>,
    }

    impl ConfigEnvScope {
        pub(crate) fn install(path: &Path) -> Self {
            let previous = env::var_os("EXTERNAL_SUBAGENT_CONFIG");
            env::set_var("EXTERNAL_SUBAGENT_CONFIG", path);
            Self { previous }
        }
    }

    impl Drop for ConfigEnvScope {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => env::set_var("EXTERNAL_SUBAGENT_CONFIG", value),
                None => env::remove_var("EXTERNAL_SUBAGENT_CONFIG"),
            }
        }
    }

    /// Fresh workspace root for admission oracles, kept under the
    /// repository's prescribed live-agent scratch directory.
    pub(crate) fn admission_root(prefix: &str) -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(root)
            .unwrap()
    }

    /// Writes an on-disk agent configuration whose DSH entry satisfies the
    /// complete production spawn gate (enabled + spawn_supported + pinned
    /// profile, version, and an executable absolute runtime).
    pub(crate) fn gated_dsh_config(default_model: Option<&str>) -> tempfile::TempDir {
        let root = admission_root("s04a-admission-config-");
        let runtime = root.path().join("dsh-runtime");
        fs::write(&runtime, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&runtime).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&runtime, permissions).unwrap();
        }
        let config = serde_json::json!({
            "schema_version": 2,
            "revision": 7,
            "subagents": {
                "zcode": {"enabled": true, "spawn_supported": true, "default_model": null},
                "dsh": {
                    "enabled": true,
                    "spawn_supported": true,
                    "default_model": default_model,
                    "runtime_path": runtime.to_string_lossy(),
                    "home": null,
                    "profile": "acp",
                    "version": external_agent_dsh::profile::PINNED_DSH_VERSION,
                }
            }
        });
        fs::write(
            root.path().join("agents.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        root
    }

    pub(crate) fn gated_dsh_snapshot(root: &tempfile::TempDir) -> AgentConfigSnapshot {
        serde_json::from_str::<AgentConfigSnapshot>(
            &fs::read_to_string(root.path().join("agents.json")).unwrap(),
        )
        .unwrap()
    }
}

#[cfg(test)]
mod admission_policy_tests {
    use super::admission_fixtures::{
        admission_root, config_env_guard, gated_dsh_config, gated_dsh_snapshot, ConfigEnvScope,
    };
    use super::*;

    fn policy_input(
        agent: Option<&str>,
        model: Option<&str>,
        permission_mode: external_core::PermissionMode,
        write_manifest: &[&str],
    ) -> GeneralSubmitInput {
        GeneralSubmitInput {
            agent: agent.map(str::to_owned),
            model: model.map(str::to_owned),
            effort: None,
            manifest: external_core::GeneralTaskManifest {
                schema: external_core::GENERAL_TASK_SCHEMA.into(),
                agent_id: "admission-oracle".into(),
                repository: PathBuf::from("/admission-oracle-repository"),
                permission_mode,
                prompt: "admission oracle".into(),
                write_manifest: write_manifest.iter().map(PathBuf::from).collect(),
            },
        }
    }

    #[test]
    fn dsh_model_precedence_is_spawn_then_configured_default_then_native() {
        let root = gated_dsh_config(Some("configured-provider:configured-default-token"));
        let config = gated_dsh_snapshot(&root);

        let explicit = policy_input(
            Some("dsh"),
            Some("  spawn-provider:spawn-token  "),
            external_core::PermissionMode::Build,
            &[],
        );
        let identity = resolve_admission(&explicit, &config).unwrap();
        assert_eq!(identity.agent, "dsh");
        assert_eq!(
            identity.model.as_deref(),
            Some("spawn-provider:spawn-token")
        );
        assert_eq!(identity.model_source, "spawn_catalog");

        let fallback = policy_input(Some("dsh"), None, external_core::PermissionMode::Build, &[]);
        let identity = resolve_admission(&fallback, &config).unwrap();
        assert_eq!(
            identity.model.as_deref(),
            Some("configured-provider:configured-default-token")
        );
        assert_eq!(identity.model_source, "configured_default");

        let native_root = gated_dsh_config(None);
        let native_config = gated_dsh_snapshot(&native_root);
        let omitted = policy_input(Some("dsh"), None, external_core::PermissionMode::Plan, &[]);
        let identity = resolve_admission(&omitted, &native_config).unwrap();
        assert_eq!(identity.model, None);
        assert_eq!(identity.model_source, "native");
    }

    #[test]
    fn dsh_invalid_resolved_model_token_is_refused_before_the_task_exists() {
        let root = gated_dsh_config(Some("configured-provider:configured-default-token"));
        let config = gated_dsh_snapshot(&root);
        // Malformed colon tokens — no colon, empty provider, empty model — are
        // refused at admission, not persisted and re-discovered at spawn.
        for invalid in ["no-colon", "provider:", ":model"] {
            let input = policy_input(
                Some("dsh"),
                Some(invalid),
                external_core::PermissionMode::Build,
                &[],
            );
            let error = resolve_admission(&input, &config).unwrap_err();
            assert_eq!(error.code, RpcErrorCode::Validation, "{invalid:?}");
            assert!(error.message.contains("provider:model"), "{invalid:?}");
            assert!(error.message.contains("prompt_count=0"), "{invalid:?}");
        }
        // A spawn token beyond the colon-token bound the ACP session enforces
        // must fail admission, not the task after it is persisted.
        let oversized = policy_input(
            Some("dsh"),
            Some(&format!("p:{}", "t".repeat(513))),
            external_core::PermissionMode::Build,
            &[],
        );
        assert_eq!(
            resolve_admission(&oversized, &config).unwrap_err().code,
            RpcErrorCode::Validation
        );
        // The configured default is a selection too: an unusable default is
        // refused before prompt rather than silently downgraded to native.
        let mut invalid_default = config.clone();
        invalid_default
            .subagents
            .get_mut("dsh")
            .unwrap()
            .default_model = Some("provider:".into());
        let defaulted = policy_input(Some("dsh"), None, external_core::PermissionMode::Build, &[]);
        assert_eq!(
            resolve_admission(&defaulted, &invalid_default)
                .unwrap_err()
                .code,
            RpcErrorCode::Validation
        );
        // The bound itself stays exact: the largest bounded colon token admits.
        let bounded_token = format!("p:{}", "t".repeat(510));
        let bounded = policy_input(
            Some("dsh"),
            Some(&bounded_token),
            external_core::PermissionMode::Build,
            &[],
        );
        assert_eq!(
            resolve_admission(&bounded, &config)
                .unwrap()
                .model
                .as_deref(),
            Some(bounded_token.as_str())
        );
    }

    #[test]
    fn dsh_first_launch_scope_rejects_unproven_modes_and_exact_manifests() {
        let root = gated_dsh_config(Some("configured-provider:configured-default-token"));
        let config = gated_dsh_snapshot(&root);
        for mode in [
            external_core::PermissionMode::Edit,
            external_core::PermissionMode::Yolo,
        ] {
            let input = policy_input(Some("dsh"), None, mode, &[]);
            assert_eq!(
                resolve_admission(&input, &config).unwrap_err().code,
                RpcErrorCode::AgentUnsupported,
                "dsh must refuse {mode:?} before prompt"
            );
        }
        for mode in [
            external_core::PermissionMode::Build,
            external_core::PermissionMode::Plan,
        ] {
            let input = policy_input(Some("dsh"), None, mode, &["src/main.rs"]);
            assert_eq!(
                resolve_admission(&input, &config).unwrap_err().code,
                RpcErrorCode::AgentUnsupported,
                "dsh must refuse an exact caller write manifest in {mode:?}"
            );
        }
        // The admitted forms remain exactly build with the caller-empty
        // write manifest (the generic layer derives the protected workspace
        // scope from it) and strict plan.
        for mode in [
            external_core::PermissionMode::Build,
            external_core::PermissionMode::Plan,
        ] {
            let input = policy_input(Some("dsh"), None, mode, &[]);
            assert_eq!(resolve_admission(&input, &config).unwrap().agent, "dsh");
        }
    }

    /// The exact `submit_general` wire frame the CLI emits over the daemon
    /// socket, so the oracle pins the public RPC entrypoint itself.
    fn cli_submit_frame(
        request_id: &str,
        agent: &str,
        model: Option<&str>,
        repository: &Path,
        mode: &str,
        write_manifest: &[&str],
    ) -> Vec<u8> {
        let mut params = serde_json::Map::new();
        params.insert("agent".into(), serde_json::json!(agent));
        if let Some(model) = model {
            params.insert("model".into(), serde_json::json!(model));
        }
        params.insert(
            "manifest".into(),
            serde_json::json!({
                "schema": external_core::GENERAL_TASK_SCHEMA,
                "agent_id": "cli-admission-oracle",
                "repository": repository.to_string_lossy(),
                "permission_mode": mode,
                "prompt": "cli admission oracle",
                "write_manifest": write_manifest,
            }),
        );
        serde_json::to_vec(&serde_json::json!({
            "request_id": request_id,
            "method": "submit_general",
            "params": params,
        }))
        .unwrap()
    }

    fn submit(
        service: &RpcService,
        request_id: &str,
        agent: &str,
        model: Option<&str>,
        repository: &Path,
        mode: &str,
        write_manifest: &[&str],
    ) -> RpcResponse {
        let frame = cli_submit_frame(request_id, agent, model, repository, mode, write_manifest);
        service.handle_bytes(&frame)
    }

    fn scoped_task_count(store: &Store, repository: &Path) -> usize {
        let repository = repository.canonicalize().unwrap();
        let repository = repository.to_string_lossy().into_owned();
        store
            .list_task_page(
                TaskQueryScope {
                    repository: Some(repository.as_str()),
                },
                TaskPageFilter {
                    agent: None,
                    phase: None,
                    outcome: None,
                },
                None,
                MAX_LIST_TASKS,
            )
            .unwrap()
            .tasks
            .len()
    }

    #[test]
    fn cli_rpc_entrypoint_enforces_dsh_scope_before_any_task_or_prompt() {
        let _env_guard = config_env_guard();
        let config_root = gated_dsh_config(Some("configured-provider:configured-default-token"));
        let _config_env = ConfigEnvScope::install(&config_root.path().join("agents.json"));
        let (_directory, service, _id) = wait_tests::fixture();
        let store = service.store_for_wait_test();
        let workspace = admission_root("s04a-cli-dsh-reject-");

        for (mode, manifest, request_id) in [
            ("edit", Vec::new(), "dsh-edit"),
            ("yolo", Vec::new(), "dsh-yolo"),
            ("build", vec!["src/main.rs"], "dsh-build-exact"),
            ("plan", vec!["src/main.rs"], "dsh-plan-exact"),
        ] {
            let response = submit(
                &service,
                request_id,
                "dsh",
                None,
                workspace.path(),
                mode,
                &manifest,
            );
            let RpcOutcome::Error { error } = response.outcome else {
                panic!("{request_id} must be rejected before prompt")
            };
            assert_eq!(error.code, RpcErrorCode::AgentUnsupported, "{request_id}");
            assert!(
                error.message.contains("prompt_count=0"),
                "{request_id}: {}",
                error.message
            );
            let wire =
                serde_json::to_value(&RpcResponse::error(Some(request_id.into()), error)).unwrap();
            assert_eq!(wire["error"]["code"], "agent_unsupported", "{request_id}");
        }
        assert_eq!(scoped_task_count(&store, workspace.path()), 0);
    }

    #[test]
    fn cli_rpc_entrypoint_admits_dsh_build_and_persists_the_resolved_identity() {
        let _env_guard = config_env_guard();
        let config_root = gated_dsh_config(Some("configured-provider:configured-default-token"));
        let _config_env = ConfigEnvScope::install(&config_root.path().join("agents.json"));
        let (directory, service, _id) = wait_tests::fixture();
        let store = service.store_for_wait_test();
        let spawn_root = admission_root("s04a-cli-dsh-admit-");

        let explicit_workspace = spawn_root.path().join("explicit");
        fs::create_dir(&explicit_workspace).unwrap();
        let response = submit(
            &service,
            "dsh-explicit",
            "dsh",
            Some("spawn-provider:spawn-token"),
            &explicit_workspace,
            "build",
            &[],
        );
        let RpcOutcome::Success { result } = response.outcome else {
            panic!("gated dsh build with the caller-empty manifest must admit")
        };
        let RpcSuccess::GeneralSubmitted { task, .. } = *result else {
            panic!("expected a submitted task")
        };
        let explicit_identity = flat_identity(&task.input_identity).unwrap();
        assert_eq!(
            explicit_identity.model.as_deref(),
            Some("spawn-provider:spawn-token")
        );
        assert_eq!(explicit_identity.model_source, "spawn_catalog");

        let default_workspace = spawn_root.path().join("default");
        fs::create_dir(&default_workspace).unwrap();
        let response = submit(
            &service,
            "dsh-default",
            "dsh",
            None,
            &default_workspace,
            "build",
            &[],
        );
        let RpcOutcome::Success { result } = response.outcome else {
            panic!("dsh build without a spawn model must fall back to the configured default")
        };
        let RpcSuccess::GeneralSubmitted {
            task: default_task, ..
        } = *result
        else {
            panic!("expected a submitted task")
        };
        let default_identity = flat_identity(&default_task.input_identity).unwrap();
        assert_eq!(
            default_identity.model.as_deref(),
            Some("configured-provider:configured-default-token")
        );
        assert_eq!(default_identity.model_source, "configured_default");

        // The resolved selection is immutable task identity: reopening the
        // store keeps it inside the validated prepared digest, and the
        // caller-empty build manifest was expanded to the protected
        // workspace scope instead of being rejected.
        let reopened = Store::open(directory.path().join("state.sqlite")).unwrap();
        for (agent_id, identity) in [
            (&task.agent_id, &explicit_identity),
            (&default_task.agent_id, &default_identity),
        ] {
            let stored = reopened.get_task(agent_id).unwrap().unwrap();
            let prepared: external_core::PreparedGeneralTask =
                serde_json::from_str(&stored.prepared_launch_json).unwrap();
            prepared.validate_digest().unwrap();
            assert_eq!(prepared.admission.as_ref(), Some(identity));
            assert_eq!(prepared.write_manifest, vec![PathBuf::from(".")]);
        }
        assert_eq!(scoped_task_count(&store, &explicit_workspace), 1);
    }

    #[test]
    fn status_publishes_dsh_first_launch_permission_modes_only() {
        let _env_guard = config_env_guard();
        let config_root = gated_dsh_config(Some("configured-provider:configured-default-token"));
        let _config_env = ConfigEnvScope::install(&config_root.path().join("agents.json"));
        let (_directory, service, _id) = wait_tests::fixture();
        let RpcSuccess::SystemStatus { status } =
            service.dispatch(RpcMethod::SystemStatus).unwrap()
        else {
            panic!("expected status")
        };
        let dsh = status
            .agents
            .iter()
            .find(|agent| agent.agent == "dsh")
            .unwrap();
        assert!(dsh.spawn_supported);
        assert_eq!(
            dsh.permission_modes,
            vec![
                AgentPermissionModeView::Build,
                AgentPermissionModeView::Plan
            ]
        );
        let zcode = status
            .agents
            .iter()
            .find(|agent| agent.agent == "zcode")
            .unwrap();
        assert_eq!(
            zcode.permission_modes,
            vec![
                AgentPermissionModeView::Build,
                AgentPermissionModeView::Edit,
                AgentPermissionModeView::Plan,
                AgentPermissionModeView::Yolo,
            ]
        );
    }

    #[test]
    fn zcode_admission_capabilities_are_unchanged() {
        // Explicit enable preserves all four modes and caller manifests.
        let _env_guard = config_env_guard();
        let config_root = gated_dsh_config(None);
        let _config_env = ConfigEnvScope::install(&config_root.path().join("agents.json"));
        let (directory, service, _id) = wait_tests::fixture();
        let zcode_root = admission_root("s04a-cli-zcode-");
        for (request_id, mode, manifest) in [
            ("zcode-edit-exact", "edit", vec!["src/zone.rs"]),
            ("zcode-yolo", "yolo", Vec::new()),
            ("zcode-build-exact", "build", vec!["docs/notes.md"]),
        ] {
            let workspace = zcode_root.path().join(request_id);
            fs::create_dir(&workspace).unwrap();
            let response = submit(
                &service, request_id, "zcode", None, &workspace, mode, &manifest,
            );
            let RpcOutcome::Success { result } = response.outcome else {
                panic!("zcode {mode} must keep its existing admission")
            };
            let RpcSuccess::GeneralSubmitted { task, .. } = *result else {
                panic!("expected a submitted task")
            };
            let identity = flat_identity(&task.input_identity).unwrap();
            assert_eq!(identity.agent, "zcode");
            assert_eq!(identity.model, None);
            assert_eq!(identity.model_source, "native");
        }
        let build_workspace = zcode_root.path().join("zcode-build-empty");
        fs::create_dir(&build_workspace).unwrap();
        let response = submit(
            &service,
            "zcode-build-empty",
            "zcode",
            None,
            &build_workspace,
            "build",
            &[],
        );
        let RpcOutcome::Success { result } = response.outcome else {
            panic!("zcode build with an empty manifest must admit")
        };
        let RpcSuccess::GeneralSubmitted { task, .. } = *result else {
            panic!("expected a submitted task")
        };
        let reopened = Store::open(directory.path().join("state.sqlite")).unwrap();
        let stored = reopened.get_task(&task.agent_id).unwrap().unwrap();
        let prepared: external_core::PreparedGeneralTask =
            serde_json::from_str(&stored.prepared_launch_json).unwrap();
        assert_eq!(prepared.write_manifest, vec![PathBuf::from(".")]);

        // ZCode model selection stays explicitly refused before prompt.
        let response = submit(
            &service,
            "zcode-model",
            "zcode",
            Some("glm-5.3"),
            &build_workspace,
            "build",
            &[],
        );
        let RpcOutcome::Error { error } = response.outcome else {
            panic!("zcode model selection must be refused")
        };
        assert_eq!(error.code, RpcErrorCode::ModelSelectionUnsupported);
        assert!(error.message.contains("prompt_count=0"));
    }
}
