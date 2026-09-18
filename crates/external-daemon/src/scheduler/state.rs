use super::*;

#[derive(Clone)]
pub struct Scheduler {
    pub(crate) inner: Arc<SchedulerInner>,
}

pub(crate) struct SchedulerInner {
    pub(super) owner_id: String,
    pub(super) store: Arc<Store>,
    pub(super) factory: Arc<dyn RuntimeFactory>,
    pub(super) config: SchedulerConfig,
    #[cfg(test)]
    pub(super) preflight_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    pub(super) response_claim_hook: Mutex<Option<Arc<ResponseClaimHook>>>,
    #[cfg(test)]
    pub(super) result_persist_hook: Mutex<Option<Arc<ResultPersistHook>>>,
    pub(crate) state: Mutex<SchedulerState>,
    pub(super) admission: Mutex<()>,
    #[cfg(test)]
    pub(super) admission_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    pub(super) draining: AtomicBool,
    pub(super) drain_cancel_running: AtomicBool,
    pub(super) updater_fired: AtomicBool,
    pub(super) activation_claim: Mutex<Option<String>>,
}

#[cfg(test)]
type ResponseClaimHook = dyn Fn(ResponseClaimHookStage, &str) + Send + Sync;

#[cfg(test)]
type ResultPersistHook = dyn Fn(&str) + Send + Sync;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponseClaimHookStage {
    BeforeClaim,
    AfterClaim,
}

