//! Windows named pipe RPC transport.
//!
//! The server and client mirror the semantics of the Unix transport
//! (`rpc/unix.rs`): a byte-mode, newline-framed JSON protocol, bounded
//! concurrent connections with a reachable `busy` rejection, a per-connection
//! deadline, and a fail-closed anti-hijack guard.
//!
//! Win32 specifics:
//! - Pipe namespace prefix `\\.\pipe\` (see `cli/paths.mjs:99`).
//! - `CreateNamedPipeW` is used directly (rather than the tokio wrapper) so the
//!   first instance can carry `FILE_FLAG_FIRST_PIPE_INSTANCE` and an explicit
//!   `SECURITY_ATTRIBUTES` whose DACL grants access only to the current user's
//!   SID (plus LocalSystem). This prevents a squatter process from creating or
//!   owning the daemon endpoint.
//! - Every pipe operation is overlapped so reads/writes honor the same
//!   `ServerOptions::connection_timeout` / client deadline semantics as the
//!   Unix arm, including cancellation with `CancelIoEx`.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::win_pipe::{create_instance, wide, SecurityAttributes};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA,
        ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE,
        HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            ConnectNamedPipe, DisconnectNamedPipe, PeekNamedPipe, WaitNamedPipeW,
        },
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

use super::transport::{handle_connection, reap_finished_workers, write_busy};
use super::{
    RpcRequest, RpcResponse, RpcService, MAX_REQUEST_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES,
};
pub use super::transport::ServerOptions;

pub struct RpcServer {
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
    accept_thread: Mutex<Option<JoinHandle<()>>>,
}

impl RpcServer {
    pub fn bind(
        path: impl AsRef<Path>,
        service: Arc<RpcService>,
        options: ServerOptions,
    ) -> io::Result<Self> {
        if options.max_connections == 0 || options.connection_timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "server bounds must be positive",
            ));
        }
        // `nMaxInstances` caps at 254 and the accept loop needs one spare
        // listener beyond the active workers, so the live bound is 253.
        if options.max_connections > 253 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_connections exceeds the Windows named pipe instance cap",
            ));
        }
        let path = path.as_ref().to_path_buf();
        let name = wide(&path)?;
        // One spare instance beyond `max_connections` keeps the `write_busy`
        // signaling path reachable: when `max_connections` workers are active,
        // the spare listener still accepts the next client so it can be told
        // the server is at capacity instead of merely blocking in the kernel.
        let max_instances = options.max_connections.saturating_add(1).clamp(1, 254) as u32;
        let security = SecurityAttributes::current_user()?;
        let first = SendHandle(create_instance(&name, max_instances, true, security.attributes())?);
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown = Arc::clone(&shutdown);
        let active = Arc::new(AtomicUsize::new(0));
        let timeout = options.connection_timeout;
        let max_connections = options.max_connections;
        let accept_thread = thread::spawn(move || {
            // `Option` keeps the ownership transfer explicit: once a handle is
            // handed to a worker it is `None`, so the post-loop cleanup can
            // never close a worker's live handle.
            let mut listen = Some(first.into_handle());
            let mut workers: Vec<JoinHandle<()>> = Vec::new();
            loop {
                reap_finished_workers(&mut workers);
                if accept_shutdown.load(Ordering::Acquire) {
                    break;
                }
                let Some(handle) = listen else { break };
                match connect_instance(handle, &accept_shutdown) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
                if active.load(Ordering::Acquire) >= max_connections {
                    // Reachable busy path: signal the capacity rejection on the
                    // accepted instance, give the client a bounded window to
                    // drain it, then disconnect and reuse the instance.
                    let written = Arc::new(AtomicUsize::new(0));
                    write_busy(BorrowedPipe {
                        handle,
                        timeout: Some(timeout),
                        written: Arc::clone(&written),
                    });
                    drain_busy_instance(handle, written.load(Ordering::Acquire), timeout, &accept_shutdown);
                    unsafe { DisconnectNamedPipe(handle) };
                    continue;
                }
                active.fetch_add(1, Ordering::AcqRel);
                let mut stream = PipeStream::from_handle(handle);
                stream.set_read_timeout(Some(timeout));
                stream.set_write_timeout(Some(timeout));
                // Ownership transferred to the worker: neutralize `listen` so a
                // later create failure cannot double-close this handle.
                listen = None;
                let service = Arc::clone(&service);
                let worker_active = Arc::clone(&active);
                let worker_shutdown = Arc::clone(&accept_shutdown);
                workers.push(thread::spawn(move || {
                    handle_connection(stream, &service, &worker_shutdown, peer_disconnected);
                    worker_active.fetch_sub(1, Ordering::AcqRel);
                }));
                match create_instance(&name, max_instances, false, security.attributes()) {
                    Ok(next) => listen = Some(next),
                    Err(_) => break,
                }
            }
            if let Some(handle) = listen {
                unsafe { CloseHandle(handle) };
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        Ok(Self {
            path,
            shutdown,
            accept_thread: Mutex::new(Some(accept_thread)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(thread) = self.accept_thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RpcServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug, Clone)]
pub struct RpcClient {
    path: PathBuf,
    timeout: Duration,
}

impl RpcClient {
    pub fn new(path: impl AsRef<Path>, timeout: Duration) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            timeout,
        }
    }

    pub fn call(&self, request: &RpcRequest) -> io::Result<RpcResponse> {
        self.call_interruptible(request, &|| false)
    }

    pub(crate) fn call_interruptible(
        &self,
        request: &RpcRequest,
        interrupted: &dyn Fn() -> bool,
    ) -> io::Result<RpcResponse> {
        let deadline = Instant::now() + self.timeout;
        let mut stream = PipeStream::from_handle(connect_client(&self.path, deadline)?);
        let mut frame = serde_json::to_vec(request)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        if frame.len() + 1 > MAX_REQUEST_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request frame exceeds cap",
            ));
        }
        frame.push(b'\n');
        write_all_until(&mut stream, &frame, deadline)?;
        let response = read_limited_frame_until(
            &mut stream,
            MAX_RESPONSE_FRAME_BYTES - 1,
            deadline,
            interrupted,
        )?;
        serde_json::from_slice(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "RPC deadline elapsed"))
}

