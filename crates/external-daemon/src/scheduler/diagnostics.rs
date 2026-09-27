use super::*;

impl Scheduler {
    /// Record a bounded failure diagnostic for logs and the in-memory latest
    /// map, and return the independent persistable representation so a
    /// terminal fill point can hand it to the store in the same transaction
    /// as the immutable result.
    pub(super) fn record_runtime_failure(
        &self,
        agent_id: &str,
        session_id: Option<&str>,
        stage: &str,
        error_code: &str,
        message: &str,
        runtime: Option<&dyn ManagedRuntime>,
    ) -> String {
        // Callers already stopped/reaped the runtime or observed its terminal
        // boundary. Do not introduce a diagnostic wait into scheduler control.
        let (known_session, tail) = runtime_diagnostic(runtime);
        let record = runtime_failure_record(
            agent_id,
            known_session.as_deref().or(session_id),
            stage,
            error_code,
            message,
            &tail,
        );
        self.record_failure_line(agent_id, record.clone(), &record);
        persistable_failure_record(
            agent_id,
            known_session.as_deref().or(session_id),
            stage,
            error_code,
            message,
            &tail,
        )
    }

    /// Build the persistable record without touching the log or the in-memory
    /// latest map. Fill points that have no existing `record_runtime_failure`
    /// call use this so persistence never invents a log side effect.
    pub(super) fn persistable_failure_record_for(
        &self,
        agent_id: &str,
        session_id: Option<&str>,
        stage: &str,
        error_code: &str,
        message: &str,
        runtime: Option<&dyn ManagedRuntime>,
    ) -> String {
        let (known_session, tail) = runtime_diagnostic(runtime);
        persistable_failure_record(
            agent_id,
            known_session.as_deref().or(session_id),
            stage,
            error_code,
            message,
            &tail,
        )
    }

    fn record_failure_line(&self, agent_id: &str, message: String, record: &str) {
        update_latest_failure(
            &mut self.inner.state.lock().unwrap().failures,
            agent_id,
            message,
        );
        let line = format!(
            "[external-subagentd] failure agent={}: {record}\n",
            bounded_error(agent_id)
        );
        if let Some(logger) =
            FAILURE_LOGGER.get_or_init(|| DiagnosticLogger::start(io::stderr()).ok())
        {
            logger.submit(line);
        }
    }

    pub(crate) fn record_failure(&self, agent_id: &str, message: String) {
        let bounded = bounded_error(&message);
        self.record_failure_line(agent_id, message, &bounded);
    }
}

/// The runtime-owned diagnostic identity at the moment a fill point observes
/// the failure. Callers must hold the runtime (or pass `None` when it was
/// never built); this never fabricates a session or tail.
fn runtime_diagnostic(runtime: Option<&dyn ManagedRuntime>) -> (Option<String>, String) {
    (
        runtime.and_then(ManagedRuntime::diagnostic_session_id),
        runtime
            .map(ManagedRuntime::diagnostic_tail)
            .unwrap_or_default(),
    )
}

