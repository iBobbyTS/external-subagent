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
    #[cfg(test)]
    pub(super) before_transport_cleanup_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    pub(super) before_stall_cleanup_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Test-only decision read fault: returning true makes the single
    /// protected store read fail once, so R3 recovery can be pinned.
    #[cfg(test)]
    pub(super) stall_read_fault: Mutex<Option<Arc<dyn Fn() -> bool + Send + Sync>>>,
    /// Monotonic clock seam shared by every stall decision. Production uses
    /// `Instant::now`; tests inject a manual clock so window arithmetic is
    /// exercised on the same semantics as production.
    pub(super) clock: Arc<dyn Fn() -> Instant + Send + Sync>,
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
    stall: Mutex<StallWatchState>,
}

/// S02 stall watchdog state, owned by one launched `(agent_id, owner_epoch)`.
///
/// A re-claim builds a fresh `RuntimeLifecycle`, so the window, the observed
/// progress revision, and the triggered flag can never leak across epochs.
#[derive(Default)]
struct StallWatchState {
    /// Monotonic instant the current no-activity window started.
    baseline: Option<Instant>,
    /// Last admitted-progress revision observed by the watchdog.
    observed_progress: u64,
    /// Monotonic instant of the last admitted progress (diagnostics only).
    last_progress_at: Option<Instant>,
    /// True while the task waits for user input: the window is frozen.
    waiting: bool,
    /// Set when the watchdog closure has taken ownership once.
    triggered: bool,
    /// One-shot flag so a transient store read failure is reported once
    /// instead of on every retry tick.
    read_error_reported: bool,
}

/// Bounded stall evidence handed to the closure diagnostics.
pub(super) struct StallStatus {
    pub(super) elapsed: Duration,
    pub(super) last_progress_age: Option<Duration>,
    pub(super) timeout: Duration,
}