fn write_all_until(stream: &mut PipeStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?));
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "RPC peer closed")),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn read_limited_frame_until(
    stream: &mut PipeStream,
    limit: usize,
    deadline: Instant,
    interrupted: &dyn Fn() -> bool,
) -> io::Result<Vec<u8>> {
    let mut frame = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        if interrupted() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "RPC wait interrupted",
            ));
        }
        let read_remaining = remaining(deadline)?;
        // Poll in bounded slices so the `interrupted` callback is observed even
        // when the peer stays silent.
        let slice = read_remaining.min(Duration::from_millis(50));
        stream.set_read_timeout(Some(slice));
        let read = match stream.read(&mut buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "RPC peer closed before frame",
                ))
            }
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        if let Some(newline) = buffer[..read].iter().position(|byte| *byte == b'\n') {
            frame.extend_from_slice(&buffer[..newline]);
            if frame.len() > limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame exceeds cap",
                ));
            }
            return Ok(frame);
        }
        frame.extend_from_slice(&buffer[..read]);
        if frame.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds cap",
            ));
        }
    }
}

fn peer_disconnected(stream: &PipeStream) -> bool {
    let mut available = 0u32;
    let ok = unsafe {
        PeekNamedPipe(
            stream.handle,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &mut available,
            ptr::null_mut(),
        )
    };
    if ok != 0 {
        return false;
    }
    matches!(
        unsafe { GetLastError() },
        ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA
    )
}

fn duration_ms(duration: Duration) -> u32 {
    duration.as_millis().min(u32::MAX as u128) as u32
}

