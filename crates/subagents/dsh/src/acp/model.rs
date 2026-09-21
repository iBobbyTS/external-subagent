//! DSH model selection through the standard ACP config-option channel.
//!
//! A caller selects a model with a `{provider}:{model}` token, split at the
//! first `:`. The ACP `model` config option, however, expects a byte-exact
//! JSON tuple `["provider","model"]` as its `value` string, so [`set_model`]
//! parses the colon token and re-serializes it with [`wire_token`] (serde_json)
//! before `session/set_config_option`. Reasoning effort stays an opaque
//! bounded token. Selection happens with `session/set_config_option` and only a
//! verified response allows the following `session/prompt` (X05: a token that
//! does not exist fails before any prompt is sent, and omitting the model never
//! overrides the provider default).
//!
//! [`set_model`]: crate::acp::session::AcpSession::set_model

use serde_json::Value;

/// Standard config-option id the upstream model-control layer registers for
/// model selection (SOURCE_INSPECTED: ACP `model-control.ts`).
pub const MODEL_CONFIG_ID: &str = "model";

/// Standard config-option id for reasoning-effort selection. The reserved
/// standard id is corroborated by this repository's own model tests, which
/// pin `{"configId":"reasoning_effort"}` as the observed non-model option
/// shape in an advertised `configOptions` array.
pub const REASONING_EFFORT_CONFIG_ID: &str = "reasoning_effort";

pub const MAX_MODEL_TOKEN_BYTES: usize = 512;

/// Why a requested model token was refused before any prompt was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSetError {
    /// The token does not satisfy the `provider:model` format: it must contain
    /// the provider/model separator with non-empty sides.
    TokenInvalid,
    /// The session did not advertise a `model` config option.
    NotOffered,
    /// The server rejected `session/set_config_option`.
    Rejected(String),
}

impl std::fmt::Display for ModelSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TokenInvalid => {
                write!(f, "model token must be provider:model with non-empty sides")
            }
            Self::NotOffered => write!(f, "dsh session advertised no model config option"),
            Self::Rejected(message) => {
                write!(f, "dsh session rejected the model selection: {message}")
            }
        }
    }
}
impl std::error::Error for ModelSetError {}

/// Why a requested reasoning-effort token was refused before any prompt was
/// sent. The failure stages follow [`ModelSetError`] (X05) minus the
/// server-rejection variant — a rejected set-option response surfaces as the
/// shared request error instead — and only the error text names the
/// reasoning-effort selection so refusals stay distinguishable in diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningEffortSetError {
    /// The token does not satisfy the opaque-token bounds.
    TokenInvalid,
    /// The session advertised config options without `reasoning_effort`.
    NotOffered,
}

impl std::fmt::Display for ReasoningEffortSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TokenInvalid => {
                write!(
                    f,
                    "reasoning effort token is not a bounded non-empty string"
                )
            }
            Self::NotOffered => {
                write!(
                    f,
                    "dsh session advertised no reasoning_effort config option"
                )
            }
        }
    }
}
impl std::error::Error for ReasoningEffortSetError {}

/// Validate an opaque catalog token client-side (bounds only, no decoding).
/// Retained for the reasoning-effort bound; model selection uses
/// [`parse_colon_token`] instead.
pub fn validate_catalog_token(token: &str) -> Result<(), ModelSetError> {
    if token.is_empty() || token.len() > MAX_MODEL_TOKEN_BYTES || token.contains('\0') {
        return Err(ModelSetError::TokenInvalid);
    }
    Ok(())
}

/// Parse a model selection token of the form `{provider}:{model}`. The split
/// is at the *first* `:`, so the model side may itself contain colons; both
/// sides may contain any non-NUL character. Refused when there is no colon, a
/// side is empty, the token contains NUL, or it exceeds
/// [`MAX_MODEL_TOKEN_BYTES`]. Callers trim the token before admission, keeping
/// the historical trim-then-validate order.
pub fn parse_colon_token(token: &str) -> Result<(&str, &str), ModelSetError> {
    if token.is_empty() || token.len() > MAX_MODEL_TOKEN_BYTES || token.contains('\0') {
        return Err(ModelSetError::TokenInvalid);
    }
    let Some(separator) = token.find(':') else {
        return Err(ModelSetError::TokenInvalid);
    };
    let provider = &token[..separator];
    let model = &token[separator + 1..];
    if provider.is_empty() || model.is_empty() {
        return Err(ModelSetError::TokenInvalid);
    }
    Ok((provider, model))
}

/// Serialize the byte-exact ACP `model` config-option value for a parsed
/// selection: the JSON string for the two-element `[provider, model]` array,
/// e.g. `["deepseek-official","deepseek-flash"]`. serde_json is used (never
/// manual concatenation) so escaping matches JSON.stringify byte for byte.
pub fn wire_token(provider: &str, model: &str) -> String {
    serde_json::to_string(&[provider, model]).expect("a string array always serializes")
}

/// Validate a reasoning-effort token with the same opaque bounds as a model
/// catalog token: dsh effort admission is bounded passthrough, so the value
/// is never decoded against a client-side closed set.
pub fn validate_reasoning_effort_token(token: &str) -> Result<(), ReasoningEffortSetError> {
    validate_catalog_token(token).map_err(|_| ReasoningEffortSetError::TokenInvalid)
}