// Every variable field is bounded before JSON escaping, whose worst-case
// expansion is six bytes per input byte. Including framing, records stay below
// 192 KiB; stderr keeps its latest 16 KiB even for invalid UTF-8 input.
pub(crate) fn runtime_failure_record(
    agent_id: &str,
    session_id: Option<&str>,
    stage: &str,
    error_code: &str,
    message: &str,
    stderr_tail: &str,
) -> String {
    let mut start = stderr_tail.len().saturating_sub(16 * 1024);
    while !stderr_tail.is_char_boundary(start) {
        start += 1;
    }
    let detail = serde_json::from_str::<serde_json::Value>(message).ok();
    let mut record = serde_json::json!({
        "agent_id": bounded_error(agent_id),
        "session_id": session_id.map(bounded_error),
        "stage": bounded_prefix(stage, 128),
        "error_code": bounded_prefix(error_code, 128),
        "message": bounded_error(detail.as_ref().and_then(|v| v.get("message"))
            .and_then(serde_json::Value::as_str).unwrap_or(message)),
        "stderr_tail": &stderr_tail[start..],
    });
    if let Some(detail) = detail {
        for field in ["operation", "remote_message", "cleanup_result"] {
            if let Some(value) = detail.get(field).and_then(serde_json::Value::as_str) {
                record[field] = bounded_prefix(value, 1024).into();
            }
        }
        // The transport closure's bounded evidence: detection lower bound,
        // frame cap, and the last admitted event sequence at latch time.
        for field in ["bytes", "cap", "last_event_seq"] {
            if let Some(value) = detail.get(field).and_then(serde_json::Value::as_u64) {
                record[field] = value.into();
            }
        }
        // The stall closure's bounded evidence: elapsed window, timeout, and
        // the age of the last admitted progress.
        for field in [
            "stall_elapsed_ms",
            "stall_timeout_ms",
            "last_progress_age_ms",
        ] {
            if let Some(value) = detail.get(field).and_then(serde_json::Value::as_u64) {
                record[field] = value.into();
            }
        }
        if let Some(code) = detail
            .get("remote_code")
            .and_then(serde_json::Value::as_i64)
        {
            record["remote_code"] = code.into();
        }
    }
    record.to_string()
}

pub(crate) fn bounded_error(message: &str) -> String {
    bounded_prefix(message, 4096)
}

/// Independent byte budgets for the persistable record. The log/memory record
/// keeps its 192 KiB envelope with 16 KiB of `stderr_tail`; the persisted
/// variant must fit a single 16 KiB `tasks.failure_message` value while still
/// carrying a latest-suffix stderr tail. Escaping expands one input byte to at
/// most six (`\u0001`), so the non-tail budget below has a hard ceiling of
/// `2*256 + 2*128 + 512 + 3*192 = 1,856` raw bytes (11,136 escaped) and the
/// empty-tail record is asserted below 12 KiB, leaving room for a bounded tail
/// inside the 16 KiB total.
pub(crate) const PERSISTABLE_RECORD_BYTES: usize = 16 * 1024;
pub(crate) const PERSISTABLE_TAIL_BYTES: usize = 12 * 1024;
const PERSISTABLE_ID_BYTES: usize = 256;
const PERSISTABLE_CODE_BYTES: usize = 128;
const PERSISTABLE_MESSAGE_BYTES: usize = 512;
const PERSISTABLE_EVIDENCE_BYTES: usize = 192;

/// Per-field string budgets for one serialized persistable record. The default
/// set is the S01 production contract; [`shrink_persistable_record`] scales it
/// down to fit a smaller consumer envelope.
#[derive(Clone, Copy)]
struct PersistableBudgets {
    id: usize,
    code: usize,
    message: usize,
    evidence: usize,
}

const PERSISTABLE_BUDGETS: PersistableBudgets = PersistableBudgets {
    id: PERSISTABLE_ID_BYTES,
    code: PERSISTABLE_CODE_BYTES,
    message: PERSISTABLE_MESSAGE_BYTES,
    evidence: PERSISTABLE_EVIDENCE_BYTES,
};

/// The smallest per-field budget that still leaves room for
/// [`bounded_prefix`]'s truncation marker.
const PERSISTABLE_MIN_BUDGET: usize = 8;

impl PersistableBudgets {
    fn shrunk(self) -> Self {
        Self {
            id: (self.id / 2).max(PERSISTABLE_MIN_BUDGET),
            code: (self.code / 2).max(PERSISTABLE_MIN_BUDGET),
            message: (self.message / 2).max(PERSISTABLE_MIN_BUDGET),
            evidence: (self.evidence / 2).max(PERSISTABLE_MIN_BUDGET),
        }
    }