/// Wait for a client to connect to `handle`, polling `shutdown` so the accept
/// loop can terminate promptly. Returns `Ok(false)` when shutdown won.
fn connect_instance(handle: HANDLE, shutdown: &AtomicBool) -> io::Result<bool> {
    let event = create_event()?;
    let mut overlapped = OVERLAPPED::default();
    overlapped.hEvent = event;
    let ok = unsafe { ConnectNamedPipe(handle, &mut overlapped) };
    let result = if ok != 0 {
        Ok(true)
    } else {
        let error = unsafe { GetLastError() };
        match error {
            ERROR_PIPE_CONNECTED => Ok(true),
            ERROR_IO_PENDING => {
                let mut connected = false;
                loop {
                    match unsafe { WaitForSingleObject(event, 50) } {
                        WAIT_OBJECT_0 => {
                            connected = true;
                            break;
                        }
                        WAIT_TIMEOUT => {
                            if shutdown.load(Ordering::Acquire) {
                                unsafe { CancelIoEx(handle, &overlapped) };
                                let mut transferred = 0u32;
                                let _ = unsafe {
                                    GetOverlappedResult(handle, &overlapped, &mut transferred, 1)
                                };
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                Ok(connected)
            }
            other => Err(io::Error::from_raw_os_error(other as i32)),
        }
    };
    unsafe { CloseHandle(event) };
    result
}

fn connect_client(path: &Path, deadline: Instant) -> io::Result<HANDLE> {
    let name = wide(path)?;
    loop {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(handle);
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_PIPE_BUSY {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let wait = remaining(deadline)?;
        if unsafe { WaitNamedPipeW(name.as_ptr(), duration_ms(wait)) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "named pipe server did not become available",
            ));
        }
    }
}

fn create_event() -> io::Result<HANDLE> {
    let event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
    if event.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(event)
    }
}

fn wait_overlapped(
    handle: HANDLE,
    overlapped: &mut OVERLAPPED,
    event: HANDLE,
    timeout: Option<Duration>,
) -> io::Result<u32> {
    let waited = match timeout {
        Some(duration) => unsafe { WaitForSingleObject(event, duration_ms(duration)) },
        None => unsafe { WaitForSingleObject(event, u32::MAX) },
    };
    match waited {
        WAIT_OBJECT_0 => {
            let mut transferred = 0u32;
            if unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(transferred)
        }
        WAIT_TIMEOUT => {
            unsafe { CancelIoEx(handle, overlapped) };
            let mut transferred = 0u32;
            // Cancel vs completion race: the operation may have completed
            // between the wait timing out and CancelIoEx. Reap it with a
            // blocking GetOverlappedResult and return the completed transfer;
            // only a genuinely aborted/incomplete operation becomes a timeout.
            if unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, 1) } != 0 {
                Ok(transferred)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "pipe operation timed out",
                ))
            }
        }
        _ => {
            unsafe { CancelIoEx(handle, overlapped) };
            let mut transferred = 0u32;
            if unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, 1) } != 0 {
                Ok(transferred)
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}

/// Issue one overlapped pipe operation, honoring the synchronous-completion
/// fast path and the pending path with a wait/cancel timeout.
fn overlapped_call<F>(handle: HANDLE, timeout: Option<Duration>, issue: F) -> io::Result<u32>
where
    F: FnOnce(*mut OVERLAPPED, *mut u32) -> i32,
{
    let event = create_event()?;
    let mut overlapped = OVERLAPPED::default();
    overlapped.hEvent = event;
    let mut transferred = 0u32;
    let ok = issue(&mut overlapped, &mut transferred);
    let result = if ok != 0 {
        Ok(transferred)
    } else {
        let error = unsafe { GetLastError() };
        if error == ERROR_IO_PENDING {
            wait_overlapped(handle, &mut overlapped, event, timeout)
        } else {
            Err(io::Error::from_raw_os_error(error as i32))
        }
    };
    unsafe { CloseHandle(event) };
    result
}

pub(crate) struct PipeStream {
    handle: HANDLE,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

unsafe impl Send for PipeStream {}

/// Moves a raw pipe handle across a thread boundary. The handle is uniquely
/// owned by the receiving thread for its lifetime.
struct SendHandle(HANDLE);

unsafe impl Send for SendHandle {}

impl SendHandle {
    fn into_handle(self) -> HANDLE {
        self.0
    }
}

impl PipeStream {
    fn from_handle(handle: HANDLE) -> Self {
        Self {
            handle,
            read_timeout: None,
            write_timeout: None,
        }
    }

