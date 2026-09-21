//! Gated DSH factory: whether the adapter may spawn at all, and the
//! strict-plan/manifest-build launch resolution against the
//! `external-agent-dsh` profile contracts.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use external_store::TaskRecord;

use super::owner::DshRuntimeOwner;
use crate::{task_route, LifecycleSink, ManagedRuntime, RuntimeFactory};

/// Whether the DSH factory may spawn adapter processes at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DshSpawnGate {
    /// S04.A production state: the adapter exists but spawn is refused.
    Closed,
    /// Production launch after the managed strict-plan preflight succeeds.
    Enabled,
    /// Controlled test harness only; never constructed by the production
    /// composition root.
    #[cfg(test)]
    TestHarness,
}

/// Factory for DSH ACP runtimes. Closed by default: production routing can
/// register the factory without enabling DSH spawn support.
pub struct DshRuntimeFactory {
    gate: DshSpawnGate,
    #[cfg(test)]
    executable: Option<PathBuf>,
}

impl DshRuntimeFactory {
    pub fn closed() -> Self {
        Self {
            gate: DshSpawnGate::Closed,
            #[cfg(test)]
            executable: None,
        }
    }

    /// Construct the production factory.  The strict patch is resolved from
    /// an explicit environment override so packaged binaries cannot depend on
    /// their current working directory.
    pub fn enabled() -> Self {
        Self {
            gate: DshSpawnGate::Enabled,
            #[cfg(test)]
            executable: None,
        }
    }

    #[cfg(test)]
    pub fn test_harness(executable: Option<PathBuf>) -> Self {
        Self {
            gate: DshSpawnGate::TestHarness,
            executable,
        }
    }
}

/// Is this the workspace-root manifest `external-core` derives for any
/// non-plan task with a caller-empty write manifest? An explicit `["."]`
/// normalizes to the same value, so both keep the legacy build composition
/// byte for byte (no patch, `preflight_build`).
fn is_workspace_root_manifest(manifest: &[PathBuf]) -> bool {
    manifest.len() == 1 && manifest[0].as_path() == Path::new(".")
}

/// Derive the installed `@deepseek-ai/dsh-fs` package directory from the dsh
/// runtime path (S03 pinned algorithm). The runtime path is canonicalized,
/// then its containing directory and every ancestor is checked for a
/// `package.json` whose `name` is exactly `@deepseek-ai/dsh`; the nested
/// `node_modules/@deepseek-ai/dsh-fs` package is joined and its own
/// `package.json` must exist. Every failure is fail-loud with the step that
/// failed, so a bad `DSH_RUNTIME_PATH` refuses the spawn instead of producing
/// a plugin tree whose bare `@deepseek-ai/dsh-fs` import cannot resolve.
fn derive_dsh_fs_package(runtime: &Path) -> io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(runtime).map_err(|error| {
        io::Error::other(format!(
            "manifest-build cannot canonicalize the dsh runtime {}: {error}",
            runtime.display()
        ))
    })?;
    let start = if canonical.is_dir() {
        canonical.as_path()
    } else {
        canonical.parent().ok_or_else(|| {
            io::Error::other(format!(
                "manifest-build dsh runtime {} has no parent directory",
                canonical.display()
            ))
        })?
    };
    fn package_name(directory: &Path) -> Option<String> {
        let bytes = std::fs::read(directory.join("package.json")).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        value.get("name")?.as_str().map(str::to_owned)
    }
    let mut cursor = Some(start);
    let dsh_root = loop {
        let Some(directory) = cursor else {
            return Err(io::Error::other(format!(
                "manifest-build cannot find a @deepseek-ai/dsh package above {}",
                canonical.display()
            )));
        };
        if package_name(directory).as_deref() == Some("@deepseek-ai/dsh") {
            break directory.to_path_buf();
        }
        cursor = directory.parent();
    };
    let dsh_fs = dsh_root.join("node_modules/@deepseek-ai/dsh-fs");
    if !dsh_fs.join("package.json").is_file() {
        return Err(io::Error::other(format!(
            "manifest-build cannot find the nested @deepseek-ai/dsh-fs package at {}",
            dsh_fs.display()
        )));
    }
    Ok(dsh_fs)
}

/// The per-task write-guard plugin tree plus the generated patch. The TempDir
/// owns every file; the caller hands it to the runtime owner so it lives
/// exactly as long as the task.
struct WriteGuardTree {
    directory: tempfile::TempDir,
    patch_path: PathBuf,
}

