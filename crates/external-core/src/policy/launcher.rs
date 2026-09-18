use crate::{AccessMode, PreparationError, PreparationResult};
use std::{
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use super::capabilities::PolicyCapabilities;
use super::commands::{
    agent_bash_path_operands, agent_bash_program_allowed, tokenize_agent_bash, valid_agent_git,
    valid_agent_sed,
};
use super::denial::{permission_denial_semantics, ValidatedPermissionDenial};
use super::paths::{lexical_confined_path, protected_worktree_path};
use super::requests::{zcode_file_path, ExternalDecision, PermissionDecision, PermissionRequest};
use super::{is_agent_metadata_path, is_credential_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Existing,
    Missing,
}

#[derive(Debug, Clone)]
pub struct PolicyLauncher {
    worktree: PathBuf,
    scratch_root: PathBuf,
    report_target: PathBuf,
    readable_inputs: Vec<PathBuf>,
    network_allowed: bool,
    capabilities: PolicyCapabilities,
    access_mode: AccessMode,
    tracked_write_roots: Vec<PathBuf>,
    interactive_bash: bool,
}

impl PolicyLauncher {
    pub fn new(
        worktree: PathBuf,
        scratch_root: PathBuf,
        report_target: PathBuf,
        readable_inputs: Vec<PathBuf>,
        network_allowed: bool,
        capabilities: PolicyCapabilities,
    ) -> PreparationResult<Self> {
        let worktree = fs::canonicalize(worktree)?;
        let scratch_root = fs::canonicalize(scratch_root)?;
        let report_parent =
            report_target
                .parent()
                .ok_or_else(|| PreparationError::InvalidPath {
                    path: report_target.clone(),
                    reason: "report target has no parent".into(),
                })?;
        let report_parent = fs::canonicalize(report_parent)?;
        let report_target = report_parent.join(report_target.file_name().ok_or_else(|| {
            PreparationError::InvalidPath {
                path: report_target.clone(),
                reason: "report target has no file name".into(),
            }
        })?);
        let readable_inputs = readable_inputs
            .into_iter()
            .map(|root| fs::canonicalize(&root).map_err(PreparationError::Io))
            .collect::<PreparationResult<Vec<_>>>()?;
        Ok(Self {
            worktree,
            scratch_root,
            report_target,
            readable_inputs,
            network_allowed,
            capabilities,
            access_mode: AccessMode::ReadOnly,
            tracked_write_roots: Vec::new(),
            interactive_bash: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn for_general(
        worktree: PathBuf,
        scratch_root: PathBuf,
        result_target: PathBuf,
        readable_inputs: Vec<PathBuf>,
        capabilities: PolicyCapabilities,
        access_mode: AccessMode,
        mut tracked_write_roots: Vec<PathBuf>,
    ) -> PreparationResult<Self> {
        let mut launcher = Self::new(
            worktree,
            scratch_root,
            result_target,
            readable_inputs,
            false,
            capabilities,
        )?;
        launcher.access_mode = access_mode;
        if access_mode == AccessMode::WorkspaceWrite {
            for root in tracked_write_roots.iter_mut() {
                *root = lexical_confined_path(&launcher.worktree, root)?;
                if protected_worktree_path(&launcher.worktree, root) {
                    return Err(PreparationError::Policy(
                        "tracked write root targets protected metadata".into(),
                    ));
                }
            }
        } else if !tracked_write_roots.is_empty() {
            return Err(PreparationError::Policy(
                "read-only access cannot define tracked write roots".into(),
            ));
        }
        launcher.tracked_write_roots = tracked_write_roots;
        Ok(launcher)
    }

    pub fn capabilities(&self) -> &PolicyCapabilities {
        &self.capabilities
    }

    pub fn set_interactive_bash(&mut self, enabled: bool) {
        self.interactive_bash = enabled;
    }

    /// Produces policy identity from the daemon's own closed decision result.
    /// An external denial deliberately uses one stable fallback reason rather
    /// than treating caller text as policy metadata.
    pub fn validated_zcode_denial(
        &self,
        params: &serde_json::Value,
        external: ExternalDecision,
    ) -> Option<ValidatedPermissionDenial> {
        self.decide_zcode_permission_validated(params, external).1
    }

    pub fn decide_zcode_permission_validated(
        &self,
        params: &serde_json::Value,
        external: ExternalDecision,
    ) -> (PermissionDecision, Option<ValidatedPermissionDenial>) {
        let decision = self.decide_zcode_permission(params, external);
        let denial = (!decision.allowed)
            .then(|| {
                let reason = if external == ExternalDecision::Deny {
                    "external_policy_denied"
                } else {
                    decision.reason
                };
                permission_denial_semantics(params, reason).map(ValidatedPermissionDenial)
            })
            .flatten();
        (decision, denial)
    }

    pub fn external_zcode_denial(params: &serde_json::Value) -> Option<ValidatedPermissionDenial> {
        permission_denial_semantics(params, "external_policy_denied").map(ValidatedPermissionDenial)
    }

    pub fn decide(
        &self,
        request: &PermissionRequest,
        external: ExternalDecision,
    ) -> PermissionDecision {
        if let Some(reason) = self.hard_deny_reason(request) {
            return PermissionDecision {
                allowed: false,
                reason,
            };
        }
        if external == ExternalDecision::Deny {
            return PermissionDecision {
                allowed: false,
                reason: "external_policy_denied",
            };
        }
        PermissionDecision {
            allowed: true,
            reason: "allowed_by_bounded_policy",
        }
    }

    pub fn decide_zcode_permission(
        &self,
        params: &serde_json::Value,
        external: ExternalDecision,
    ) -> PermissionDecision {
        let Some(tool_name) = params.get("toolName").and_then(serde_json::Value::as_str) else {
            return PermissionDecision {
                allowed: false,
                reason: "permission_tool_missing",
            };
        };
        let input = params.get("input").unwrap_or(&serde_json::Value::Null);
        if tool_name == "Bash" {
            if self.access_mode != AccessMode::ReadOnly {
                if self.interactive_bash {
                    return if external == ExternalDecision::Deny {
                        PermissionDecision {
                            allowed: false,
                            reason: "external_policy_denied",
                        }
                    } else {
                        PermissionDecision {
                            allowed: true,
                            reason: "interactive_permission_required",
                        }
                    };
                }
                return PermissionDecision {
                    allowed: false,
                    reason: "permission_request_unrecognized",
                };
            }
            return self.decide_agent_bash(input, external);
        }
        let request = match tool_name.to_ascii_lowercase().as_str() {
            "read" => zcode_file_path(input)
                .map(|path| PermissionRequest::Read(self.resolve_job_path(path))),
            "grep" | "glob" => input
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(|path| PermissionRequest::Read(self.resolve_job_path(path))),
            "write" => zcode_file_path(input)
                .map(|path| PermissionRequest::Write(self.resolve_job_path(path))),
            "edit" => zcode_file_path(input)
                .map(|path| PermissionRequest::Edit(self.resolve_job_path(path))),
            "delete" => zcode_file_path(input)
                .map(|path| PermissionRequest::Delete(self.resolve_job_path(path))),
            "move" => {
                let source = input
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .map(|path| self.resolve_job_path(path));
                let destination = input
                    .get("destination")
                    .and_then(serde_json::Value::as_str)
                    .map(|path| self.resolve_job_path(path));
                match (source, destination) {
                    (Some(source), Some(destination)) => Some(PermissionRequest::Move {
                        source,
                        destination,
                    }),
                    _ => None,
                }
            }
            "network" => input
                .get("target")
                .and_then(serde_json::Value::as_str)
                .map(|target| PermissionRequest::Network(target.into())),
            "git_ref_mutation" => Some(PermissionRequest::GitRefMutation),
            "execute" | "terminal" => {
                let program = input
                    .get("program")
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from);
                let args = input
                    .get("args")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|values| {
                        values
                            .iter()
                            .map(|value| value.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()
                    });
                let cwd = input
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .map(|path| self.resolve_job_path(path));
                match (program, args, cwd) {
                    (Some(program), Some(args), Some(cwd)) => {
                        Some(PermissionRequest::Execute { program, args, cwd })
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        match request {
            Some(request) => self.decide(&request, external),
            None => PermissionDecision {
                allowed: false,
                reason: "permission_request_unrecognized",
            },
        }
    }

    fn decide_agent_bash(
        &self,
        input: &serde_json::Value,
        external: ExternalDecision,
    ) -> PermissionDecision {
        if external == ExternalDecision::Deny {
            return PermissionDecision {
                allowed: false,
                reason: "external_policy_denied",
            };
        }
        let Some(command) = input.get("command").and_then(serde_json::Value::as_str) else {
            return PermissionDecision {
                allowed: false,
                reason: "bash_command_missing",
            };
        };
        let cwd = input
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.worktree.clone());
        let cwd = match fs::canonicalize(&cwd) {
            Ok(path) if self.is_agent_root(&path) => path,
            _ => {
                return PermissionDecision {
                    allowed: false,
                    reason: "cwd_outside_agent_roots",
                }
            }
        };
        let Some(argv) = tokenize_agent_bash(command) else {
            return PermissionDecision {
                allowed: false,
                reason: "shell_composition_or_expansion_denied",
            };
        };
        let program = argv.first().map(String::as_str).unwrap_or_default();
        if !agent_bash_program_allowed(program) {
            return PermissionDecision {
                allowed: false,
                reason: "command_not_allowlisted",
            };
        }
        if program == "sed" && !valid_agent_sed(&argv[1..]) {
            return PermissionDecision {
                allowed: false,
                reason: "sed_form_not_bounded",
            };
        }
        if program == "git"
            && argv[2..].iter().any(|arg| {
                is_credential_path(Path::new(arg)) || is_agent_metadata_path(Path::new(arg))
            })
        {
            return PermissionDecision {
                allowed: false,
                reason: "git_sensitive_path_denied",
            };
        }
        if program == "git" && !valid_agent_git(&argv[1..]) {
            return PermissionDecision {
                allowed: false,
                reason: "git_option_or_mutation_denied",
            };
        }
        let path_args = match agent_bash_path_operands(program, &argv[1..]) {
            Some(paths) => paths,
            None => {
                return PermissionDecision {
                    allowed: false,
                    reason: "command_option_not_allowlisted",
                }
            }
        };
        let require_file = matches!(
            program,
            "cat" | "wc" | "head" | "tail" | "sed" | "shasum" | "cksum"
        );
        if path_args.iter().any(|path| path == "-") {
            return PermissionDecision {
                allowed: false,
                reason: "stdin_input_denied",
            };
        }
        if !self.agent_paths_confined(&path_args, &cwd, require_file) {
            return PermissionDecision {
                allowed: false,
                reason: "path_outside_agent_roots",
            };
        }
        PermissionDecision {
            allowed: true,
            reason: "agent_bash_allowlisted",
        }
    }

    fn is_agent_root(&self, path: &Path) -> bool {
        path.starts_with(&self.worktree)
            || self
                .readable_inputs
                .iter()
                .any(|root| path.starts_with(root))
    }

    fn enters_agent_metadata(&self, path: &Path) -> bool {
        std::iter::once(&self.worktree)
            .chain(self.readable_inputs.iter())
            .filter_map(|root| path.strip_prefix(root).ok())
            .any(is_agent_metadata_path)
    }

    fn agent_paths_confined(&self, args: &[String], cwd: &Path, require_file: bool) -> bool {
        args.iter().all(|arg| {
            let path = Path::new(arg);
            if arg == "-"
                || arg.starts_with('~')
                || path
                    .components()
                    .any(|component| component == Component::ParentDir)
                || is_credential_path(path)
                || is_agent_metadata_path(path)
            {
                return false;
            }
            let candidate = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            let Ok(real) = fs::canonicalize(candidate) else {
                return false;
            };
            if !self.is_agent_root(&real)
                || is_credential_path(&real)
                || self.enters_agent_metadata(&real)
            {
                return false;
            }
            !require_file || real.is_file()
        })
    }

    fn hard_deny_reason(&self, request: &PermissionRequest) -> Option<&'static str> {
        match request {
            PermissionRequest::Network(_) if !self.network_allowed => {
                Some("network_not_enforced_and_request_denied")
            }
            PermissionRequest::Network(_) => None,
            PermissionRequest::GitRefMutation => Some("git_ref_mutation_denied"),
            PermissionRequest::CredentialRead(_) => Some("credential_read_denied"),
            PermissionRequest::Read(path) => {
                if is_credential_path(path) {
                    return Some("credential_read_denied");
                }
                let Ok(path) = fs::canonicalize(path) else {
                    return Some("read_path_unverifiable");
                };
                if self.enters_agent_metadata(&path) {
                    return Some("agent_metadata_read_denied");
                }
                if path.starts_with(&self.worktree)
                    || self
                        .readable_inputs
                        .iter()
                        .any(|root| path.starts_with(root))
                {
                    None
                } else {
                    Some("read_path_escape_denied")
                }
            }
            PermissionRequest::Write(path) => self.followed_write_denial(path, false),
            PermissionRequest::Edit(path) => self.followed_write_denial(path, true),
            PermissionRequest::Delete(path) => self.lexical_entry_denial(path, true),
            PermissionRequest::Move {
                source,
                destination,
            } => self
                .lexical_entry_denial(source, true)
                .or_else(|| self.lexical_entry_denial(destination, false)),
            PermissionRequest::Execute { .. } => None,
        }
    }

    fn resolve_job_path(&self, value: &str) -> PathBuf {
        let path = Path::new(value);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.worktree.join(path)
        }
    }

    fn followed_write_denial(&self, path: &Path, must_exist: bool) -> Option<&'static str> {
        match self.lexical_entry_state(path, must_exist) {
            Ok(EntryState::Existing) => self.canonical_write_denial(path),
            Ok(EntryState::Missing) => None,
            Err(reason) => Some(reason),
        }
    }

    fn lexical_entry_denial(&self, path: &Path, must_exist: bool) -> Option<&'static str> {
        self.lexical_entry_state(path, must_exist).err()
    }

    fn lexical_entry_state(
        &self,
        path: &Path,
        must_exist: bool,
    ) -> Result<EntryState, &'static str> {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err("write_path_unverifiable");
        }
        if let Some(reason) = self.resolved_write_denial(path) {
            return Err(reason);
        }
        match fs::symlink_metadata(path) {
            Ok(_) => {
                let candidate = self.canonical_parent_entry(path)?;
                if let Some(reason) = self.resolved_write_denial(&candidate) {
                    Err(reason)
                } else {
                    Ok(EntryState::Existing)
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound && !must_exist => {
                let Some(candidate) = self.resolve_nonexistent_entry(path) else {
                    return Err("write_path_unverifiable");
                };
                if let Some(reason) = self.resolved_write_denial(&candidate) {
                    Err(reason)
                } else {
                    Ok(EntryState::Missing)
                }
            }
            Err(_) => Err("write_path_unverifiable"),
        }
    }

    fn canonical_parent_entry(&self, path: &Path) -> Result<PathBuf, &'static str> {
        let parent = path.parent().ok_or("write_path_unverifiable")?;
        let filename = path.file_name().ok_or("write_path_unverifiable")?;
        let canonical_parent = fs::canonicalize(parent).map_err(|_| "write_path_unverifiable")?;
        if !canonical_parent.is_dir() {
            return Err("write_path_unverifiable");
        }
        Ok(canonical_parent.join(filename))
    }

    fn canonical_write_denial(&self, path: &Path) -> Option<&'static str> {
        let Ok(canonical) = fs::canonicalize(path) else {
            return Some("write_path_unverifiable");
        };
        self.resolved_write_denial(&canonical)
    }

    fn resolve_nonexistent_entry(&self, path: &Path) -> Option<PathBuf> {
        let mut current = path.to_path_buf();
        let mut missing = Vec::<OsString>::new();
        loop {
            match fs::symlink_metadata(&current) {
                Ok(_) => {
                    let canonical = fs::canonicalize(&current).ok()?;
                    if !canonical.is_dir() {
                        return None;
                    }
                    let mut candidate = canonical;
                    for component in missing.iter().rev() {
                        candidate.push(component);
                    }
                    return Some(candidate);
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    missing.push(current.file_name()?.to_os_string());
                    if !current.pop() {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }

    fn resolved_write_denial(&self, target: &Path) -> Option<&'static str> {
        let Some(report_root) = self.report_target.parent() else {
            return Some("write_path_unverifiable");
        };
        if target.starts_with(&self.worktree) {
            if protected_worktree_path(&self.worktree, target) {
                return Some("protected_worktree_metadata_denied");
            }
            return match self.access_mode {
                AccessMode::WorkspaceWrite
                    if self
                        .tracked_write_roots
                        .iter()
                        .any(|root| target.starts_with(root)) =>
                {
                    None
                }
                AccessMode::WorkspaceWrite => Some("tracked_path_not_allowlisted"),
                AccessMode::ReadOnly => Some("tracked_writes_denied_for_access_mode"),
            };
        }
        if target.starts_with(report_root) {
            return Some("daemon_result_root_denied");
        }
        if target == self.scratch_root
            || target == report_root
            || (!target.starts_with(&self.scratch_root) && !target.starts_with(report_root))
        {
            Some("write_outside_result_roots_denied")
        } else {
            None
        }
    }
}
