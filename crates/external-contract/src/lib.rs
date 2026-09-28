use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const WORKSPACE_READ_STATE: &str = "workspace/readState";
pub const SESSION_CREATE: &str = "session/create";
pub const SESSION_RESUME: &str = "session/resume";
pub const SESSION_SUBSCRIBE: &str = "session/subscribe";
pub const SESSION_SET_MODEL: &str = "session/setModel";
pub const SESSION_SEND: &str = "session/send";
pub const SESSION_STOP: &str = "session/stop";
pub const SESSION_CLOSE: &str = "session/close";
pub const SESSION_EVENT: &str = "session/event";
pub const SESSION_REQUEST_RUNTIME_PREFERENCES: &str = "session/requestRuntimePreferences";
pub const INTERACTION_REQUEST_PERMISSION: &str = "interaction/requestPermission";
pub const INTERACTION_REQUEST_USER_INPUT: &str = "interaction/requestUserInput";
/// Daemon-internal sentinel for a runtime server request no adapter can
/// interpret. It stays observable as a pending record but is never a real
/// producer of any response contract, so it must never become respondable.
pub const INTERACTION_REQUEST_UNSUPPORTED_INPUT: &str = "interaction/unsupportedInput";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WireId {
    Integer(i64),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub id: WireId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    pub id: WireId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireMessage {
    Request(RequestEnvelope),
    Response(ResponseEnvelope),
    Event(EventEnvelope),
    UnknownEvent { method: String, raw: Value },
}

impl RequestEnvelope {
    pub fn new(id: WireId, method: impl Into<String>, params: Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }
}

impl ResponseEnvelope {
    pub fn success(id: WireId, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: WireId, error: Value) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    Missing(&'static str),
    Invalid(&'static str),
    UnobservedAlternate(&'static str),
    ModelConflict,
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(path) => write!(formatter, "missing authoritative field {path}"),
            Self::Invalid(path) => write!(formatter, "invalid pinned field {path}"),
            Self::UnobservedAlternate(path) => {
                write!(formatter, "unobserved alternate field {path} is present")
            }
            Self::ModelConflict => write!(formatter, "requested and observed model conflict"),
        }
    }
}

impl std::error::Error for ProjectionError {}

/// One entry of the session/create `settings.model.available` catalog.
///
/// Every field is optional on the wire; a present field with the wrong type
/// or an unbounded value fails the whole projection closed. An entry without
/// a `ref` still projects, but cannot be matched by a provider-qualified
/// request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModelEntry {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub reasoning_levels: Vec<String>,
    pub default_level: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCreateProjection {
    pub session_id: String,
    pub requested_model: Option<String>,
    pub configured_thought_level: Option<String>,
    /// Full model catalog advertised by session/create. This is the only
    /// durable whitelist for a model switch: `settings.model.available`
    /// shrinks to the selected model after `session/setModel`.
    pub available_models: Vec<CatalogModelEntry>,
}

impl SessionCreateProjection {
    pub fn from_result(result: &Value) -> Result<Self, ProjectionError> {
        let root = result
            .as_object()
            .ok_or(ProjectionError::Invalid("result"))?;
        reject_alternate(root, "sessionId", "result.sessionId")?;
        reject_alternate(root, "session_id", "result.session_id")?;
        reject_alternate(root, "modelId", "result.modelId")?;
        reject_alternate(root, "model", "result.model")?;

        let session = root
            .get("session")
            .ok_or(ProjectionError::Missing("result.session.sessionId"))?
            .as_object()
            .ok_or(ProjectionError::Invalid("result.session"))?;
        reject_alternate(session, "id", "result.session.id")?;
        reject_alternate(session, "session_id", "result.session.session_id")?;
        reject_alternate(session, "modelId", "result.session.modelId")?;
        reject_alternate(session, "settings", "result.session.settings")?;

        let session_id =
            required_bounded_string(session.get("sessionId"), "result.session.sessionId", 512)?
                .to_owned();
        // result.projection.sessionId is an independent identifier in pinned
        // 3.8.1. It is intentionally not inspected or used as provenance.

        let requested_model = settings_current_model(root)?;
        let consistency_model = session_consistency_model(session)?;
        match (&requested_model, &consistency_model) {
            (Some(requested), Some(observed))
                if normalized_zai_model(requested) != normalized_zai_model(observed) =>
            {
                return Err(ProjectionError::ModelConflict);
            }
            (None, Some(_)) => {
                return Err(ProjectionError::Missing(
                    "result.settings.model.current.modelId",
                ));
            }
            _ => {}
        }

        Ok(Self {
            session_id,
            requested_model,
            configured_thought_level: settings_thought_level(root)?,
            available_models: settings_available_models(root)?,
        })
    }
}

/// The model echoed by a successful `session/setModel`. `provider_id` and
/// `model_id` are read from `settings.model.current`; `thought_level` from
/// `settings.thoughtLevel.current` under the same strict alternate
/// rejections as the create projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetModelProjection {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub configured_thought_level: Option<String>,
}

pub fn set_model_projection_from_result(
    result: &Value,
) -> Result<SetModelProjection, ProjectionError> {
    let root = result
        .as_object()
        .ok_or(ProjectionError::Invalid("result"))?;
    let (provider_id, model_id) = settings_current_reference(root)?;
    Ok(SetModelProjection {
        provider_id,
        model_id,
        configured_thought_level: settings_thought_level(root)?,
    })
}

/// Parse the session/create `settings.model.available[]` catalog. This is a
/// distinct entry point from `WorkspaceDiagnosticProjection`'s `modelCatalog`
/// path and deliberately does not share its shape or helper.
fn settings_available_models(
    root: &serde_json::Map<String, Value>,
) -> Result<Vec<CatalogModelEntry>, ProjectionError> {
    let Some(settings) = optional_object(root, "settings", "result.settings")? else {
        return Ok(Vec::new());
    };
    let Some(model) = optional_object(settings, "model", "result.settings.model")? else {
        return Ok(Vec::new());
    };
    let Some(available) = model.get("available") else {
        return Ok(Vec::new());
    };
    let available = available
        .as_array()
        .ok_or(ProjectionError::Invalid("result.settings.model.available"))?;
    let mut entries = Vec::with_capacity(available.len());
    for item in available {
        let item = item.as_object().ok_or(ProjectionError::Invalid(
            "result.settings.model.available[]",
        ))?;
        let (provider_id, model_id) = match item.get("ref") {
            None => (None, None),
            Some(reference) => {
                let reference = reference.as_object().ok_or(ProjectionError::Invalid(
                    "result.settings.model.available[].ref",
                ))?;
                (
                    optional_bounded_string(
                        reference.get("providerId"),
                        "result.settings.model.available[].ref.providerId",
                        128,
                    )?
                    .map(str::to_owned),
                    optional_bounded_string(
                        reference.get("modelId"),
                        "result.settings.model.available[].ref.modelId",
                        128,
                    )?
                    .map(str::to_owned),
                )
            }
        };
        let (reasoning_levels, default_level) = match item.get("reasoning") {
            None => (Vec::new(), None),
            Some(reasoning) => {
                let reasoning = reasoning.as_object().ok_or(ProjectionError::Invalid(
                    "result.settings.model.available[].reasoning",
                ))?;
                let mut levels = Vec::new();
                if let Some(level_list) = reasoning.get("levels") {
                    let level_list = level_list.as_array().ok_or(ProjectionError::Invalid(
                        "result.settings.model.available[].reasoning.levels",
                    ))?;
                    for level in level_list {
                        let level = level.as_object().ok_or(ProjectionError::Invalid(
                            "result.settings.model.available[].reasoning.levels[]",
                        ))?;
                        if let Some(value) = optional_bounded_string(
                            level.get("value"),
                            "result.settings.model.available[].reasoning.levels[].value",
                            128,
                        )? {
                            levels.push(value.to_owned());
                        }
                    }
                }
                let default_level = optional_bounded_string(
                    reasoning.get("defaultLevel"),
                    "result.settings.model.available[].reasoning.defaultLevel",
                    128,
                )?
                .map(str::to_owned);
                (levels, default_level)
            }
        };
        entries.push(CatalogModelEntry {
            provider_id,
            model_id,
            reasoning_levels,
            default_level,
        });
    }
    Ok(entries)
}

/// Read `settings.model.current` as a raw provider/model pair.
fn settings_current_reference(
    root: &serde_json::Map<String, Value>,
) -> Result<(Option<String>, Option<String>), ProjectionError> {
    let Some(settings) = optional_object(root, "settings", "result.settings")? else {
        return Ok((None, None));
    };
    let Some(model) = optional_object(settings, "model", "result.settings.model")? else {
        return Ok((None, None));
    };
    reject_alternate(model, "value", "result.settings.model.value")?;
    let Some(current) = optional_object(model, "current", "result.settings.model.current")? else {
        return Ok((None, None));
    };
    reject_alternate(current, "id", "result.settings.model.current.id")?;
    Ok((
        optional_bounded_string(
            current.get("providerId"),
            "result.settings.model.current.providerId",
            128,
        )?
        .map(str::to_owned),
        optional_bounded_string(
            current.get("modelId"),
            "result.settings.model.current.modelId",
            128,
        )?
        .map(str::to_owned),
    ))
}

/// Effective thought level projected by a session/create or session/resume
/// result: `result.settings.thoughtLevel.current`. The official settings state
/// schema only carries `current` when the effective level is one of the
/// model's supported levels, so absence is a distinct observed state.
pub fn configured_thought_level_from_result(
    result: &Value,
) -> Result<Option<String>, ProjectionError> {
    result
        .as_object()
        .ok_or(ProjectionError::Invalid("result"))
        .and_then(|root| settings_thought_level(root))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDiagnosticProjection {
    pub current_model: Option<String>,
    pub available_models: Vec<String>,
}

impl WorkspaceDiagnosticProjection {
    pub fn from_result(result: &Value) -> Result<Self, ProjectionError> {
        let root = result
            .as_object()
            .ok_or(ProjectionError::Invalid("result"))?;
        let current_model = settings_current_model(root)?;
        let mut available_models = Vec::new();
        if let Some(catalog) = optional_object(root, "modelCatalog", "result.modelCatalog")? {
            if let Some(available) = catalog.get("available") {
                let available = available
                    .as_array()
                    .ok_or(ProjectionError::Invalid("result.modelCatalog.available"))?;
                for item in available {
                    let item = item
                        .as_object()
                        .ok_or(ProjectionError::Invalid("result.modelCatalog.available[]"))?;
                    let reference = item.get("ref").and_then(Value::as_object).ok_or(
                        ProjectionError::Invalid("result.modelCatalog.available[].ref"),
                    )?;
                    available_models.push(
                        required_bounded_string(
                            reference.get("modelId"),
                            "result.modelCatalog.available[].ref.modelId",
                            128,
                        )?
                        .to_owned(),
                    );
                }
            }
        }
        available_models.sort();
        available_models.dedup();
        Ok(Self {
            current_model,
            available_models,
        })
    }
}

/// Normalize a model token into `(provider, modelId)`.
///
/// An explicit `provider/model` reference requires non-empty sides, exactly
/// one `/`, and a provider segment limited to `[A-Za-z0-9._-]+`; the model id
/// comparison is case-insensitive while the provider segment is preserved as
/// written. A bare token keeps its historical `zai/<token>` interpretation
/// for the legacy parse path only.
pub fn normalized_model_reference(value: &str) -> Option<(String, String)> {
    if value.is_empty() || value.len() > 128 || value.contains('\0') {
        return None;
    }
    let (provider, model) = match value.split_once('/') {
        Some((provider, model)) => (provider, model),
        None => ("zai", value),
    };
    if provider.is_empty() || model.is_empty() || model.contains('/') {
        return None;
    }
    if !is_provider_segment(provider) {
        return None;
    }
    Some((provider.to_owned(), model.to_ascii_lowercase()))
}

/// Generalized `zai` normalization retained for the legacy parse path: the
/// returned model id is provider-independent and case-insensitive.
pub fn normalized_zai_model(value: &str) -> Option<String> {
    normalized_model_reference(value).map(|(_, model)| model)
}

/// Provider-qualified normalization for an explicit `provider/model` token.
///
/// Unlike [`normalized_model_reference`], a bare token is rejected: the
/// provider segment is mandatory so a bare legacy token can never be silently
/// compared as `zai/<token>`.
pub fn normalized_scoped_model(value: &str) -> Option<(String, String)> {
    value
        .contains('/')
        .then(|| normalized_model_reference(value))
        .flatten()
}

/// Compare an explicit requested model token against an observed
/// provider/model pair. Both segments participate; the bare compatibility of
/// [`normalized_model_reference`] never applies here.
pub fn provider_model_matches(requested: &str, provider_id: &str, model_id: &str) -> bool {
    let Some((requested_provider, requested_model)) = normalized_scoped_model(requested) else {
        return false;
    };
    if !is_provider_segment(provider_id) {
        return false;
    }
    if model_id.is_empty()
        || model_id.len() > 128
        || model_id.contains('\0')
        || model_id.contains('/')
    {
        return false;
    }
    requested_provider == provider_id && requested_model == model_id.to_ascii_lowercase()
}

fn is_provider_segment(provider: &str) -> bool {
    !provider.is_empty()
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn optional_bounded_string<'a>(
    value: Option<&'a Value>,
    path: &'static str,
    max_len: usize,
) -> Result<Option<&'a str>, ProjectionError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.as_str().ok_or(ProjectionError::Invalid(path))?;
    if value.is_empty() || value.len() > max_len || value.contains('\0') {
        return Err(ProjectionError::Invalid(path));
    }
    Ok(Some(value))
}

fn reject_alternate(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<(), ProjectionError> {
    if object.contains_key(key) {
        Err(ProjectionError::UnobservedAlternate(path))
    } else {
        Ok(())
    }
}

fn optional_object<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<Option<&'a serde_json::Map<String, Value>>, ProjectionError> {
    object
        .get(key)
        .map(|value| value.as_object().ok_or(ProjectionError::Invalid(path)))
        .transpose()
}

fn settings_current_model(
    root: &serde_json::Map<String, Value>,
) -> Result<Option<String>, ProjectionError> {
    let Some(settings) = optional_object(root, "settings", "result.settings")? else {
        return Ok(None);
    };
    let Some(model) = optional_object(settings, "model", "result.settings.model")? else {
        return Ok(None);
    };
    reject_alternate(model, "value", "result.settings.model.value")?;
    let Some(current) = optional_object(model, "current", "result.settings.model.current")? else {
        return Ok(None);
    };
    reject_alternate(current, "id", "result.settings.model.current.id")?;
    let Some(value) = current.get("modelId") else {
        return Ok(None);
    };
    let value = required_bounded_string(Some(value), "result.settings.model.current.modelId", 128)?;
    if normalized_zai_model(value).is_none() {
        return Err(ProjectionError::Invalid(
            "result.settings.model.current.modelId",
        ));
    }
    Ok(Some(value.to_owned()))
}

fn settings_thought_level(
    root: &serde_json::Map<String, Value>,
) -> Result<Option<String>, ProjectionError> {
    let Some(settings) = optional_object(root, "settings", "result.settings")? else {
        return Ok(None);
    };
    // The effective thought level lives under the settings state schema's
    // `thoughtLevel` section only; a sibling `thought` key is an unobserved
    // alternate shape and fails closed like every other projection fallback.
    reject_alternate(settings, "thought", "result.settings.thought")?;
    let Some(thought_level) =
        optional_object(settings, "thoughtLevel", "result.settings.thoughtLevel")?
    else {
        return Ok(None);
    };
    let Some(value) = thought_level.get("current") else {
        return Ok(None);
    };
    let value = required_bounded_string(Some(value), "result.settings.thoughtLevel.current", 128)?;
    Ok(Some(value.to_owned()))
}

fn session_consistency_model(
    session: &serde_json::Map<String, Value>,
) -> Result<Option<String>, ProjectionError> {
    let Some(model) = optional_object(session, "model", "result.session.model")? else {
        return Ok(None);
    };
    let Some(value) = model.get("modelId") else {
        return Ok(None);
    };
    let value = required_bounded_string(Some(value), "result.session.model.modelId", 128)?;
    if normalized_zai_model(value).is_none() {
        return Err(ProjectionError::Invalid("result.session.model.modelId"));
    }
    Ok(Some(value.to_owned()))
}

fn required_bounded_string<'a>(
    value: Option<&'a Value>,
    path: &'static str,
    max_len: usize,
) -> Result<&'a str, ProjectionError> {
    let value = value.ok_or(ProjectionError::Missing(path))?;
    let value = value.as_str().ok_or(ProjectionError::Invalid(path))?;
    if value.is_empty() || value.len() > max_len || value.contains('\0') {
        return Err(ProjectionError::Invalid(path));
    }
    Ok(value)
}

