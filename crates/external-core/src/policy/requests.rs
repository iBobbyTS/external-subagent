use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRequest {
    Read(PathBuf),
    Write(PathBuf),
    Edit(PathBuf),
    Delete(PathBuf),
    Move {
        source: PathBuf,
        destination: PathBuf,
    },
    Execute {
        program: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
    },
    Network(String),
    GitRefMutation,
    CredentialRead(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    pub allowed: bool,
    pub reason: &'static str,
}

pub(super) fn zcode_file_path(input: &serde_json::Value) -> Option<&str> {
    match (input.get("file_path"), input.get("path")) {
        (Some(file_path), None) => file_path.as_str(),
        (None, Some(path)) => path.as_str(),
        (Some(file_path), Some(path)) => file_path
            .as_str()
            .zip(path.as_str())
            .and_then(|(file_path, path)| (file_path == path).then_some(file_path)),
        (None, None) => None,
    }
}
