mod general;
mod policy;

pub use general::PermissionMode;
pub use policy::AGENT_BASH_COMMAND_FAMILIES;

use std::{fmt, path::PathBuf};

const AGENT_BASH_POLICY_VERSION: &str = "zcode-agent-bash/v1.0.0";

pub use general::{
    canonical_general_repository, AccessMode, AdmissionIdentity, CompletionOutcome,
    GeneralCompletion, GeneralFinalizer, GeneralTaskManifest, GeneralTaskPreparer,
    PreparedGeneralTask, PreparedWorkspace, GENERAL_TASK_SCHEMA,
};

pub use policy::{
    ExternalDecision, PermissionDecision, PermissionRequest, PolicyCapabilities, PolicyLauncher,
    SandboxEnforcement, ValidatedPermissionDenial,
};

#[derive(Debug)]
pub enum PreparationError {
    InvalidManifest(String),
    InvalidPath { path: PathBuf, reason: String },
    PathEscape { path: PathBuf, root: PathBuf },
    SymlinkInput(PathBuf),
    MissingInput(PathBuf),
    ForbiddenInput(PathBuf),
    CredentialInput(PathBuf),
    Policy(String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl PreparationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidManifest(_) => "INVALID_MANIFEST",
            Self::InvalidPath { .. } => "INVALID_PATH",
            Self::PathEscape { .. } => "PATH_ESCAPE",
            Self::SymlinkInput(_) => "SYMLINK_INPUT",
            Self::MissingInput(_) => "MISSING_INPUT",
            Self::ForbiddenInput(_) => "FORBIDDEN_INPUT",
            Self::CredentialInput(_) => "CREDENTIAL_INPUT",
            Self::Policy(_) => "POLICY_DENIED",
            Self::Io(_) => "IO_ERROR",
            Self::Json(_) => "JSON_ERROR",
        }
    }
}

impl fmt::Display for PreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidManifest(message) => write!(formatter, "invalid manifest: {message}"),
            Self::InvalidPath { path, reason } => {
                write!(formatter, "invalid path {}: {reason}", path.display())
            }
            Self::PathEscape { path, root } => write!(
                formatter,
                "path {} escapes allowed root {}",
                path.display(),
                root.display()
            ),
            Self::SymlinkInput(path) => {
                write!(formatter, "symlink input is forbidden: {}", path.display())
            }
            Self::MissingInput(path) => write!(formatter, "input is missing: {}", path.display()),
            Self::ForbiddenInput(path) => {
                write!(
                    formatter,
                    "forbidden agent metadata input is forbidden: {}",
                    path.display()
                )
            }
            Self::CredentialInput(path) => {
                write!(
                    formatter,
                    "credential input is forbidden: {}",
                    path.display()
                )
            }
            Self::Policy(message) => write!(formatter, "policy denied request: {message}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
        }
    }
}

impl std::error::Error for PreparationError {}

impl From<std::io::Error> for PreparationError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for PreparationError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub type PreparationResult<T> = Result<T, PreparationError>;