pub fn turn_id_from_result(result: &Value) -> Option<&str> {
    result.get("turnId").and_then(Value::as_str)
}

pub fn event_type(event: &EventEnvelope) -> Option<&str> {
    (event.method == SESSION_EVENT)
        .then(|| event.params.get("type").and_then(Value::as_str))
        .flatten()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRef<'a> {
    pub workspace_key: &'a str,
    pub workspace_path: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceParams<'a> {
    pub workspace: WorkspaceRef<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionParams<'a> {
    pub workspace: WorkspaceRef<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<&'a str>,
    #[serde(skip_serializing_if = "is_empty_mcp_servers")]
    pub mcp_servers: &'a [StdioMcpServer],
}

/// `session/setModel` selects the model (and its reasoning level) for an
/// already-created session. `options` is emitted only when a reasoning level
/// resolved, and `persistAsWorkspaceLastUsed` is always false so switching a
/// task's model never mutates the user's workspace selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModelParams<'a> {
    pub session_id: &'a str,
    pub model: SetModelRef<'a>,
    pub persist_as_workspace_last_used: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModelRef<'a> {
    pub provider_id: &'a str,
    pub model_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<SetModelOptions<'a>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModelOptions<'a> {
    pub reasoning_level: &'a str,
}

