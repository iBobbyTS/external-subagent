//! Windows daemon lifecycle.
//!
//! Mirrors the Unix implementation in `daemon.rs` for the non-Unix platform:
//! a single-instance database lock, the RPC named pipe listener, the MCP
//! named pipe listener, and the claim/scheduler thread with graceful shutdown.

use super::*;

pub struct Daemon {
    scheduler: Scheduler,
    shutdown_requested: Arc<AtomicBool>,
    shutdown_started: AtomicBool,
    claim_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    server: Mutex<Option<rpc::RpcServer>>,
    mcp_server: Mutex<Option<McpServer>>,
    _singleton_lock: SingletonLock,
}

struct McpServer {
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl McpServer {
    fn bind(path: PathBuf, service: Arc<rpc::RpcService>) -> io::Result<Self> {
        use crate::win_pipe::{create_instance, wide, SecurityAttributes};
        use std::os::windows::io::RawHandle;
        use tokio::net::windows::named_pipe::NamedPipeServer;
        let shutdown = Arc::new(AtomicBool::new(false));
        let loop_shutdown = Arc::clone(&shutdown);
        let listen_path = path.clone();
        let name = wide(&listen_path)?;
        // The RPC and MCP endpoints share the same current-user-only DACL and
        // anti-hijack flag; both are built here via the shared helper.
        let security = SecurityAttributes::current_user()?;
        // Build the accept-loop runtime on the calling thread and perform the
        // authoritative first instance create here (inside its reactor
        // context) so a squatter already owning this endpoint makes
        // `Daemon::start_inner` fail loudly instead of being silently
        // swallowed inside the accept loop. This mirrors the RPC bind's
        // anti-hijack contract. The runtime is then moved into the accept
        // thread, which drives the already-registered instance.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let first = runtime.block_on(async {
            let handle = create_instance(&name, 254, true, security.attributes())?;
            unsafe { NamedPipeServer::from_raw_handle(handle as RawHandle) }
        })?;
        let thread = thread::spawn(move || {
            runtime.block_on(async move {
                let mut pending = Some(first);
                loop {
                    if loop_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let server = match pending.take() {
                        Some(server) => server,
                        None => {
                            // Later instances must not carry
                            // FILE_FLAG_FIRST_PIPE_INSTANCE.
                            let handle =
                                match create_instance(&name, 254, false, security.attributes()) {
                                    Ok(handle) => handle,
                                    Err(_) => break,
                                };
                            match unsafe { NamedPipeServer::from_raw_handle(handle as RawHandle) } {
                                Ok(server) => server,
                                Err(_) => break,
                            }
                        }
                    };
                    match server.connect().await {
                        Ok(()) => {
                            let service = Arc::clone(&service);
                            tokio::spawn(async move {
                                let result = rmcp::service::serve_server(
                                    crate::mcp::SubagentMcp::from_service(service),
                                    server,
                                )
                                .await;
                                if let Ok(running) = result {
                                    let _ = running.waiting().await;
                                }
                            });
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                            pending = Some(server);
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            });
        });
        Ok(Self {
            path,
            shutdown,
            thread: Mutex::new(Some(thread)),
        })
    }

    fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        // Wake the pending `connect()` with a throwaway client. Tokio's named
        // pipe client registration needs a reactor context, so open inside a
        // short-lived current-thread runtime. The accept loop can be between
        // `create` and `connect`, so retry briefly.
        let wake_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        for _ in 0..50 {
            let opened = wake_runtime.as_ref().ok().map(|runtime| {
                runtime.block_on(async {
                    tokio::net::windows::named_pipe::ClientOptions::new().open(&self.path)
                })
            });
            if matches!(opened, Some(Ok(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct SingletonLock {
    _file: std::fs::File,
}

impl SingletonLock {
    fn acquire(database: &std::path::Path) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{GetLastError, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION},
            Storage::FileSystem::{
                LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
            },
            System::IO::OVERLAPPED,
        };

        let mut lock_name = database.as_os_str().to_os_string();
        lock_name.push(".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(std::path::PathBuf::from(lock_name))?;
        let mut overlapped = OVERLAPPED::default();
        let result = unsafe {
            LockFileEx(
                file.as_raw_handle() as _,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if result == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_LOCK_VIOLATION || error == ERROR_SHARING_VIOLATION {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "agent database already has a lifecycle owner",
                ));
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        Ok(Self { _file: file })
    }
}

impl Daemon {
    pub fn start(
        socket: impl AsRef<std::path::Path>,
        scheduler: Scheduler,
        server_options: rpc::ServerOptions,
        claim_interval: Duration,
    ) -> io::Result<Self> {
        Self::start_with_shutdown(
            socket,
            scheduler,
            server_options,
            claim_interval,
            Arc::new(AtomicBool::new(false)),
        )
    }

    pub fn start_with_shutdown(
        socket: impl AsRef<std::path::Path>,
        scheduler: Scheduler,
        server_options: rpc::ServerOptions,
        claim_interval: Duration,
        shutdown_requested: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        Self::start_inner(
            socket,
            scheduler,
            server_options,
            claim_interval,
            shutdown_requested,
            || {},
        )
    }

    fn start_inner<F>(
        socket: impl AsRef<std::path::Path>,
        scheduler: Scheduler,
        server_options: rpc::ServerOptions,
        claim_interval: Duration,
        shutdown_requested: Arc<AtomicBool>,
        before_reconcile: F,
    ) -> io::Result<Self>
    where
        F: FnOnce(),
    {
        if claim_interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "claim interval must be positive",
            ));
        }
        check_startup_shutdown(&shutdown_requested)?;
        let singleton_lock = SingletonLock::acquire(scheduler.store().database_path())?;
        check_startup_shutdown(&shutdown_requested)?;
        before_reconcile();
        check_startup_shutdown(&shutdown_requested)?;
        scheduler
            .reconcile_startup()
            .map_err(|error| io::Error::other(error.to_string()))?;
        check_startup_shutdown(&shutdown_requested)?;
        let service = Arc::new(
            rpc::RpcService::new(scheduler.clone(), scheduler.store())
                .map_err(|_| io::Error::other("RPC service initialization failed"))?,
        );
        let server = rpc::RpcServer::bind(socket, Arc::clone(&service), server_options)?;
        let mut mcp_name = server.path().as_os_str().to_os_string();
        mcp_name.push(".mcp");
        let mcp_path = PathBuf::from(mcp_name);
        let mcp_server = match McpServer::bind(mcp_path, Arc::clone(&service)) {
            Ok(server) => server,
            Err(error) => {
                server.shutdown();
                return Err(error);
            }
        };
        if let Err(error) = check_startup_shutdown(&shutdown_requested) {
            server.shutdown();
            return Err(error);
        }
        let loop_shutdown = Arc::clone(&shutdown_requested);
        let loop_scheduler = scheduler.clone();
        let claim_thread = thread::spawn(move || {
            while !loop_shutdown.load(Ordering::Acquire) {
                if let Err(error) = loop_scheduler.start_ready() {
                    loop_scheduler.record_failure("__daemon__", error.to_string());
                }
                thread::sleep(claim_interval);
            }
        });
        Ok(Self {
            scheduler,
            shutdown_requested,
            shutdown_started: AtomicBool::new(false),
            claim_thread: Mutex::new(Some(claim_thread)),
            server: Mutex::new(Some(server)),
            mcp_server: Mutex::new(Some(mcp_server)),
            _singleton_lock: singleton_lock,
        })
    }

    pub fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(server) = self.server.lock().unwrap().take() {
            server.shutdown();
        }
        if let Some(server) = self.mcp_server.lock().unwrap().take() {
            server.shutdown();
        }
        if let Some(claim_thread) = self.claim_thread.lock().unwrap().take() {
            let _ = claim_thread.join();
        }
        self.scheduler.shutdown_all();
    }
}

fn check_startup_shutdown(shutdown_requested: &AtomicBool) -> io::Result<()> {
    if shutdown_requested.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "daemon shutdown requested during startup",
        ))
    } else {
        Ok(())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}
