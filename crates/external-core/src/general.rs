use crate::{PolicyCapabilities, PolicyLauncher, PreparationError, PreparationResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub const GENERAL_TASK_SCHEMA: &str = "zcode-general-task/v1";
const MAX_PROMPT_BYTES: usize = 256 * 1024;
static SUBMISSION_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    ReadOnly,
    WorkspaceWrite,
}

impl Default for AccessMode {
    fn default() -> Self {
        Self::WorkspaceWrite
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Build,
    Edit,
    Plan,
    Yolo,
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Build
    }
}

impl PermissionMode {
    pub fn access_mode(self) -> AccessMode {
        match self {
            Self::Plan => AccessMode::ReadOnly,
            Self::Build | Self::Edit | Self::Yolo => AccessMode::WorkspaceWrite,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralTaskManifest {
    pub schema: String,
    pub agent_id: String,
    pub repository: PathBuf,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    pub prompt: String,
    #[serde(default)]
    pub write_manifest: Vec<PathBuf>,
}

/// Filesystem-only identity for the caller-owned workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedWorkspace {
    pub path: PathBuf,
    pub scratch_root: PathBuf,
}

/// Immutable admission facts persisted alongside the task preparation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionIdentity {
    pub agent: String,
    pub config_revision: u64,
    pub adapter_version: String,
    pub model: Option<String>,
    pub model_source: String,
    /// Reasoning-effort token admitted at submit time. Omitted entirely for
    /// legacy rows and spawns without a selection: the field carries both
    /// `#[serde(default)]` (old persisted rows decode with None) and
    /// `skip_serializing_if` (round-tripping those rows keeps the persisted
    /// bytes free of a synthetic `"effort": null`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedGeneralTask {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionIdentity>,
    pub schema: String,
    pub agent_id: String,
    pub repository: PathBuf,
    pub workspace: PreparedWorkspace,
    pub permission_mode: PermissionMode,
    pub write_manifest: Vec<PathBuf>,
}

impl PreparedGeneralTask {
    pub fn with_admission(mut self, identity: AdmissionIdentity) -> PreparationResult<Self> {
        self.admission = Some(identity);
        Ok(self)
    }

    pub fn launcher(&self) -> PreparationResult<PolicyLauncher> {
        self.build_launcher(Vec::new())
    }

    pub fn resume_launcher(&self) -> PreparationResult<PolicyLauncher> {
        fs::create_dir_all(&self.workspace.scratch_root)?;
        self.build_launcher(Vec::new())
    }

    pub fn final_tree_launcher(&self) -> PreparationResult<PolicyLauncher> {
        self.launcher()
    }

    fn build_launcher(&self, inputs: Vec<PathBuf>) -> PreparationResult<PolicyLauncher> {
        PolicyLauncher::for_general(
            self.workspace.path.clone(),
            self.workspace.scratch_root.clone(),
            self.workspace.scratch_root.join("result.json"),
            inputs,
            PolicyCapabilities::default(),
            self.permission_mode.access_mode(),
            self.write_manifest.clone(),
        )
    }
}

pub struct GeneralTaskPreparer;

impl GeneralTaskPreparer {
    pub fn new(_unused_roots: Vec<PathBuf>) -> PreparationResult<Self> {
        Ok(Self)
    }

    pub fn prepare(
        &self,
        manifest: &GeneralTaskManifest,
    ) -> PreparationResult<PreparedGeneralTask> {
        self.prepare_direct_submission(manifest)
    }

    pub fn prepare_submission(
        &self,
        manifest: &GeneralTaskManifest,
    ) -> PreparationResult<PreparedGeneralTask> {
        self.prepare_direct_submission(manifest)
    }

