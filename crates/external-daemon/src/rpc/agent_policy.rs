//! Per-agent admission policy: the single owner of every per-subagent fact.
//!
//! `AgentKind` is the closed subagent roster and `AgentPolicy` its table row.
//! Every production dispatch that used to re-test agent-name literals — status
//! derivation and admission in `rpc::agents`, the named method faces in
//! `rpc::handlers`, the probe/catalog dispatch in `crate::agent_status`, and
//! the factual claims in the MCP spawn description (locked by a contract test)
//! — reads the fact from this table. Adding an agent or changing one capability
//! is one row edit here; the derived faces follow.
//!
//! The table carries only facts, not behavior: gateway predicates and status
//! projections live beside it as small functions parameterized by these rows.
//! Error codes and message text stay at the call sites that own them, and the
//! table stores the exact published refusal sentinel where a fact is also a
//! public message.

use super::config::AgentConfigEntry;
use super::views::{
    AgentEffortSelectionCapabilityView, AgentEffortSelectionModeView,
    AgentModelSelectionCapabilityView, AgentModelSelectionModeView, AgentPermissionModeView,
    AgentTransportSupportView, AgentTransportView,
};
use external_core::PermissionMode;

/// The closed subagent roster. Order is the historical status projection order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentKind {
    Zcode,
    Dsh,
    Codex,
    Agy,
}

impl AgentKind {
    pub(crate) const ALL: [Self; 4] = [Self::Zcode, Self::Dsh, Self::Codex, Self::Agy];

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Zcode => "zcode",
            Self::Dsh => "dsh",
            Self::Codex => "codex",
            Self::Agy => "agy",
        }
    }

    pub(crate) fn policy(self) -> &'static AgentPolicy {
        match self {
            Self::Zcode => &ZCODE,
            Self::Dsh => &DSH,
            Self::Codex => &CODEX,
            Self::Agy => &AGY,
        }
    }
}

/// The policy row for a name, or `None` for a name outside the roster.
///
/// Production paths only ever see roster names (the config parser rejects
/// anything else and the handlers answer `agent_unknown` first). The `None`
/// arm exists for directly constructed config snapshots and keeps the
/// historical fallback behavior at each call site.
pub(crate) fn policy_for(name: &str) -> Option<&'static AgentPolicy> {
    AgentKind::from_name(name).map(AgentKind::policy)
}

/// One row of the per-agent admission policy.
pub(crate) struct AgentPolicy {
    pub(crate) kind: AgentKind,
    pub(crate) transport: AgentTransportView,
    pub(crate) spawn_gate: SpawnGate,
    /// The admitted permission-mode set, in status projection order.
    pub(crate) permission_modes: &'static [PermissionMode],
    /// The exact `agent_unsupported` message for a mode outside
    /// `permission_modes`. `None` means every mode is admitted.
    pub(crate) permission_refusal: Option<&'static str>,
    pub(crate) model_shape: ModelShape,
    pub(crate) write_manifest: WriteManifestPolicy,
    pub(crate) home: HomeRequirement,
    pub(crate) effort: EffortPolicy,
    pub(crate) developer_instructions: DeveloperInstructionsChannel,
    pub(crate) expected_version: ExpectedVersion,
    pub(crate) probe_home: ProbeHome,
    pub(crate) runtime_source: RuntimeSource,
    pub(crate) probe_depth: ProbeDepth,
    pub(crate) catalog: CatalogProbe,
    pub(crate) catalog_runtime: CatalogRuntime,
    pub(crate) observation: ObservationSupport,
}

impl AgentPolicy {
    pub(super) fn spawn_supported(&self, entry: &AgentConfigEntry) -> bool {
        self.spawn_gate.is_open(entry)
    }

    pub(super) fn transport_support(&self, entry: &AgentConfigEntry) -> AgentTransportSupportView {
        AgentTransportSupportView {
            transport: self.transport,
            probe: true,
            spawn: self.spawn_supported(entry),
        }
    }

    /// The published permission-mode set is empty until the spawn gate opens,
    /// so a configured-but-ungated agent advertises no selectable mode.
    pub(super) fn permission_mode_views(
        &self,
        entry: &AgentConfigEntry,
    ) -> Vec<AgentPermissionModeView> {
        if !self.spawn_supported(entry) {
            return Vec::new();
        }
        self.permission_modes
            .iter()
            .copied()
            .map(permission_mode_view)
            .collect()
    }

    pub(super) fn model_selection_view(
        &self,
        entry: &AgentConfigEntry,
    ) -> AgentModelSelectionCapabilityView {
        AgentModelSelectionCapabilityView {
            supported: self.spawn_supported(entry),
            mode: AgentModelSelectionModeView::CatalogToken,
        }
    }