    fn at_floor(self) -> bool {
        self.id <= PERSISTABLE_MIN_BUDGET
            && self.code <= PERSISTABLE_MIN_BUDGET
            && self.message <= PERSISTABLE_MIN_BUDGET
            && self.evidence <= PERSISTABLE_MIN_BUDGET
    }
}

/// The already-extracted materials of a persistable failure record. `detail`
/// mirrors the evidence shape: either the original detail JSON (production) or
/// a parsed record with the same top-level keys (shrink path).
struct PersistableFields<'a> {
    agent_id: &'a str,
    session_id: Option<&'a str>,
    stage: &'a str,
    error_code: &'a str,
    message: &'a str,
    detail: Option<&'a serde_json::Value>,
    stderr_tail: &'a str,
    tail_truncated: bool,
}

fn render_persistable(fields: &PersistableFields<'_>, budgets: &PersistableBudgets) -> String {
    let mut record = serde_json::json!({
        "agent_id": bounded_prefix(fields.agent_id, budgets.id),
        "session_id": fields.session_id.map(|value| bounded_prefix(value, budgets.id)),
        "stage": bounded_prefix(fields.stage, budgets.code),
        "error_code": bounded_prefix(fields.error_code, budgets.code),
        "message": bounded_prefix(
            fields
                .detail
                .and_then(|value| value.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(fields.message),
            budgets.message,
        ),
        "stderr_tail": fields.stderr_tail,
        "tail_truncated": fields.tail_truncated,
    });
    if let Some(detail) = fields.detail {
        for field in ["operation", "remote_message", "cleanup_result"] {
            if let Some(value) = detail.get(field).and_then(serde_json::Value::as_str) {
                record[field] = bounded_prefix(value, budgets.evidence).into();
            }
        }
        for field in ["bytes", "cap", "last_event_seq"] {
            if let Some(value) = detail.get(field).and_then(serde_json::Value::as_u64) {
                record[field] = value.into();
            }
        }
        for field in [
            "stall_elapsed_ms",
            "stall_timeout_ms",
            "last_progress_age_ms",
        ] {
            if let Some(value) = detail.get(field).and_then(serde_json::Value::as_u64) {
                record[field] = value.into();
            }
        }
        if let Some(code) = detail
            .get("remote_code")
            .and_then(serde_json::Value::as_i64)
        {
            record["remote_code"] = code.into();
        }
    }
    record.to_string()
}

pub(crate) fn persistable_record_with_tail(
    agent_id: &str,
    session_id: Option<&str>,
    stage: &str,
    error_code: &str,
    message: &str,
    stderr_tail: &str,
    tail_truncated: bool,
) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(message).ok();
    render_persistable(
        &PersistableFields {
            agent_id,
            session_id,
            stage,
            error_code,
            message,
            detail: detail.as_ref(),
            stderr_tail,
            tail_truncated,
        },
        &PERSISTABLE_BUDGETS,
    )
}

