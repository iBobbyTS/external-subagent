//! Typed DSH ACP session sequence over the shared runtime driver.
//!
//! One child process hosts one session (one external task per process). The
//! bootstrap order is fixed: `initialize` → `session/new` → optional
//! `session/set_config_option` (model, then reasoning effort) →
//! `session/prompt`. A model or reasoning-effort selection is only applied
//! after its verified response, and a prompt is never sent when either
//! selection failed (X05). `session/prompt` is started, not awaited:
//! settlement arrives when the agent turn ends, and the caller owns the
//! settlement watcher so control-plane operations stay responsive.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use external_runtime::{Driver, FrameCodec, PendingRequest, RequestError};

use super::model::{self, ModelSetError, ReasoningEffortSetError};
use super::transport::{self, ShapeError};

pub struct AcpSession {
    driver: Arc<Driver>,
    session_id: Option<String>,
    config_options: Option<serde_json::Value>,
    /// Set when a model selection was refused; a prompt must never follow.
    model_refused: bool,
    /// Set when a reasoning-effort selection was refused; a prompt must
    /// never follow.
    effort_refused: bool,
}

impl AcpSession {
    pub fn new(driver: Arc<Driver>) -> Self {
        Self {
            driver,
            session_id: None,
            config_options: None,
            model_refused: false,
            effort_refused: false,
        }
    }