/// Whether the advertised `session/new` config options contain an option
/// with the given standard config id.
fn config_id_offered(config_options: Option<&Value>, config_id: &str) -> bool {
    config_options
        .and_then(Value::as_array)
        .is_some_and(|options| {
            options.iter().any(|option| {
                option
                    .get("id")
                    .or_else(|| option.get("configId"))
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == config_id)
            })
        })
}

/// Whether the advertised `session/new` config options contain a `model`
/// option. `None` (options absent) counts as not offered: fail closed rather
/// than guessing a provider default on the caller's behalf.
pub fn model_option_offered(config_options: Option<&Value>) -> bool {
    config_id_offered(config_options, MODEL_CONFIG_ID)
}

/// Whether the advertised `session/new` config options contain a
/// `reasoning_effort` option, using the same existence check as
/// [`model_option_offered`].
pub fn reasoning_effort_option_offered(config_options: Option<&Value>) -> bool {
    config_id_offered(config_options, REASONING_EFFORT_CONFIG_ID)
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
    fn colon_tokens_split_at_the_first_colon_and_bound_both_sides() {
        assert_eq!(
            parse_colon_token("deepseek-official:deepseek-flash"),
            Ok(("deepseek-official", "deepseek-flash"))
        );
        // The first colon is the only separator: later colons belong to model.
        assert_eq!(parse_colon_token("a:b:c"), Ok(("a", "b:c")));
        assert_eq!(
            parse_colon_token("provider:model:variant:2"),
            Ok(("provider", "model:variant:2"))
        );
        // Side character sets are unrestricted.
        assert_eq!(
            parse_colon_token("p-1.2/3:m@4+v").unwrap(),
            ("p-1.2/3", "m@4+v")
        );
        for invalid in ["", "no-colon", ":model", "provider:", "bad\0token", "p:m\0"] {
            assert_eq!(
                parse_colon_token(invalid),
                Err(ModelSetError::TokenInvalid),
                "invalid token {invalid:?}"
            );
        }
        // The bound is exactly MAX_MODEL_TOKEN_BYTES on the whole token.
        let bounded = format!("p:{}", "t".repeat(MAX_MODEL_TOKEN_BYTES - 2));
        assert_eq!(bounded.len(), MAX_MODEL_TOKEN_BYTES);
        assert!(parse_colon_token(&bounded).is_ok());
        let oversized = format!("p:{}", "t".repeat(MAX_MODEL_TOKEN_BYTES - 1));
        assert_eq!(oversized.len(), MAX_MODEL_TOKEN_BYTES + 1);
        assert_eq!(
            parse_colon_token(&oversized),
            Err(ModelSetError::TokenInvalid)
        );
        // A bare over-long token is refused before the missing colon matters.
        assert_eq!(
            parse_colon_token(&"x".repeat(MAX_MODEL_TOKEN_BYTES + 1)),
            Err(ModelSetError::TokenInvalid)
        );
    }

    #[test]
    fn wire_tokens_are_byte_exact_serde_json_string_arrays() {
        assert_eq!(
            wire_token("deepseek-official", "deepseek-flash"),
            r#"["deepseek-official","deepseek-flash"]"#
        );
        // Quotes, backslashes, and non-ASCII survive exactly as JSON.stringify
        // would emit them (serde_json leaves non-ASCII unescaped).
        let provider = "pro\"vider\\x";
        let model = "模型:flash";
        let wire = wire_token(provider, model);
        assert_eq!(wire, r#"["pro\"vider\\x","模型:flash"]"#);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&wire).unwrap(),
            vec![provider.to_owned(), model.to_owned()]
        );
        // The colon token round-trips: parse then re-emit is the same wire.
        let token = format!("{provider}:{model}");
        let (parsed_provider, parsed_model) = parse_colon_token(&token).unwrap();
        assert_eq!(wire_token(parsed_provider, parsed_model), wire);
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
    fn reasoning_effort_option_detection_mirrors_the_model_check() {
        assert!(!reasoning_effort_option_offered(None));
        assert!(!reasoning_effort_option_offered(Some(&json!([]))));
        assert!(!reasoning_effort_option_offered(Some(&json!([
            {"configId": "model"}
        ]))));
        assert!(reasoning_effort_option_offered(Some(&json!([
            {"configId": "model"},
            {"configId": "reasoning_effort", "title": "Reasoning effort"}
        ]))));
        assert!(reasoning_effort_option_offered(Some(&json!([
            {"id": "reasoning_effort"}
        ]))));
        assert!(!reasoning_effort_option_offered(Some(&json!({
            "configId": "reasoning_effort"
        }))));
    }

    #[test]
    fn reasoning_effort_tokens_share_the_opaque_bounds() {
        assert!(validate_reasoning_effort_token("high").is_ok());
        assert_eq!(
            validate_reasoning_effort_token(""),
            Err(ReasoningEffortSetError::TokenInvalid)
        );
        assert_eq!(
            validate_reasoning_effort_token(&"x".repeat(MAX_MODEL_TOKEN_BYTES + 1)),
            Err(ReasoningEffortSetError::TokenInvalid)
        );
        assert_eq!(
            validate_reasoning_effort_token("bad\0token"),
            Err(ReasoningEffortSetError::TokenInvalid)
        );
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
