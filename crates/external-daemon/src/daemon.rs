use super::*;

#[cfg(unix)]
pub struct Daemon {
    scheduler: Scheduler,
    shutdown_requested: Arc<AtomicBool>,
    shutdown_started: AtomicBool,
    claim_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    server: Mutex<Option<rpc::RpcServer>>,
    mcp_server: Mutex<Option<McpServer>>,
    _singleton_lock: SingletonLock,
}

#[cfg(unix)]
struct McpServer {
    path: PathBuf,
    socket_identity: rpc::SocketIdentity,
    shutdown: Arc<AtomicBool>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

#[cfg(unix)]
impl McpServer {
    fn bind(path: PathBuf, service: Arc<rpc::RpcService>) -> io::Result<Self> {
        use std::os::unix::net::UnixListener;
        rpc::remove_stale_socket(&path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = std::fs::symlink_metadata(&path)?;
        let socket_identity = rpc::SocketIdentity {
            device: std::os::unix::fs::MetadataExt::dev(&metadata),
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        };
        listener.set_nonblocking(true)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let loop_shutdown = Arc::clone(&shutdown);
        let wake_path = path.clone();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("MCP runtime");
            runtime.block_on(async move {
                let listener = match tokio::net::UnixListener::from_std(listener) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                while !loop_shutdown.load(Ordering::Acquire) {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            let service = Arc::clone(&service);
                            tokio::spawn(async move {
                                let result = rmcp::service::serve_server(
                                    crate::mcp::SubagentMcp::from_service(service),
                                    stream,
                                )
                                .await;
                                if let Ok(running) = result {
                                    let _ = running.waiting().await;
                                }
                            });
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            });
            let _ = rpc::remove_matching_socket(&wake_path, socket_identity);
        });
        Ok(Self {
            path,
            socket_identity,
            shutdown,
            thread: Mutex::new(Some(thread)),
        })
    }
    fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
        let _ = rpc::remove_matching_socket(&self.path, self.socket_identity);
    }
}

#[cfg(unix)]
impl Drop for McpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
struct SingletonLock {
    _file: std::fs::File,
}

#[cfg(unix)]
impl SingletonLock {
    fn acquire(database: &std::path::Path) -> io::Result<Self> {
        use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};

        let mut lock_name = database.as_os_str().to_os_string();
        lock_name.push(".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(std::path::PathBuf::from(lock_name))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "agent database already has a lifecycle owner",
                ));
            }
            return Err(error);
        }
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
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
        let mcp_path = server.path().with_extension("mcp");
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
                    // Claim-loop failures are non-fatal scheduling diagnostics:
                    // preserve the existing daemon error projection without
                    // rejecting or altering any task outcome.
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

#[cfg(unix)]
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

#[cfg(unix)]
impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}
