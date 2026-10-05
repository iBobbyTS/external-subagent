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
pub const PROFILE_ERROR_ENVELOPE_OVERHEAD: usize = 8192;
pub const MAX_ALLOWED_PROFILE_ERROR_JSON_BYTES: usize =
    MAX_RESPONSE_FRAME_BYTES - PROFILE_ERROR_ENVELOPE_OVERHEAD;

/// Calculate the byte length of a string when encoded inside a JSON string literal.
pub fn json_escaped_byte_len(s: &str) -> usize {
    let mut len = 0;
    for &b in s.as_bytes() {
        match b {
            b'"' | b'\\' => len += 2,
            0x08 | 0x09 | 0x0A | 0x0C | 0x0D => len += 2, // \b, \t, \n, \f, \r
            0x00..=0x1F => len += 6, // \u00xx
            _ => len += 1,
        }
    }
    len
}

/// Truncate a string in-place at a valid UTF-8 character boundary such that its
/// JSON-escaped representation does not exceed `max_json_bytes`.
pub fn truncate_json_escaped(s: &mut String, max_json_bytes: usize) {
    if json_escaped_byte_len(s) <= max_json_bytes {
        return;
    }
    let mut current_json = 0;
    let mut last_valid_idx = 0;
    for (idx, ch) in s.char_indices() {
        let ch_json_len = match ch {
            '"' | '\\' => 2,
            '\x08' | '\t' | '\n' | '\x0C' | '\r' => 2,
            c if (c as u32) <= 0x1F => 6,
            _ => ch.len_utf8(),
        };
        if current_json + ch_json_len > max_json_bytes {
            s.truncate(last_valid_idx);
            return;
        }
        current_json += ch_json_len;
        last_valid_idx = idx + ch.len_utf8();
    }
}

/// Formats a profile rejection error with file diagnostic and available profiles list,
/// ensuring the JSON-encoded size stays safely within the RPC frame cap.
pub fn format_profile_error_with_names(diagnostic: &str, names: &[String]) -> String {
    if names.is_empty() {
        return format!("{diagnostic}; available profiles: none");
    }
    const SUFFIX_RESERVE: usize = 32;
    let max_diag_json = MAX_ALLOWED_PROFILE_ERROR_JSON_BYTES.saturating_sub(256);
    let mut diag = diagnostic.to_string();
    truncate_json_escaped(&mut diag, max_diag_json);

    let prefix = format!("{diag}; available profiles: [");
    let mut message = prefix;
    let mut current_json_len = json_escaped_byte_len(&message);
    let mut truncated_count = 0;

    for (i, name) in names.iter().enumerate() {
        let separator = if i == 0 { "" } else { ", " };
        let addition = format!("{separator}{name}");
        let addition_json_len = json_escaped_byte_len(&addition);
        if current_json_len + addition_json_len + SUFFIX_RESERVE > MAX_ALLOWED_PROFILE_ERROR_JSON_BYTES {
            truncated_count = names.len() - i;
            break;
        }
        message.push_str(&addition);
        current_json_len += addition_json_len;
    }

    if truncated_count > 0 {
        message.push_str(&format!(", ... +{} more]", truncated_count));
    } else {
        message.push(']');
    }
    message
}

/// Formats the unknown profile error message with available profiles list.
pub fn format_unknown_profile_error(requested: &str, names: &[String]) -> String {
    format_profile_error_with_names(
        &format!("profile '{requested}' not found"),
        names,
    )
}

#[derive(Debug, Clone)]
pub struct ProfileFileDiagnostic {
    pub file_path: PathBuf,
    pub profile_name: Option<String>,
    pub diagnostic: String,
}

#[derive(Debug, Clone)]
pub struct LoadedProfiles {
    pub profiles: HashMap<String, Profile>,
    pub file_errors: Vec<ProfileFileDiagnostic>,
}

#[derive(serde::Deserialize)]
struct LooseName {
    #[serde(default)]
    name: Option<String>,
}