    pub(super) fn effort_selection_view(
        &self,
        entry: &AgentConfigEntry,
    ) -> AgentEffortSelectionCapabilityView {
        AgentEffortSelectionCapabilityView {
            supported: self.spawn_supported(entry),
            mode: match self.effort {
                EffortPolicy::Passthrough => AgentEffortSelectionModeView::PassthroughToken,
                EffortPolicy::Closed(_) => AgentEffortSelectionModeView::ClosedSet,
            },
        }
    }

    /// The daemon-pinned version advertised in status. Only a pinned row has
    /// one; a configured-only row leaves the field to `expected_version`.
    pub(crate) fn required_version(&self) -> Option<String> {
        match self.expected_version {
            ExpectedVersion::PinnedDsh => {
                Some(external_agent_dsh::profile::PINNED_DSH_VERSION.into())
            }
            ExpectedVersion::Configured => None,
        }
    }

    /// The version a mismatch advisory is judged against: the pinned reference
    /// for a pinned row, otherwise the persisted configuration value.
    pub(crate) fn expected_version(&self, configured: Option<&str>) -> Option<String> {
        match self.expected_version {
            ExpectedVersion::PinnedDsh => {
                Some(external_agent_dsh::profile::PINNED_DSH_VERSION.into())
            }
            ExpectedVersion::Configured => configured.map(str::to_owned),
        }
    }
}

/// The spawn gate: the identity terms that must hold before any task exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnGate {
    /// enabled + spawn_supported.
    ConfigOnly,
    /// enabled + spawn_supported + the pinned ACP profile + an absolute
    /// executable runtime path.
    AcpProfileRuntime,
    /// enabled + spawn_supported + an absolute executable runtime path.
    ExecutableRuntime,
}

impl SpawnGate {
    pub(super) fn is_open(self, entry: &AgentConfigEntry) -> bool {
        match self {
            Self::ConfigOnly => entry.enabled && entry.spawn_supported,
            Self::AcpProfileRuntime => {
                entry.enabled
                    && entry.spawn_supported
                    && entry.profile.as_deref() == Some("acp")
                    && runtime_path_is_executable(entry)
            }
            Self::ExecutableRuntime => {
                entry.enabled && entry.spawn_supported && runtime_path_is_executable(entry)
            }
        }
    }
}

/// The spawn gate for a name outside the roster falls back to the config-only
/// gate, matching the historical `_` arm.
pub(super) fn spawn_supported_for(name: &str, entry: &AgentConfigEntry) -> bool {
    AgentKind::from_name(name)
        .map(|kind| kind.policy().spawn_gate)
        .unwrap_or(SpawnGate::ConfigOnly)
        .is_open(entry)
}

fn runtime_path_is_executable(entry: &AgentConfigEntry) -> bool {
    entry
        .runtime_path
        .as_deref()
        .map(std::path::Path::new)
        .is_some_and(|path| {
            // Host executable semantics: the Unix execute bit, or the Windows
            // PATHEXT extension gate over a regular file.
            path.is_absolute() && crate::executable::is_executable_file(path)
        })
}

/// The shape of the model selection admission validates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelShape {
    /// An explicit `{provider}/{model}` token is required; no configured or
    /// native fallback exists.
    ZcodeProviderModel,
    /// `{provider}:{model}` split at the first colon, with configured-default
    /// and native fallback.
    DshColonToken,
    /// A bounded non-empty token; absence is refused.
    CodexBoundedToken,
    /// A bare slug with no separator or whitespace; configured-default and
    /// native fallback.
    AgyBareSlug,
}

/// How a caller write manifest is treated at admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteManifestPolicy {
    /// No admission check; the runtime's policy environment owns the bound.
    Unrestricted,
    /// A non-empty manifest is admitted in build through the guarded
    /// manifest-build composition under the shared external-core bounds.
    GuardedBuildComposition,
    /// Any non-empty manifest is refused with this exact `agent_unsupported`
    /// message before the task exists.
    Refused(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HomeRequirement {
    /// No home term participates in admission.
    NotRequired,
    /// A configured home or the inherited variable must be present; the
    /// daemon never falls back to the runtime's default home.
    ConfiguredOrEnv(&'static str),
}

/// The reasoning-effort admission rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffortPolicy {
    /// A bounded lowercase token is passed through to the runtime.
    Passthrough,
    /// A bounded token is restricted to this measured closed set.
    Closed(&'static [&'static str]),
}

