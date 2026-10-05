//! Darwin orphan spawning. Descriptor ownership, parent reaping and directive
//! writes form one trust boundary; no PID observation after a failed write is
//! allowed to authorize cleanup.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    fs::{File, OpenOptions},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
};

pub const HELPER_PARENT_WAIT: Duration = Duration::from_secs(2);
pub const HELPER_ACK_TIMEOUT: Duration = Duration::from_secs(10);
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const REAP_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const SPEC_LIMIT: usize = 256 * 1024;
const FRAME_LIMIT: usize = 512; // below Darwin PIPE_BUF; every frame is one write

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandSpec {
    program: String,
    argv: Vec<String>,
    cwd: Option<String>,
    env_set: BTreeMap<String, String>,
    env_remove: Vec<String>,
}
impl CommandSpec {
    fn from_command(command: &Command) -> io::Result<Self> {
        fn utf8(value: &OsStr) -> io::Result<String> {
            value.to_str().map(str::to_owned).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "detached Command contains non-UTF-8 text",
                )
            })
        }
        let mut spec = Self {
            program: utf8(command.get_program())?,
            argv: command.get_args().map(utf8).collect::<io::Result<_>>()?,
            cwd: command
                .get_current_dir()
                .map(|p| utf8(p.as_os_str()))
                .transpose()?,
            env_set: BTreeMap::new(),
            env_remove: Vec::new(),
        };
        for (key, value) in command.get_envs() {
            let key = utf8(key)?;
            match value {
                Some(value) => {
                    spec.env_set.insert(key, utf8(value)?);
                }
                None => spec.env_remove.push(key),
            }
        }
        Ok(spec)
    }
}

/// Deterministic integration-test gate. Production spawning uses no gates.
#[doc(hidden)]
#[derive(Default)]
pub struct TestGate {
    state: Mutex<(bool, bool)>,
    ready: Condvar,
}
impl TestGate {
    pub fn wait_arrived(&self, timeout: Duration) -> bool {
        let (guard, _) = self
            .ready
            .wait_timeout_while(self.state.lock().unwrap(), timeout, |s| !s.0)
            .unwrap();
        guard.0
    }
    pub fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.ready.notify_all();
    }
    pub(super) fn arrive_and_wait(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.0 = true;
        self.ready.notify_all();
        while !guard.1 {
            guard = self.ready.wait(guard).unwrap();
        }
    }
}

/// Hooks only accepted by Driver::spawn_for_test in debug builds. They never
/// enter the runtime spec, and normal production APIs always use defaults.
#[doc(hidden)]
#[derive(Default, Clone)]
pub struct DetachedTestOptions {
    pub helper: Option<PathBuf>,
    pub helper_env: BTreeMap<String, String>,
    pub trace: Option<PathBuf>,
    pub after_first_frame_delay: Duration,
    pub observed_identity: Option<ProcessIdentity>,
    pub register_error: Option<i32>,
    pub reader_done: Option<Sender<()>>,
    pub before_observe_delay: Duration,
    pub before_register_delay: Duration,
    pub force_nack: bool,
    pub handshake_timeout: Option<Duration>,
    pub reap_reply_timeout: Option<Duration>,
    pub reader_gate: Option<Arc<TestGate>>,
    pub reader_start_gate: Option<Arc<TestGate>>,
    pub monitor_gate: Option<Arc<TestGate>>,
}

pub(super) struct GatedReader {
    pub inner: Box<dyn Read + Send>,
    pub gate: Option<Arc<TestGate>>,
    pub start_gate: Option<Arc<TestGate>>,
}
impl Read for GatedReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if let Some(gate) = self.start_gate.take() {
            gate.arrive_and_wait();
        }
        let n = self.inner.read(bytes)?;
        if n == 0 {
            if let Some(gate) = self.gate.take() {
                gate.arrive_and_wait();
            }
        }
        Ok(n)
    }
}