/// Whether the monitor keeps looping after a fault decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FaultDisposition {
    /// A fault closure took the task; the monitor returns.
    Handled,
    /// A concurrent activity, pending input, stop, or a transient read
    /// failure suppressed the watchdog; the monitor keeps looping.
    Suppressed,
    /// This monitor no longer owns the task; the monitor returns.
    Abandoned,
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
            stall: Mutex::new(StallWatchState::default()),
        }
    }

    /// Establish the initial window baseline at the first RUNNING transition
    /// (called synchronously by the claim path, before the monitor thread
    /// starts, so the baseline never depends on polling delay).
    pub(super) fn stall_start(&self, now: Instant) {
        let mut state = self.stall.lock().unwrap();
        if !state.triggered {
            state.baseline.get_or_insert(now);
        }
    }

    /// Freeze or unfreeze the no-activity window for user input. Entering a
    /// wait discards the consumed window; leaving it (without an explicit
    /// response resume) starts a fresh full window.
    pub(super) fn stall_set_waiting(&self, waiting: bool, now: Instant) {
        let mut state = self.stall.lock().unwrap();
        if waiting == state.waiting {
            return;
        }
        state.waiting = waiting;
        if waiting {
            state.baseline = None;
        } else {
            state.baseline = Some(now);
            state.last_progress_at = Some(now);
        }
    }

    /// Restart the window from the successful response that resolved the last
    /// awaitable pending request (S02 B-B02 resume point).
    pub(super) fn stall_resume(&self, now: Instant) {
        let mut state = self.stall.lock().unwrap();
        state.waiting = false;
        state.baseline = Some(now);
        state.last_progress_at = Some(now);
    }

    /// Fold in admitted-progress revisions. A new revision restarts the
    /// window from `now`; `OversizedLine`/`Malformed` never advance the
    /// revision, so they cannot mask a stalled task.
    pub(super) fn stall_observe_progress(&self, revision: u64, now: Instant) {
        let mut state = self.stall.lock().unwrap();
        if revision == state.observed_progress {
            return;
        }
        state.observed_progress = revision;
        state.last_progress_at = Some(now);
        if !state.waiting {
            state.baseline = Some(now);
        }
    }

    /// Report the current no-activity window once it reaches `timeout`
    /// (`elapsed >= timeout`). `None` while waiting for input, already
    /// triggered, disabled, or still inside the window.
    pub(super) fn stall_poll(&self, now: Instant, timeout: Duration) -> Option<StallStatus> {
        if timeout.is_zero() {
            return None;
        }
        let mut state = self.stall.lock().unwrap();
        state.baseline.get_or_insert(now);
        if state.waiting || state.triggered {
            return None;
        }
        let elapsed = now.saturating_duration_since(state.baseline?);
        (elapsed >= timeout).then(|| StallStatus {
            elapsed,
            last_progress_age: state
                .last_progress_at
                .map(|at| now.saturating_duration_since(at)),
            timeout,
        })
    }

    pub(super) fn stall_mark_triggered(&self) {
        self.stall.lock().unwrap().triggered = true;
    }

    /// Report a transient decision read failure at most once per claim.
    /// Returns true when this call is the first to report it.
    pub(super) fn stall_note_read_error(&self) -> bool {
        let mut state = self.stall.lock().unwrap();
        if state.read_error_reported {
            false
        } else {
            state.read_error_reported = true;
            true
        }
    }

    /// The admission latch shared with the lifecycle sink. While held, no
    /// event can be admitted (`admit_event` blocks), so a decision made under
    /// it sees exactly the events admitted before the linearization point.
    pub(super) fn decision_latch(&self) -> MutexGuard<'_, RuntimeLifecycleSnapshot> {
        self.state.lock().unwrap()
    }

    /// [`Self::request_stop`] against an already-held admission latch.
    pub(super) fn request_stop_locked(state: &mut RuntimeLifecycleSnapshot, turn: &TurnSnapshot) {
        if state.phase == RuntimeLifecyclePhase::Running {
            state.phase = RuntimeLifecyclePhase::StopRequested;
            state.turn_generation = turn.generation;
            state.stop_requested_at = Some(Instant::now());
        }
    }

    /// [`Self::force_terminating`] against an already-held admission latch.
    pub(super) fn force_terminating_locked(state: &mut RuntimeLifecycleSnapshot) {
        if !matches!(
            state.phase,
            RuntimeLifecyclePhase::ForceTerminating | RuntimeLifecyclePhase::Terminal
        ) {
            state.phase = RuntimeLifecyclePhase::ForceTerminating;
            state.force_termination_count = state.force_termination_count.saturating_add(1);
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> RuntimeLifecycleSnapshot {
        self.state.lock().unwrap().clone()
    }

    pub(super) fn request_stop(&self, turn: &TurnSnapshot) {
        let mut state = self.state.lock().unwrap();
        Self::request_stop_locked(&mut state, turn);
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
        Self::force_terminating_locked(&mut state);
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
                #[cfg(test)]
                before_transport_cleanup_hook: Mutex::new(None),
                #[cfg(test)]
                before_stall_cleanup_hook: Mutex::new(None),
                #[cfg(test)]
                stall_read_fault: Mutex::new(None),
                clock: Arc::new(Instant::now),
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
            None => {
                // No activity means no captured history, even when the adapter
                // has a public protocol. Never borrow a global runtime proof.
                let mut snapshot = observation::ObservationSnapshot::unavailable();
                if self
                    .inner
                    .store
                    .get_task(agent_id)
                    .ok()
                    .flatten()
                    .is_some_and(|task| task_agent(&task) == "dsh")
                {
                    snapshot.reasoning.source = observation::ReasoningSource::dsh();
                }
                (snapshot, false)
            }
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

    /// Whether the in-memory active instance still belongs to this owner
    /// epoch. The transport-failure closure validates this before it acts on
    /// a task that another control path may already have released.
    pub(super) fn active_instance_matches(&self, agent_id: &str, owner_epoch: u64) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .active
            .get(agent_id)
            .is_some_and(|active| active.owner_epoch == owner_epoch)
    }

    /// The scheduler's monotonic now. Production and tests share this one
    /// seam, so window arithmetic never depends on which clock filled it.
    pub(super) fn now(&self) -> Instant {
        (self.inner.clock)()
    }

    /// The durable task read used by fault decisions, with a test-only
    /// single-shot failure seam.
    pub(super) fn decision_task(
        &self,
        agent_id: &str,
    ) -> Result<Option<TaskRecord>, external_store::StoreError> {
        #[cfg(test)]
        if let Some(fault) = self.inner.stall_read_fault.lock().unwrap().clone() {
            if fault() {
                return Err(external_store::StoreError::InvalidState(
                    "injected decision read failure".into(),
                ));
            }
        }
        self.inner.store.get_task(agent_id)
    }

    /// Release the in-memory slot and advance the queue for a task this
    /// monitor can no longer own. Epoch-guarded, so a newer claim is never
    /// disturbed (`release_active` is a no-op when the epoch changed).
    pub(super) fn abandon_active_monitor(&self, agent_id: &str, owner_epoch: u64) {
        self.release_active(agent_id, owner_epoch);
        if let Err(error) = self.start_ready() {
            self.record_failure(agent_id, error.to_string());
        }
    }

    #[cfg(test)]
    pub(super) fn set_stall_read_fault(&self, fault: Arc<dyn Fn() -> bool + Send + Sync>) {
        *self.inner.stall_read_fault.lock().unwrap() = Some(fault);
    }

    #[cfg(test)]
    pub(super) fn set_clock(&mut self, clock: Arc<dyn Fn() -> Instant + Send + Sync>) {
        Arc::get_mut(&mut self.inner)
            .expect("clock must attach before the scheduler is cloned")
            .clock = clock;
    }

    #[cfg(test)]
    pub(super) fn set_before_stall_cleanup_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.inner.before_stall_cleanup_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn run_before_stall_cleanup_hook(&self) {
        if let Some(hook) = self.inner.before_stall_cleanup_hook.lock().unwrap().clone() {
            hook();
        }
    }

    #[cfg(test)]
    pub(super) fn set_before_transport_cleanup_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.inner.before_transport_cleanup_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn run_before_transport_cleanup_hook(&self) {
        if let Some(hook) = self
            .inner
            .before_transport_cleanup_hook
            .lock()
            .unwrap()
            .clone()
        {
            hook();
        }
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
