#[cfg(unix)]
mod unix_transport {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    fn socket() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "zcode-bridge-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn forwards_bytes_and_exits_when_daemon_closes() {
        let path = socket();
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut input = [0_u8; 4];
            stream.read_exact(&mut input).unwrap();
            assert_eq!(&input, b"ping");
            stream.write_all(b"pong").unwrap();
            thread::sleep(Duration::from_millis(500));
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"ping").unwrap();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(stdin);
        });
        let output = child.wait_with_output().unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"pong");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn connection_failure_is_nonzero_and_explains_socket() {
        let path = socket();
        let output = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("failed to connect to daemon MCP socket"),
            "{stderr}"
        );
    }

    #[test]
    fn stdin_eof_half_closes_without_hanging() {
        let path = socket();
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0_u8; 1];
            assert_eq!(stream.read(&mut buf).unwrap(), 0);
            thread::sleep(Duration::from_millis(100));
        });
        let output = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stdin_eof_exits_while_daemon_keeps_session_open() {
        let path = socket();
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0_u8; 1];
            assert_eq!(stream.read(&mut buf).unwrap(), 0);
            thread::sleep(Duration::from_millis(500));
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("bridge did not exit after stdin EOF");
            }
            thread::sleep(Duration::from_millis(10));
        };
        server.join().unwrap();
        assert!(status.success());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn daemon_eof_is_transport_failure_with_nonzero_exit() {
        let path = socket();
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Keep stdin open while the server closes first, deterministically
        // exercising daemon-first termination.
        let stdin = child.stdin.take().unwrap();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(stdin);
        });
        let output = child.wait_with_output().unwrap();
        let status = output.status;
        let stderr = output.stderr;
        server.join().unwrap();
        assert!(!status.success());
        let stderr = String::from_utf8_lossy(&stderr);
        assert!(
            stderr.contains("daemon MCP stream closed unexpectedly (EOF)"),
            "{stderr}"
        );
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
mod windows_transport {
    use std::{
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions},
    };

    fn pipe_name() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            r"\\.\pipe\zcode-bridge-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Create and accept exactly one connection on `name` inside a private
    /// current-thread runtime, then drive `serve` on the connected server.
    fn spawn_server<F>(name: String, serve: F) -> thread::JoinHandle<()>
    where
        F: FnOnce(NamedPipeServer) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + 'static,
    {
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let server = ServerOptions::new()
                    .pipe_mode(PipeMode::Byte)
                    .create(&name)
                    .unwrap();
                server.connect().await.unwrap();
                serve(server).await;
            });
        })
    }

    #[test]
    fn forwards_bytes_and_exits_when_daemon_closes() {
        let name = pipe_name();
        let server = spawn_server(name.clone(), |mut server| {
            Box::pin(async move {
                let mut input = [0_u8; 4];
                server.read_exact(&mut input).await.unwrap();
                assert_eq!(&input, b"ping");
                server.write_all(b"pong").await.unwrap();
                server.flush().await.unwrap();
                std::thread::sleep(Duration::from_millis(500));
            })
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        std::io::Write::write_all(&mut stdin, b"ping").unwrap();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(stdin);
        });
        let output = child.wait_with_output().unwrap();
        server.join().unwrap();
        assert!(
            output.status.success(),
            "{:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"pong");
    }

    #[test]
    fn connection_failure_is_nonzero_and_explains_socket() {
        let name = pipe_name();
        let output = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("failed to connect to daemon MCP socket"),
            "{stderr}"
        );
    }

    #[test]
    fn stdin_eof_exits_while_daemon_keeps_session_open() {
        let name = pipe_name();
        let server = spawn_server(name.clone(), |_server| {
            Box::pin(async move {
                std::thread::sleep(Duration::from_millis(500));
            })
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                panic!("bridge did not exit after stdin EOF");
            }
            thread::sleep(Duration::from_millis(10));
        };
        server.join().unwrap();
        assert!(status.success());
    }

    #[test]
    fn daemon_eof_is_transport_failure_with_nonzero_exit() {
        let name = pipe_name();
        let server = spawn_server(name.clone(), |_server| {
            // Return immediately: the server closes the pipe while the client
            // still holds stdin open.
            Box::pin(async move {})
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &name)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(stdin);
        });
        let output = child.wait_with_output().unwrap();
        server.join().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("daemon MCP stream closed unexpectedly (EOF)"),
            "{stderr}"
        );
    }

    /// The daemon serves MCP at `<rpc pipe>.mcp`; the bridge must derive that
    /// sibling endpoint from the default product RPC pipe name rather than
    /// connecting to the RPC pipe itself. `EXTERNAL_SUBAGENT_SOCKET` is the
    /// only env var set, so a missing derivation connects to the (absent) RPC
    /// pipe and fails.
    #[test]
    fn default_socket_name_derives_the_mcp_endpoint() {
        let rpc_pipe = r"\\.\pipe\external-subagent-daemon".to_string();
        let mcp_pipe = format!("{rpc_pipe}.mcp");
        let server = spawn_server(mcp_pipe.clone(), |mut server| {
            Box::pin(async move {
                let mut input = [0_u8; 4];
                server.read_exact(&mut input).await.unwrap();
                assert_eq!(&input, b"ping");
                server.write_all(b"pong").await.unwrap();
                server.flush().await.unwrap();
                std::thread::sleep(Duration::from_millis(500));
            })
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_external-subagent-mcp"))
            .env("EXTERNAL_SUBAGENT_SOCKET", &rpc_pipe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        std::io::Write::write_all(&mut stdin, b"ping").unwrap();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(stdin);
        });
        let output = child.wait_with_output().unwrap();
        server.join().unwrap();
        assert!(
            output.status.success(),
            "bridge must derive the .mcp endpoint; stderr: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"pong");
    }
}