fn trace(path: Option<&Path>, event: &str) {
    if let Some(path) = path {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{event}");
        }
    }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut pair = [-1; 2];
    if unsafe { libc::pipe(pair.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let pair = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
    for fd in [&pair.0, &pair.1] {
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(pair)
}
fn fixed_source(fd: OwnedFd) -> io::Result<OwnedFd> {
    let new = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5) };
    if new < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

pub(super) fn spawn(
    command: Command,
    helper: &Path,
    hooks: &DetachedTestOptions,
) -> io::Result<StartedRuntime> {
    let spec =
        serde_json::to_string(&CommandSpec::from_command(&command)?).map_err(io::Error::other)?;
    if spec.len() > SPEC_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "detached spec exceeds 256KB",
        ));
    }
    // RAII closes all five retained ends on EVERY error path, including spawn,
    // parent timeout, handshake, observe, register and directive failures.
    let gate = SPAWN_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let (input_r, input_w) = pipe()?;
    let (output_r, output_w) = pipe()?;
    let (error_r, error_w) = pipe()?;
    let (handshake_r, handshake_w) = pipe()?;
    let (directive_r, directive_w) = pipe()?;
    let handshake_w = fixed_source(handshake_w)?;
    let directive_r = fixed_source(directive_r)?;
    let hfd = handshake_w.as_raw_fd();
    let dfd = directive_r.as_raw_fd();
    let mut cmd = Command::new(helper);
    cmd.arg("__orphan-spawn")
        .arg(spec)
        .stdin(Stdio::from(input_r))
        .stdout(Stdio::from(output_w))
        .stderr(Stdio::from(error_w));
    if cfg!(debug_assertions) {
        for (key, value) in &hooks.helper_env {
            if key.starts_with("ES_HELPER_TEST_") {
                cmd.env(key, value);
            }
        }
        if let Some(path) = &hooks.trace {
            cmd.env("ES_HELPER_TEST_TRACE", path);
        }
    }
    unsafe {
        cmd.pre_exec(move || {
            if libc::setpgid(0, 0) < 0 || libc::dup2(hfd, 3) < 0 || libc::dup2(dfd, 4) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let parent_result = cmd.spawn(); // already inside SPAWN_GATE
                                     // Close originals even on Command::spawn failure, before leaving the gate.
    drop(cmd);
    drop(handshake_w);
    drop(directive_r);
    let mut parent = parent_result?;
    trace(hooks.trace.as_deref(), &format!("parent:{}", parent.id()));
    reap_parent(&mut parent, hooks.trace.as_deref())?;
    trace(hooks.trace.as_deref(), "parent_reaped");
    drop(gate);
    let handshake_deadline = Instant::now() + hooks.handshake_timeout.unwrap_or(HANDSHAKE_TIMEOUT);
    let mut handshake = File::from(handshake_r);
    let first = read_frame(&mut handshake, handshake_deadline).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "detached first frame: {e}; helper lost, runtime may remain; late frames ignored"
            ),
        )
    })?;
    if let Some(error) = first.get("error").and_then(serde_json::Value::as_str) {
        return Err(io::Error::other(format!("detached helper spawn: {error}")));
    }
    let pid = first
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 1 && *pid <= i32::MAX as u32)
        .ok_or_else(|| {
            io::Error::other("detached first frame has invalid pid; no cleanup authorization")
        })?;
    trace(hooks.trace.as_deref(), "first_frame_read");
    thread::sleep(hooks.after_first_frame_delay);
    thread::sleep(hooks.before_observe_delay);
    let observed = observe_process(pid);
    let identity = match observed {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return reap_reply(&directive_w, &mut handshake, hooks);
        }
        Err(error) => return nack(&directive_w, None, error, hooks),
    };
    let identity = hooks.observed_identity.clone().unwrap_or(identity);
    if let Err(error) = validate_spawn_identity(pid, &identity) {
        // A malformed identity is NEVER a signalling target, even if NACK succeeds.
        return nack(&directive_w, None, error, hooks);
    }
    trace(hooks.trace.as_deref(), "observed");
    thread::sleep(hooks.before_register_delay);
    let registration = match hooks.register_error {
        Some(code) => Err(io::Error::from_raw_os_error(code)),
        None => ExitQueue::register(pid),
    };
    let queue = match registration {
        Ok(queue) => queue,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
            return reap_reply(&directive_w, &mut handshake, hooks);
        }
        Err(error) => return nack(&directive_w, Some(&identity), error, hooks),
    };
    if hooks.force_nack {
        return nack(
            &directive_w,
            Some(&identity),
            io::Error::other("injected internal registration failure"),
            hooks,
        );
    }
    directive(&directive_w, b"ACK\n", hooks)?;
    Ok(StartedRuntime {
        stdin: RuntimeInput::Detached(File::from(input_w)),
        stdout: Box::new(File::from(output_r)),
        stderr: Box::new(File::from(error_r)),
        identity,
        child: None,
        queue: Some(queue),
    })
}

