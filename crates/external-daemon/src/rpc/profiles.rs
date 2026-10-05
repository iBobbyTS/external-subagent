//! Global spawn profile loading, validation, and error reporting.
//!
//! Profiles are defined in TOML files under the global `profiles/` directory
//! (sibling to the agent configuration file). Each file defines one profile
//! with authoritative top-level name and optional presets:
//! - subagent
//! - permission_mode
//! - model
//! - effort
//! - developer_instructions

use external_core::PermissionMode;
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

use super::errors::{RpcError, RpcErrorCode};
use super::types::MAX_RESPONSE_FRAME_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub subagent: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub developer_instructions: Option<String>,
    pub source_path: PathBuf,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    name: String,
    #[serde(default)]
    subagent: Option<String>,
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    developer_instructions: Option<String>,
}

/// Locates the global profiles directory from configuration environment variables.
pub fn profiles_directory() -> Option<PathBuf> {
    let env_path = env::var_os("EXTERNAL_SUBAGENT_CONFIG")
        .or_else(|| env::var_os("ZCODE_AGENT_CONFIG"))?;
    let path = PathBuf::from(env_path);
    path.parent().map(|p| p.join("profiles"))
}

/// Parse and validate a single profile from a TOML string and file path.
pub fn parse_profile_toml(content: &str, file_path: &Path) -> Result<Profile, RpcError> {
    let raw: RawProfile = toml::from_str(content).map_err(|error| {
        RpcError::new_profile_error(
            RpcErrorCode::Validation,
            format!("profile file '{}' is invalid: {error}", file_path.display()),
        )
    })?;

    let name = raw.name.trim().to_string();
    if name.is_empty() {
        return Err(RpcError::new_profile_error(
            RpcErrorCode::Validation,
            format!(
                "profile file '{}' is invalid: field 'name' cannot be empty",
                file_path.display()
            ),
        ));
    }
    if raw.name.len() > 128 {
        return Err(RpcError::new_profile_error(
            RpcErrorCode::Validation,
            format!(
                "profile file '{}' is invalid: field 'name' exceeds 128 bytes",
                file_path.display()
            ),
        ));
    }

    let permission_mode = match raw.permission_mode {
        Some(mode) => {
            let trimmed = mode.trim();
            if trimmed.is_empty() {
                None
            } else {
                match trimmed {
                    "build" => Some(PermissionMode::Build),
                    "edit" => Some(PermissionMode::Edit),
                    "plan" => Some(PermissionMode::Plan),
                    "yolo" => Some(PermissionMode::Yolo),
                    other => {
                        return Err(RpcError::new_profile_error(
                            RpcErrorCode::Validation,
                            format!(
                                "profile file '{}' is invalid: field 'permission_mode' must be build, edit, plan, yolo, or empty, got '{other}'",
                                file_path.display()
                            ),
                        ));
                    }
                }
            }
        }
        None => None,
    };

    let subagent = raw
        .subagent
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let model = raw
        .model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let effort = raw
        .effort
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let developer_instructions = raw
        .developer_instructions
        .filter(|s| !s.trim().is_empty());

    Ok(Profile {
        name,
        subagent,
        permission_mode,
        model,
        effort,
        developer_instructions,
        source_path: file_path.to_path_buf(),
    })
}

/// Load all profiles from a specified directory, detecting duplicates and TOML errors.
pub fn load_profiles_from_dir(dir: &Path) -> Result<HashMap<String, Profile>, RpcError> {
    let mut profiles = HashMap::new();
    if !dir.is_dir() {
        return Ok(profiles);
    }

    let mut entries = Vec::new();
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(_) => return Ok(profiles),
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("toml") {
            entries.push(path);
        }
    }
    // Sort file paths for stable, deterministic processing and error reporting
    entries.sort();

    for path in entries {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                return Err(RpcError::new_profile_error(
                    RpcErrorCode::Validation,
                    format!("profile file '{}' is unreadable: {error}", path.display()),
                ));
            }
        };

        let profile = parse_profile_toml(&content, &path)?;
        if let Some(existing) = profiles.get(&profile.name) {
            return Err(RpcError::new_profile_error(
                RpcErrorCode::Validation,
                format!(
                    "profile file '{}' is invalid: duplicate profile name '{}' already defined in '{}'",
                    path.display(),
                    profile.name,
                    existing.source_path.display()
                ),
            ));
        }
        profiles.insert(profile.name.clone(), profile);
    }

    Ok(profiles)
}

/// Formats the unknown profile error message with available profiles list.
pub fn format_unknown_profile_error(requested: &str, names: &[String]) -> String {
    if names.is_empty() {
        return format!("profile '{requested}' not found; available profiles: none");
    }
    const MAX_ALLOWED: usize = MAX_RESPONSE_FRAME_BYTES - 8192;
    let prefix = format!("profile '{requested}' not found; available profiles: [");
    let mut message = prefix;
    let mut truncated_count = 0;
    for (i, name) in names.iter().enumerate() {
        let separator = if i == 0 { "" } else { ", " };
        let addition = format!("{separator}{name}");
        let suffix_reserve = 32;
        if message.len() + addition.len() + suffix_reserve > MAX_ALLOWED {
            truncated_count = names.len() - i;
            break;
        }
        message.push_str(&addition);
    }
    if truncated_count > 0 {
        message.push_str(&format!(", ... +{} more]", truncated_count));
    } else {
        message.push(']');
    }
    message
}

