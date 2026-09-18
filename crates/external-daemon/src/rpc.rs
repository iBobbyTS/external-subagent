//! Daemon RPC facade.
//!
//! The wire protocol, projections, error mapping, agent admission policy,
//! service handlers, and the bounded wait loop live in the private
//! `rpc::{types, views, errors, config, agents, handlers, wait}` modules.
//! This facade re-exports every historical path so existing `use
//! crate::rpc::X` consumers keep resolving unchanged.

mod agents;
mod config;
mod errors;
mod handlers;
mod types;
mod views;
mod wait;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{remove_matching_socket, remove_stale_socket, SocketIdentity};
#[cfg(unix)]
pub use unix::{RpcClient, RpcServer, ServerOptions};

pub use config::parse_subagent_config;
pub use errors::{RpcError, RpcErrorCode};
pub use handlers::{RpcService, RpcServiceConfigError};
pub use types::{
    GeneralSubmitInput, MessageInput, RespondInput, ResponseDecision, RpcMethod, RpcOutcome,
    RpcRequest, RpcResponse, RpcSuccess, TaskListQuery, TaskPhaseFilter, TaskWaitQuery,
    DEFAULT_WAIT_TIME, MAX_LIST_TASKS, MAX_PENDING_REQUESTS, MAX_REQUEST_FRAME_BYTES,
    MAX_RESPONSE_FRAME_BYTES, MAX_RESULT_CHUNK_BYTES, MAX_WAIT, MCP_VERSION,
    RPC_TRANSPORT_SUPPORTED,
};
pub use views::{
    running_component_identity, AgentCapabilitiesView, AgentModelSelectionCapabilityView,
    AgentModelSelectionModeView, AgentPermissionModeView, AgentScopeStatusView, AgentStatusView,
    AgentTransportSupportView, AgentTransportView, ArtifactIdentityView, CapabilityMaturityView,
    ComponentIdentityView, ComponentStateView, DaemonIdentityView, InputIdentityView,
    MessageDispositionView, MessageReceiptView, ModelIdentityFactView, ModelIdentityView,
    ObservationCapabilityView, ObservationDefaultsView, PendingRequestView, QuestionView,
    ResponseDispositionView, ResponseOutcomeView, SystemStatusView, TaskActivityView,
    TaskObservationView, TaskResultView, TaskView, TelemetryStatusView,
};

#[cfg(test)]
pub(crate) use agents::admission_fixtures;
#[cfg(test)]
pub(crate) use wait::wait_tests;
