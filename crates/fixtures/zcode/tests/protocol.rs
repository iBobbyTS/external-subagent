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
        match echo {
            Some(echo) => Self::spawn_with_env(&[("ZCODE_FAKE_EFFORT_ECHO", echo)]),
            None => Self::spawn_with_env(&[]),
        }
    }

    fn spawn_with_env(envs: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_external-fixture-zcode"));
        command
            .arg("session")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (name, value) in envs {
            command.env(name, value);
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

    /// Send one `session/setModel` but do not read: a successful switch
    /// emits a `state.updated{reason:"model_changed"}` notification before
    /// the response, while a rejection emits only the error response.
    fn send_set_model(&mut self, model: Value, persist: bool) {
        self.send(&json!({
            "id": 9,
            "method": "session/setModel",
            "params": {
                "sessionId": "fake-session-7f3a",
                "model": model,
                "persistAsWorkspaceLastUsed": persist
            }
        }));
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

#[test]
fn create_advertises_the_configurable_catalog_and_set_model_echoes_it() {
    let mut fixture = Fixture::spawn(None);
    let created = fixture.create(json!({"workspace": workspace()}));
    let available = &created["result"]["settings"]["model"]["available"];
    assert_eq!(available[0]["ref"]["providerId"], "zai");
    assert_eq!(available[0]["ref"]["modelId"], "GLM-5.3");
    assert_eq!(available[0]["reasoning"]["defaultLevel"], "max");
    assert_eq!(available[1]["ref"]["providerId"], "deepseek");

    fixture.send_set_model(
        json!({"providerId": "zai", "modelId": "GLM-5.3", "options": {"reasoningLevel": "high"}}),
        false,
    );
    let notification = fixture.read_line();
    let response = fixture.read_line();
    assert_eq!(
        notification["method"], "state.updated",
        "the model_changed notification must precede the response: {notification}"
    );
    assert_eq!(notification["params"]["reason"], "model_changed");
    assert_eq!(
        response["result"]["settings"]["model"]["current"]["providerId"],
        "zai"
    );
    assert_eq!(
        response["result"]["settings"]["model"]["current"]["modelId"],
        "GLM-5.3"
    );
    assert_eq!(
        response["result"]["settings"]["thoughtLevel"]["current"],
        "high"
    );
}

#[test]
fn set_model_remote_error_carries_the_discriminating_data_code() {
    let mut fixture =
        Fixture::spawn_with_env(&[("ZCODE_FAKE_SETMODEL_ERROR_CODE", "model_not_found")]);
    fixture.create(json!({"workspace": workspace()}));
    fixture.send_set_model(json!({"providerId": "zai", "modelId": "GLM-5.1"}), false);
    let response = fixture.read_line();
    assert_eq!(response["error"]["code"], json!(-32603));
    assert_eq!(response["error"]["data"]["code"], "model_not_found");
}

#[test]
fn set_model_echo_can_be_configured_to_diverge_for_readback_tests() {
    let mut fixture = Fixture::spawn_with_env(&[(
        "ZCODE_FAKE_SETMODEL_CURRENT",
        r#"{"providerId":"zai","modelId":"GLM-5.3-Flash"}"#,
    )]);
    fixture.create(json!({"workspace": workspace()}));
    fixture.send_set_model(
        json!({"providerId": "zai", "modelId": "GLM-5.3", "options": {"reasoningLevel": "high"}}),
        false,
    );
    let _notification = fixture.read_line();
    let response = fixture.read_line();
    assert_eq!(
        response["result"]["settings"]["model"]["current"]["modelId"], "GLM-5.3-Flash",
        "fixture must be able to misreport the read-back"
    );
    assert_eq!(
        response["result"]["settings"]["thoughtLevel"]["current"],
        "high"
    );
}

#[test]
fn configured_catalog_can_omit_a_default_reasoning_level() {
    let mut fixture = Fixture::spawn_with_env(&[(
        "ZCODE_FAKE_MODEL_CATALOG",
        r#"[{"ref":{"providerId":"zai","modelId":"GLM-5.3"},"reasoning":{"levels":[{"value":"low","label":"low"}]}}]"#,
    )]);
    let created = fixture.create(json!({"workspace": workspace()}));
    let available = &created["result"]["settings"]["model"]["available"];
    assert!(available[0]["reasoning"].get("defaultLevel").is_none());
    fixture.send_set_model(json!({"providerId": "zai", "modelId": "GLM-5.3"}), false);
    let _notification = fixture.read_line();
    let response = fixture.read_line();
    assert!(
        response["result"]["settings"].get("thoughtLevel").is_none(),
        "no requested level and no default must omit thoughtLevel: {response}"
    );
}