/// Scan a directory for all profile TOML files, collecting valid and non-conflicting profiles
/// alongside file-level diagnostics for unreadable, invalid, or duplicate files.
pub fn scan_profiles_dir(dir: &Path) -> LoadedProfiles {
    let mut profiles = HashMap::new();
    let mut file_errors = Vec::new();
    let mut conflicting_names = std::collections::HashSet::new();

    if !dir.is_dir() {
        return LoadedProfiles {
            profiles,
            file_errors,
        };
    }

    let mut entries = Vec::new();
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            file_errors.push(ProfileFileDiagnostic {
                file_path: dir.to_path_buf(),
                profile_name: None,
                diagnostic: format!("profile directory '{}' is unreadable: {err}", dir.display()),
            });
            return LoadedProfiles {
                profiles,
                file_errors,
            };
        }
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
                file_errors.push(ProfileFileDiagnostic {
                    file_path: path.clone(),
                    profile_name: None,
                    diagnostic: format!("profile file '{}' is unreadable: {error}", path.display()),
                });
                continue;
            }
        };

        let raw_name = toml::from_str::<LooseName>(&content)
            .ok()
            .and_then(|l| l.name)
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());

        match parse_profile_toml(&content, &path) {
            Ok(profile) => {
                if conflicting_names.contains(&profile.name) {
                    file_errors.push(ProfileFileDiagnostic {
                        file_path: path.clone(),
                        profile_name: Some(profile.name.clone()),
                        diagnostic: format!(
                            "profile file '{}' is invalid: duplicate profile name '{}'",
                            path.display(),
                            profile.name,
                        ),
                    });
                } else if let Some(existing) = profiles.remove(&profile.name) {
                    conflicting_names.insert(profile.name.clone());
                    file_errors.push(ProfileFileDiagnostic {
                        file_path: path.clone(),
                        profile_name: Some(profile.name.clone()),
                        diagnostic: format!(
                            "profile file '{}' is invalid: duplicate profile name '{}' already defined in '{}'",
                            path.display(),
                            profile.name,
                            existing.source_path.display()
                        ),
                    });
                } else {
                    profiles.insert(profile.name.clone(), profile);
                }
            }
            Err(err) => {
                file_errors.push(ProfileFileDiagnostic {
                    file_path: path.clone(),
                    profile_name: raw_name,
                    diagnostic: err.message,
                });
            }
        }
    }

    LoadedProfiles {
        profiles,
        file_errors,
    }
}

/// Load all profiles from a specified directory. If any file failed or conflicted,
/// returns a validation error containing the first diagnostic and the available profile list.
pub fn load_profiles_from_dir(dir: &Path) -> Result<HashMap<String, Profile>, RpcError> {
    let loaded = scan_profiles_dir(dir);
    let mut available_names: Vec<String> = loaded.profiles.keys().cloned().collect();
    available_names.sort();

    if let Some(first_err) = loaded.file_errors.first() {
        let message = format_profile_error_with_names(&first_err.diagnostic, &available_names);
        return Err(RpcError::new_profile_error(RpcErrorCode::Validation, message));
    }

    Ok(loaded.profiles)
}

/// Retrieve the stably sorted list of currently available and valid profile names.
pub fn available_profile_names() -> Vec<String> {
    let profiles_dir = profiles_directory();
    match profiles_dir.as_ref() {
        Some(dir) => {
            let loaded = scan_profiles_dir(dir);
            let mut names: Vec<String> = loaded.profiles.into_keys().collect();
            names.sort();
            names
        }
        None => Vec::new(),
    }
}