#[derive(Default)]
pub(crate) struct SchedulerState {
    pub(super) active: HashMap<String, ActiveRuntime>,
    pub(crate) activities: HashMap<String, Arc<PassiveActivityTracker>>,
    pub(super) failures: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeLifecyclePhase {
    Running,
    StopRequested,
    StopAcknowledged,
    ForceTerminating,
    Terminal,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeLifecycleSnapshot {
    pub(crate) phase: RuntimeLifecyclePhase,
    runtime_generation: u64,
    turn_generation: u64,
    stop_requested_at: Option<Instant>,
    observed_boundary: Option<TurnBoundary>,
    force_termination_count: u64,
    late_event_count: u64,
}

pub(crate) struct RuntimeLifecycle {
    pub(crate) state: Mutex<RuntimeLifecycleSnapshot>,
}

const MAX_BOUNDED_LATE_EVENT_DIAGNOSTICS: u64 = 64;

impl RuntimeLifecycle {
    pub(super) fn new(runtime_generation: u64) -> Self {
        Self {
            state: Mutex::new(RuntimeLifecycleSnapshot {
                phase: RuntimeLifecyclePhase::Running,
                runtime_generation,
                turn_generation: 0,
                stop_requested_at: None,
                observed_boundary: None,
                force_termination_count: 0,
                late_event_count: 0,
            }),
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> RuntimeLifecycleSnapshot {
        self.state.lock().unwrap().clone()
    }

    pub(super) fn request_stop(&self, turn: &TurnSnapshot) {
        let mut state = self.state.lock().unwrap();
        if state.phase == RuntimeLifecyclePhase::Running {
            state.phase = RuntimeLifecyclePhase::StopRequested;
            state.turn_generation = turn.generation;
            state.stop_requested_at = Some(Instant::now());
        }
    }

    pub(super) fn acknowledge_boundary(&self, turn: &TurnSnapshot) -> bool {
        let mut state = self.state.lock().unwrap();
        if matches!(
            state.phase,
            RuntimeLifecyclePhase::StopRequested | RuntimeLifecyclePhase::StopAcknowledged
        ) && turn.generation == state.turn_generation
            && !turn.active
            && turn.boundary.is_some()
        {
            state.phase = RuntimeLifecyclePhase::StopAcknowledged;
            state.observed_boundary = turn.boundary;
            true
        } else {
            false
        }
    }

    pub(super) fn force_terminating(&self) {
        let mut state = self.state.lock().unwrap();
        if !matches!(
            state.phase,
            RuntimeLifecyclePhase::ForceTerminating | RuntimeLifecyclePhase::Terminal
        ) {
            state.phase = RuntimeLifecyclePhase::ForceTerminating;
            state.force_termination_count = state.force_termination_count.saturating_add(1);
        }
    }

    pub(super) fn terminalize(&self) {
        self.state.lock().unwrap().phase = RuntimeLifecyclePhase::Terminal;
    }

    pub(super) fn ingress_reason(&self) -> Option<&'static str> {
        let state = self.state.lock().unwrap();
        debug_assert!(state.runtime_generation > 0);
        match state.phase {
            RuntimeLifecyclePhase::Running => None,
            RuntimeLifecyclePhase::StopRequested
            | RuntimeLifecyclePhase::StopAcknowledged
            | RuntimeLifecyclePhase::ForceTerminating => Some("TASK_STOPPING"),
            RuntimeLifecyclePhase::Terminal => Some("LATE_AFTER_STOP"),
        }
    }

    pub(crate) fn admit_event(&self) -> Option<MutexGuard<'_, RuntimeLifecycleSnapshot>> {
        let mut state = self.state.lock().unwrap();
        if state.phase == RuntimeLifecyclePhase::Running {
            return Some(state);
        }
        state.late_event_count = state
            .late_event_count
            .saturating_add(1)
            .min(MAX_BOUNDED_LATE_EVENT_DIAGNOSTICS);
        None
    }
}

pub(super) struct ActiveRuntime {
    pub(super) owner_epoch: u64,
    pub(super) runtime: Arc<dyn ManagedRuntime>,
    pub(super) sink: Arc<StoreLifecycleSink>,
    pub(super) session_id: String,
    pub(super) operation: Arc<Mutex<()>>,
    pub(super) runtime_lifecycle: Arc<RuntimeLifecycle>,
    pub(super) route: TaskRoute,
    pub(super) task: Option<TaskRecord>,
    pub(super) check: Arc<ActiveCheck>,
}

#[derive(Debug, Default)]
pub(super) struct ActiveCheck {
    cancelled: AtomicBool,
}

impl ActiveCheck {
    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

pub(super) struct TerminalTarget<'a> {
    pub(super) agent_id: &'a str,
    pub(super) sink: &'a StoreLifecycleSink,
    pub(super) route: &'a TaskRoute,
    pub(super) runtime: &'a Arc<dyn ManagedRuntime>,
}

pub(super) struct TerminalDecision {
    pub(super) terminal: RuntimeTerminal,
    pub(super) natural_completion: bool,
    pub(super) forced_outcome: Option<(CompletionOutcome, String)>,
    pub(super) failure_message: Option<String>,
}

pub(super) struct MonitorContext {
    pub(super) agent_id: String,
    pub(super) owner_epoch: u64,
    pub(super) runtime: Arc<dyn ManagedRuntime>,
    pub(super) sink: Arc<StoreLifecycleSink>,
    pub(super) session_id: String,
    pub(super) operation: Arc<Mutex<()>>,
    pub(super) runtime_lifecycle: Arc<RuntimeLifecycle>,
    pub(super) route: TaskRoute,
    pub(super) task: Option<TaskRecord>,
    pub(super) check: Arc<ActiveCheck>,
}

pub(super) type ActiveSession = (
    u64,
    Arc<dyn ManagedRuntime>,
    String,
    Arc<Mutex<()>>,
    Arc<RuntimeLifecycle>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDisposition {
    Queued,
    Delivered,
    AlreadyDelivered,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseDisposition {
    Responded,
    AlreadyResponded,
    InFlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseOutcome {
    pub disposition: ResponseDisposition,
    pub requested_decision: String,
    pub effective_decision: String,
    pub policy_overrode: bool,
    pub policy_reason_code: Option<String>,
}

impl Scheduler {
    pub fn new(
        owner_id: impl Into<String>,
        store: Arc<Store>,
        factory: Arc<dyn RuntimeFactory>,
        config: SchedulerConfig,
    ) -> Result<Self, SchedulerError> {
        if config.per_workspace_max_agents == 0
            || config.bootstrap_timeout.is_zero()
            || config.control_timeout.is_zero()
        {
            return Err(SchedulerError::InvalidConfig(
                "scheduler limits and bounded control waits must be positive".into(),
            ));
        }
        Ok(Self {
            inner: Arc::new(SchedulerInner {
                owner_id: owner_id.into(),
                store,
                factory,
                config,
                #[cfg(test)]
                preflight_hook: None,
                #[cfg(test)]
                response_claim_hook: Mutex::new(None),
                #[cfg(test)]
                result_persist_hook: Mutex::new(None),
                state: Mutex::new(SchedulerState::default()),
                admission: Mutex::new(()),
                #[cfg(test)]
                admission_hook: Mutex::new(None),
                draining: AtomicBool::new(false),
                drain_cancel_running: AtomicBool::new(false),
                updater_fired: AtomicBool::new(false),
                activation_claim: Mutex::new(None),
            }),
        })
    }

    #[cfg(test)]
    fn with_preflight_hook(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("preflight hook must attach before scheduler cloning")
            .preflight_hook = Some(Arc::new(hook));
        self
    }

    #[cfg(test)]
    pub(super) fn run_response_claim_hook(&self, stage: ResponseClaimHookStage, agent_id: &str) {
        if let Some(hook) = self.inner.response_claim_hook.lock().unwrap().clone() {
            hook(stage, agent_id);
        }
    }

    #[cfg(test)]
    fn set_result_persist_hook(&self, hook: Arc<ResultPersistHook>) {
        *self.inner.result_persist_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn run_result_persist_hook(&self, agent_id: &str) {
        if let Some(hook) = self.inner.result_persist_hook.lock().unwrap().clone() {
            hook(agent_id);
        }
    }

    pub fn store(&self) -> Arc<Store> {
        Arc::clone(&self.inner.store)
    }

    /// Return only the configured runtime path. Reading this value never
    /// starts or probes the runtime, so callers must not label it observed.
    pub fn configured_runtime_source(&self) -> Option<PathBuf> {
        self.inner.config.runtime_source.clone()
    }

    pub(super) fn active_session(&self, agent_id: &str) -> Option<ActiveSession> {
        let state = self.inner.state.lock().unwrap();
        state.active.get(agent_id).map(|active| {
            (
                active.owner_epoch,
                Arc::clone(&active.runtime),
                active.session_id.clone(),
                Arc::clone(&active.operation),
                Arc::clone(&active.runtime_lifecycle),
            )
        })
    }

    pub fn active_count(&self) -> usize {
        self.inner.state.lock().unwrap().active.len()
    }

    pub fn active_turn_observation(&self, agent_id: &str) -> Option<(TurnSnapshot, u64)> {
        self.active_session(agent_id)
            .map(|(_, runtime, _, _, _)| (runtime.turn_snapshot(), runtime.stop_boundary_count()))
    }

    pub(crate) fn passive_activity_snapshot(
        &self,
        agent_id: &str,
    ) -> Option<PassiveActivitySnapshot> {
        self.inner
            .state
            .lock()
            .unwrap()
            .activities
            .get(agent_id)
            .map(|activity| activity.snapshot())
    }

    pub(crate) fn observation_snapshot(
        &self,
        agent_id: &str,
    ) -> (observation::ObservationSnapshot, bool) {
        let activity = self
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .get(agent_id)
            .cloned();
        match activity {
            Some(activity) => (
                activity.observation_snapshot(),
                activity.runtime_source_verified(),
            ),
            None => (
                observation::ObservationSnapshot::unavailable(),
                // A task without a launch-scoped activity (never started, or
                // claimed before this daemon's ownership) has no adapter
                // evidence of its own; the scheduler-global ZCode proof must
                // not stand in for a missing or retired activity.
                false,
            ),
        }
    }

    pub fn last_error(&self, agent_id: &str) -> Option<String> {
        self.inner
            .state
            .lock()
            .unwrap()
            .failures
            .get(agent_id)
            .cloned()
    }

    pub(super) fn release_active(&self, agent_id: &str, owner_epoch: u64) {
        let mut state = self.inner.state.lock().unwrap();
        if state
            .active
            .get(agent_id)
            .is_some_and(|active| active.owner_epoch == owner_epoch)
        {
            if let Some(active) = state.active.get(agent_id) {
                active.runtime_lifecycle.terminalize();
            }
            state.active.remove(agent_id);
        }
    }
}
