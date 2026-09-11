use std::{fmt, path::PathBuf, time::{Duration, Instant}};
use external_store::StoreError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub global_max_agents: usize,
    pub per_workspace_max_agents: usize,
    pub stop_grace: Duration,
    pub bootstrap_timeout: Duration,
    pub control_timeout: Duration,
    pub runtime_source: Option<PathBuf>,
}

impl Default for SchedulerConfig {
    fn default() -> Self { Self { global_max_agents: usize::MAX, per_workspace_max_agents: 1, stop_grace: Duration::from_secs(1), bootstrap_timeout: Duration::from_secs(2), control_timeout: Duration::from_secs(2), runtime_source: None } }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ControlDeadline { expires_at: Instant }
impl ControlDeadline {
    pub(crate) fn new(budget: Duration) -> Self { Self { expires_at: Instant::now() + budget } }
    pub(crate) fn remaining(self) -> Option<Duration> { self.expires_at.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) }
    pub(crate) fn runtime_phase(self, stop_grace: Duration) -> Option<Duration> { self.runtime_phase_deadline(stop_grace)?.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) }
    pub(crate) fn runtime_phase_deadline(self, stop_grace: Duration) -> Option<Instant> { let remaining = self.remaining()?; let cleanup = stop_grace.checked_mul(3).unwrap_or(remaining).min(remaining / 2); self.expires_at.checked_sub(cleanup).filter(|deadline| *deadline > Instant::now()) }
    pub(crate) fn cleanup_grace(self, configured: Duration) -> Duration { self.remaining().map(|remaining| configured.min(remaining / 3)).unwrap_or(Duration::ZERO) }
}

#[derive(Debug)]
pub enum SchedulerError { Store(StoreError), InvalidConfig(String), RuntimeSpawn { agent_id: String, message: String }, LifecycleSink { agent_id: String, message: String }, RuntimeCommand { agent_id: String, message: String } }
impl fmt::Display for SchedulerError { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { match self { Self::Store(e) => write!(f, "{e}"), Self::InvalidConfig(m) => write!(f, "invalid scheduler config: {m}"), Self::RuntimeSpawn {agent_id,message} => write!(f,"runtime spawn failed for {agent_id}: {message}"), Self::LifecycleSink {agent_id,message} => write!(f,"lifecycle sink failed for {agent_id}: {message}"), Self::RuntimeCommand {agent_id,message} => write!(f,"runtime command failed for {agent_id}: {message}") } } }
impl std::error::Error for SchedulerError {}
impl From<StoreError> for SchedulerError { fn from(value: StoreError) -> Self { Self::Store(value) } }