/// Load a specific profile by name from the global profiles directory.
pub fn load_profile(name: &str) -> Result<Profile, RpcError> {
    let profiles_dir = profiles_directory();
    let profiles = match profiles_dir.as_ref() {
        Some(dir) => load_profiles_from_dir(dir)?,
        None => HashMap::new(),
    };

    if let Some(profile) = profiles.get(name) {
        return Ok(profile.clone());
    }

    let mut available: Vec<String> = profiles.into_keys().collect();
    available.sort();
    let message = format_unknown_profile_error(name, &available);
    Err(RpcError::new_profile_error(
        RpcErrorCode::Validation,
        message,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_profile_with_all_fields() {
        let toml = r#"
name = "full-profile"
subagent = "codex"
permission_mode = "edit"
model = "gpt-5"
effort = "high"
developer_instructions = """
Line 1
Line 2
"""
"#;
        let profile = parse_profile_toml(toml, Path::new("test.toml")).unwrap();
        assert_eq!(profile.name, "full-profile");
        assert_eq!(profile.subagent.as_deref(), Some("codex"));
        assert_eq!(profile.permission_mode, Some(PermissionMode::Edit));
        assert_eq!(profile.model.as_deref(), Some("gpt-5"));
        assert_eq!(profile.effort.as_deref(), Some("high"));
        assert_eq!(
            profile.developer_instructions.as_deref(),
            Some("Line 1\nLine 2\n")
        );
    }

    #[test]
    fn parse_valid_profile_with_empty_and_omitted_fields() {
        let toml = r#"
name = "minimal-profile"
subagent = ""
permission_mode = ""
model = ""
effort = ""
developer_instructions = ""
"#;
        let profile = parse_profile_toml(toml, Path::new("test.toml")).unwrap();
        assert_eq!(profile.name, "minimal-profile");
        assert_eq!(profile.subagent, None);
        assert_eq!(profile.permission_mode, None);
        assert_eq!(profile.model, None);
        assert_eq!(profile.effort, None);
        assert_eq!(profile.developer_instructions, None);
    }

    #[test]
    fn parse_unicode_and_multiline_developer_instructions() {
        let toml = r#"
name = "unicode-中文-profile"
developer_instructions = "你好，世界！\n第二行。"
"#;
        let profile = parse_profile_toml(toml, Path::new("test.toml")).unwrap();
        assert_eq!(profile.name, "unicode-中文-profile");
        assert_eq!(
            profile.developer_instructions.as_deref(),
            Some("你好，世界！\n第二行。")
        );
    }

    #[test]
    fn parse_rejects_missing_name() {
        let toml = r#"
subagent = "codex"
"#;
        let err = parse_profile_toml(toml, Path::new("no-name.toml")).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("no-name.toml"));
        assert!(err.message.contains("name"));
    }

    #[test]
    fn parse_rejects_empty_name() {
        let toml = r#"
name = "  "
"#;
        let err = parse_profile_toml(toml, Path::new("empty-name.toml")).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("empty-name.toml"));
        assert!(err.message.contains("name"));
    }

    #[test]
    fn parse_rejects_oversized_name() {
        let long_name = "a".repeat(129);
        let toml = format!(r#"name = "{long_name}""#);
        let err = parse_profile_toml(&toml, Path::new("long-name.toml")).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("long-name.toml"));
        assert!(err.message.contains("128 bytes"));
    }

    #[test]
    fn parse_rejects_unknown_field() {
        let toml = r#"
name = "test"
unknown_field = "value"
"#;
        let err = parse_profile_toml(toml, Path::new("unknown.toml")).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("unknown.toml"));
        assert!(err.message.contains("unknown_field"));
    }

    #[test]
    fn parse_rejects_invalid_field_type() {
        let toml = r#"
name = "test"
model = 12345
"#;
        let err = parse_profile_toml(toml, Path::new("type-err.toml")).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("type-err.toml"));
        assert!(err.message.contains("model"));
    }

    #[test]
    fn parse_rejects_invalid_permission_mode() {
        let toml = r#"
name = "test"
permission_mode = "superuser"
"#;
        let err = parse_profile_toml(toml, Path::new("bad-mode.toml")).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("bad-mode.toml"));
        assert!(err.message.contains("permission_mode"));
        assert!(err.message.contains("superuser"));
    }

    #[test]
    fn load_profiles_detects_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.toml"), "name = \"dup\"\n").unwrap();
        fs::write(dir.path().join("b.toml"), "name = \"dup\"\n").unwrap();
        let err = load_profiles_from_dir(dir.path()).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("duplicate profile name 'dup'"));
        assert!(err.message.contains("a.toml"));
        assert!(err.message.contains("b.toml"));
    }

    #[test]
    fn load_profiles_ignores_non_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("profile.toml"), "name = \"good\"\n").unwrap();
        fs::write(dir.path().join("README.md"), "junk\n").unwrap();
        let profiles = load_profiles_from_dir(dir.path()).unwrap();
        assert_eq!(profiles.len(), 1);
        assert!(profiles.contains_key("good"));
    }

    #[test]
    fn format_unknown_profile_error_stable_and_truncation() {
        let names = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let msg = format_unknown_profile_error("missing", &names);
        assert_eq!(
            msg,
            "profile 'missing' not found; available profiles: [alpha, beta, gamma]"
        );

        let empty_msg = format_unknown_profile_error("missing", &[]);
        assert_eq!(
            empty_msg,
            "profile 'missing' not found; available profiles: none"
        );
    }
}
