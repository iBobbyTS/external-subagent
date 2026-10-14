//! Windows named pipe RPC transport skeleton.
//!
//! # Real-machine design notes (TODO(windows-real-machine)):
//! - Pipe namespace prefix: Windows named pipe paths MUST use the `\\.\pipe\` prefix
//!   (9 characters, starting with two backslashes).
//! - Instance limits: `CreateNamedPipeW` should specify a bounded `nMaxInstances`
//!   (matching `ServerOptions::max_connections`).
//! - Anti-hijacking: The first pipe instance MUST be created with `FILE_FLAG_FIRST_PIPE_INSTANCE`
//!   to ensure the daemon process is the authoritative creator and prevent squatter attacks.
//! - Stale socket equivalent: Windows named pipes do not have POSIX `dev`/`ino` file identities.
//!   Instead of unlinking stale filesystem sockets, stale detection is handled by probing
//!   `WaitNamedPipeW` / attempting connection to see if an active server is listening,
//!   or using security descriptors (DACL) restricting ownership to the current user token.
//!
//! All runtime operations currently fail closed with [`io::ErrorKind::Unsupported`].

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use super::{RpcRequest, RpcResponse, RpcService};
pub use super::transport::ServerOptions;

pub struct RpcServer {
    path: PathBuf,
}

impl RpcServer {
    pub fn bind(
        path: impl AsRef<Path>,
        service: Arc<RpcService>,
        options: ServerOptions,
    ) -> io::Result<Self> {
        let _ = (path, service, options);
        // TODO(windows-real-machine): implement Windows named pipe server transport
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "windows: named pipe transport pending real-machine implementation",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn shutdown(&self) {}
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
        let _ = (&self.path, &self.timeout, request, interrupted);
        // TODO(windows-real-machine): implement Windows named pipe client transport
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "windows: named pipe transport pending real-machine implementation",
        ))
    }
}
