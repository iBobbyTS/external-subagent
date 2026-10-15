use std::{env, io, path::PathBuf};

#[tokio::main]
async fn main() -> io::Result<()> {
    let socket = env::var_os("EXTERNAL_SUBAGENT_SOCKET")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "EXTERNAL_SUBAGENT_SOCKET is required",
            )
        })?;
    if !socket.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "EXTERNAL_SUBAGENT_SOCKET must be absolute",
        ));
    }
    // The installer keeps the stable daemon RPC socket in configuration;
    // MCP is exposed by the sibling `.mcp` stream endpoint.  Resolve that
    // endpoint when it is present while retaining the explicit socket for
    // isolated process tests and non-standard deployments.
    #[cfg(unix)]
    let endpoint =
        if socket.file_name().and_then(|name| name.to_str()) == Some("external-subagent.sock") {
            socket.with_extension("mcp")
        } else {
            socket
        };
    // Windows mirror of `daemon_windows.rs`'s MCP derivation: the daemon always
    // serves MCP at `<rpc endpoint>.mcp`. Only the product RPC named pipe
    // (`\\.\pipe\<product>-daemon`, release or debug) is rewritten; isolated
    // and non-standard sockets are retained verbatim, matching the Unix arm's
    // explicit-socket behavior. The string append mirrors the daemon.
    #[cfg(windows)]
    let endpoint = {
        let is_product_daemon_pipe = {
            let text = socket.to_string_lossy();
            text.starts_with(r"\\.\pipe\") && text.ends_with("-daemon")
        };
        if is_product_daemon_pipe {
            let mut name = socket.as_os_str().to_os_string();
            name.push(".mcp");
            PathBuf::from(name)
        } else {
            socket
        }
    };
    #[cfg(not(any(unix, windows)))]
    let endpoint = socket;
    external_mcp::serve_stdio(endpoint).await
}
