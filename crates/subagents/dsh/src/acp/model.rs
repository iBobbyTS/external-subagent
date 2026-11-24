//! DSH model selection through the standard ACP config-option channel.
//!
//! A caller selects a model with a `{provider}:{model}` token, split at the
//! first `:`. The ACP `model` config option, however, expects a byte-exact
//! JSON tuple `["provider","model"]` as its `value` string, so [`set_model`]
//! parses the colon token and re-serializes it with [`wire_token`] (serde_json)
//! before `session/set_config_option`. Reasoning effort stays an opaque
//! token. Selection happens with `session/set_config_option` and only a
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

/// Why a requested model token was refused before any prompt was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSetError {
    /// The token does not satisfy the `provider:model` format: it must contain
    /// the provider/model separator with non-empty sides.
    TokenInvalid,
    /// The server rejected `session/set_config_option`.
    Rejected(String),
}

impl std::fmt::Display for ModelSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TokenInvalid => {
                write!(f, "model token must be provider:model with non-empty sides")
            }
            Self::Rejected(message) => {
                write!(f, "dsh session rejected the model selection: {message}")
            }
        }
    }
}
impl std::error::Error for ModelSetError {}

/// Parse a model selection token of the form `{provider}:{model}`. The split
/// is at the *first* `:`, so the model side may itself contain colons; both
/// sides may contain any non-NUL character. Refused when there is no colon, a
/// side is empty, or the token contains NUL.
pub fn parse_colon_token(token: &str) -> Result<(&str, &str), ModelSetError> {
    if token.contains('\0') {
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
    fn colon_tokens_split_at_the_first_colon_and_require_both_sides() {
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
        let new_model = "t".repeat(600);
        let token = format!("p:{new_model}");
        assert_eq!(parse_colon_token(&token).unwrap(), ("p", new_model.as_str()));
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
