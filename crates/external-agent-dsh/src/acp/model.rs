//! DSH model selection through the standard ACP config-option channel.
//!
//! Model values are opaque catalog tokens (`models/list` / advertised
//! `configOptions`); this module never decodes provider-internal encodings.
//! Selection happens with `session/set_config_option` and only a verified
//! response allows the following `session/prompt` (X05: a token that does not
//! exist fails before any prompt is sent, and omitting the model never
//! overrides the provider default).

use serde_json::Value;

/// Standard config-option id the upstream model-control layer registers for
/// model selection (SOURCE_INSPECTED: ACP `model-control.ts`).
pub const MODEL_CONFIG_ID: &str = "model";

pub const MAX_MODEL_TOKEN_BYTES: usize = 512;

/// Why a requested model token was refused before any prompt was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSetError {
    /// The token does not satisfy the opaque-token bounds.
    TokenInvalid,
    /// The session did not advertise a `model` config option.
    NotOffered,
    /// The server rejected `session/set_config_option`.
    Rejected(String),
}

impl std::fmt::Display for ModelSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TokenInvalid => write!(f, "model token is not a bounded non-empty string"),
            Self::NotOffered => write!(f, "dsh session advertised no model config option"),
            Self::Rejected(message) => {
                write!(f, "dsh session rejected the model selection: {message}")
            }
        }
    }
}
impl std::error::Error for ModelSetError {}

/// Validate an opaque catalog token client-side (bounds only, no decoding).
pub fn validate_catalog_token(token: &str) -> Result<(), ModelSetError> {
    if token.is_empty() || token.len() > MAX_MODEL_TOKEN_BYTES || token.contains('\0') {
        return Err(ModelSetError::TokenInvalid);
    }
    Ok(())
}

/// Whether the advertised `session/new` config options contain a `model`
/// option. `None` (options absent) counts as not offered: fail closed rather
/// than guessing a provider default on the caller's behalf.
pub fn model_option_offered(config_options: Option<&Value>) -> bool {
    config_options
        .and_then(Value::as_array)
        .is_some_and(|options| {
            options.iter().any(|option| {
                option
                    .get("id")
                    .or_else(|| option.get("configId"))
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == MODEL_CONFIG_ID)
            })
        })
}

/// Interpret a `session/set_config_option` outcome. A success result applies
/// the token; a JSON-RPC error is a bounded rejection (unknown option, no
/// selection, provider refusal) and must stop the task before any prompt.
pub fn classify_set_response(response: Result<&Value, &Value>) -> Result<(), ModelSetError> {
    match response {
        Ok(_) => Ok(()),
        Err(error) => {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("session rejected the config option")
                .chars()
                .take(256)
                .collect::<String>();
            Err(ModelSetError::Rejected(message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokens_are_opaque_and_bounded() {
        assert!(validate_catalog_token("fixture-model").is_ok());
        assert_eq!(validate_catalog_token(""), Err(ModelSetError::TokenInvalid));
        assert_eq!(
            validate_catalog_token(&"x".repeat(513)),
            Err(ModelSetError::TokenInvalid)
        );
        assert_eq!(
            validate_catalog_token("bad\0token"),
            Err(ModelSetError::TokenInvalid)
        );
    }

    #[test]
    fn model_option_detection_fail_closes_on_absent_options() {
        assert!(!model_option_offered(None));
        assert!(!model_option_offered(Some(&json!([]))));
        assert!(!model_option_offered(Some(&json!([
            {"configId": "reasoning_effort"}
        ]))));
        assert!(model_option_offered(Some(&json!([
            {"configId": "model", "title": "Model"}
        ]))));
        assert!(!model_option_offered(Some(&json!({"configId": "model"}))));
    }

    #[test]
    fn set_response_classification_keeps_rejections_bounded() {
        assert!(classify_set_response(Ok(&json!({"configOptions": []}))).is_ok());
        let error = json!({"code": -32602, "message": "unknown model option: nope"});
        match classify_set_response(Err(&error)) {
            Err(ModelSetError::Rejected(message)) => {
                assert!(message.contains("unknown model option"));
            }
            other => panic!("expected bounded rejection, got {other:?}"),
        }
        let long = json!({"code": -32602, "message": "x".repeat(1024)});
        match classify_set_response(Err(&long)) {
            Err(ModelSetError::Rejected(message)) => assert!(message.chars().count() <= 256),
            other => panic!("expected bounded rejection, got {other:?}"),
        }
    }
}