impl<'a> SetModelParams<'a> {
    pub fn new(
        session_id: &'a str,
        provider_id: &'a str,
        model_id: &'a str,
        reasoning_level: Option<&'a str>,
    ) -> Self {
        Self {
            session_id,
            model: SetModelRef {
                provider_id,
                model_id,
                options: reasoning_level.map(|reasoning_level| SetModelOptions { reasoning_level }),
            },
            persist_as_workspace_last_used: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionParams<'a> {
    pub session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<&'a str>,
    #[serde(skip_serializing_if = "is_empty_mcp_servers")]
    pub mcp_servers: &'a [StdioMcpServer],
}

fn is_empty_mcp_servers(servers: &&[StdioMcpServer]) -> bool {
    servers.is_empty()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdioMcpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<McpEnvironmentVariable>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEnvironmentVariable {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeParams<'a> {
    pub session_id: &'a str,
    pub delivery_kind: &'static str,
    pub include_snapshot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendParams<'a> {
    pub session_id: &'a str,
    pub content: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionParams<'a> {
    pub session_id: &'a str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePreferences {
    pub native_search_enhancements_enabled: bool,
    pub memory_enabled: bool,
    pub ask_user_question_auto_resolution_enabled: bool,
}

pub fn offered_permission_response(params: &Value, decision: &str) -> Option<Value> {
    let expected_kind = match decision {
        "allow" => "allow_once",
        "deny" => "deny",
        _ => return None,
    };
    let matches = params
        .get("options")?
        .as_array()?
        .iter()
        .filter(|option| option.get("kind").and_then(Value::as_str) == Some(expected_kind))
        .collect::<Vec<_>>();
    let [option] = matches.as_slice() else {
        return None;
    };
    let response = option.get("response")?.as_object()?;
    (response.get("decision").and_then(Value::as_str) == Some(decision))
        .then(|| Value::Object(response.clone()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    InvalidJson(String),
    NotObject,
    MissingKind,
    InvalidEnvelope(String),
    ContradictoryResponse,
}

pub fn parse_line(line: &str) -> Result<WireMessage, ParseError> {
    let mut value: Value =
        serde_json::from_str(line).map_err(|e| ParseError::InvalidJson(e.to_string()))?;
    let obj = value.as_object().ok_or(ParseError::NotObject)?;
    if obj.contains_key("jsonrpc") {
        return Err(ParseError::InvalidEnvelope(
            "jsonrpc is not part of the strict ZCode envelope".into(),
        ));
    }
    if obj.contains_key("id") && (obj.contains_key("result") || obj.contains_key("error")) {
        if obj.contains_key("result") && obj.contains_key("error") {
            return Err(ParseError::ContradictoryResponse);
        }
        if obj
            .get("result")
            .or_else(|| obj.get("error"))
            .is_none_or(Value::is_null)
        {
            return Err(ParseError::InvalidEnvelope(
                "response requires exactly one non-null result or error".into(),
            ));
        }
        return serde_json::from_value(value)
            .map(WireMessage::Response)
            .map_err(|e| ParseError::InvalidEnvelope(e.to_string()));
    }
    if let Some(method) = obj.get("method").and_then(Value::as_str) {
        if obj.contains_key("id") {
            // Official resumed sessions attach persisted trace metadata to
            // reverse requests (including requestRuntimePreferences). It has
            // no command semantics; keep all other envelope fields strict.
            value
                .as_object_mut()
                .expect("object checked above")
                .remove("trace");
            return serde_json::from_value(value)
                .map(WireMessage::Request)
                .map_err(|e| ParseError::InvalidEnvelope(e.to_string()));
        }
        let known = method == SESSION_EVENT
            && obj
                .get("params")
                .and_then(|params| params.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    matches!(kind, "turn.started" | "turn.completed" | "turn.failed")
                });
        return if known {
            serde_json::from_value(value)
                .map(WireMessage::Event)
                .map_err(|e| ParseError::InvalidEnvelope(e.to_string()))
        } else {
            Ok(WireMessage::UnknownEvent {
                method: method.into(),
                raw: value,
            })
        };
    }
    Err(ParseError::MissingKind)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleOrder {
    NotLifecycle,
    InOrder,
    OutOfOrder { expected: &'static str },
}

/// Classify lifecycle events without reordering or dropping the wire stream.
pub fn classify_lifecycle(method: &str, turn_active: bool) -> LifecycleOrder {
    match (method, turn_active) {
        ("turn.started", false) => LifecycleOrder::InOrder,
        ("turn.started", true) => LifecycleOrder::OutOfOrder {
            expected: "turn.completed or turn.failed",
        },
        ("turn.completed" | "turn.failed", true) => LifecycleOrder::InOrder,
        ("turn.completed" | "turn.failed", false) => LifecycleOrder::OutOfOrder {
            expected: "turn.started",
        },
        _ => LifecycleOrder::NotLifecycle,
    }
}

pub fn encode<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip() {
        let req = RequestEnvelope {
            id: WireId::Integer(1),
            method: SESSION_CREATE.into(),
            params: serde_json::json!({"x":1}),
        };
        let parsed = parse_line(&encode(&req).unwrap()).unwrap();
        assert_eq!(parsed, WireMessage::Request(req));
        assert!(!encode(&RequestEnvelope::new(
            WireId::Integer(2),
            SESSION_SEND,
            serde_json::json!({"sessionId":"s1","content":"review"}),
        ))
        .unwrap()
        .contains("jsonrpc"));
    }
    #[test]
    fn resumed_runtime_preferences_accept_trace_without_changing_request_semantics() {
        let params =
            serde_json::json!({"sessionId":"persisted-session", "scope":"runtime-materialization"});
        let request = serde_json::json!({
            "id":"server-1", "method":SESSION_REQUEST_RUNTIME_PREFERENCES,
            "params":params, "trace":{"traceId":"persisted-trace"}
        });
        let parsed = parse_line(&request.to_string()).unwrap();
        assert_eq!(
            parsed,
            WireMessage::Request(RequestEnvelope::new(
                WireId::String("server-1".into()),
                SESSION_REQUEST_RUNTIME_PREFERENCES,
                params
            ))
        );
        // Accept only this observed metadata extension, not arbitrary envelope fields.
        let mut unsupported = request;
        unsupported["unexpected"] = serde_json::json!(true);
        assert!(matches!(
            parse_line(&unsupported.to_string()),
            Err(ParseError::InvalidEnvelope(_))
        ));
    }

    #[test]
    fn unknown_preserved() {
        let msg = parse_line(r#"{"method":"new/event","params":{"a":2}}"#).unwrap();
        assert!(matches!(msg, WireMessage::UnknownEvent { .. }));
    }

    #[test]
    fn observed_permission_resolution_remains_bounded_without_typed_semantics() {
        let msg = parse_line(
            r#"{"method":"session/event","params":{"sessionId":"s1","type":"permission.resolved","payload":{"future":"shape"}}}"#,
        )
        .unwrap();
        assert!(matches!(msg, WireMessage::UnknownEvent { .. }));
    }
    #[test]
    fn malformed_classified() {
        assert!(matches!(parse_line("{"), Err(ParseError::InvalidJson(_))));
    }

    #[test]
    fn contradictory_response_is_rejected() {
        assert_eq!(
            parse_line(r#"{"id":1,"result":{},"error":{}}"#),
            Err(ParseError::ContradictoryResponse)
        );
    }

    #[test]
    fn wire_ids_are_limited_to_integer_or_string() {
        for id in ["true", "null", "{}", "[]", "1.5"] {
            let request = format!(r#"{{"id":{id},"method":"session/stop","params":{{}}}}"#);
            assert!(matches!(
                parse_line(&request),
                Err(ParseError::InvalidEnvelope(_))
            ));
            let response = format!(r#"{{"id":{id},"result":{{}}}}"#);
            assert!(matches!(
                parse_line(&response),
                Err(ParseError::InvalidEnvelope(_))
            ));
        }
        assert!(matches!(
            parse_line(r#"{"id":"server-1","method":"interaction/requestPermission","params":{}}"#),
            Ok(WireMessage::Request(RequestEnvelope {
                id: WireId::String(ref id),
                ..
            })) if id == "server-1"
        ));
    }

    #[test]
    fn response_requires_exactly_one_non_null_outcome() {
        for frame in [
            r#"{"id":1}"#,
            r#"{"id":1,"result":null}"#,
            r#"{"id":1,"error":null}"#,
            r#"{"id":1,"result":null,"error":null}"#,
            r#"{"id":1,"result":{},"error":null}"#,
            r#"{"id":1,"result":null,"error":{}}"#,
            r#"{"id":1,"result":{},"error":{}}"#,
        ] {
            assert!(
                parse_line(frame).is_err(),
                "accepted malformed response: {frame}"
            );
        }
        assert!(matches!(
            parse_line(r#"{"id":1,"result":{}}"#),
            Ok(WireMessage::Response(ResponseEnvelope {
                id: WireId::Integer(1),
                result: Some(_),
                error: None,
            }))
        ));
    }

    #[test]
    fn legacy_jsonrpc_envelope_is_rejected() {
        assert!(matches!(
            parse_line(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            Err(ParseError::InvalidEnvelope(_))
        ));
    }

    #[test]
    fn lifecycle_order_is_explicit() {
        assert_eq!(
            classify_lifecycle("turn.completed", false),
            LifecycleOrder::OutOfOrder {
                expected: "turn.started"
            }
        );
    }

    #[test]
    fn session_projection_requires_the_authoritative_nested_id() {
        let nested = serde_json::json!({"session": {"sessionId": "s1"}, "turnId": "t1"});
        assert_eq!(
            SessionCreateProjection::from_result(&nested).unwrap(),
            SessionCreateProjection {
                session_id: "s1".into(),
                requested_model: None,
                configured_thought_level: None,
                available_models: Vec::new(),
            }
        );
        assert_eq!(turn_id_from_result(&nested), Some("t1"));

        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"session": null}),
            serde_json::json!({"session": {}}),
            serde_json::json!({"session": {"sessionId": null}}),
            serde_json::json!({"session": {"sessionId": ""}}),
            serde_json::json!({"session": {"sessionId": 7}}),
        ] {
            assert!(SessionCreateProjection::from_result(&invalid).is_err());
        }
    }

    #[test]
    fn projection_identifier_is_always_ignored_for_session_provenance() {
        for projection in [
            None,
            Some(serde_json::json!({"sessionId": "different-projection"})),
            Some(serde_json::json!({"sessionId": null})),
            Some(serde_json::json!({"sessionId": 42})),
        ] {
            let mut result = serde_json::json!({"session": {"sessionId": "authoritative"}});
            if let Some(projection) = projection {
                result["projection"] = projection;
            }
            assert_eq!(
                SessionCreateProjection::from_result(&result)
                    .unwrap()
                    .session_id,
                "authoritative"
            );
        }
    }

    #[test]
    fn unobserved_session_fallbacks_fail_closed_even_when_they_coexist() {
        for invalid in [
            serde_json::json!({"sessionId": "fallback", "session": {}}),
            serde_json::json!({
                "sessionId": "authoritative",
                "session": {"sessionId": "authoritative"}
            }),
        ] {
            assert!(SessionCreateProjection::from_result(&invalid).is_err());
        }
    }

    #[test]
    fn model_projection_uses_settings_current_with_optional_session_consistency() {
        let base = serde_json::json!({
            "session": {"sessionId": "session"},
            "settings": {"model": {"current": {"modelId": "GLM-5.3"}}}
        });
        assert_eq!(
            SessionCreateProjection::from_result(&base)
                .unwrap()
                .requested_model
                .as_deref(),
            Some("GLM-5.3")
        );

        let equal = serde_json::json!({
            "session": {
                "sessionId": "session",
                "model": {"modelId": "zai/glm-5.3"}
            },
            "settings": {"model": {"current": {"modelId": "GLM-5.3"}}}
        });
        assert!(SessionCreateProjection::from_result(&equal).is_ok());

        for consistency in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!(7),
            serde_json::json!("glm-5.1"),
        ] {
            let mut invalid = base.clone();
            invalid["session"]["model"] = serde_json::json!({"modelId": consistency});
            assert!(SessionCreateProjection::from_result(&invalid).is_err());
        }
    }

    #[test]
    fn model_projection_preserves_absence_and_rejects_fallbacks_or_alternates() {
        let absent = serde_json::json!({"session": {"sessionId": "session"}});
        assert_eq!(
            SessionCreateProjection::from_result(&absent)
                .unwrap()
                .requested_model,
            None
        );

        let consistency_only = serde_json::json!({
            "session": {
                "sessionId": "session",
                "model": {"modelId": "glm-5.3"}
            }
        });
        assert!(SessionCreateProjection::from_result(&consistency_only).is_err());

        for invalid in [
            serde_json::json!({
                "session": {"sessionId": "session"},
                "settings": {"model": {"current": {"modelId": null}}}
            }),
            serde_json::json!({
                "session": {"sessionId": "session"},
                "settings": {"model": {"current": {"modelId": ""}}}
            }),
            serde_json::json!({
                "session": {"sessionId": "session"},
                "settings": {"model": {"current": {"modelId": 7}}}
            }),
            serde_json::json!({
                "modelId": "glm-5.3",
                "session": {"sessionId": "session"},
                "settings": {"model": {"current": {"modelId": "glm-5.3"}}}
            }),
            serde_json::json!({
                "session": {"sessionId": "session", "modelId": "glm-5.3"},
                "settings": {"model": {"current": {"modelId": "glm-5.3"}}}
            }),
            serde_json::json!({
                "session": {
                    "sessionId": "session",
                    "settings": {"model": {"current": {"modelId": "glm-5.3"}}}
                },
                "settings": {"model": {"current": {"modelId": "glm-5.3"}}}
            }),
        ] {
            assert!(SessionCreateProjection::from_result(&invalid).is_err());
        }
    }

    #[test]
    fn workspace_diagnostic_projection_uses_only_observed_catalog_paths() {
        let projection = WorkspaceDiagnosticProjection::from_result(&serde_json::json!({
            "settings": {"model": {"current": {"modelId": "glm-current"}}},
            "modelCatalog": {"available": [
                {"ref": {"modelId": "glm-other"}},
                {"ref": {"modelId": "glm-current"}},
                {"ref": {"modelId": "glm-current"}}
            ]}
        }))
        .unwrap();
        assert_eq!(projection.current_model.as_deref(), Some("glm-current"));
        assert_eq!(
            projection.available_models,
            vec!["glm-current", "glm-other"]
        );

        assert!(
            WorkspaceDiagnosticProjection::from_result(&serde_json::json!({
                "settings": {"model": {"current": {"modelId": 7}}}
            }))
            .is_err()
        );
        assert!(
            WorkspaceDiagnosticProjection::from_result(&serde_json::json!({
                "modelCatalog": {"available": [{"modelId": "unobserved-direct"}]}
            }))
            .is_err()
        );
    }

    #[test]
    fn session_event_exposes_lifecycle_discriminator() {
        let event = parse_line(
            r#"{"method":"session/event","params":{"sessionId":"s1","type":"turn.started"}}"#,
        )
        .unwrap();
        let WireMessage::Event(event) = event else {
            panic!("session/event must remain on the event stream");
        };
        assert_eq!(event_type(&event), Some("turn.started"));
    }

    #[test]
    fn session_create_serializes_the_observed_acp_mcp_array_shape() {
        let servers = vec![StdioMcpServer {
            name: "fixture-helper".into(),
            command: "/usr/bin/helperd".into(),
            args: vec!["--fixture".into(), "job".into()],
            env: Vec::new(),
        }];
        let value = serde_json::to_value(CreateSessionParams {
            workspace: WorkspaceRef {
                workspace_key: "/work",
                workspace_path: "/work",
            },
            mode: None,
            thought_level: None,
            mcp_servers: &servers,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "workspace":{"workspaceKey":"/work","workspacePath":"/work"},
                "mcpServers":[{
                    "name":"fixture-helper","command":"/usr/bin/helperd",
                    "args":["--fixture","job"],"env":[]
                }]
            })
        );
    }

    #[test]
    fn create_and_resume_params_carry_thought_level_only_when_admitted() {
        let workspace = WorkspaceRef {
            workspace_key: "/work",
            workspace_path: "/work",
        };
        assert_eq!(
            serde_json::to_value(CreateSessionParams {
                workspace,
                mode: Some("build"),
                thought_level: Some("high"),
                mcp_servers: &[],
            })
            .unwrap(),
            serde_json::json!({
                "workspace":{"workspaceKey":"/work","workspacePath":"/work"},
                "mode":"build",
                "thoughtLevel":"high"
            })
        );
        assert_eq!(
            serde_json::to_value(ResumeSessionParams {
                session_id: "s1",
                workspace: None,
                thought_level: Some("high"),
                mcp_servers: &[],
            })
            .unwrap(),
            serde_json::json!({"sessionId":"s1","thoughtLevel":"high"})
        );
        // A task without an admitted effort must stay byte-identical to the
        // pre-effort wire: no thoughtLevel key on either command.
        assert_eq!(
            serde_json::to_value(ResumeSessionParams {
                session_id: "s1",
                workspace: None,
                thought_level: None,
                mcp_servers: &[],
            })
            .unwrap(),
            serde_json::json!({"sessionId":"s1"})
        );
    }

    #[test]
    fn thought_level_projection_reads_settings_current_with_bounded_tokens() {
        let echo = serde_json::json!({
            "session": {"sessionId": "session"},
            "settings": {
                "thoughtLevel": {
                    "enabled": true,
                    "current": "high",
                    "available": [{"value": "high", "label": "high"}]
                }
            }
        });
        assert_eq!(
            SessionCreateProjection::from_result(&echo)
                .unwrap()
                .configured_thought_level
                .as_deref(),
            Some("high")
        );
        assert_eq!(
            configured_thought_level_from_result(&echo)
                .unwrap()
                .as_deref(),
            Some("high")
        );

        // Absence is a distinct observed state: no settings at all, no
        // thoughtLevel section, or a section without a current level.
        for missing in [
            serde_json::json!({"session": {"sessionId": "session"}}),
            serde_json::json!({
                "session": {"sessionId": "session"},
                "settings": {"model": {"current": {"modelId": "glm-5.3"}}}
            }),
            serde_json::json!({
                "session": {"sessionId": "session"},
                "settings": {"thoughtLevel": {"enabled": false}}
            }),
        ] {
            assert_eq!(
                SessionCreateProjection::from_result(&missing)
                    .unwrap()
                    .configured_thought_level,
                None,
                "absence must project to None: {missing}"
            );
            assert_eq!(
                configured_thought_level_from_result(&missing).unwrap(),
                None
            );
        }

        for invalid in [
            serde_json::json!({
                "session": {"sessionId": "session"},
                "settings": {"thoughtLevel": {"current": "high"}, "thought": "high"}
            }),
            serde_json::json!({
                "session": {"sessionId": "session"},
                "settings": {"thoughtLevel": {"current": 7}}
            }),
            serde_json::json!({
                "session": {"sessionId": "session"},
                "settings": {"thoughtLevel": {"current": ""}}
            }),
            serde_json::json!({
                "session": {"sessionId": "session"},
                "settings": {"thoughtLevel": null}
            }),
        ] {
            assert!(
                configured_thought_level_from_result(&invalid).is_err(),
                "unbounded or alternate thought echo must fail closed: {invalid}"
            );
        }
    }

    #[test]
    fn official_workspace_preferences_and_permission_shapes_are_exact() {
        assert_eq!(
            serde_json::to_value(WorkspaceParams {
                workspace: WorkspaceRef {
                    workspace_key: "/work",
                    workspace_path: "/work",
                },
            })
            .unwrap(),
            serde_json::json!({"workspace":{"workspaceKey":"/work","workspacePath":"/work"}})
        );
        assert_eq!(
            serde_json::to_value(RuntimePreferences::default()).unwrap(),
            serde_json::json!({
                "nativeSearchEnhancementsEnabled":false,
                "memoryEnabled":false,
                "askUserQuestionAutoResolutionEnabled":false
            })
        );
        let params = serde_json::json!({"options":[
            {"kind":"allow_once","response":{"decision":"allow","reason":"once"}},
            {"kind":"deny","response":{"decision":"deny","reason":"bounded"}}
        ]});
        assert_eq!(
            offered_permission_response(&params, "deny"),
            Some(serde_json::json!({"decision":"deny","reason":"bounded"}))
        );
        assert!(offered_permission_response(&params, "other").is_none());
        let mismatched = serde_json::json!({"options":[
            {"kind":"deny","response":{"decision":"allow"}}
        ]});
        assert!(offered_permission_response(&mismatched, "deny").is_none());
        let duplicate = serde_json::json!({"options":[
            {"kind":"deny","response":{"decision":"deny"}},
            {"kind":"deny","response":{"decision":"deny"}}
        ]});
        assert!(offered_permission_response(&duplicate, "deny").is_none());
    }

    #[test]
    fn set_model_params_pin_the_observed_wire_shape_and_never_persist() {
        assert_eq!(
            serde_json::to_value(SetModelParams::new("s1", "zai", "GLM-5.3", Some("high")))
                .unwrap(),
            serde_json::json!({
                "sessionId": "s1",
                "model": {
                    "providerId": "zai",
                    "modelId": "GLM-5.3",
                    "options": {"reasoningLevel": "high"}
                },
                "persistAsWorkspaceLastUsed": false
            })
        );
        // No resolved level: the options key is omitted entirely.
        assert_eq!(
            serde_json::to_value(SetModelParams::new(
                "s1",
                "deepseek",
                "deepseek-flash",
                None
            ))
            .unwrap(),
            serde_json::json!({
                "sessionId": "s1",
                "model": {"providerId": "deepseek", "modelId": "deepseek-flash"},
                "persistAsWorkspaceLastUsed": false
            })
        );
    }

    #[test]
    fn session_create_projects_the_available_catalog_without_relaxing_alternates() {
        let result = serde_json::json!({
            "session": {"sessionId": "s"},
            "settings": {"model": {
                "current": {"providerId": "zai", "modelId": "GLM-5.3"},
                "available": [
                    {
                        "ref": {"providerId": "zai", "modelId": "GLM-5.3"},
                        "label": "GLM-5.3",
                        "contextWindow": 1000000,
                        "reasoning": {
                            "levels": [
                                {"value": "low", "label": "low"},
                                {"value": "high", "label": "high"},
                                {"value": "max", "label": "max"}
                            ],
                            "defaultLevel": "max"
                        }
                    },
                    {
                        "ref": {"providerId": "deepseek", "modelId": "deepseek-flash"},
                        "reasoning": {"levels": [{"value": "low", "label": "low"}]}
                    },
                    {"ref": {"providerId": "zai", "modelId": "no-reasoning"}}
                ]
            }}
        });
        let projection = SessionCreateProjection::from_result(&result).unwrap();
        assert_eq!(
            projection.available_models[0],
            CatalogModelEntry {
                provider_id: Some("zai".into()),
                model_id: Some("GLM-5.3".into()),
                reasoning_levels: vec!["low".into(), "high".into(), "max".into()],
                default_level: Some("max".into()),
            }
        );
        assert_eq!(projection.available_models[1].default_level, None);
        assert!(projection.available_models[2].reasoning_levels.is_empty());
        // Absence of the catalog is an empty vector, not a projection error.
        assert!(SessionCreateProjection::from_result(&serde_json::json!({
            "session": {"sessionId": "s"}
        }))
        .unwrap()
        .available_models
        .is_empty());
        for invalid in [
            serde_json::json!({
                "session": {"sessionId": "s"},
                "settings": {"model": {"available": {}}}
            }),
            serde_json::json!({
                "session": {"sessionId": "s"},
                "settings": {"model": {"available": [{"ref": 7}]}}
            }),
            serde_json::json!({
                "session": {"sessionId": "s"},
                "settings": {"model": {"available": [
                    {"ref": {"providerId": 7, "modelId": "m"}}
                ]}}
            }),
            serde_json::json!({
                "session": {"sessionId": "s"},
                "settings": {"model": {"available": [
                    {"ref": {"providerId": "p", "modelId": "m"}, "reasoning": {"levels": {}}}
                ]}}
            }),
            serde_json::json!({
                "session": {"sessionId": "s"},
                "settings": {"model": {"available": [
                    {"ref": {"providerId": "p", "modelId": "m"}, "reasoning": {"levels": [{"value": 7}]}}
                ]}}
            }),
        ] {
            assert!(
                SessionCreateProjection::from_result(&invalid).is_err(),
                "catalog drift must fail closed: {invalid}"
            );
        }
    }

    #[test]
    fn set_model_projection_reads_scoped_current_and_thought_level() {
        let result = serde_json::json!({
            "session": {"sessionId": "s"},
            "settings": {
                "model": {"current": {
                    "providerId": "zai",
                    "modelId": "GLM-5.3",
                    "options": {"reasoningLevel": "high"}
                }},
                "thoughtLevel": {"enabled": true, "current": "high"}
            }
        });
        assert_eq!(
            set_model_projection_from_result(&result).unwrap(),
            SetModelProjection {
                provider_id: Some("zai".into()),
                model_id: Some("GLM-5.3".into()),
                configured_thought_level: Some("high".into()),
            }
        );
        for invalid in [
            serde_json::json!({
                "settings": {"model": {"value": "x", "current": {"modelId": "m"}}}
            }),
            serde_json::json!({
                "settings": {"model": {"current": {"id": "other", "modelId": "m"}}}
            }),
            serde_json::json!({
                "settings": {"model": {"current": {"providerId": 7, "modelId": "m"}}}
            }),
            serde_json::json!({
                "settings": {
                    "thoughtLevel": {"current": "high"},
                    "thought": "high"
                }
            }),
        ] {
            assert!(
                set_model_projection_from_result(&invalid).is_err(),
                "unbounded setModel echo must fail closed: {invalid}"
            );
        }
    }

    #[test]
    fn model_normalization_is_provider_scoped_and_bare_only_for_legacy() {
        assert_eq!(
            normalized_zai_model("zai/GLM-5.3").as_deref(),
            Some("glm-5.3")
        );
        assert_eq!(normalized_zai_model("GLM-5.3").as_deref(), Some("glm-5.3"));
        assert_eq!(
            normalized_zai_model("deepseek/deepseek-flash").as_deref(),
            Some("deepseek-flash")
        );
        assert_eq!(
            normalized_model_reference("deepseek/deepseek-flash"),
            Some(("deepseek".into(), "deepseek-flash".into()))
        );
        assert_eq!(
            normalized_model_reference("GLM-5.3"),
            Some(("zai".into(), "glm-5.3".into()))
        );
        for invalid in ["", "zai/", "/GLM", "zai/a/b", "za i/GLM", "zai/\0", "a//b"] {
            assert!(
                normalized_model_reference(invalid).is_none(),
                "malformed token must not normalize: {invalid:?}"
            );
        }
        // The bare legacy compatibility never enters provider-qualified helpers.
        assert!(normalized_scoped_model("GLM-5.3").is_none());
        assert!(!provider_model_matches("GLM-5.3", "zai", "GLM-5.3"));
        assert!(provider_model_matches("zai/GLM-5.3", "zai", "glm-5.3"));
        assert!(!provider_model_matches(
            "zai/GLM-5.3",
            "deepseek",
            "glm-5.3"
        ));
        assert!(!provider_model_matches(
            "deepseek/deepseek-flash",
            "deepseek",
            "other"
        ));
        assert!(!provider_model_matches("zai/GLM-5.3", "zai", ""));
        assert!(!provider_model_matches("zai/GLM-5.3", "za i", "glm-5.3"));
    }
}