fn reap_parent(parent: &mut Child, path: Option<&Path>) -> io::Result<()> {
    fn bounded_wait(parent: &mut Child) -> io::Result<bool> {
        let deadline = Instant::now() + HELPER_PARENT_WAIT;
        loop {
            if parent.try_wait()?.is_some() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    match bounded_wait(parent) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(error) => {
            let _ = parent.kill();
            let rewait = bounded_wait(parent);
            return Err(io::Error::other(format!("helper parent wait failed: {error}; SIGKILL and re-wait: {rewait:?}; runtime may remain")));
        }
    }
    trace(path, "parent_SIGKILL");
    let killed = parent.kill();
    let rewait = bounded_wait(parent);
    trace(path, &format!("parent_rewait:{rewait:?}"));
    Err(io::Error::new(io::ErrorKind::TimedOut, format!(
        "helper parent wait exceeded 2s; SIGKILL={killed:?}; re-wait={rewait:?}; no directive sent; runtime may remain")))
}

fn directive(fd: &OwnedFd, bytes: &[u8], hooks: &DetachedTestOptions) -> io::Result<()> {
    write_atomic(fd.as_raw_fd(), bytes).map_err(|e| io::Error::new(e.kind(),
        format!("helper lost: directive write failed ({e}); no cleanup authorization; runtime may remain")))?;
    trace(
        hooks.trace.as_deref(),
        &format!("directive:{}", String::from_utf8_lossy(bytes).trim()),
    );
    Ok(())
}
fn nack(
    fd: &OwnedFd,
    identity: Option<&ProcessIdentity>,
    error: io::Error,
    hooks: &DetachedTestOptions,
) -> io::Result<StartedRuntime> {
    directive(fd, b"NACK\n", hooks)?; // only a successful write grants cleanup authority
    if let Some(identity) = identity {
        let cleanup = stop_and_reap_persisted_process_group(identity, Duration::from_secs(1));
        return Err(io::Error::other(format!(
            "detached NACK: {error}; identity cleanup={cleanup:?}"
        )));
    }
    Err(io::Error::other(format!(
        "detached NACK: {error}; no validated identity, no cleanup"
    )))
}
fn reap_reply(
    fd: &OwnedFd,
    input: &mut File,
    hooks: &DetachedTestOptions,
) -> io::Result<StartedRuntime> {
    directive(fd, b"REAP\n", hooks)?;
    let frame = read_frame(
        input,
        Instant::now() + hooks.reap_reply_timeout.unwrap_or(REAP_REPLY_TIMEOUT),
    )
    .map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("REAP reply failed: {e}; late frames ignored; no exit code"),
        )
    })?;
    let status = frame
        .get("exit")
        .and_then(serde_json::Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| io::Error::other("invalid REAP exit frame"))?;
    Err(io::Error::other(format!(
        "runtime died before detached registration: {:?}",
        decode_status(status)
    )))
}
fn write_atomic(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > FRAME_LIMIT {
        return Err(io::Error::other("helper frame exceeds atomic bound"));
    }
    loop {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n == bytes.len() as isize {
            return Ok(());
        }
        if n >= 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "partial helper frame",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
fn read_frame(input: &mut File, deadline: Instant) -> io::Result<serde_json::Value> {
    let mut bytes = Vec::new();
    loop {
        poll_read(input.as_raw_fd(), deadline)?;
        let mut byte = [0];
        match input.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "helper EOF without complete frame",
                ))
            }
            Ok(_) if byte[0] == b'\n' => {
                return serde_json::from_slice(&bytes).map_err(io::Error::other)
            }
            Ok(_) => bytes.push(byte[0]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        if bytes.len() >= FRAME_LIMIT {
            return Err(io::Error::other("oversized helper frame"));
        }
    }
}
fn poll_read(fd: RawFd, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "helper deadline elapsed"))?;
        let mut item = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe {
            libc::poll(
                &mut item,
                1,
                remaining
                    .as_millis()
                    .saturating_add(1)
                    .min(i32::MAX as u128) as i32,
            )
        };
        if result > 0 {
            if item.revents & libc::POLLNVAL != 0 {
                return Err(io::Error::other("invalid helper pipe"));
            }
            return Ok(()); // POLLHUP: read buffered bytes before EOF
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

pub(super) struct ExitQueue(OwnedFd);
impl ExitQueue {
    fn register(pid: u32) -> io::Result<Self> {
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let queue = Self(unsafe { OwnedFd::from_raw_fd(raw) });
        if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if unsafe { libc::kevent(raw, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(queue)
    }
    pub fn wait(self) -> io::Result<ChildExit> {
        loop {
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let n = unsafe {
                libc::kevent(
                    self.0.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    std::ptr::null(),
                )
            };
            if n == 1 {
                if event.flags & libc::EV_ERROR != 0 || event.fflags & libc::NOTE_EXIT == 0 {
                    return Err(io::Error::other("invalid process exit event"));
                }
                return Ok(decode_status(event.data as i32));
            }
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}
fn decode_status(status: i32) -> ChildExit {
    if libc::WIFEXITED(status) {
        ChildExit::Exited(Some(libc::WEXITSTATUS(status)))
    } else if libc::WIFSIGNALED(status) {
        ChildExit::Signaled(libc::WTERMSIG(status))
    } else {
        ChildExit::Unknown
    }
}

fn test_delay(name: &str) {
    if cfg!(debug_assertions) {
        if let Ok(value) = std::env::var(name) {
            if let Ok(ms) = value.parse::<u64>() {
                thread::sleep(Duration::from_millis(ms));
            }
        }
    }
}
fn test_flag(name: &str) -> bool {
    cfg!(debug_assertions) && std::env::var_os(name).is_some()
}
fn helper_trace(event: &str) {
    if cfg!(debug_assertions) {
        trace(
            std::env::var_os("ES_HELPER_TEST_TRACE")
                .as_deref()
                .map(Path::new),
            event,
        );
    }
}

/// Hidden same-binary entry point. Must run before daemon threads/configuration.
pub fn orphan_spawn_main() -> ! {
    let result = helper();
    if let Err(ref error) = result {
        let frame = serde_json::json!({"error": error.to_string()}).to_string() + "\n";
        let _ = write_atomic(3, frame.as_bytes());
    }
    unsafe { libc::_exit(if result.is_ok() { 0 } else { 126 }) }
}
fn helper() -> io::Result<()> {
    let spec_text = std::env::args()
        .nth(2)
        .ok_or_else(|| io::Error::other("missing helper spec"))?;
    if spec_text.len() > SPEC_LIMIT {
        return Err(io::Error::other("helper spec exceeds 256KB"));
    }
    let spec: CommandSpec = serde_json::from_str(&spec_text).map_err(io::Error::other)?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid > 0 {
        // This named hook pauses BEFORE closing the directive read end. Parent
        // death/reap, not scheduling order, is the daemon's exclusivity barrier.
        test_delay("ES_HELPER_TEST_PAUSE");
        helper_trace("parent_closing");
        unsafe {
            for fd in 0..=4 {
                libc::close(fd);
            }
            libc::_exit(0);
        }
    }
    let runtime = spawn_runtime(&spec)?;
    helper_trace(&format!("runtime:{runtime}"));
    unsafe {
        for fd in 0..=2 {
            libc::close(fd);
        }
    }
    if test_flag("ES_HELPER_TEST_CRASH_BEFORE_FRAME") {
        unsafe {
            libc::_exit(125);
        }
    }
    test_delay("ES_HELPER_TEST_FRAME_DELAY");
    let frame = serde_json::json!({"pid": runtime}).to_string() + "\n";
    write_atomic(3, frame.as_bytes())?;
    let deadline = Instant::now() + HELPER_ACK_TIMEOUT;
    let mut input = unsafe { File::from_raw_fd(4) };
    let mut instruction = Vec::new();
    loop {
        if let Err(error) = poll_read(4, deadline) {
            helper_trace(if error.kind() == io::ErrorKind::TimedOut {
                "helper_timeout"
            } else {
                "helper_read_error"
            });
            return Ok(()); // never reap without REAP
        }
        let mut byte = [0];
        let n = input.read(&mut byte)?;
        if n == 0 {
            helper_trace("helper_eof");
            return Ok(());
        }
        if byte[0] == b'\n' {
            break;
        }
        instruction.push(byte[0]);
        if instruction.len() > 4 {
            return Ok(());
        }
    }
    match instruction.as_slice() {
        b"ACK" | b"NACK" => Ok(()),
        b"REAP" => {
            if test_flag("ES_HELPER_TEST_REAP_EOF") {
                return Ok(());
            }
            let mut status = 0;
            loop {
                if unsafe { libc::waitpid(runtime, &mut status, 0) } == runtime {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            helper_trace(&format!("reaped:{runtime}"));
            test_delay("ES_HELPER_TEST_REAP_DELAY");
            let frame = serde_json::json!({"exit": status}).to_string() + "\n";
            write_atomic(3, frame.as_bytes())
        }
        _ => Ok(()),
    }
}

fn spawn_runtime(spec: &CommandSpec) -> io::Result<libc::pid_t> {
    if let Some(cwd) = &spec.cwd {
        std::env::set_current_dir(cwd)?;
    }
    // spawnp consults the caller's environ PATH. Apply the entire delta before
    // spawning, matching std's chdir -> environment -> execvp ordering.
    for key in spec.env_set.keys().chain(spec.env_remove.iter()) {
        if key.is_empty() || key.contains(['=', '\0']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid detached environment key",
            ));
        }
    }
    for (key, value) in &spec.env_set {
        CString::new(value.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in environment value"))?;
        std::env::set_var(key, value);
    }
    for key in &spec.env_remove {
        std::env::remove_var(key);
    }
    let cstring = |s: &str| {
        CString::new(s)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in detached spec"))
    };
    let program = cstring(&spec.program)?;
    let args: Vec<CString> = std::iter::once(&spec.program)
        .chain(spec.argv.iter())
        .map(|s| cstring(s))
        .collect::<io::Result<_>>()?;
    let mut argv: Vec<*mut libc::c_char> = args.iter().map(|s| s.as_ptr() as *mut _).collect();
    argv.push(std::ptr::null_mut());
    let env: Vec<CString> = std::env::vars_os()
        .map(|(k, v)| {
            let mut bytes = k.as_os_str().as_bytes().to_vec();
            bytes.push(b'=');
            bytes.extend_from_slice(v.as_os_str().as_bytes());
            CString::new(bytes).map_err(io::Error::other)
        })
        .collect::<io::Result<_>>()?;
    let mut envp: Vec<*mut libc::c_char> = env.iter().map(|s| s.as_ptr() as *mut _).collect();
    envp.push(std::ptr::null_mut());
    unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        let mut attrs: libc::posix_spawnattr_t = std::mem::zeroed();
        let check = |code| {
            if code == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(code))
            }
        };
        check(libc::posix_spawn_file_actions_init(&mut actions))?;
        if let Err(error) = check(libc::posix_spawnattr_init(&mut attrs)) {
            libc::posix_spawn_file_actions_destroy(&mut actions);
            return Err(error);
        }
        let result = (|| {
            for fd in 0..=2 {
                check(libc::posix_spawn_file_actions_adddup2(&mut actions, fd, fd))?;
            }
            for fd in 3..=4 {
                check(libc::posix_spawn_file_actions_addclose(&mut actions, fd))?;
            }
            check(libc::posix_spawnattr_setpgroup(&mut attrs, 0))?;
            let mut signals = std::mem::zeroed();
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, libc::SIGPIPE);
            check(libc::posix_spawnattr_setsigdefault(&mut attrs, &signals))?;
            check(libc::posix_spawnattr_setflags(
                &mut attrs,
                (libc::POSIX_SPAWN_SETPGROUP | libc::POSIX_SPAWN_SETSIGDEF) as i16,
            ))?;
            let mut pid = 0;
            check(libc::posix_spawnp(
                &mut pid,
                program.as_ptr(),
                &actions,
                &attrs,
                argv.as_ptr(),
                envp.as_ptr(),
            ))?;
            Ok(pid)
        })();
        libc::posix_spawnattr_destroy(&mut attrs);
        libc::posix_spawn_file_actions_destroy(&mut actions);
        result
    }
}