/// Where spawn developer instructions travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeveloperInstructionsChannel {
    /// The adapter's native per-thread channel; the prompt stays verbatim.
    NativeThreadStart,
    /// The daemon splices a `Developer Instructions` block into the prompt.
    PromptSplice,
}

/// Which version the daemon pins or trusts from configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpectedVersion {
    /// The daemon-tested pinned reference (dsh).
    PinnedDsh,
    /// The persisted `agents.<agent>.version`.
    Configured,
}

/// The home scope inherited by a probe when none is supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeHome {
    pub(crate) variable: &'static str,
    /// Whether an unset agent-specific variable falls back to `$HOME`.
    pub(crate) falls_back_to_home: bool,
}

/// Where the probe finds the runtime executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeSource {
    /// The daemon injects the runtime path (the ZCode app bundle).
    Injected,
    /// The environment variable, then a PATH scan for the binary name.
    Variable {
        variable: &'static str,
        binary: &'static str,
    },
}

/// Which probe depth a `through` layer triggers. Every row's auth-only layer
/// is side-effect free unless a richer hi probe implies otherwise: no session
/// or prompt is ever created just to infer credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeDepth {
    /// The local layer only; auth/hi stay `*_not_probed`.
    LocalOnly,
    /// A rich hi probe in the dsh dialect.
    DshHi,
    /// A rich hi probe in the agy dialect.
    AgyHi,
    /// A session/create-only hi probe; auth is gated behind hi.
    ZcodeHi,
}

/// Which catalog probe reads the model directory, and the source label its
/// unsupported evidence carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogProbe {
    ZcodeSessionCreate,
    DshAcpModels,
    CodexModelList,
    AgyModelsList,
}

impl CatalogProbe {
    pub(crate) fn unsupported_source(self) -> &'static str {
        match self {
            Self::ZcodeSessionCreate => "zcode_native_model",
            Self::DshAcpModels => "dsh_acp_models_list",
            Self::CodexModelList => "codex_app_server_model_list",
            Self::AgyModelsList => "agy_models_list",
        }
    }
}

/// How a catalog probe resolves the runtime executable. The dsh catalog is a
/// configuration-only preflight and does not scan PATH, unlike codex/agy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogRuntime {
    Injected,
    EnvironmentVariable(&'static str),
    Discovered {
        variable: &'static str,
        binary: &'static str,
    },
}

/// Per-agent observation policy: whether public reasoning activity is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObservationSupport {
    pub(crate) reports_reasoning: bool,
}

impl ObservationSupport {
    /// Unknown adapters report the supported public reasoning projection.
    pub(crate) fn for_name(name: &str) -> Self {
        policy_for(name).map_or(
            Self {
                reports_reasoning: true,
            },
            |policy| policy.observation,
        )
    }
}

// The policy rows. These are the only place a per-agent admission fact is
// written down; every consumer derives from them.
static ALL_PERMISSION_MODES: [PermissionMode; 4] = [
    PermissionMode::Build,
    PermissionMode::Edit,
    PermissionMode::Plan,
    PermissionMode::Yolo,
];
static DSH_PERMISSION_MODES: [PermissionMode; 2] = [PermissionMode::Build, PermissionMode::Plan];
static AGY_PERMISSION_MODES: [PermissionMode; 2] = [PermissionMode::Build, PermissionMode::Yolo];
static CODEX_EFFORT: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
static AGY_EFFORT: [&str; 4] = ["low", "medium", "high", "max"];

static ZCODE: AgentPolicy = AgentPolicy {
    kind: AgentKind::Zcode,
    transport: AgentTransportView::ZcodeAppServer,
    spawn_gate: SpawnGate::ConfigOnly,
    permission_modes: &ALL_PERMISSION_MODES,
    permission_refusal: None,
    model_shape: ModelShape::ZcodeProviderModel,
    write_manifest: WriteManifestPolicy::Unrestricted,
    home: HomeRequirement::NotRequired,
    effort: EffortPolicy::Passthrough,
    developer_instructions: DeveloperInstructionsChannel::PromptSplice,
    expected_version: ExpectedVersion::Configured,
    probe_home: ProbeHome {
        variable: "ZCODE_HOME",
        falls_back_to_home: true,
    },
    runtime_source: RuntimeSource::Injected,
    probe_depth: ProbeDepth::ZcodeHi,
    catalog: CatalogProbe::ZcodeSessionCreate,
    catalog_runtime: CatalogRuntime::Injected,
    observation: ObservationSupport {
        reports_reasoning: true,
    },
};

