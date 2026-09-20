//! Direct-drive harness for the pinned ZCode protocol fixture. No test in
//! the repository referenced this binary before, so the fixture's strict
//! session/create acceptance and its controlled effective-thought echo are
//! exercised here against the real executable, one child per case.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

struct Fixture {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl Fixture {
    fn spawn(echo: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_external-fixture-zcode"));
        command
            .arg("session")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(echo) = echo {
            command.env("ZCODE_FAKE_EFFORT_ECHO", echo);
        }
        let mut child = command.spawn().expect("fixture binary must spawn");
        let reader = BufReader::new(child.stdout.take().expect("piped stdout"));
        Self { child, reader }
    }

    fn send(&mut self, request: &Value) {
        let stdin = self.child.stdin.as_mut().expect("piped stdin");
        stdin
            .write_all(format!("{}\n", request).as_bytes())
            .expect("write request");
        stdin.flush().expect("flush request");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .expect("fixture stays responsive");
        serde_json::from_str(line.trim()).expect("fixture emits one JSON value per line")
    }

    /// Run the full session/create handshake: send the create request,
    /// answer the runtime-preferences reverse request, and return the create
    /// response.
    fn create(&mut self, params: Value) -> Value {
        self.send(&json!({"id": 1, "method": "session/create", "params": params}));
        let preferences = self.read_line();
        assert_eq!(
            preferences["method"], "session/requestRuntimePreferences",
            "unexpected reverse request: {preferences}"
        );
        self.send(&json!({
            "id": preferences["id"],
            "result": {
                "nativeSearchEnhancementsEnabled": false,
                "memoryEnabled": false,
                "askUserQuestionAutoResolutionEnabled": false
            }
        }));
        self.read_line()
    }

    /// Send a create request that the strict fixture must reject and return
    /// the rejection frame (rejected requests never start the
    /// runtime-preferences handshake).
    fn rejected_create(&mut self, params: Value) -> Value {
        self.send(&json!({"id": 1, "method": "session/create", "params": params}));
        self.read_line()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn workspace() -> Value {
    json!({
        "workspaceKey": "/workspace",
        "workspacePath": "/workspace"
    })
}

#[test]
fn session_create_accepts_mode_and_thought_level_and_rejects_invented_keys() {
    let mut fixture = Fixture::spawn(None);
    let accepted = fixture.create(json!({
        "workspace": workspace(),
        "mode": "build",
        "thoughtLevel": "high"
    }));
    assert_eq!(accepted["id"], json!(1));
    assert_eq!(
        accepted["result"]["session"]["sessionId"],
        "fake-session-7f3a"
    );

    let mut fixture = Fixture::spawn(None);
    let rejected = fixture.rejected_create(json!({
        "workspace": workspace(),
        "mode": "build",
        "thoughtLevel": "high",
        "reasoningEffort": "high"
    }));
    assert_eq!(
        rejected["error"]["code"],
        json!(-32602),
        "strict fixture must reject an unobserved key: {rejected}"
    );

    let mut fixture = Fixture::spawn(None);
    let legacy = fixture.create(json!({"workspace": workspace()}));
    assert!(legacy.get("result").is_some(), "pre-effort frame: {legacy}");
}

#[test]
fn controlled_thought_echo_covers_missing_equal_and_diverging_states() {
    // Unset echo: the settings section carries no thoughtLevel at all — the
    // observed UNKNOWN state of an unsupported or unprojected level.
    let mut fixture = Fixture::spawn(None);
    let missing = fixture.create(json!({
        "workspace": workspace(),
        "thoughtLevel": "high"
    }));
    assert!(
        missing["result"]["settings"].get("thoughtLevel").is_none(),
        "unset echo must keep the section absent: {missing}"
    );

    for (echo, expected) in [("high", "high"), ("low", "low")] {
        let mut fixture = Fixture::spawn(Some(echo));
        let echoed = fixture.create(json!({
            "workspace": workspace(),
            "thoughtLevel": "high"
        }));
        assert_eq!(
            echoed["result"]["settings"]["thoughtLevel"]["current"], expected,
            "echo must pin the projected current level"
        );
        assert_eq!(
            echoed["result"]["settings"]["thoughtLevel"]["enabled"],
            true
        );
    }
}
