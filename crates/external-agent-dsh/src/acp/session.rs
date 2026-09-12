//! Typed DSH ACP session sequence over the shared runtime driver.
//!
//! One child process hosts one session (one external task per process). The
//! bootstrap order is fixed: `initialize` → `session/new` → optional
//! `session/set_config_option` (model) → `session/prompt`. A model selection
//! is only applied after its verified response, and a prompt is never sent
//! when the model selection failed (X05). `session/prompt` is started, not
//! awaited: settlement arrives when the agent turn ends, and the caller owns
//! the settlement watcher so control-plane operations stay responsive.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use external_runtime::{Driver, FrameCodec, PendingRequest, RequestError};

use super::model::{self, ModelSetError};
use super::transport::{self, ShapeError};

pub struct AcpSession {
    driver: Arc<Driver>,
    session_id: Option<String>,
    config_options: Option<serde_json::Value>,
    /// Set when a model selection was refused; a prompt must never follow.
    model_refused: bool,
}

impl AcpSession {
    pub fn new(driver: Arc<Driver>) -> Self {
        Self {
            driver,
            session_id: None,
            config_options: None,
            model_refused: false,
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

    /// Apply a model token. The token must be a bounded opaque catalog token;
    /// when the session advertised config options but no `model` option the
    /// selection fails closed here. When options were not advertised at all
    /// (some observed dialects omit them), the token is forwarded and the
    /// server's verdict decides — the response must succeed before any prompt
    /// is sent (X05).
    pub fn set_model(&mut self, token: &str, timeout: Duration) -> Result<(), SessionError> {
        model::validate_catalog_token(token).map_err(|error| {
            self.model_refused = true;
            SessionError::Model(error)
        })?;
        if let Some(options) = self.config_options.as_ref() {
            if !model::model_option_offered(Some(options)) {
                self.model_refused = true;
                return Err(SessionError::Model(ModelSetError::NotOffered));
            }
        }
        let response = self.driver.request(
            transport::SESSION_SET_CONFIG_OPTION,
            transport::set_config_option_params(self.session_id.as_deref().unwrap_or(""), model::MODEL_CONFIG_ID, token),
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

    /// Begin one prompt turn. The pending settlement is returned for the
    /// caller's watcher; nothing here waits for the agent turn to finish.
    /// A session whose model selection was refused never sends a prompt.
    pub fn prompt(&mut self, prompt: &str) -> Result<(u64, PendingRequest), SessionError> {
        if self.model_refused {
            return Err(SessionError::Shape(ShapeError(
                "model selection was refused; no prompt may be sent".into(),
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
    Transport(String),
    Timeout,
    Remote(serde_json::Value),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shape(error) => write!(f, "{error}"),
            Self::Model(error) => write!(f, "{error}"),
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
            .set_model("fixture-model", Duration::from_secs(2))
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
        assert_eq!(frames[2]["params"]["value"], "fixture-model");
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
            .set_model("nope", Duration::from_secs(2))
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
    fn codec_check_rejects_strict_drivers() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let driver = Driver::spawn(command).unwrap();
        assert!(AcpSession::codec_check(&driver).is_err());
        driver.stop().unwrap();
    }
}