/// Structured re-shrink of an already-serialized persistable record for a
/// consumer whose envelope must stay smaller than the 16 KiB persistence
/// budget. It keeps the same field set, the same single-line parseable JSON,
/// and the latest `stderr_tail` suffix; the per-field budgets and the tail are
/// reduced only as far as `max_bytes` demands. Returns the floor record when
/// even the smallest budgets cannot fit `max_bytes`, and `None` only when
/// `record` is not a JSON object (for example the legacy plain-text
/// `fail_claim` message), which callers keep as an opaque string.
pub(crate) fn shrink_persistable_record(record: &str, max_bytes: usize) -> Option<String> {
    fn materials<'a>(
        value: &'a serde_json::Value,
        stderr_tail: &'a str,
        tail_truncated: bool,
    ) -> PersistableFields<'a> {
        PersistableFields {
            agent_id: value
                .get("agent_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            session_id: value.get("session_id").and_then(serde_json::Value::as_str),
            stage: value
                .get("stage")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            error_code: value
                .get("error_code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            message: value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            // The parsed record mirrors the evidence keys at its top level, so
            // it is its own detail source for the rebuild.
            detail: Some(value),
            stderr_tail,
            tail_truncated,
        }
    }

    let value = serde_json::from_str::<serde_json::Value>(record).ok()?;
    if !value.is_object() {
        return None;
    }
    let original_tail = value
        .get("stderr_tail")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let original_truncated = value
        .get("tail_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // Reduce the non-tail budgets until the empty-tail record fits the budget
    // (or the floor is reached and the caller must degrade further).
    let mut budgets = PERSISTABLE_BUDGETS;
    loop {
        let probe = render_persistable(&materials(&value, "", false), &budgets);
        if probe.len() <= max_bytes || budgets.at_floor() {
            break;
        }
        budgets = budgets.shrunk();
    }

    // The encoded length is monotone in the suffix size, so the smallest
    // feasible cut is the maximal feasible suffix. A cut also marks the tail
    // truncated, like the S01 path.
    let mut boundaries: Vec<usize> = original_tail.char_indices().map(|(index, _)| index).collect();
    boundaries.push(original_tail.len());
    let feasible = |index: usize| {
        let cut = boundaries[index];
        render_persistable(
            &materials(&value, &original_tail[cut..], original_truncated || cut > 0),
            &budgets,
        )
        .len()
            <= max_bytes
    };
    let chosen = if feasible(0) {
        0
    } else if !feasible(boundaries.len() - 1) {
        boundaries.len() - 1
    } else {
        let mut lo = 0;
        let mut hi = boundaries.len() - 1;
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if feasible(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        hi
    };
    let cut = boundaries[chosen];
    Some(render_persistable(
        &materials(&value, &original_tail[cut..], original_truncated || cut > 0),
        &budgets,
    ))
}

/// Build the bounded, persistable form of `runtime_failure_record`. The
/// returned string is always a single-line, parseable JSON object no larger
/// than [`PERSISTABLE_RECORD_BYTES`] whose `stderr_tail` keeps the latest
/// usable suffix. The whole serialized string is never prefix-cut.
pub(crate) fn persistable_failure_record(
    agent_id: &str,
    session_id: Option<&str>,
    stage: &str,
    error_code: &str,
    message: &str,
    stderr_tail: &str,
) -> String {
    // The independent 12 KiB cap is a hard limit, not a global optimum: the
    // search below only ever shrinks this already-capped window.
    let mut window_start = stderr_tail.len().saturating_sub(PERSISTABLE_TAIL_BYTES);
    while !stderr_tail.is_char_boundary(window_start) {
        window_start += 1;
    }
    let window = &stderr_tail[window_start..];
    let mut boundaries: Vec<usize> = window.char_indices().map(|(index, _)| index).collect();
    boundaries.push(window.len());
    let feasible = |index: usize| {
        let cut = boundaries[index];
        let truncated = window_start > 0 || cut > 0;
        persistable_record_with_tail(
            agent_id,
            session_id,
            stage,
            error_code,
            message,
            &window[cut..],
            truncated,
        )
        .len()
            <= PERSISTABLE_RECORD_BYTES
    };
    // `boundaries[0]` is the largest suffix (the whole capped window); the
    // last boundary is the empty suffix, which the 12 KiB empty-tail contract
    // guarantees feasible. Encoded length is monotone in suffix size, so the
    // smallest feasible cut is the maximal feasible suffix.
    let chosen = if feasible(0) {
        0
    } else {
        let mut lo = 0;
        let mut hi = boundaries.len() - 1;
        debug_assert!(feasible(hi));
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if feasible(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        hi
    };
    let cut = boundaries[chosen];
    persistable_record_with_tail(
        agent_id,
        session_id,
        stage,
        error_code,
        message,
        &window[cut..],
        window_start > 0 || cut > 0,
    )
}

pub(crate) fn bounded_prefix(message: &str, max_bytes: usize) -> String {
    if message.len() <= max_bytes {
        return message.to_owned();
    }
    const MARKER: &str = "…";
    let limit = max_bytes - MARKER.len();
    let end = message
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= limit)
        .last()
        .unwrap_or(0);
    format!("{}{}", &message[..end], MARKER)
}

pub(crate) fn update_latest_failure(
    failures: &mut HashMap<String, String>,
    agent_id: &str,
    message: String,
) {
    failures.insert(agent_id.into(), message);
}

pub(crate) const DIAGNOSTIC_QUEUE_CAPACITY: usize = 32;
pub(crate) const DIAGNOSTIC_RECORD_BYTES: usize = 192 * 1024;
pub(crate) const DIAGNOSTIC_FILE_BYTES: u64 = 1024 * 1024;
static FAILURE_LOGGER: OnceLock<Option<DiagnosticLogger>> = OnceLock::new();

// Installed LaunchAgents pass their existing stderr path here. There is only
// one writer per process; a broken diagnostic sink never prevents startup.
pub fn configure_diagnostic_log(path: Option<PathBuf>) {
    FAILURE_LOGGER.get_or_init(|| match path {
        Some(path) => DiagnosticLogger::start(RotatingDiagnosticWriter { path }).ok(),
        None => DiagnosticLogger::start(io::stderr()).ok(),
    });
}

pub(super) struct DiagnosticLogger {
    pub(super) sender: SyncSender<String>,
    pub(super) dropped: Arc<AtomicU64>,
}

impl DiagnosticLogger {
    pub(super) fn start<W: Write + Send + 'static>(mut writer: W) -> io::Result<Self> {
        let (sender, receiver) = sync_channel::<String>(DIAGNOSTIC_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let pending_drops = Arc::clone(&dropped);
        thread::Builder::new()
            .name("zcode-diagnostic-write".into())
            .spawn(move || {
                while let Ok(line) = receiver.recv() {
                    let count = pending_drops.swap(0, Ordering::Relaxed);
                    if count > 0 {
                        let marker =
                            format!("[external-subagentd] diagnostic_writes_dropped={count}\n");
                        if writer.write_all(marker.as_bytes()).is_err() {
                            pending_drops.fetch_add(count, Ordering::Relaxed);
                        }
                    }
                    if writer.write_all(line.as_bytes()).is_err() {
                        pending_drops.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })?;
        Ok(Self { sender, dropped })
    }

    pub(super) fn submit(&self, line: String) {
        // Bound both the queue length and each entry before enqueueing. Logging
        // never blocks scheduler control, even when the sink stops consuming.
        if line.len() > DIAGNOSTIC_RECORD_BYTES || self.sender.try_send(line).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(super) struct RotatingDiagnosticWriter {
    pub(super) path: PathBuf,
}

impl RotatingDiagnosticWriter {
    fn copy_tail(source: &Path, destination: &Path) -> io::Result<()> {
        let mut input = match fs::File::open(source) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let size = input.metadata()?.len();
        input.seek(SeekFrom::Start(size.saturating_sub(DIAGNOSTIC_FILE_BYTES)))?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(destination)?;
        io::copy(&mut input.take(DIAGNOSTIC_FILE_BYTES), &mut output)?;
        Ok(())
    }
}

impl Write for RotatingDiagnosticWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > DIAGNOSTIC_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic record exceeds file cap",
            ));
        }
        let mut output = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)?;
        if output.metadata()?.len().saturating_add(bytes.len() as u64) > DIAGNOSTIC_FILE_BYTES {
            let first = PathBuf::from(format!("{}.1", self.path.display()));
            let second = PathBuf::from(format!("{}.2", self.path.display()));
            Self::copy_tail(&first, &second)?;
            Self::copy_tail(&self.path, &first)?;
            // Keep the inode: launchd still owns an open stderr descriptor.
            // Renaming would strand that descriptor on an old rotation.
            output.set_len(0)?;
        }
        output.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
