use crate::{PreparationError, PreparationResult};
use std::path::{Component, Path, PathBuf};

pub(super) fn lexical_confined_path(root: &Path, value: &Path) -> PreparationResult<PathBuf> {
    if value.is_absolute()
        || value
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(PreparationError::InvalidPath {
            path: value.to_path_buf(),
            reason: "path must be repository-relative and confined".into(),
        });
    }
    Ok(root.join(value))
}

pub(super) fn protected_worktree_path(worktree: &Path, target: &Path) -> bool {
    let Ok(relative) = target.strip_prefix(worktree) else {
        return false;
    };
    relative.components().any(|component| {
        component.as_os_str() == ".git"
            || component.as_os_str() == ".agent-work"
            || component.as_os_str() == ".gitmodules"
    })
}

pub(crate) fn is_credential_path(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        matches!(
            value.as_str(),
            ".ssh"
                | ".git"
                | ".env"
                | ".env.local"
                | ".env.production"
                | ".aws"
                | ".gnupg"
                | ".netrc"
                | ".npmrc"
                | "credentials"
                | "credentials.json"
                | "auth.json"
                | "id_rsa"
                | "id_ed25519"
                | "known_hosts"
                | "authorized_keys"
        ) || value.contains("access_token")
            || value.contains("api_key")
            || value.contains("secret_key")
            || value.ends_with(".pem")
            || value.ends_with(".key")
            || value.ends_with(".p12")
            || value.ends_with(".pfx")
            || value.ends_with(".jks")
            || value.ends_with(".kdbx")
    })
}

pub(crate) fn is_agent_metadata_path(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    normalized
        .split('/')
        .any(|component| component == ".agent-work")
}