/// Materialize the byte-exact `dsh-write-guard` package (plus its nested
/// `@deepseek-ai/dsh-fs` symlink) and the S02 patch inside a fresh
/// `external-dsh-manifest-` TempDir. Pure filesystem work, so the derivation
/// and byte-equality tests can exercise it without a runtime that answers
/// `--dump-config`.
fn materialize_write_guard_tree(
    runtime: &Path,
    manifest: &[PathBuf],
) -> io::Result<WriteGuardTree> {
    let directory = tempfile::Builder::new()
        .prefix("external-dsh-manifest-")
        .tempdir()
        .map_err(|error| io::Error::other(format!("manifest-build tempdir failed: {error}")))?;
    let plugin_root = directory.path().join("dsh-write-guard");
    for (relative, contents) in external_agent_dsh::profile::WRITE_GUARD_FILES {
        let path = plugin_root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                io::Error::other(format!(
                    "manifest-build cannot create {}: {error}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(&path, contents).map_err(|error| {
            io::Error::other(format!(
                "manifest-build cannot write {}: {error}",
                path.display()
            ))
        })?;
    }
    let dsh_fs = derive_dsh_fs_package(runtime)?;
    let link = plugin_root.join("node_modules/@deepseek-ai/dsh-fs");
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            io::Error::other(format!(
                "manifest-build cannot create {}: {error}",
                parent.display()
            ))
        })?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dsh_fs, &link).map_err(|error| {
        io::Error::other(format!(
            "manifest-build cannot link {} -> {}: {error}",
            link.display(),
            dsh_fs.display()
        ))
    })?;
    #[cfg(not(unix))]
    return Err(io::Error::other(
        "manifest-build requires unix symlink support",
    ));

    let guard_entry = plugin_root.join("lib/index.js");
    let patch = external_agent_dsh::profile::manifest_build_patch(manifest, &guard_entry)
        .map_err(|error| io::Error::other(format!("manifest-build patch failed: {error}")))?;
    let patch_path = directory.path().join("manifest-build.patch.yml");
    std::fs::write(&patch_path, patch).map_err(|error| {
        io::Error::other(format!(
            "manifest-build cannot write {}: {error}",
            patch_path.display()
        ))
    })?;
    Ok(WriteGuardTree {
        directory,
        patch_path,
    })
}

/// The resolved manifest-build launch: the TempDir owns the plugin tree and
/// the patch the child reads.
struct ManifestBuild {
    directory: tempfile::TempDir,
    command: std::process::Command,
}

/// Materialize one manifest-build composition, run the S02 preflight against
/// the generated patch, and resolve the ACP command. Shared by the production
/// `Enabled` gate and the test-only `TestHarness` gate so the derivation,
/// byte-exact plugin files, symlink and patch cannot drift between them.
fn materialize_manifest_build(
    runtime: &Path,
    workspace: &Path,
    home: Option<PathBuf>,
    manifest: &[PathBuf],
) -> io::Result<ManifestBuild> {
    let tree = materialize_write_guard_tree(runtime, manifest)?;
    let mut launch = external_agent_dsh::profile::DshLaunch::new(
        Some(runtime.to_path_buf()),
        workspace.to_path_buf(),
        home,
    );
    launch.profile = std::env::var("DSH_PROFILE").ok();
    launch.version = std::env::var("DSH_VERSION").ok();
    launch.permission_mode = Some("workspace-write".into());
    launch.patch = Some(tree.patch_path.clone());
    external_agent_dsh::profile::preflight_build_manifest(&launch, manifest)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
    let command = external_agent_dsh::profile::resolve_launch(&launch)?;
    Ok(ManifestBuild {
        directory: tree.directory,
        command,
    })
}