static DSH: AgentPolicy = AgentPolicy {
    kind: AgentKind::Dsh,
    transport: AgentTransportView::DshAcp,
    spawn_gate: SpawnGate::AcpProfileRuntime,
    permission_modes: &DSH_PERMISSION_MODES,
    permission_refusal: Some(
        "dsh first-launch admission supports only the build and plan permission modes",
    ),
    model_shape: ModelShape::DshColonToken,
    write_manifest: WriteManifestPolicy::GuardedBuildComposition,
    home: HomeRequirement::NotRequired,
    effort: EffortPolicy::Passthrough,
    developer_instructions: DeveloperInstructionsChannel::PromptSplice,
    expected_version: ExpectedVersion::PinnedDsh,
    probe_home: ProbeHome {
        variable: "DSH_HOME",
        falls_back_to_home: true,
    },
    runtime_source: RuntimeSource::Variable {
        variable: "DSH_RUNTIME_PATH",
        binary: "dsh",
    },
    probe_depth: ProbeDepth::DshHi,
    catalog: CatalogProbe::DshAcpModels,
    catalog_runtime: CatalogRuntime::EnvironmentVariable("DSH_RUNTIME_PATH"),
    observation: ObservationSupport {
        reports_reasoning: true,
    },
};

static CODEX: AgentPolicy = AgentPolicy {
    kind: AgentKind::Codex,
    transport: AgentTransportView::CodexAppServer,
    spawn_gate: SpawnGate::ExecutableRuntime,
    permission_modes: &ALL_PERMISSION_MODES,
    permission_refusal: None,
    model_shape: ModelShape::CodexBoundedToken,
    write_manifest: WriteManifestPolicy::Refused("CODEX_WRITE_MANIFEST_UNSUPPORTED"),
    home: HomeRequirement::ConfiguredOrEnv("CODEX_HOME"),
    effort: EffortPolicy::Closed(&CODEX_EFFORT),
    developer_instructions: DeveloperInstructionsChannel::NativeThreadStart,
    expected_version: ExpectedVersion::Configured,
    probe_home: ProbeHome {
        variable: "CODEX_HOME",
        falls_back_to_home: false,
    },
    runtime_source: RuntimeSource::Variable {
        variable: "CODEX_RUNTIME_PATH",
        binary: "codex",
    },
    probe_depth: ProbeDepth::LocalOnly,
    catalog: CatalogProbe::CodexModelList,
    catalog_runtime: CatalogRuntime::Discovered {
        variable: "CODEX_RUNTIME_PATH",
        binary: "codex",
    },
    observation: ObservationSupport {
        reports_reasoning: false,
    },
};

static AGY: AgentPolicy = AgentPolicy {
    kind: AgentKind::Agy,
    transport: AgentTransportView::AgyStreamJson,
    spawn_gate: SpawnGate::ExecutableRuntime,
    permission_modes: &AGY_PERMISSION_MODES,
    permission_refusal: Some("AGY_PERMISSION_MODE_UNSUPPORTED"),
    model_shape: ModelShape::AgyBareSlug,
    write_manifest: WriteManifestPolicy::Refused("AGY_WRITE_MANIFEST_UNSUPPORTED"),
    home: HomeRequirement::NotRequired,
    effort: EffortPolicy::Closed(&AGY_EFFORT),
    developer_instructions: DeveloperInstructionsChannel::PromptSplice,
    expected_version: ExpectedVersion::Configured,
    probe_home: ProbeHome {
        // agy has no home override (there is no AGY_HOME); the default HOME is
        // the only source.
        variable: "HOME",
        falls_back_to_home: true,
    },
    runtime_source: RuntimeSource::Variable {
        variable: "AGY_RUNTIME_PATH",
        binary: "agy",
    },
    probe_depth: ProbeDepth::AgyHi,
    catalog: CatalogProbe::AgyModelsList,
    catalog_runtime: CatalogRuntime::Discovered {
        variable: "AGY_RUNTIME_PATH",
        binary: "agy",
    },
    observation: ObservationSupport {
        reports_reasoning: false,
    },
};

/// The projection of one permission mode into the public status vocabulary.
fn permission_mode_view(mode: PermissionMode) -> AgentPermissionModeView {
    match mode {
        PermissionMode::Build => AgentPermissionModeView::Build,
        PermissionMode::Edit => AgentPermissionModeView::Edit,
        PermissionMode::Plan => AgentPermissionModeView::Plan,
        PermissionMode::Yolo => AgentPermissionModeView::Yolo,
    }
}

/// The lowercase mode label used by the admission contract prose. Only the
/// spawn-description contract test reads it; production projects the view
/// enum instead.
#[cfg(test)]
pub(crate) fn permission_mode_name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Build => "build",
        PermissionMode::Edit => "edit",
        PermissionMode::Plan => "plan",
        PermissionMode::Yolo => "yolo",
    }
}
