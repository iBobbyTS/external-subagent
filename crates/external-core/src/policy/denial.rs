use std::{
    ffi::OsStr,
    path::{Component, Path},
};

use super::commands::{agent_bash_program_allowed, tokenize_agent_bash};
use super::requests::zcode_file_path;
use super::{is_agent_metadata_path, is_credential_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PermissionDenialSemantics {
    program_family: String,
    category: String,
    reason_code: String,
    operand_class: String,
    retry_class: &'static str,
    recommended_action: &'static str,
}

impl PermissionDenialSemantics {
    fn fingerprint(&self) -> String {
        format!(
            "family={};category={};reason={};operand={}",
            self.program_family, self.category, self.reason_code, self.operand_class
        )
    }

    fn feedback(&self, repeated: bool) -> String {
        if repeated {
            format!(
                "DENY[policy_version={};code=REPEATED_DENIED_OPERATION;retry=do_not_retry_equivalent;next=use_read_or_existing_inputs;original_code={}]",
                crate::AGENT_BASH_POLICY_VERSION,
                self.reason_code
            )
        } else {
            format!(
                "DENY[policy_version={};code={};retry={};next={}]",
                crate::AGENT_BASH_POLICY_VERSION,
                self.reason_code,
                self.retry_class,
                self.recommended_action
            )
        }
    }
}

/// Daemon-authoritative denial identity. Its fields are private so callers
/// cannot turn public descriptive text into policy metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPermissionDenial(pub(super) PermissionDenialSemantics);

impl ValidatedPermissionDenial {
    pub fn fingerprint(&self) -> String {
        self.0.fingerprint()
    }

    pub fn feedback(&self, repeated: bool) -> String {
        self.0.feedback(repeated)
    }
}

pub(super) fn permission_denial_semantics(
    params: &serde_json::Value,
    validated_reason: &'static str,
) -> Option<PermissionDenialSemantics> {
    let tool = params
        .get("toolName")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    let input = params.get("input").unwrap_or(&serde_json::Value::Null);
    let (program_family, category, inferred_reason, operand_class) = match tool.as_str() {
        "bash" => input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(bash_denial_identity)
            .unwrap_or_else(|| {
                (
                    "bash".into(),
                    "command".into(),
                    "bash_command_missing".into(),
                    "missing_command".into(),
                )
            }),
        "read" | "grep" | "glob" => read_denial_identity(&tool, input, validated_reason),
        "write" | "edit" | "delete" | "move" => (
            tool.clone(),
            "write".into(),
            "write_denied".into(),
            "mutation".into(),
        ),
        "execute" | "terminal" => {
            let program = input
                .get("program")
                .and_then(serde_json::Value::as_str)
                .and_then(|program| Path::new(program).file_name())
                .and_then(OsStr::to_str)
                .unwrap_or("unknown")
                .to_ascii_lowercase();
            (
                program,
                "execute".into(),
                "command_not_allowlisted".into(),
                "program".into(),
            )
        }
        "network" => (
            "network".into(),
            "network".into(),
            "network_not_enforced_and_request_denied".into(),
            "network".into(),
        ),
        "git_ref_mutation" => (
            "git".into(),
            "ref_mutation".into(),
            "git_ref_mutation_denied".into(),
            "mutation".into(),
        ),
        _ if tool.starts_with("mcp__") => (
            "mcp_tool".into(),
            "mcp_tool".into(),
            "permission_request_unrecognized".into(),
            "unknown".into(),
        ),
        _ => (
            tool.clone(),
            "unknown".into(),
            "permission_request_unrecognized".into(),
            "unknown".into(),
        ),
    };
    let reason_code = if validated_reason.is_empty() {
        inferred_reason
    } else {
        validated_reason.into()
    };
    let (retry_class, recommended_action) =
        denial_recovery(&tool, &program_family, &reason_code, &operand_class);
    Some(PermissionDenialSemantics {
        program_family,
        category,
        reason_code,
        operand_class,
        retry_class,
        recommended_action,
    })
}

fn read_denial_identity(
    tool: &str,
    input: &serde_json::Value,
    validated_reason: &'static str,
) -> (String, String, String, String) {
    let path = if tool == "read" {
        zcode_file_path(input).map(Path::new)
    } else {
        input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(Path::new)
    };
    if validated_reason == "credential_read_denied" || path.is_some_and(is_credential_path) {
        return (
            tool.into(),
            "path".into(),
            "credential_read_denied".into(),
            "sensitive_path".into(),
        );
    }
    if matches!(
        validated_reason,
        "read_path_escape_denied" | "agent_metadata_read_denied"
    ) || path.is_some_and(|path| {
        path.components()
            .any(|component| component == Component::ParentDir)
    }) {
        return (
            tool.into(),
            "path".into(),
            "read_path_escape_denied".into(),
            "outside_scope".into(),
        );
    }
    (
        tool.into(),
        "path".into(),
        validated_reason.into(),
        if path.is_some() {
            "unavailable_path"
        } else {
            "missing_path"
        }
        .into(),
    )
}

fn bash_denial_identity(command: &str) -> (String, String, String, String) {
    let Some(argv) = tokenize_agent_bash(command) else {
        let operand_class = if command.contains('\n') || command.contains('\r') {
            "multiline"
        } else {
            "compound_command"
        };
        return (
            "shell".into(),
            "composition".into(),
            "shell_composition_or_expansion_denied".into(),
            operand_class.into(),
        );
    };
    let family = argv
        .first()
        .and_then(|program| Path::new(program).file_name())
        .and_then(OsStr::to_str)
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    if !agent_bash_program_allowed(&family) {
        return (
            family,
            "program".into(),
            "command_not_allowlisted".into(),
            "program".into(),
        );
    }
    if family == "git" {
        let category = git_denial_category(&argv[1..]);
        let operand_class = git_denial_operand_class(&argv[1..]);
        let reason = if matches!(operand_class.as_str(), "sensitive_path") {
            "git_sensitive_path_denied"
        } else {
            "git_option_or_mutation_denied"
        };
        return (family, category, reason.into(), operand_class);
    }
    let credential_path = argv[1..]
        .iter()
        .any(|value| is_credential_path(Path::new(value)));
    let agent_metadata_path = argv[1..]
        .iter()
        .any(|value| is_agent_metadata_path(Path::new(value)));
    let operand_class = if credential_path || agent_metadata_path {
        "sensitive_path"
    } else if argv[1..].iter().any(|value| value.starts_with('-')) {
        "option"
    } else if argv.len() <= 1 {
        "missing_path"
    } else {
        "path"
    };
    let inferred_reason = match operand_class {
        "sensitive_path" if agent_metadata_path => "agent_metadata_read_denied",
        "sensitive_path" => "credential_read_denied",
        "missing_path" => "command_option_not_allowlisted",
        _ => "external_policy_denied",
    };
    (
        family.clone(),
        family,
        inferred_reason.into(),
        operand_class.into(),
    )
}

fn git_denial_category(args: &[String]) -> String {
    let mut index = 0;
    while let Some(value) = args.get(index) {
        if matches!(value.as_str(), "-C" | "-c" | "--git-dir" | "--work-tree") {
            index = index.saturating_add(2);
            continue;
        }
        if value.starts_with("--git-dir=")
            || value.starts_with("--work-tree=")
            || value.starts_with("-c")
            || value == "--no-pager"
        {
            index = index.saturating_add(1);
            continue;
        }
        return value.trim_start_matches('-').to_ascii_lowercase();
    }
    "unknown".into()
}

fn git_denial_operand_class(args: &[String]) -> String {
    if args.iter().any(|value| value == "-C") {
        return "cwd_override".into();
    }
    if args
        .iter()
        .any(|value| value == "-c" || value.starts_with("-c"))
    {
        return "config_override".into();
    }
    if args.iter().any(|value| {
        matches!(value.as_str(), "--git-dir" | "--work-tree")
            || value.starts_with("--git-dir=")
            || value.starts_with("--work-tree=")
    }) {
        return "repository_override".into();
    }
    if args.iter().any(|value| {
        matches!(
            value.as_str(),
            "--output" | "--ext-diff" | "--textconv" | "--no-index"
        ) || value.starts_with("--output=")
    }) {
        return "write_option".into();
    }
    if args.iter().any(|value| {
        is_credential_path(Path::new(value)) || is_agent_metadata_path(Path::new(value))
    }) {
        return "sensitive_path".into();
    }
    if args.first().is_some_and(|value| {
        matches!(
            value.as_str(),
            "add"
                | "apply"
                | "branch"
                | "checkout"
                | "clean"
                | "commit"
                | "merge"
                | "mv"
                | "rebase"
                | "reset"
                | "restore"
                | "rm"
                | "switch"
                | "tag"
        )
    }) {
        return "mutation".into();
    }
    "option_or_operand".into()
}

fn denial_recovery(
    tool: &str,
    program_family: &str,
    reason_code: &str,
    operand_class: &str,
) -> (&'static str, &'static str) {
    if reason_code.starts_with("shell_")
        || reason_code.contains("multiline")
        || reason_code == "shell_composition_or_expansion_denied"
    {
        return ("split_once", "split_into_single_commands");
    }
    if tool == "read"
        && matches!(
            reason_code,
            "read_path_unverifiable" | "permission_request_unrecognized"
        )
    {
        return ("simplify_once", "correct_read_path_once");
    }
    if program_family == "git"
        && matches!(
            operand_class,
            "cwd_override" | "config_override" | "repository_override"
        )
    {
        return ("simplify_once", "remove_denied_option_once");
    }
    if reason_code.contains("option_not_allowlisted")
        && !matches!(
            operand_class,
            "write_option" | "mutation" | "sensitive_path"
        )
    {
        return ("simplify_once", "remove_denied_option_once");
    }
    if matches!(
        reason_code,
        "command_not_allowlisted" | "program_not_allowlisted"
    ) {
        if matches!(
            program_family,
            "curl" | "wget" | "ssh" | "scp" | "sftp" | "nc" | "ncat" | "telnet"
        ) {
            return ("do_not_retry_equivalent", "stop_evidence_path");
        }
        if matches!(
            program_family,
            "cargo"
                | "rustc"
                | "npm"
                | "npx"
                | "pnpm"
                | "yarn"
                | "bun"
                | "docker"
                | "make"
                | "cmake"
                | "pytest"
                | "python"
                | "python3"
                | "go"
        ) {
            return ("use_prepared_inputs", "use_prepared_inputs");
        }
        return ("use_read", "use_read_or_prepared_inputs");
    }
    if reason_code.contains("credential")
        || reason_code.contains("sensitive")
        || reason_code.contains("secret")
        || reason_code.contains("network")
        || reason_code.contains("write")
        || reason_code.contains("mutation")
        || reason_code.contains("outside")
        || reason_code.contains("escape")
        || reason_code.contains("protected")
        || reason_code.contains("agent_metadata")
        || reason_code == "external_policy_denied"
        || matches!(
            operand_class,
            "write_option" | "mutation" | "sensitive_path" | "outside_scope"
        )
    {
        return ("do_not_retry_equivalent", "stop_evidence_path");
    }
    ("use_read", "use_read_or_prepared_inputs")
}