impl RuntimeFactory for DshRuntimeFactory {
    fn spawn(
        &self,
        _task: &TaskRecord,
        _sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        match self.gate {
            DshSpawnGate::Closed => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "dsh spawn gate is closed; production DSH spawn is not enabled",
            )),
            DshSpawnGate::Enabled => {
                let prepared = match task_route(_task) {
                    Ok(crate::TaskRoute::General(prepared)) => prepared,
                    Err(message) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, message))
                    }
                };
                let executable = std::env::var_os("DSH_RUNTIME_PATH")
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "DSH_RUNTIME_PATH is unavailable")
                    })?;
                let home = std::env::var_os("DSH_HOME").map(PathBuf::from);
                let plan = matches!(
                    prepared.permission_mode,
                    external_core::PermissionMode::Plan
                );
                if plan {
                    let managed_patch = if std::env::var_os("DSH_STRICT_PLAN_PATCH").is_none() {
                        Some(
                            tempfile::Builder::new()
                                .prefix("external-dsh-strict-")
                                .tempdir()
                                .map_err(|e| io::Error::other(e.to_string()))?,
                        )
                    } else {
                        None
                    };
                    let patch = std::env::var_os("DSH_STRICT_PLAN_PATCH")
                        .map(PathBuf::from)
                        .or_else(|| {
                            managed_patch
                                .as_ref()
                                .map(|d| d.path().join("strict-plan.patch.yml"))
                        });
                    if let Some(dir) = managed_patch.as_ref() {
                        std::fs::write(
                            dir.path().join("strict-plan.patch.yml"),
                            external_agent_dsh::profile::STRICT_PLAN_PATCH_YAML,
                        )
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    }
                    let mut launch = external_agent_dsh::profile::DshLaunch::new(
                        Some(executable),
                        prepared.workspace.path.clone(),
                        home,
                    );
                    launch.profile = std::env::var("DSH_PROFILE").ok();
                    launch.version = std::env::var("DSH_VERSION").ok();
                    launch.permission_mode = Some("read-only".into());
                    launch.patch = patch;
                    external_agent_dsh::profile::preflight(&launch)
                        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
                    let command = external_agent_dsh::profile::resolve_launch(&launch)?;
                    Ok(Arc::new(DshRuntimeOwner::spawn_with_patch(
                        command,
                        _sink,
                        managed_patch,
                    )?))
                } else if is_workspace_root_manifest(&prepared.write_manifest) {
                    // Legacy build: no patch, `preflight_build`, byte-identical
                    // to the pre-S03 composition.
                    let mut launch = external_agent_dsh::profile::DshLaunch::new(
                        Some(executable),
                        prepared.workspace.path.clone(),
                        home,
                    );
                    launch.profile = std::env::var("DSH_PROFILE").ok();
                    launch.version = std::env::var("DSH_VERSION").ok();
                    launch.permission_mode = Some("workspace-write".into());
                    external_agent_dsh::profile::preflight_build(&launch)
                        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
                    let command = external_agent_dsh::profile::resolve_launch(&launch)?;
                    Ok(Arc::new(DshRuntimeOwner::spawn(command, _sink)?))
                } else {
                    // A caller write manifest: materialize the guard tree and
                    // the manifest-build patch, then preflight the composition.
                    let materialized = materialize_manifest_build(
                        &executable,
                        &prepared.workspace.path,
                        home,
                        &prepared.write_manifest,
                    )?;
                    Ok(Arc::new(DshRuntimeOwner::spawn_with_patch(
                        materialized.command,
                        _sink,
                        Some(materialized.directory),
                    )?))
                }
            }
            #[cfg(test)]
            DshSpawnGate::TestHarness => {
                let prepared = match task_route(_task) {
                    Ok(crate::TaskRoute::General(prepared)) => prepared,
                    Err(message) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, message))
                    }
                };
                let executable = self
                    .executable
                    .clone()
                    .or_else(|| std::env::var_os("DSH_RUNTIME_PATH").map(PathBuf::from));
                let home = std::env::var_os("DSH_HOME").map(PathBuf::from);
                let plan = matches!(
                    prepared.permission_mode,
                    external_core::PermissionMode::Plan
                );
                if plan || is_workspace_root_manifest(&prepared.write_manifest) {
                    let mut launch = external_agent_dsh::profile::DshLaunch::new(
                        executable,
                        prepared.workspace.path.clone(),
                        home,
                    );
                    launch.profile = std::env::var("DSH_PROFILE").ok();
                    launch.version = std::env::var("DSH_VERSION").ok();
                    let command = external_agent_dsh::profile::resolve_launch(&launch)?;
                    let owner = DshRuntimeOwner::spawn(command, _sink)?;
                    Ok(Arc::new(owner))
                } else {
                    let runtime = executable.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "DSH_RUNTIME_PATH is unavailable")
                    })?;
                    let materialized = materialize_manifest_build(
                        &runtime,
                        &prepared.workspace.path,
                        home,
                        &prepared.write_manifest,
                    )?;
                    Ok(Arc::new(DshRuntimeOwner::spawn_with_patch(
                        materialized.command,
                        _sink,
                        Some(materialized.directory),
                    )?))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake `@deepseek-ai/dsh` package rooted at `dsh` with a runnable
    /// (non-`.js`) runtime in `bin/`.
    fn fake_dsh_package(dsh: &Path, package_name: &str) -> PathBuf {
        std::fs::create_dir_all(dsh.join("bin")).unwrap();
        std::fs::write(
            dsh.join("package.json"),
            format!("{{\"name\":\"{package_name}\",\"version\":\"0.1.5-rc.1\"}}"),
        )
        .unwrap();
        let runtime = dsh.join("bin/dsh-runtime");
        std::fs::write(&runtime, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        runtime
    }

    /// Add the **nested** `@deepseek-ai/dsh-fs` package the symlink joins (the
    /// real install shape, not a sibling `node_modules` entry).
    fn write_nested_fs_package(dsh: &Path) -> PathBuf {
        let fs = dsh.join("node_modules/@deepseek-ai/dsh-fs");
        std::fs::create_dir_all(fs.join("lib")).unwrap();
        std::fs::write(
            fs.join("package.json"),
            br#"{"name":"@deepseek-ai/dsh-fs"}"#,
        )
        .unwrap();
        std::fs::write(fs.join("lib/index.js"), b"// fake fs\n").unwrap();
        fs
    }

    #[test]
    fn derive_dsh_fs_package_finds_the_nested_install() {
        let directory = tempfile::tempdir().unwrap();
        let dsh = directory.path().join("node_modules/@deepseek-ai/dsh");
        let fs = write_nested_fs_package(&dsh);
        let runtime = fake_dsh_package(&dsh, "@deepseek-ai/dsh");
        assert_eq!(
            derive_dsh_fs_package(&runtime).unwrap(),
            std::fs::canonicalize(&fs).unwrap()
        );
    }

    #[test]
    fn derive_dsh_fs_package_fails_loud_without_the_package_marker() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("bin/dsh-runtime");
        std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        std::fs::write(&runtime, b"#!/bin/sh\n").unwrap();
        let error = derive_dsh_fs_package(&runtime).unwrap_err();
        assert!(error.to_string().contains("@deepseek-ai/dsh"), "{error}");
    }

    #[test]
    fn derive_dsh_fs_package_fails_loud_on_a_drifted_package_name() {
        let directory = tempfile::tempdir().unwrap();
        let dsh = directory.path().join("node_modules/@deepseek-ai/dsh");
        let runtime = fake_dsh_package(&dsh, "@deepseek-ai/not-dsh");
        assert!(derive_dsh_fs_package(&runtime).is_err());
    }

    #[test]
    fn derive_dsh_fs_package_fails_loud_without_the_nested_fs_package() {
        let directory = tempfile::tempdir().unwrap();
        let dsh = directory.path().join("node_modules/@deepseek-ai/dsh");
        let runtime = fake_dsh_package(&dsh, "@deepseek-ai/dsh");
        let error = derive_dsh_fs_package(&runtime).unwrap_err();
        assert!(error.to_string().contains("dsh-fs"), "{error}");
    }

    #[test]
    fn materialize_write_guard_tree_is_byte_exact_and_patches_the_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let dsh = directory.path().join("node_modules/@deepseek-ai/dsh");
        let fs = write_nested_fs_package(&dsh);
        let runtime = fake_dsh_package(&dsh, "@deepseek-ai/dsh");
        let manifest = vec![PathBuf::from("src/a.rs"), PathBuf::from("docs")];
        let tree = materialize_write_guard_tree(&runtime, &manifest).unwrap();

        // The TempDir carries the pinned prefix and owns the plugin tree.
        assert!(tree
            .directory
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("external-dsh-manifest-"));
        let plugin = tree.directory.path().join("dsh-write-guard");
        let guard_entry = plugin.join("lib/index.js");
        assert_eq!(
            tree.patch_path,
            tree.directory.path().join("manifest-build.patch.yml")
        );
        // Byte-for-byte embedded artifacts.
        assert_eq!(
            std::fs::read_to_string(plugin.join("package.json")).unwrap(),
            external_agent_dsh::profile::WRITE_GUARD_PACKAGE_JSON
        );
        assert_eq!(
            std::fs::read_to_string(plugin.join("lib/index.js")).unwrap(),
            external_agent_dsh::profile::WRITE_GUARD_INDEX_JS
        );
        // The nested symlink resolves the bare peer import to the derived
        // install closure.
        assert_eq!(
            std::fs::read_link(plugin.join("node_modules/@deepseek-ai/dsh-fs")).unwrap(),
            std::fs::canonicalize(&fs).unwrap()
        );
        // Parsed patch assertions: guard insert + the caller manifest, the
        // filesystem tool stays enabled and bash is disabled.
        let patch = std::fs::read_to_string(&tree.patch_path).unwrap();
        assert!(
            patch.contains("- id: sandbox-policy\n  config:\n    mode: workspace-write"),
            "{patch}"
        );
        assert!(
            patch.contains(&format!("  - name: {}\n", guard_entry.display())),
            "{patch}"
        );
        assert!(patch.contains("      - src/a.rs\n"), "{patch}");
        assert!(patch.contains("      - docs\n"), "{patch}");
        assert!(!patch.contains("id: tool-fs"), "{patch}");
        assert!(
            patch.contains("- id: tool-bash\n  disabled: true"),
            "{patch}"
        );
    }
}