    pub fn prepare_direct_submission(
        &self,
        manifest: &GeneralTaskManifest,
    ) -> PreparationResult<PreparedGeneralTask> {
        validate_manifest(manifest)?;
        let repository = canonical_general_repository(&manifest.repository)?;
        let (agent_id, scratch_root) = if is_public_task_id(&manifest.agent_id) {
            let (_, scratch) = allocate_submission(&repository, &manifest.agent_id)?;
            (manifest.agent_id.clone(), scratch)
        } else {
            allocate_submission(&repository, &manifest.agent_id)?
        };
        let permission_mode = manifest.permission_mode;
        let write_manifest = manifest
            .write_manifest
            .iter()
            .map(|path| confined_relative(path))
            .collect::<PreparationResult<Vec<_>>>()?;
        let write_manifest = if permission_mode != PermissionMode::Plan && write_manifest.is_empty()
        {
            vec![PathBuf::from(".")]
        } else {
            write_manifest
        };
        validate_write_scope(manifest.permission_mode, &write_manifest)?;

        let prepared = PreparedGeneralTask {
            admission: None,
            schema: manifest.schema.clone(),
            agent_id,
            repository: repository.clone(),
            workspace: PreparedWorkspace {
                path: repository,
                scratch_root,
            },
            permission_mode,
            write_manifest,
        };
        Ok(prepared)
    }
}

fn allocate_submission(
    repository: &Path,
    manifest_agent_id: &str,
) -> PreparationResult<(String, PathBuf)> {
    let parent = std::env::temp_dir().join("external-subagent");
    fs::create_dir_all(&parent)?;
    for _ in 0..32 {
        let nonce = SUBMISSION_NONCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let mut entropy = [0u8; 16];
        if let Ok(mut source) = fs::File::open("/dev/urandom") {
            let _ = source.read_exact(&mut entropy);
        }
        let scratch_name = format!(
            "{}-{}",
            manifest_agent_id,
            hash(&serde_json::to_vec(&(
                repository,
                manifest_agent_id,
                timestamp,
                nonce,
                entropy
            ))?)
        );
        let scratch_root = parent.join(&scratch_name);
        match fs::create_dir(&scratch_root) {
            Ok(()) => {
                return Ok((
                    manifest_agent_id.to_owned(),
                    fs::canonicalize(scratch_root)?,
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(PreparationError::InvalidManifest(
        "could not allocate a unique task scratch directory".into(),
    ))
}

fn is_public_task_id(value: &str) -> bool {
    value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompletionOutcome {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    RuntimeLost,
    ResultInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralCompletion {
    pub outcome: CompletionOutcome,
    pub reason_code: Option<String>,
    pub summary: String,
    pub residual_gaps: Vec<String>,
    pub cleaned: bool,
}

pub struct GeneralFinalizer;

impl GeneralFinalizer {
    pub fn retry_cleanup(
        _prepared: &PreparedGeneralTask,
        persisted: &GeneralCompletion,
    ) -> GeneralCompletion {
        let mut completion = persisted.clone();
        completion.cleaned = true;
        completion
    }

    pub fn finalize(
        _prepared: &PreparedGeneralTask,
        requested: CompletionOutcome,
    ) -> GeneralCompletion {
        GeneralCompletion {
            outcome: requested,
            reason_code: None,
            summary: String::new(),
            residual_gaps: Vec::new(),
            cleaned: true,
        }
    }

    pub fn finish_cleanup(
        _prepared: &PreparedGeneralTask,
        mut completion: GeneralCompletion,
    ) -> GeneralCompletion {
        completion.cleaned = true;
        completion
    }
}

pub fn canonical_general_repository(path: &Path) -> PreparationResult<PathBuf> {
    if !path.is_absolute() {
        return Err(PreparationError::InvalidPath {
            path: path.into(),
            reason: "repository must be absolute".into(),
        });
    }
    let canonical = fs::canonicalize(path)?;
    if !canonical.is_dir() {
        return Err(PreparationError::InvalidPath {
            path: canonical,
            reason: "workspace is not a directory".into(),
        });
    }
    Ok(canonical)
}

fn validate_manifest(manifest: &GeneralTaskManifest) -> PreparationResult<()> {
    if manifest.schema != GENERAL_TASK_SCHEMA {
        return Err(PreparationError::InvalidManifest(
            "unsupported general task schema".into(),
        ));
    }
    if manifest.agent_id.is_empty()
        || manifest.agent_id.len() > 256
        || !manifest.agent_id.bytes().all(identifier_byte)
        || manifest.prompt.trim().is_empty()
        || manifest.prompt.len() > MAX_PROMPT_BYTES
        || manifest.prompt.contains('\0')
    {
        return Err(PreparationError::InvalidManifest(
            "invalid task identity or prompt".into(),
        ));
    }
    let mut unique = std::collections::HashSet::new();
    if manifest
        .write_manifest
        .iter()
        .any(|path| !unique.insert(path))
    {
        return Err(PreparationError::InvalidManifest(
            "write_manifest contains duplicates".into(),
        ));
    }
    Ok(())
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
}

fn confined_relative(path: &Path) -> PreparationResult<PathBuf> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(PreparationError::InvalidPath {
            path: path.into(),
            reason: "path must be repository-relative".into(),
        });
    }
    Ok(path.components().collect())
}

fn validate_write_scope(
    permission_mode: PermissionMode,
    write_manifest: &[PathBuf],
) -> PreparationResult<()> {
    if permission_mode == PermissionMode::Plan && !write_manifest.is_empty() {
        return Err(PreparationError::InvalidManifest(
            "plan mode does not accept a write manifest".into(),
        ));
    }
    for path in write_manifest {
        if path.components().any(|component| {
            matches!(component, Component::Normal(name) if name == ".git" || name == ".gitmodules")
        }) {
            return Err(PreparationError::Policy(
                "protected repository metadata path".into(),
            ));
        }
    }
    Ok(())
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::{GeneralTaskManifest, GeneralTaskPreparer, PermissionMode, GENERAL_TASK_SCHEMA};
    use serde_json::json;

    #[test]
    fn unknown_manifest_fields_are_rejected() {
        for field in ["legacy_field", "unsupported_option"] {
            let mut value = json!({
                "schema": "zcode-general-task/v1",
                "agent_id": "test",
                "repository": "/tmp",
                "permission_mode": "plan",
                "prompt": "inspect",
                "write_manifest": []
            });
            value
                .as_object_mut()
                .unwrap()
                .insert(field.into(), json!(null));
            assert!(
                serde_json::from_value::<GeneralTaskManifest>(value).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn strict_plan_admission_rejects_non_empty_write_manifest() {
        let error = super::validate_write_scope(
            PermissionMode::Plan,
            &[std::path::PathBuf::from("src/main.rs")],
        )
        .expect_err("plan admission must reject writes");
        assert!(error.to_string().contains("plan mode does not accept"));
    }

    #[test]
    fn permission_mode_maps_to_internal_policy() {
        assert_eq!(
            PermissionMode::Plan.access_mode(),
            super::AccessMode::ReadOnly
        );
        assert_eq!(
            PermissionMode::Edit.access_mode(),
            super::AccessMode::WorkspaceWrite
        );
    }

    #[test]
    fn each_submission_allocates_a_fresh_identity_and_scratch_root() {
        let repository = tempfile::tempdir().expect("repository");
        let manifest = GeneralTaskManifest {
            schema: GENERAL_TASK_SCHEMA.into(),
            agent_id: "daemon-prepared".into(),
            repository: repository.path().to_path_buf(),
            permission_mode: PermissionMode::Plan,
            prompt: "inspect".into(),
            write_manifest: Vec::new(),
        };
        let preparer = GeneralTaskPreparer::new(Vec::new()).expect("preparer");
        let first = preparer.prepare(&manifest).expect("first submission");
        let second = preparer.prepare(&manifest).expect("second submission");
        assert_eq!(first.agent_id, "daemon-prepared");
        assert_ne!(first.workspace.scratch_root, second.workspace.scratch_root);
        assert!(first.workspace.scratch_root.exists());
        assert!(second.workspace.scratch_root.exists());
        assert!(!first.workspace.scratch_root.join("prompt.txt").exists());
    }

    #[test]
    fn allocated_public_id_is_preserved_in_runtime_identity() {
        let repository = tempfile::tempdir().expect("repository");
        let manifest = GeneralTaskManifest {
            schema: GENERAL_TASK_SCHEMA.into(),
            agent_id: "10000000".into(),
            repository: repository.path().to_path_buf(),
            permission_mode: PermissionMode::Plan,
            prompt: "inspect".into(),
            write_manifest: Vec::new(),
        };
        let prepared = GeneralTaskPreparer::new(Vec::new())
            .unwrap()
            .prepare(&manifest)
            .unwrap();
        assert_eq!(prepared.agent_id, "10000000");
        assert!(prepared
            .workspace
            .scratch_root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("10000000-"));
        assert!(prepared.launcher().is_ok());
    }

    #[test]
    fn prepared_task_contains_no_adapter_selected_runtime_deadlines() {
        let repository = tempfile::tempdir().expect("repository");
        let manifest = GeneralTaskManifest {
            schema: GENERAL_TASK_SCHEMA.into(),
            agent_id: "no-runtime-deadline".into(),
            repository: repository.path().to_path_buf(),
            permission_mode: PermissionMode::Plan,
            prompt: "wait for the official runtime".into(),
            write_manifest: Vec::new(),
        };
        let prepared = GeneralTaskPreparer::new(Vec::new())
            .unwrap()
            .prepare(&manifest)
            .unwrap();
        let encoded = serde_json::to_string(&prepared).unwrap();
        for forbidden in [
            "runtime_activity_idle_timeout_ms",
            "model_stream_idle_timeout_ms",
            "tool_call_timeout_ms",
            "input_wait_timeout_ms",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "prepared task leaked {forbidden}"
            );
        }
    }
}
