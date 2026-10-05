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

pub const PROFILE_ERROR_ENVELOPE_OVERHEAD: usize = 8192;
pub const MAX_ALLOWED_PROFILE_ERROR_JSON_BYTES: usize =
    MAX_RESPONSE_FRAME_BYTES - PROFILE_ERROR_ENVELOPE_OVERHEAD;

pub const PROFILE_ERROR_PREFIXES: [&str; 6] = [
    "profile cannot be combined with",
    "profile is invalid",
    "profile file '",
    "profile directory '",
    "profile field '",
    "profile '",
];

/// Internal rejection representation carrying problem file path, field, and diagnostic message.
#[derive(Debug, Clone)]
pub struct ProfileRejection {
    pub file_path: Option<PathBuf>,
    pub field: Option<String>,
    pub diagnostic: String,
}

impl ProfileRejection {
    pub fn new(
        file_path: Option<PathBuf>,
        field: Option<String>,
        diagnostic: impl Into<String>,
    ) -> Self {
        Self {
            file_path,
            field,
            diagnostic: diagnostic.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProfileFileDiagnostic {
    pub file_path: PathBuf,
    pub profile_name: Option<String>,
    pub diagnostic: String,
    pub rejection: ProfileRejection,
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

pub fn compose_diagnostic_text(rejection: &ProfileRejection) -> String {
    let diag = &rejection.diagnostic;
    if PROFILE_ERROR_PREFIXES
        .iter()
        .any(|prefix| diag.starts_with(prefix))
    {
        return diag.clone();
    }
    match (&rejection.file_path, &rejection.field) {
        (Some(path), Some(field)) => {
            if diag.starts_with(&format!("field '{field}'")) {
                format!("profile file '{}' is invalid: {diag}", path.display())
            } else if diag.starts_with("cannot ")
                || diag.starts_with("exceeds ")
                || diag.starts_with("must be ")
            {
                format!(
                    "profile file '{}' is invalid: field '{field}' {diag}",
                    path.display()
                )
            } else {
                format!(
                    "profile file '{}' is invalid: field '{field}': {diag}",
                    path.display()
                )
            }
        }
        (Some(path), None) => {
            format!("profile file '{}' is invalid: {diag}", path.display())
        }
        (None, Some(field)) => {
            format!("profile field '{field}' is invalid: {diag}")
        }
        (None, None) => {
            format!("profile is invalid: {diag}")
        }
    }
}

/// Unified error formatting function for all profile-related rejections.
/// Always:
/// (a) Pre-reserves budget for the fixed suffix ("; available profiles: [...]" or "; available profiles: none")
///     based on JSON encoding before truncating the diagnostic.
/// (b) Appends stably sorted available names list.
pub fn format_profile_rejection(
    rejection: &ProfileRejection,
    available_names: &[String],
) -> String {
    let mut sorted_names = available_names.to_vec();
    sorted_names.sort();

    let diag_text = compose_diagnostic_text(rejection);

    if sorted_names.is_empty() {
        const EMPTY_SUFFIX: &str = "; available profiles: none";
        let suffix_json_len = json_escaped_byte_len(EMPTY_SUFFIX);
        let max_diag_json = MAX_ALLOWED_PROFILE_ERROR_JSON_BYTES.saturating_sub(suffix_json_len);
        let mut diag = diag_text;
        truncate_json_escaped(&mut diag, max_diag_json);
        return format!("{diag}{EMPTY_SUFFIX}");
    }

    const SUFFIX_RESERVE: usize = 32;
    let max_diag_json = MAX_ALLOWED_PROFILE_ERROR_JSON_BYTES.saturating_sub(256);
    let mut diag = diag_text;
    truncate_json_escaped(&mut diag, max_diag_json);

    let prefix = format!("{diag}; available profiles: [");
    let mut message = prefix;
    let mut current_json_len = json_escaped_byte_len(&message);
    let mut truncated_count = 0;

    for (i, name) in sorted_names.iter().enumerate() {
        let separator = if i == 0 { "" } else { ", " };
        let addition = format!("{separator}{name}");
        let addition_json_len = json_escaped_byte_len(&addition);
        if current_json_len + addition_json_len + SUFFIX_RESERVE > MAX_ALLOWED_PROFILE_ERROR_JSON_BYTES {
            truncated_count = sorted_names.len() - i;
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

/// Construct an RpcError through the unified profile rejection formatting function,
/// preserving the provided error code.
pub fn make_profile_rejection_error_with_code(
    code: RpcErrorCode,
    rejection: &ProfileRejection,
    available_names: &[String],
) -> RpcError {
    let message = format_profile_rejection(rejection, available_names);
    RpcError::new_profile_error(code, message)
}

/// Construct an RpcError through the unified profile rejection formatting function
/// defaulting to Validation code.
pub fn make_profile_rejection_error(
    rejection: &ProfileRejection,
    available_names: &[String],
) -> RpcError {
    make_profile_rejection_error_with_code(RpcErrorCode::Validation, rejection, available_names)
}

/// Formats a profile rejection error with file diagnostic and available profiles list,
/// delegating to the unified constructor.
#[allow(dead_code)]
pub fn format_profile_error_with_names(diagnostic: &str, names: &[String]) -> String {
    let rejection = ProfileRejection::new(None, None, diagnostic);
    format_profile_rejection(&rejection, names)
}

/// Formats the unknown profile error message with available profiles list,
/// delegating to the unified constructor.
#[allow(dead_code)]
pub fn format_unknown_profile_error(requested: &str, names: &[String]) -> String {
    let rejection = ProfileRejection::new(None, None, format!("profile '{requested}' not found"));
    format_profile_rejection(&rejection, names)
}

fn parse_profile_toml_internal(
    content: &str,
    file_path: &Path,
) -> Result<Profile, ProfileRejection> {
    let raw: RawProfile = match toml::from_str(content) {
        Ok(raw) => raw,
        Err(error) => {
            return Err(ProfileRejection::new(
                Some(file_path.to_path_buf()),
                None,
                error.to_string(),
            ));
        }
    };

    let trimmed = raw.name.trim().to_string();
    if trimmed.is_empty() {
        return Err(ProfileRejection::new(
            Some(file_path.to_path_buf()),
            Some("name".to_string()),
            "cannot be empty",
        ));
    }
    if raw.name.len() > 128 {
        return Err(ProfileRejection::new(
            Some(file_path.to_path_buf()),
            Some("name".to_string()),
            "exceeds 128 bytes",
        ));
    }
    if raw.name.contains('\0') {
        return Err(ProfileRejection::new(
            Some(file_path.to_path_buf()),
            Some("name".to_string()),
            "cannot contain NUL byte",
        ));
    }

    let permission_mode = match raw.permission_mode {
        Some(mode) => {
            let trimmed_mode = mode.trim();
            if trimmed_mode.is_empty() {
                None
            } else {
                match trimmed_mode {
                    "build" => Some(PermissionMode::Build),
                    "edit" => Some(PermissionMode::Edit),
                    "plan" => Some(PermissionMode::Plan),
                    "yolo" => Some(PermissionMode::Yolo),
                    other => {
                        return Err(ProfileRejection::new(
                            Some(file_path.to_path_buf()),
                            Some("permission_mode".to_string()),
                            format!("must be build, edit, plan, yolo, or empty, got '{other}'"),
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
        name: trimmed,
        subagent,
        permission_mode,
        model,
        effort,
        developer_instructions,
        source_path: file_path.to_path_buf(),
    })
}

/// Parse and validate a single profile from a TOML string and file path.
#[allow(dead_code)]
pub fn parse_profile_toml(content: &str, file_path: &Path) -> Result<Profile, RpcError> {
    parse_profile_toml_internal(content, file_path)
        .map_err(|rejection| make_profile_rejection_error(&rejection, &[]))
}

/// Scan a directory for all profile TOML files, collecting valid and non-conflicting profiles
/// alongside file-level diagnostics for unreadable, invalid, or duplicate files.
pub fn scan_profiles_dir(dir: &Path) -> LoadedProfiles {
    let mut profiles = HashMap::new();
    let mut file_errors = Vec::new();

    if !dir.is_dir() {
        return LoadedProfiles {
            profiles,
            file_errors,
        };
    }

    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            let rejection = ProfileRejection::new(
                Some(dir.to_path_buf()),
                None,
                format!("profile directory '{}' is unreadable: {err}", dir.display()),
            );
            let diagnostic = compose_diagnostic_text(&rejection);
            file_errors.push(ProfileFileDiagnostic {
                file_path: dir.to_path_buf(),
                profile_name: None,
                diagnostic,
                rejection,
            });
            return LoadedProfiles {
                profiles,
                file_errors,
            };
        }
    };

    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("toml") {
            entries.push(path);
        }
    }
    entries.sort();

    // F01: Map valid_name -> Vec<PathBuf>
    let mut name_owners: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut candidates: Vec<Profile> = Vec::new();

    for path in entries {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                let rejection = ProfileRejection::new(
                    Some(path.clone()),
                    None,
                    format!("profile file '{}' is unreadable: {error}", path.display()),
                );
                let diagnostic = compose_diagnostic_text(&rejection);
                file_errors.push(ProfileFileDiagnostic {
                    file_path: path.clone(),
                    profile_name: None,
                    diagnostic,
                    rejection,
                });
                continue;
            }
        };

        // F01 / F05: Extract and validate top-level name independently
        let validated_name = match toml::from_str::<LooseName>(&content) {
            Ok(loose) => match loose.name {
                Some(raw) => {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() || raw.len() > 128 || raw.contains('\0') {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                }
                None => None,
            },
            Err(_) => None,
        };

        if let Some(ref name) = validated_name {
            name_owners.entry(name.clone()).or_default().push(path.clone());
        }

        match parse_profile_toml_internal(&content, &path) {
            Ok(profile) => {
                candidates.push(profile);
            }
            Err(rejection) => {
                let diagnostic = compose_diagnostic_text(&rejection);
                file_errors.push(ProfileFileDiagnostic {
                    file_path: path.clone(),
                    profile_name: validated_name,
                    diagnostic,
                    rejection,
                });
            }
        }
    }

    // Process collisions and candidates (F01)
    for profile in candidates {
        let owners = name_owners.get(&profile.name).cloned().unwrap_or_default();
        if owners.len() > 1 {
            let other = owners.iter().find(|p| **p != profile.source_path).unwrap();
            let rejection = ProfileRejection::new(
                Some(profile.source_path.clone()),
                Some("name".to_string()),
                format!(
                    "profile file '{}' is invalid: duplicate profile name '{}' already defined in '{}'",
                    profile.source_path.display(),
                    profile.name,
                    other.display()
                ),
            );
            let diagnostic = compose_diagnostic_text(&rejection);
            file_errors.push(ProfileFileDiagnostic {
                file_path: profile.source_path.clone(),
                profile_name: Some(profile.name.clone()),
                diagnostic,
                rejection,
            });
            // Candidate evicted from available profiles map
        } else {
            profiles.insert(profile.name.clone(), profile);
        }
    }

    // Ensure all conflicting files (even those with field errors) have duplicate diagnostics reported (F01)
    for (name, owners) in &name_owners {
        if owners.len() > 1 {
            for owner in owners {
                let has_dup_diag = file_errors.iter().any(|d| {
                    d.file_path == *owner
                        && d.profile_name.as_deref() == Some(name)
                        && d.diagnostic.contains("duplicate profile name")
                });
                if !has_dup_diag {
                    let other = owners.iter().find(|p| *p != owner).unwrap();
                    let rejection = ProfileRejection::new(
                        Some(owner.clone()),
                        Some("name".to_string()),
                        format!(
                            "profile file '{}' is invalid: duplicate profile name '{}' already defined in '{}'",
                            owner.display(),
                            name,
                            other.display()
                        ),
                    );
                    let diagnostic = compose_diagnostic_text(&rejection);
                    file_errors.push(ProfileFileDiagnostic {
                        file_path: owner.clone(),
                        profile_name: Some(name.clone()),
                        diagnostic,
                        rejection,
                    });
                }
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
#[allow(dead_code)]
pub fn load_profiles_from_dir(dir: &Path) -> Result<HashMap<String, Profile>, RpcError> {
    let loaded = scan_profiles_dir(dir);
    let mut available_names: Vec<String> = loaded.profiles.keys().cloned().collect();
    available_names.sort();

    if let Some(first_err) = loaded.file_errors.first() {
        return Err(make_profile_rejection_error(
            &first_err.rejection,
            &available_names,
        ));
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

    // 1. Check if the requested name matches any failed file's decoded authoritative profile_name
    let matched_diag = loaded
        .file_errors
        .iter()
        .find(|diag| diag.profile_name.as_deref() == Some(name));

    let rejection = if let Some(d) = matched_diag {
        d.rejection.clone()
    } else {
        // F02: Check if there are unattributed diagnostics (syntax error / I/O error)
        let unattributed: Vec<&ProfileFileDiagnostic> = loaded
            .file_errors
            .iter()
            .filter(|diag| diag.profile_name.is_none())
            .collect();

        if !unattributed.is_empty() {
            let details = unattributed
                .iter()
                .map(|d| d.diagnostic.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            ProfileRejection::new(
                None,
                None,
                format!("profile '{name}' not found ({details})"),
            )
        } else {
            ProfileRejection::new(
                None,
                None,
                format!("profile '{name}' not found"),
            )
        }
    };

    Err(make_profile_rejection_error(&rejection, &available))
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

    #[test]
    fn duplicate_profiles_with_invalid_fields_evicted_both_orders_and_three_files() {
        // F01: Test Order 1: a.toml (valid "dup"), b.toml (invalid "dup" with superuser), good.toml (valid "good")
        let dir1 = tempfile::tempdir().unwrap();
        fs::write(dir1.path().join("a.toml"), "name = \"dup\"\nsubagent = \"zcode\"\n").unwrap();
        fs::write(
            dir1.path().join("b.toml"),
            "name = \"dup\"\npermission_mode = \"superuser\"\n",
        )
        .unwrap();
        fs::write(dir1.path().join("good.toml"), "name = \"good\"\nsubagent = \"zcode\"\n").unwrap();

        let loaded1 = scan_profiles_dir(dir1.path());
        assert_eq!(loaded1.profiles.len(), 1);
        assert!(loaded1.profiles.contains_key("good"));
        assert!(!loaded1.profiles.contains_key("dup"), "dup must be evicted from available profiles");

        // F01: Test Order 2: a.toml (invalid "dup" with superuser), b.toml (valid "dup"), good.toml (valid "good")
        let dir2 = tempfile::tempdir().unwrap();
        fs::write(
            dir2.path().join("a.toml"),
            "name = \"dup\"\npermission_mode = \"superuser\"\n",
        )
        .unwrap();
        fs::write(dir2.path().join("b.toml"), "name = \"dup\"\nsubagent = \"zcode\"\n").unwrap();
        fs::write(dir2.path().join("good.toml"), "name = \"good\"\nsubagent = \"zcode\"\n").unwrap();

        let loaded2 = scan_profiles_dir(dir2.path());
        assert_eq!(loaded2.profiles.len(), 1);
        assert!(loaded2.profiles.contains_key("good"));
        assert!(!loaded2.profiles.contains_key("dup"), "dup must be evicted from available profiles in reverse order");

        // F01: Test 3 files: a.toml (valid "dup"), b.toml (invalid "dup"), c.toml (valid "dup"), good.toml (valid "good")
        let dir3 = tempfile::tempdir().unwrap();
        fs::write(dir3.path().join("a.toml"), "name = \"dup\"\n").unwrap();
        fs::write(
            dir3.path().join("b.toml"),
            "name = \"dup\"\npermission_mode = \"superuser\"\n",
        )
        .unwrap();
        fs::write(dir3.path().join("c.toml"), "name = \"dup\"\n").unwrap();
        fs::write(dir3.path().join("good.toml"), "name = \"good\"\n").unwrap();

        let loaded3 = scan_profiles_dir(dir3.path());
        assert_eq!(loaded3.profiles.len(), 1);
        assert!(loaded3.profiles.contains_key("good"));
        assert!(!loaded3.profiles.contains_key("dup"), "dup must be evicted when three files conflict");

        // Reference evicted name:
        let guard = crate::rpc::agents::admission_fixtures::config_env_guard();
        let config_dir = crate::rpc::agents::admission_fixtures::admission_root("test-dup-");
        let profiles_dir = config_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).unwrap();
        fs::write(profiles_dir.join("a.toml"), "name = \"dup\"\nsubagent = \"zcode\"\n").unwrap();
        fs::write(profiles_dir.join("b.toml"), "name = \"dup\"\npermission_mode = \"superuser\"\n").unwrap();
        fs::write(profiles_dir.join("good.toml"), "name = \"good\"\nsubagent = \"zcode\"\n").unwrap();
        let config_path = config_dir.path().join("agents.json");
        let _scope = crate::rpc::agents::admission_fixtures::ConfigEnvScope::install(&config_path);
        let err_load = load_profile("dup").unwrap_err();
        assert_eq!(err_load.code, RpcErrorCode::Validation);
        assert!(err_load.message.contains("dup"));
        assert!(err_load.message.contains("available profiles: [good]"));
        drop(_scope);
        drop(guard);
    }

    #[test]
    fn broken_toml_syntax_diagnostic_preserved_in_not_found_with_available_profiles() {
        // F02: worker.toml has name="broken" but syntax is broken (unclosed quote on model)
        // referencing broken -> returns "profile 'broken' not found", preserves worker.toml parse diagnostic, and available profiles: [good]
        let guard = crate::rpc::agents::admission_fixtures::config_env_guard();
        let config_dir = crate::rpc::agents::admission_fixtures::admission_root("test-broken-");
        let profiles_dir = config_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).unwrap();
        fs::write(
            profiles_dir.join("worker.toml"),
            "name = \"broken\"\nmodel = \"unclosed string\n",
        )
        .unwrap();
        fs::write(
            profiles_dir.join("good.toml"),
            "name = \"good\"\nsubagent = \"zcode\"\n",
        )
        .unwrap();

        let config_path = config_dir.path().join("agents.json");
        let _scope = crate::rpc::agents::admission_fixtures::ConfigEnvScope::install(&config_path);

        let err = load_profile("broken").unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(
            err.message.contains("profile 'broken' not found"),
            "Must preserve unknown profile explanation"
        );
        assert!(
            err.message.contains("worker.toml"),
            "Must preserve broken file diagnostic"
        );
        assert!(
            err.message.contains("available profiles: [good]"),
            "Must preserve available profiles list"
        );
        drop(_scope);
        drop(guard);
    }

    #[test]
    fn oversized_invalid_permission_mode_preserves_none_suffix_and_bounds_frame() {
        // F04: Single file whose permission_mode is an ultra-long invalid string (>2MiB).
        // Message must still contain "; available profiles: none" and frame <= MAX_RESPONSE_FRAME_BYTES.
        let guard = crate::rpc::agents::admission_fixtures::config_env_guard();
        let config_dir = crate::rpc::agents::admission_fixtures::admission_root("test-oversized-");
        let profiles_dir = config_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).unwrap();
        let long_val = "x".repeat(2 * 1024 * 1024 + 100);
        let toml_content = format!("name = \"long_bad\"\npermission_mode = \"{long_val}\"\n");
        fs::write(profiles_dir.join("long_bad.toml"), toml_content).unwrap();

        let config_path = config_dir.path().join("agents.json");
        let _scope = crate::rpc::agents::admission_fixtures::ConfigEnvScope::install(&config_path);

        let err = load_profile("long_bad").unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(
            err.message.ends_with("; available profiles: none"),
            "Oversized diagnostic must preserve '; available profiles: none'"
        );
        let escaped_len = json_escaped_byte_len(&err.message);
        assert!(
            escaped_len <= MAX_ALLOWED_PROFILE_ERROR_JSON_BYTES,
            "JSON escaped length {escaped_len} must be <= {MAX_ALLOWED_PROFILE_ERROR_JSON_BYTES}"
        );
        let frame_len = escaped_len + PROFILE_ERROR_ENVELOPE_OVERHEAD;
        assert!(
            frame_len <= MAX_RESPONSE_FRAME_BYTES,
            "Total frame length {frame_len} must be <= {MAX_RESPONSE_FRAME_BYTES}"
        );
        drop(_scope);
        drop(guard);
    }

    #[test]
    fn parse_rejects_name_with_nul_byte() {
        // F05: name contains \0 (TOML \u0000) -> rejected and reports name field problem
        let toml = "name = \"call\\u0000name\"\n";
        let err = parse_profile_toml(toml, Path::new("nul.toml")).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Validation);
        assert!(err.message.contains("nul.toml"));
        assert!(err.message.contains("name"));
        assert!(err.message.contains("NUL"));
    }

    #[test]
    fn scan_excludes_file_with_nul_name_from_available_profiles() {
        // F05: File with NUL in name does not enter available profiles
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("nul_file.toml"),
            "name = \"call\\u0000name\"\nsubagent = \"zcode\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("good.toml"),
            "name = \"good\"\nsubagent = \"zcode\"\n",
        )
        .unwrap();

        let loaded = scan_profiles_dir(dir.path());
        assert_eq!(loaded.profiles.len(), 1);
        assert!(loaded.profiles.contains_key("good"));
        assert_eq!(loaded.file_errors.len(), 1);
        assert!(loaded.file_errors[0].diagnostic.contains("NUL"));
    }

    #[test]
    fn cross_name_invalid_profiles_attribute_diagnostic_by_authoritative_name_not_file_stem() {
        // R3-02: a.toml (name="other", invalid) + b.toml (name="a", invalid).
        // Referencing "a" must attribute diagnostic to b.toml, not preempted by a.toml file_stem.
        let guard = crate::rpc::agents::admission_fixtures::config_env_guard();
        let config_dir = crate::rpc::agents::admission_fixtures::admission_root("test-cross-name-");
        let profiles_dir = config_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).unwrap();

        fs::write(
            profiles_dir.join("a.toml"),
            "name = \"other\"\npermission_mode = \"superuser\"\n",
        )
        .unwrap();
        fs::write(
            profiles_dir.join("b.toml"),
            "name = \"a\"\npermission_mode = \"superuser\"\n",
        )
        .unwrap();
        fs::write(
            profiles_dir.join("good.toml"),
            "name = \"good\"\nsubagent = \"zcode\"\n",
        )
        .unwrap();

        let config_path = config_dir.path().join("agents.json");
        let _scope = crate::rpc::agents::admission_fixtures::ConfigEnvScope::install(&config_path);

        // 1. Referencing "a" points to b.toml (where name="a" is defined), NOT a.toml
        let err_a = load_profile("a").unwrap_err();
        assert_eq!(err_a.code, RpcErrorCode::Validation);
        assert!(
            err_a.message.contains("b.toml"),
            "Diagnostic must point to b.toml which declared name='a', but got: {}",
            err_a.message
        );
        assert!(
            !err_a.message.contains("a.toml"),
            "Diagnostic must NOT point to a.toml, but got: {}",
            err_a.message
        );
        assert!(err_a.message.contains("available profiles: [good]"));

        // 2. Referencing "other" points to a.toml (where name="other" is defined)
        let err_other = load_profile("other").unwrap_err();
        assert_eq!(err_other.code, RpcErrorCode::Validation);
        assert!(
            err_other.message.contains("a.toml"),
            "Diagnostic must point to a.toml which declared name='other', but got: {}",
            err_other.message
        );
        assert!(err_other.message.contains("available profiles: [good]"));

        // 3. Referencing "b" (stem of b.toml, but no file has name="b") follows F02 not-found path
        let err_b = load_profile("b").unwrap_err();
        assert_eq!(err_b.code, RpcErrorCode::Validation);
        assert!(err_b.message.contains("profile 'b' not found"));
        assert!(err_b.message.contains("available profiles: [good]"));

        drop(_scope);
        drop(guard);
    }

    /// Anchors the CLI's hand-written TOML scanner (`cli/commands/tasks.mjs`)
    /// against the authoritative `toml` crate on a shared differential corpus
    /// (`tests/cli/profiles-corpus/`). Each corpus item asserts two facets the
    /// CLI scanner must reproduce:
    ///   1. the decoded top-level `name` when the document is TOML-syntax valid
    ///      (mirrors `toml::from_str::<LooseName>` used for owner registration);
    ///   2. profile-shape validity: `RawProfile` (`deny_unknown_fields`) decodes
    ///      and yields a usable name. Field-value validation (e.g.
    ///      `permission_mode = "superuser"`) is intentionally excluded, because
    ///      the CLI deliberately leaves value-level validation to the daemon.
    /// A byte-level item whose bytes are not valid UTF-8 is asserted to own no
    /// name and to be invalid, mirroring `fs::read_to_string` failing.
    #[test]
    fn profile_corpus_matches_toml_crate_identity_and_shape() {
        let corpus =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cli/profiles-corpus");
        assert!(corpus.is_dir(), "corpus directory missing: {}", corpus.display());

        let mut toml_files: Vec<PathBuf> = fs::read_dir(&corpus)
            .expect("read corpus directory")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("toml"))
            .collect();
        toml_files.sort();
        assert!(!toml_files.is_empty(), "corpus must contain at least one .toml input");

        for toml_path in toml_files {
            let stem = toml_path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("corpus .toml stem");
            let expected_path = corpus.join(format!("{stem}.expected.json"));
            let expected: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(&expected_path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", expected_path.display())),
            )
            .unwrap_or_else(|e| panic!("parse {}: {e}", expected_path.display()));
            let expected_valid = expected["valid"].as_bool().expect("expected.valid must be bool");
            let expected_name = expected["name"].as_str().map(str::to_string);
            let bytes = fs::read(&toml_path)
                .unwrap_or_else(|e| panic!("read {}: {e}", toml_path.display()));
            let content = match String::from_utf8(bytes) {
                Ok(content) => content,
                Err(_) => {
                    // The daemon reads each profile with `fs::read_to_string`;
                    // invalid UTF-8 fails that read, so the file is skipped
                    // entirely: it owns no name and is never a valid profile.
                    assert!(
                        expected_name.is_none(),
                        "corpus item {stem}: non-UTF8 file must not own a name"
                    );
                    assert!(
                        !expected_valid,
                        "corpus item {stem}: non-UTF8 file must not be a valid profile"
                    );
                    continue;
                }
            };

            let decoded_name = toml::from_str::<toml::Value>(&content)
                .ok()
                .and_then(|value| {
                    let raw = value.get("name").and_then(|v| v.as_str())?;
                    let trimmed = raw.trim();
                    if trimmed.is_empty() || raw.len() > 128 || raw.contains('\0') {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                });

            assert_eq!(
                decoded_name, expected_name,
                "corpus item {stem}: decoded top-level name disagrees with expectation"
            );

            let shape_ok = toml::from_str::<RawProfile>(&content).is_ok();
            let computed_valid = shape_ok && decoded_name.is_some();
            assert_eq!(
                computed_valid, expected_valid,
                "corpus item {stem}: profile-shape validity disagrees with expectation"
            );
        }
    }
}
