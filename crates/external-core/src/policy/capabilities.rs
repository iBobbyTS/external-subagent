use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEnforcement {
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyCapabilities {
    pub exact_command_allowlist: bool,
    pub sanitized_environment: bool,
    pub bounded_time_and_output: bool,
    pub local_hard_deny: bool,
    pub source_integrity_diagnostics: bool,
    pub network_isolation_enforced: bool,
    pub network_control: String,
    pub os_sandbox: SandboxEnforcement,
}

impl Default for PolicyCapabilities {
    fn default() -> Self {
        Self::for_network(false)
    }
}

impl PolicyCapabilities {
    pub(crate) fn for_network(network_allowed: bool) -> Self {
        Self {
            exact_command_allowlist: true,
            sanitized_environment: true,
            bounded_time_and_output: true,
            local_hard_deny: true,
            source_integrity_diagnostics: true,
            network_isolation_enforced: false,
            network_control: if network_allowed {
                "manifest allows network; no network isolation is enforced".into()
            } else {
                "known network clients and URL arguments denied; general network isolation unsupported"
                    .into()
            },
            os_sandbox: SandboxEnforcement::Unsupported,
        }
    }
}
