use super::*;

impl Scheduler {
    pub(super) fn record_runtime_failure(
        &self,
        agent_id: &str,
        session_id: Option<&str>,
        stage: &str,
        error_code: &str,
        message: &str,
        runtime: Option<&dyn ManagedRuntime>,
    ) {
        // Callers already stopped/reaped the runtime or observed its terminal
        // boundary. Do not introduce a diagnostic wait into scheduler control.
        let known_session = runtime.and_then(ManagedRuntime::diagnostic_session_id);
        let tail = runtime
            .map(ManagedRuntime::diagnostic_tail)
            .unwrap_or_default();
        let record = runtime_failure_record(
            agent_id,
            known_session.as_deref().or(session_id),
            stage,
            error_code,
            message,
            &tail,
        );
        self.record_failure_line(agent_id, record.clone(), &record);
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
                        let marker = format!("[external-subagentd] diagnostic_writes_dropped={count}\n");
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