    pub(crate) fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    pub(crate) fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl Read for PipeStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let handle = self.handle;
        let length = buffer.len().min(u32::MAX as usize) as u32;
        let read = overlapped_call(handle, self.read_timeout, |overlapped, transferred| unsafe {
            ReadFile(handle, buffer.as_mut_ptr(), length, transferred, overlapped)
        })?;
        Ok(read as usize)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let handle = self.handle;
        let length = buffer.len().min(u32::MAX as usize) as u32;
        let written = overlapped_call(handle, self.write_timeout, |overlapped, transferred| unsafe {
            WriteFile(handle, buffer.as_ptr(), length, transferred, overlapped)
        })?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A non-owning write view over a listening pipe instance so the accept loop
/// can reuse the handle after signaling a busy rejection. The shared counter
/// records how many frame bytes were buffered for the client.
struct BorrowedPipe {
    handle: HANDLE,
    timeout: Option<Duration>,
    written: Arc<AtomicUsize>,
}

impl Write for BorrowedPipe {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let handle = self.handle;
        let length = buffer.len().min(u32::MAX as usize) as u32;
        let written = overlapped_call(handle, self.timeout, |overlapped, transferred| unsafe {
            WriteFile(handle, buffer.as_ptr(), length, transferred, overlapped)
        })?;
        self.written.fetch_add(written as usize, Ordering::AcqRel);
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Bound the busy rejection drain so the accept thread (and therefore
/// `RpcServer::shutdown()`'s join) never blocks unboundedly. `write_busy` has
/// already buffered the frame; wait in short slices for the client to consume
/// it and close, or for `timeout` to elapse, then disconnect and reuse the
/// instance.
///
/// Note: a server-side `PeekNamedPipe` reports only inbound availability
/// (client→server) and cannot directly observe outbound consumption of the
/// frame, so the observable proxy for "client consumed the frame and went away"
/// is a broken/not-connected peek. A client that reads and then closes (the
/// `cli/rpc.mjs` behavior) ends the drain immediately; otherwise it is bounded
/// by `timeout`.
fn drain_busy_instance(handle: HANDLE, written: usize, timeout: Duration, shutdown: &AtomicBool) {
    if written == 0 {
        return;
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // Observe an in-flight shutdown within one slice so `RpcServer::shutdown`
        // never waits out the remaining connection_timeout.
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let mut available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                handle,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let error = unsafe { GetLastError() };
            if matches!(
                error,
                ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA
            ) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_status::AgentEvidenceStore, CommandRuntimeFactory, Scheduler, SchedulerConfig,
    };
    use external_store::{Store, TaskRecord};
    use std::process::Command as StdCommand;

    fn unique_pipe() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        PathBuf::from(format!(
            r"\\.\pipe\external-subagent-s01-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn fixture() -> (tempfile::TempDir, Arc<RpcService>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| -> io::Result<StdCommand> {
            panic!("transport fixture must never start a runtime")
        });
        let scheduler = Scheduler::new(
            "s01-transport",
            store.clone(),
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let service = Arc::new(
            RpcService::new_with_service_generation_and_evidence(
                scheduler,
                store,
                "s01-transport".into(),
                AgentEvidenceStore::unprobed(),
            )
            .unwrap(),
        );
        (directory, service)
    }

    fn status_request(request_id: &str) -> RpcRequest {
        RpcRequest {
            request_id: request_id.into(),
            method: super::super::RpcMethod::SystemStatus,
        }
    }

    #[test]
    fn client_round_trips_status_over_the_named_pipe() {
        let (_directory, service) = fixture();
        let path = unique_pipe();
        let server = RpcServer::bind(&path, service, ServerOptions::default()).unwrap();
        let client = RpcClient::new(&path, Duration::from_secs(5));
        let response = client.call(&status_request("roundtrip")).unwrap();
        assert!(matches!(
            response.outcome,
            super::super::RpcOutcome::Success { .. }
        ));
        server.shutdown();
    }

    #[test]
    fn second_bind_is_rejected_as_in_use() {
        let (_directory, service) = fixture();
        let path = unique_pipe();
        let server = RpcServer::bind(&path, service, ServerOptions::default()).unwrap();
        let (_directory2, service2) = fixture();
        let error = RpcServer::bind(&path, service2, ServerOptions::default())
            .err()
            .expect("a squatted endpoint must reject the second bind");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        server.shutdown();
    }

    #[test]
    fn connection_bound_beyond_the_instance_cap_is_rejected() {
        let (_directory, service) = fixture();
        let path = unique_pipe();
        let error = RpcServer::bind(
            &path,
            service,
            ServerOptions {
                max_connections: 254,
                connection_timeout: Duration::from_secs(1),
            },
        )
        .err()
        .expect("max_connections above the Windows named pipe instance cap must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn busy_frame_is_reachable_at_the_connection_limit() {
        let (_directory, service) = fixture();
        let path = unique_pipe();
        let server = RpcServer::bind(
            &path,
            service,
            ServerOptions {
                max_connections: 1,
                connection_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        // Hold the single slot with an incomplete request frame.
        let mut hold = PipeStream::from_handle(
            connect_client(&path, Instant::now() + Duration::from_secs(5)).unwrap(),
        );
        hold.set_write_timeout(Some(Duration::from_secs(2)));
        hold.write_all(b"{\"request_id\":\"hold\"").unwrap();
        hold.flush().unwrap();
        thread::sleep(Duration::from_millis(200));
        let client = RpcClient::new(&path, Duration::from_secs(5));
        let response = client.call(&status_request("busy")).unwrap();
        match response.outcome {
            super::super::RpcOutcome::Error { error } => {
                assert_eq!(error.code, super::super::RpcErrorCode::Conflict);
                assert!(
                    error.message.contains("connection limit reached"),
                    "{}",
                    error.message
                );
            }
            other => panic!("expected a busy rejection, got {other:?}"),
        }
        drop(hold);
        server.shutdown();
    }

    #[test]
    fn shutdown_releases_the_endpoint_for_rebinding() {
        let (_directory, service) = fixture();
        let path = unique_pipe();
        let server = RpcServer::bind(&path, service, ServerOptions::default()).unwrap();
        let started = Instant::now();
        server.shutdown();
        assert!(started.elapsed() < Duration::from_secs(2));
        let (_directory2, service2) = fixture();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match RpcServer::bind(&path, Arc::clone(&service2), ServerOptions::default()) {
                Ok(server) => {
                    server.shutdown();
                    break;
                }
                Err(error) if Instant::now() < deadline => {
                    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("endpoint was not released: {error}"),
            }
        }
    }
}

#[cfg(test)]
mod daemon_tests {
    use super::*;
    use crate::{CommandRuntimeFactory, Scheduler, SchedulerConfig};
    use external_store::{Store, TaskRecord};
    use std::process::Command as StdCommand;

    #[test]
    fn start_with_shutdown_binds_and_stops_cleanly() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| -> io::Result<StdCommand> {
            panic!("daemon fixture must never start a runtime")
        });
        let scheduler = Scheduler::new(
            "s01-daemon",
            store,
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = PathBuf::from(format!(
            r"\\.\pipe\external-subagent-s01-daemon-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let shutdown = Arc::new(AtomicBool::new(false));
        let daemon = crate::Daemon::start_with_shutdown(
            &path,
            scheduler,
            ServerOptions::default(),
            Duration::from_millis(20),
            Arc::clone(&shutdown),
        )
        .unwrap();
        let client = RpcClient::new(&path, Duration::from_secs(5));
        let response = client
            .call(&RpcRequest {
                request_id: "daemon".into(),
                method: crate::rpc::RpcMethod::SystemStatus,
            })
            .unwrap();
        assert!(matches!(response.outcome, crate::rpc::RpcOutcome::Success { .. }));
        let started = Instant::now();
        shutdown.store(true, Ordering::Release);
        daemon.shutdown();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn squatted_mcp_endpoint_fails_daemon_start() {
        let directory = tempfile::tempdir().unwrap();
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let rpc_path = PathBuf::from(format!(
            r"\\.\pipe\external-subagent-s01-mcp-squat-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mcp_path = {
            let mut name = rpc_path.as_os_str().to_os_string();
            name.push(".mcp");
            PathBuf::from(name)
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _squatter = runtime
            .block_on(async {
                tokio::net::windows::named_pipe::ServerOptions::new()
                    .pipe_mode(tokio::net::windows::named_pipe::PipeMode::Byte)
                    .create(&mcp_path)
            })
            .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| -> io::Result<StdCommand> {
            panic!("mcp squat fixture must never start a runtime")
        });
        let scheduler = Scheduler::new(
            "s01-mcp-squat",
            store,
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let error = crate::Daemon::start_with_shutdown(
            &rpc_path,
            scheduler,
            ServerOptions::default(),
            Duration::from_millis(20),
            Arc::clone(&shutdown),
        )
        .err()
        .expect("a squatted MCP endpoint must fail daemon start");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }
}