    /// The driver must be spawned in JSON-RPC 2.0 codec mode.
    pub fn codec_check(driver: &Driver) -> Result<(), ShapeError> {
        if driver.codec() == FrameCodec::JsonRpc2 {
            Ok(())
        } else {
            Err(ShapeError(
                "dsh acp session requires the JSON-RPC 2.0 frame codec".into(),
            ))
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn config_options(&self) -> Option<&serde_json::Value> {
        self.config_options.as_ref()
    }

    pub fn driver(&self) -> &Arc<Driver> {
        &self.driver
    }

    pub fn initialize(
        &self,
        timeout: Duration,
    ) -> Result<transport::AcpCapabilities, SessionError> {
        let response = self
            .driver
            .request(
                transport::INITIALIZE,
                transport::initialize_params(),
                timeout,
            )
            .map_err(SessionError::from)?;
        let result = response.result.ok_or_else(|| {
            SessionError::Shape(ShapeError("initialize returned no result".into()))
        })?;
        let parsed = transport::parse_initialize_result(&result)?;
        Ok(parsed.capabilities)
    }

    pub fn new_session(&mut self, cwd: &Path, timeout: Duration) -> Result<(), SessionError> {
        let response = self
            .driver
            .request(
                transport::SESSION_NEW,
                transport::session_new_params(cwd),
                timeout,
            )
            .map_err(SessionError::from)?;
        let result = response.result.ok_or_else(|| {
            SessionError::Shape(ShapeError("session/new returned no result".into()))
        })?;
        let parsed = transport::parse_session_new_result(&result)?;
        self.config_options = parsed.config_options;
        self.session_id = Some(parsed.session_id);
        Ok(())
    }

    /// Apply a model selection. The token is `{provider}:{model}` (split at
    /// the first colon); it is re-serialized as the byte-exact ACP wire value
    /// (`["provider","model"]`) before `session/set_config_option`. A token
    /// that does not parse is refused here; when the session advertised config
    /// options but no `model` option the selection fails closed. When options
    /// were not advertised at all (some observed dialects omit them), the wire
    /// value is forwarded and the server's verdict decides — the response must
    /// succeed before any prompt is sent (X05).
    pub fn set_model(&mut self, token: &str, timeout: Duration) -> Result<(), SessionError> {
        let (provider, model) = model::parse_colon_token(token).map_err(|error| {
            self.model_refused = true;
            SessionError::Model(error)
        })?;
        if let Some(options) = self.config_options.as_ref() {
            if !model::model_option_offered(Some(options)) {
                self.model_refused = true;
                return Err(SessionError::Model(ModelSetError::NotOffered));
            }
        }
        let wire = model::wire_token(provider, model);
        let response = self.driver.request(
            transport::SESSION_SET_CONFIG_OPTION,
            transport::set_config_option_params(
                self.session_id.as_deref().unwrap_or(""),
                model::MODEL_CONFIG_ID,
                &wire,
            ),
            timeout,
        );
        match response {
            Ok(response) => match &response.result {
                Some(_) => Ok(()),
                None => {
                    self.model_refused = true;
                    Err(SessionError::from(RequestError::Remote(
                        serde_json::Value::Null,
                    )))
                }
            },
            Err(error) => {
                self.model_refused = true;
                Err(SessionError::from(error))
            }
        }
    }

    /// Apply a reasoning-effort token through the same X05 contract as
    /// [`set_model`](Self::set_model): the token must be a bounded opaque
    /// token; when the session advertised config options but no
    /// `reasoning_effort` option the selection fails closed here; when
    /// options were not advertised at all the token is forwarded and the
    /// server's verdict decides — the response must succeed before any
    /// prompt is sent. Every failure sets `effort_refused`, which blocks
    /// `prompt` exactly like a refused model selection.
    pub fn set_reasoning_effort(
        &mut self,
        token: &str,
        timeout: Duration,
    ) -> Result<(), SessionError> {
        model::validate_reasoning_effort_token(token).map_err(|error| {
            self.effort_refused = true;
            SessionError::ReasoningEffort(error)
        })?;
        if let Some(options) = self.config_options.as_ref() {
            if !model::reasoning_effort_option_offered(Some(options)) {
                self.effort_refused = true;
                return Err(SessionError::ReasoningEffort(
                    ReasoningEffortSetError::NotOffered,
                ));
            }
        }
        let response = self.driver.request(
            transport::SESSION_SET_CONFIG_OPTION,
            transport::set_config_option_params(
                self.session_id.as_deref().unwrap_or(""),
                model::REASONING_EFFORT_CONFIG_ID,
                token,
            ),
            timeout,
        );
        match response {
            Ok(response) => match &response.result {
                Some(_) => Ok(()),
                None => {
                    self.effort_refused = true;
                    Err(SessionError::from(RequestError::Remote(
                        serde_json::Value::Null,
                    )))
                }
            },
            Err(error) => {
                self.effort_refused = true;
                Err(SessionError::from(error))
            }
        }
    }

    /// Begin one prompt turn. The pending settlement is returned for the
    /// caller's watcher; nothing here waits for the agent turn to finish.
    /// A session whose model or reasoning-effort selection was refused never
    /// sends a prompt.
    pub fn prompt(&mut self, prompt: &str) -> Result<(u64, PendingRequest), SessionError> {
        if self.model_refused {
            return Err(SessionError::Shape(ShapeError(
                "model selection was refused; no prompt may be sent".into(),
            )));
        }
        if self.effort_refused {
            return Err(SessionError::Shape(ShapeError(
                "reasoning effort selection was refused; no prompt may be sent".into(),
            )));
        }
        let wire_id = self.driver.reserve_id();
        let numeric = match &wire_id {
            external_contract::WireId::Integer(value) => *value,
            _ => {
                return Err(SessionError::Shape(ShapeError(
                    "request id reservation failed".into(),
                )))
            }
        };
        let pending = self
            .driver
            .begin_request_with_id(
                wire_id,
                transport::SESSION_PROMPT,
                transport::prompt_params(self.session_id.as_deref().unwrap_or(""), prompt),
            )
            .map_err(SessionError::from)?;
        Ok((
            u64::try_from(numeric)
                .map_err(|_| SessionError::Shape(ShapeError("request id is negative".into())))?,
            pending,
        ))
    }

    /// `session/cancel` is a notification in the upstream method set.
    pub fn cancel(&self) -> Result<(), SessionError> {
        let session_id = self
            .session_id
            .as_deref()
            .ok_or_else(|| SessionError::Shape(ShapeError("session is not open".into())))?;
        self.driver
            .send(&external_contract::EventEnvelope {
                method: transport::SESSION_CANCEL.into(),
                params: transport::cancel_params(session_id),
            })
            .map_err(|error| SessionError::Transport(error.to_string()))
    }

    pub fn close(&self, timeout: Duration) -> Result<(), SessionError> {
        let session_id = self
            .session_id
            .as_deref()
            .ok_or_else(|| SessionError::Shape(ShapeError("session is not open".into())))?;
        self.driver
            .request(
                transport::SESSION_CLOSE,
                transport::close_params(session_id),
                timeout,
            )
            .map(|_| ())
            .map_err(SessionError::from)
    }
}

#[derive(Debug)]
pub enum SessionError {
    Shape(ShapeError),
    Model(ModelSetError),
    ReasoningEffort(ReasoningEffortSetError),
    Transport(String),
    Timeout,
    Remote(serde_json::Value),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shape(error) => write!(f, "{error}"),
            Self::Model(error) => write!(f, "{error}"),
            Self::ReasoningEffort(error) => write!(f, "{error}"),
            Self::Transport(message) => write!(f, "dsh acp transport failed: {message}"),
            Self::Timeout => write!(f, "dsh acp request deadline elapsed"),
            Self::Remote(value) => write!(f, "dsh acp request was rejected: {value}"),
        }
    }
}
impl std::error::Error for SessionError {}