/// Load a specific profile by name from the global profiles directory.
/// If the requested profile does not exist or was rejected (invalid TOML/duplicate/unreadable),
/// returns an error with file diagnostic and available profile names.
pub fn load_profile(name: &str) -> Result<Profile, RpcError> {
    let profiles_dir = profiles_directory();
    let loaded = match profiles_dir.as_ref() {
        Some(dir) => scan_profiles_dir(dir),
        None => LoadedProfiles {
            profiles: HashMap::new(),
            file_errors: Vec::new(),
        },
    };

    if let Some(profile) = loaded.profiles.get(name) {
        return Ok(profile.clone());
    }

    let mut available: Vec<String> = loaded.profiles.into_keys().collect();
    available.sort();

    // Check if the requested name matches any failed file's profile_name or file_stem
    let matched_diag = loaded.file_errors.iter().find(|diag| {
        diag.profile_name.as_deref() == Some(name)
            || diag.file_path.file_stem().and_then(|s| s.to_str()) == Some(name)
    });

    let diagnostic = match matched_diag {
        Some(d) => d.diagnostic.clone(),
        None => format!("profile '{name}' not found"),
    };

    let message = format_profile_error_with_names(&diagnostic, &available);
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

    #[test]
    fn json_escaped_byte_len_matches_serde_json() {
        let test_strings = [
            "",
            "hello world",
            "backslash: \\ and quote: \"",
            "control chars: \0 \t \n \r \x08 \x0c \x1f",
            "unicode: 中文测试 🚀 ñáéíóú",
            &"\\".repeat(122),
            &"\"".repeat(50),
        ];

        for s in test_strings {
            let serde_len = serde_json::to_string(&s).unwrap().len() - 2;
            let helper_len = json_escaped_byte_len(s);
            assert_eq!(
                helper_len, serde_len,
                "Mismatch for string of raw len {}: helper={}, serde={}",
                s.len(),
                helper_len,
                serde_len
            );
        }
    }

    #[test]
    fn truncate_json_escaped_bounds_json_expansion_safely() {
        // String of 1,000 backslashes: raw len 1,000, JSON len 2,000
        let mut s = "\\".repeat(1000);
        truncate_json_escaped(&mut s, 500);
        let escaped_len = json_escaped_byte_len(&s);
        assert!(escaped_len <= 500);
        assert_eq!(s.len(), 250); // 250 backslashes = 500 JSON bytes

        // Unicode multi-byte character boundary safety
        let mut u = "你好世界，人工智能".to_string(); // 9 chars * 3 bytes = 27 bytes
        truncate_json_escaped(&mut u, 10);
        let u_escaped_len = json_escaped_byte_len(&u);
        assert!(u_escaped_len <= 10);
        assert_eq!(u, "你好世"); // 3 chars * 3 bytes = 9 bytes <= 10
    }

    #[test]
    fn load_profile_with_bad_and_good_toml_reports_diagnostic_and_available_list() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("good.toml"),
            "name = \"good\"\nsubagent = \"zcode\"\npermission_mode = \"edit\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("bad.toml"),
            "name = \"bad\"\npermission_mode = \"superuser\"\n",
        )
        .unwrap();

        let loaded = scan_profiles_dir(dir.path());
        assert_eq!(loaded.profiles.len(), 1);
        assert!(loaded.profiles.contains_key("good"));
        assert_eq!(loaded.file_errors.len(), 1);
        assert!(loaded.file_errors[0].diagnostic.contains("superuser"));

        // Direct directory load returns Err with bad.toml diagnostic AND available profiles: [good]
        let err = load_profiles_from_dir(dir.path()).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("bad.toml"));
        assert!(err.message.contains("superuser"));
        assert!(err.message.contains("available profiles: [good]"));
    }

    #[test]
    fn duplicate_profiles_removed_from_available_and_reports_diagnostic_with_available() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.toml"), "name = \"dup\"\n").unwrap();
        fs::write(dir.path().join("b.toml"), "name = \"dup\"\n").unwrap();
        fs::write(dir.path().join("c.toml"), "name = \"good\"\n").unwrap();

        let loaded = scan_profiles_dir(dir.path());
        assert_eq!(loaded.profiles.len(), 1);
        assert!(loaded.profiles.contains_key("good"));
        assert!(!loaded.profiles.contains_key("dup"));

        let err = load_profiles_from_dir(dir.path()).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("duplicate profile name 'dup'"));
        assert!(err.message.contains("available profiles: [good]"));
    }
}