impl From<RequestError> for SessionError {
    fn from(error: RequestError) -> Self {
        match error {
            RequestError::Timeout => Self::Timeout,
            RequestError::Remote(value) => Self::Remote(value),
            other => Self::Transport(other.to_string()),
        }
    }
}
impl From<ShapeError> for SessionError {
    fn from(error: ShapeError) -> Self {
        Self::Shape(error)
    }
}
impl From<ModelSetError> for SessionError {
    fn from(error: ModelSetError) -> Self {
        Self::Model(error)
    }
}
impl From<ReasoningEffortSetError> for SessionError {
    fn from(error: ReasoningEffortSetError) -> Self {
        Self::ReasoningEffort(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A scripted ACP child that records every inbound frame to a file and
    /// replies in the pinned order. Reading the file after the run proves the
    /// emitted wire order.
    fn scripted_child(script: &str) -> (Driver, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "dsh-session-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        let mut command = Command::new("sh");
        command.env("LOG_PATH", &path).args(["-c", script]);
        let driver = Driver::spawn_with_codec(command, FrameCodec::JsonRpc2).unwrap();
        (driver, path)
    }

    fn wait_for_logged_frames(path: &std::path::Path, expected: usize) -> Vec<serde_json::Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(contents) = std::fs::read_to_string(path) {
                let frames: Vec<serde_json::Value> = contents
                    .lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect();
                if frames.len() >= expected {
                    return frames;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture log never reached {expected} frames"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn bootstrap_orders_initialize_new_model_and_prompt() {
        let script = r#"
log() { printf '%s\n' "$1" >> "$LOG_PATH"; }
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"capabilities":{"models":true,"cancel":true,"permission":true}}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1","configOptions":[{"configId":"model"}]}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"configOptions":[]}}'
IFS= read -r line; log "$line"
sleep 5
"#;
        let (driver, path) = scripted_child(script);
        let mut session = AcpSession::new(Arc::new(driver));
        let capabilities = session.initialize(Duration::from_secs(2)).unwrap();
        transport::require_build_capabilities(&capabilities).unwrap();
        session
            .new_session(Path::new("/tmp"), Duration::from_secs(2))
            .unwrap();
        assert_eq!(session.session_id(), Some("sess-1"));
        session
            .set_model("fixture-provider:fixture-model", Duration::from_secs(2))
            .unwrap();
        let (_prompt_id, pending) = session.prompt("build it").unwrap();
        pending.cancel();
        let frames = wait_for_logged_frames(&path, 4);
        let methods: Vec<&str> = frames
            .iter()
            .map(|frame| frame["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            vec![
                "initialize",
                "session/new",
                "session/set_config_option",
                "session/prompt"
            ]
        );
        assert_eq!(frames[2]["params"]["configId"], "model");
        assert_eq!(
            frames[2]["params"]["value"],
            "[\"fixture-provider\",\"fixture-model\"]"
        );
        assert_eq!(frames[3]["params"]["sessionId"], "sess-1");
        assert_eq!(frames[3]["params"]["prompt"][0]["text"], "build it");
        for frame in &frames {
            assert_eq!(frame["jsonrpc"], "2.0");
        }
        session
            .driver()
            .stop_and_reap(Duration::from_millis(100))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn rejected_model_selection_stops_before_any_prompt() {
        let script = r#"
log() { printf '%s\n' "$1" >> "$LOG_PATH"; }
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"capabilities":{"permission":true,"cancel":true}}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"unknown model option: nope"}}'
sleep 5
"#;
        let (driver, path) = scripted_child(script);
        let mut session = AcpSession::new(Arc::new(driver));
        session.initialize(Duration::from_secs(2)).unwrap();
        session
            .new_session(Path::new("/tmp"), Duration::from_secs(2))
            .unwrap();
        let error = session
            .set_model("fixture-provider:nope", Duration::from_secs(2))
            .unwrap_err();
        assert!(matches!(error, SessionError::Remote(_)), "{error}");
        assert!(session.prompt("never sent").is_err());
        let frames = wait_for_logged_frames(&path, 3);
        let methods: Vec<&str> = frames
            .iter()
            .map(|frame| frame["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            vec!["initialize", "session/new", "session/set_config_option"]
        );
        session
            .driver()
            .stop_and_reap(Duration::from_millis(100))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn invalid_tokens_fail_before_any_wire_traffic() {
        let (driver, path) = scripted_child("sleep 5");
        let mut session = AcpSession::new(Arc::new(driver));
        session.initialize(Duration::from_millis(50)).unwrap_err();
        let error = session
            .set_model("", Duration::from_millis(50))
            .unwrap_err();
        assert!(matches!(
            error,
            SessionError::Model(ModelSetError::TokenInvalid)
        ));
        assert!(!path.exists());
        session
            .driver()
            .stop_and_reap(Duration::from_millis(100))
            .unwrap();
    }

    #[test]
    fn offered_reasoning_effort_is_set_between_model_and_prompt() {
        let script = r#"
log() { printf '%s\n' "$1" >> "$LOG_PATH"; }
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"capabilities":{"models":true,"cancel":true,"permission":true}}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1","configOptions":[{"configId":"model"},{"configId":"reasoning_effort"}]}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"configOptions":[]}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"configOptions":[]}}'
IFS= read -r line; log "$line"
sleep 5
"#;
        let (driver, path) = scripted_child(script);
        let mut session = AcpSession::new(Arc::new(driver));
        session.initialize(Duration::from_secs(2)).unwrap();
        session
            .new_session(Path::new("/tmp"), Duration::from_secs(2))
            .unwrap();
        session
            .set_model("fixture-provider:fixture-model", Duration::from_secs(2))
            .unwrap();
        session
            .set_reasoning_effort("high", Duration::from_secs(2))
            .unwrap();
        let (_prompt_id, pending) = session.prompt("build it").unwrap();
        pending.cancel();
        let frames = wait_for_logged_frames(&path, 5);
        let methods: Vec<&str> = frames
            .iter()
            .map(|frame| frame["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            vec![
                "initialize",
                "session/new",
                "session/set_config_option",
                "session/set_config_option",
                "session/prompt"
            ]
        );
        assert_eq!(frames[2]["params"]["configId"], "model");
        assert_eq!(
            frames[2]["params"]["value"],
            "[\"fixture-provider\",\"fixture-model\"]"
        );
        assert_eq!(frames[3]["params"]["configId"], "reasoning_effort");
        assert_eq!(frames[3]["params"]["value"], "high");
        assert_eq!(frames[3]["params"]["sessionId"], "sess-1");
        assert_eq!(frames[4]["params"]["prompt"][0]["text"], "build it");
        session
            .driver()
            .stop_and_reap(Duration::from_millis(100))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unoffered_reasoning_effort_fails_closed_before_any_prompt() {
        let script = r#"
log() { printf '%s\n' "$1" >> "$LOG_PATH"; }
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"capabilities":{"models":true,"cancel":true,"permission":true}}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1","configOptions":[{"configId":"model"}]}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"configOptions":[]}}'
sleep 5
"#;
        let (driver, path) = scripted_child(script);
        let mut session = AcpSession::new(Arc::new(driver));
        session.initialize(Duration::from_secs(2)).unwrap();
        session
            .new_session(Path::new("/tmp"), Duration::from_secs(2))
            .unwrap();
        session
            .set_model("fixture-provider:fixture-model", Duration::from_secs(2))
            .unwrap();
        let error = session
            .set_reasoning_effort("high", Duration::from_secs(2))
            .unwrap_err();
        assert!(matches!(
            error,
            SessionError::ReasoningEffort(ReasoningEffortSetError::NotOffered)
        ));
        match session.prompt("never sent") {
            Err(refusal) => assert!(
                refusal.to_string().contains("reasoning effort"),
                "{refusal}"
            ),
            Ok(_) => panic!("prompt must be refused after a refused effort selection"),
        };
        let frames = wait_for_logged_frames(&path, 3);
        let methods: Vec<&str> = frames
            .iter()
            .map(|frame| frame["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            vec!["initialize", "session/new", "session/set_config_option"]
        );
        session
            .driver()
            .stop_and_reap(Duration::from_millis(100))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unadvertised_config_options_forward_the_reasoning_effort_token() {
        let script = r#"
log() { printf '%s\n' "$1" >> "$LOG_PATH"; }
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"capabilities":{"models":true,"cancel":true,"permission":true}}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"configOptions":[]}}'
IFS= read -r line; log "$line"
sleep 5
"#;
        let (driver, path) = scripted_child(script);
        let mut session = AcpSession::new(Arc::new(driver));
        session.initialize(Duration::from_secs(2)).unwrap();
        session
            .new_session(Path::new("/tmp"), Duration::from_secs(2))
            .unwrap();
        // No configOptions were advertised, so the token is forwarded and
        // the server's (scripted) success is the verdict.
        session
            .set_reasoning_effort("high", Duration::from_secs(2))
            .unwrap();
        let (_prompt_id, pending) = session.prompt("build it").unwrap();
        pending.cancel();
        let frames = wait_for_logged_frames(&path, 4);
        let methods: Vec<&str> = frames
            .iter()
            .map(|frame| frame["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            vec![
                "initialize",
                "session/new",
                "session/set_config_option",
                "session/prompt"
            ]
        );
        assert_eq!(frames[2]["params"]["configId"], "reasoning_effort");
        assert_eq!(frames[2]["params"]["value"], "high");
        session
            .driver()
            .stop_and_reap(Duration::from_millis(100))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn rejected_reasoning_effort_stops_before_any_prompt() {
        let script = r#"
log() { printf '%s\n' "$1" >> "$LOG_PATH"; }
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"capabilities":{"models":true,"cancel":true,"permission":true}}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1","configOptions":[{"configId":"model"},{"configId":"reasoning_effort"}]}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"configOptions":[]}}'
IFS= read -r line; log "$line"; printf '%s\n' '{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"unknown reasoning_effort option: high"}}'
sleep 5
"#;
        let (driver, path) = scripted_child(script);
        let mut session = AcpSession::new(Arc::new(driver));
        session.initialize(Duration::from_secs(2)).unwrap();
        session
            .new_session(Path::new("/tmp"), Duration::from_secs(2))
            .unwrap();
        session
            .set_model("fixture-provider:fixture-model", Duration::from_secs(2))
            .unwrap();
        let error = session
            .set_reasoning_effort("high", Duration::from_secs(2))
            .unwrap_err();
        assert!(matches!(error, SessionError::Remote(_)), "{error}");
        match session.prompt("never sent") {
            Err(refusal) => assert!(
                refusal.to_string().contains("reasoning effort"),
                "{refusal}"
            ),
            Ok(_) => panic!("prompt must be refused after a refused effort selection"),
        };
        let frames = wait_for_logged_frames(&path, 4);
        let methods: Vec<&str> = frames
            .iter()
            .map(|frame| frame["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            vec![
                "initialize",
                "session/new",
                "session/set_config_option",
                "session/set_config_option"
            ]
        );
        session
            .driver()
            .stop_and_reap(Duration::from_millis(100))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn invalid_effort_tokens_fail_before_any_wire_traffic() {
        let (driver, path) = scripted_child("sleep 5");
        let mut session = AcpSession::new(Arc::new(driver));
        session.initialize(Duration::from_millis(50)).unwrap_err();
        let error = session
            .set_reasoning_effort("", Duration::from_millis(50))
            .unwrap_err();
        assert!(matches!(
            error,
            SessionError::ReasoningEffort(ReasoningEffortSetError::TokenInvalid)
        ));
        assert!(session.prompt("never sent").is_err());
        assert!(!path.exists());
        session
            .driver()
            .stop_and_reap(Duration::from_millis(100))
            .unwrap();
    }

    #[test]
    fn codec_check_rejects_strict_drivers() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let driver = Driver::spawn(command).unwrap();
        assert!(AcpSession::codec_check(&driver).is_err());
        driver.stop().unwrap();
    }
}
