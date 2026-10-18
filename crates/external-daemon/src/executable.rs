//! Host executable resolution shared by the daemon gates and probe layer.
//!
//! Unix trusts the execute bit. Windows has no POSIX execute bit: a path is
//! executable when it is a regular file whose extension appears in `PATHEXT`
//! (case-insensitive). The documented default set applies when `PATHEXT` is
//! unset or empty — `.COM;.EXE;.BAT;.CMD` — matching CreateProcess's own
//! fallback. PATH discovery appends those extensions to a bare name, exactly
//! like the Windows loader, instead of trusting the first same-named file.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The default extension set Windows uses when `PATHEXT` is absent/empty.
pub const WINDOWS_DEFAULT_PATHEXT: [&str; 4] = [".COM", ".EXE", ".BAT", ".CMD"];

/// Parse `PATHEXT` into normalized, upper-case, dot-prefixed extensions.
///
/// Windows treats the variable as a semicolon-separated list; entries without
/// a leading dot are normalized (`EXE` -> `.EXE`) and duplicates are removed
/// while preserving order. An unset or blank value yields the documented
/// default set. This is a pure parser so tests can drive it directly.
pub fn parse_pathext(raw: Option<&OsStr>) -> Vec<String> {
    let mut extensions = Vec::new();
    if let Some(raw) = raw {
        for part in raw.to_string_lossy().split(';') {
            let part = part.trim().trim_matches('"').trim();
            if part.is_empty() {
                continue;
            }
            let extension = if part.starts_with('.') {
                part.to_ascii_uppercase()
            } else {
                format!(".{}", part.to_ascii_uppercase())
            };
            if !extensions.iter().any(|existing| existing == &extension) {
                extensions.push(extension);
            }
        }
    }
    if extensions.is_empty() {
        extensions.extend(WINDOWS_DEFAULT_PATHEXT.iter().map(|value| (*value).to_owned()));
    }
    extensions
}

/// The host's effective `PATHEXT` set. Non-Windows hosts have no extension
/// gate, so callers must not consult this there; it still returns the default
/// set to keep the pure surface testable.
pub fn pathext_extensions() -> Vec<String> {
    parse_pathext(env::var_os("PATHEXT").as_deref())
}

/// Case-insensitive extension membership test against the parsed set.
pub fn pathext_contains(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(OsStr::to_str) else {
        return false;
    };
    let dotted = format!(".{extension}");
    pathext_extensions()
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(&dotted))
}

/// Is `path` a regular file this host can execute?
///
/// Unix keeps the historical execute-bit contract. Windows verifies the file
/// exists, is regular, and carries a `PATHEXT` extension; an extensionless
/// file is not executable (a configured runtime path must name the real
/// artifact, never a bare stem).
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        pathext_contains(path)
    }
}

/// True when Windows must run the file through `cmd.exe` (`cmd`/`bat`).
#[cfg(windows)]
pub fn is_batch_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some(extension)
            if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
    )
}

#[cfg(not(windows))]
pub fn is_batch_file(_path: &Path) -> bool {
    false
}

/// Build a `Command` for a resolved executable. On Windows a `.cmd`/`.bat`
/// artifact is a batch script, not a PE image; CreateProcess cannot execute it
/// directly, so the probe/spawn layer wraps it in `cmd.exe /D /C`.
#[cfg(windows)]
pub fn executable_command(path: &Path) -> Command {
    if is_batch_file(path) {
        let mut command = Command::new("cmd.exe");
        command.arg("/D").arg("/C").arg(path);
        return command;
    }
    Command::new(path)
}

#[cfg(not(windows))]
pub fn executable_command(path: &Path) -> Command {
    Command::new(path)
}

/// Resolve `name` (the product's bare executable identity) on `search`.
///
/// Unix keeps the historical single-candidate lookup. Windows first accepts an
/// exact executable path, then tries `name` plus every `PATHEXT` extension in
/// order, mirroring the loader. Directory order is preserved; the first match
/// wins. Returned paths are absolute when the directory itself is absolute.
pub fn discover_executable(search: Option<&OsStr>, name: &str) -> Option<PathBuf> {
    let search = search?;
    for directory in env::split_paths(search) {
        #[cfg(windows)]
        {
            let exact = directory.join(name);
            if is_executable_file(&exact) {
                return std::path::absolute(exact).ok();
            }
            for extension in pathext_extensions() {
                let candidate = directory.join(format!("{name}{extension}"));
                if is_executable_file(&candidate) {
                    return std::path::absolute(candidate).ok();
                }
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = directory.join(name);
            if is_executable_file(&candidate) {
                return std::path::absolute(candidate).ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn pathext_guard() -> &'static Mutex<()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn default_pathext_set_applies_when_unset_or_blank() {
        assert_eq!(
            parse_pathext(None),
            vec![".COM", ".EXE", ".BAT", ".CMD"]
        );
        assert_eq!(
            parse_pathext(Some(OsStr::new("  ; "))),
            vec![".COM", ".EXE", ".BAT", ".CMD"]
        );
    }

    #[test]
    fn custom_pathext_is_normalized_deduplicated_and_order_preserving() {
        assert_eq!(
            parse_pathext(Some(OsStr::new("exe;.Ps1;.EXE; cmd ;\"bat\";"))),
            vec![".EXE", ".PS1", ".CMD", ".BAT"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn pathext_membership_is_case_insensitive_and_requires_an_extension() {
        let _guard = pathext_guard().lock().unwrap();
        let previous = env::var_os("PATHEXT");
        env::set_var("PATHEXT", ".EXE;.CMD");
        assert!(pathext_contains(Path::new("C:/tools/codex.EXE")));
        assert!(pathext_contains(Path::new("C:/tools/codex.Cmd")));
        assert!(!pathext_contains(Path::new("C:/tools/codex.PS1")));
        assert!(!pathext_contains(Path::new("C:/tools/codex")));
        match previous {
            Some(value) => env::set_var("PATHEXT", value),
            None => env::remove_var("PATHEXT"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn path_discovery_appends_pathext_and_rejects_non_executables() {
        let _guard = pathext_guard().lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("tool.exe"), b"pe").unwrap();
        std::fs::write(directory.path().join("script.cmd"), b"@echo off").unwrap();
        std::fs::write(directory.path().join("notes.txt"), b"text").unwrap();

        let search = env::join_paths([directory.path()]).unwrap();
        let found = discover_executable(Some(&search), "tool").unwrap();
        assert_eq!(
            found.to_string_lossy().to_ascii_lowercase(),
            directory.path().join("tool.exe").to_string_lossy().to_ascii_lowercase()
        );
        let found = discover_executable(Some(&search), "script").unwrap();
        assert_eq!(
            found.to_string_lossy().to_ascii_lowercase(),
            directory.path().join("script.cmd").to_string_lossy().to_ascii_lowercase()
        );
        assert_eq!(discover_executable(Some(&search), "notes"), None);
        assert_eq!(discover_executable(Some(&search), "missing"), None);
    }

    #[cfg(windows)]
    #[test]
    fn batch_files_are_wrapped_in_cmd_exe() {
        let command = executable_command(Path::new(r"C:\tools\agent.cmd"));
        assert_eq!(command.get_program(), "cmd.exe");
        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(arguments, vec!["/D", "/C", r"C:\tools\agent.cmd"]);
        let command = executable_command(Path::new(r"C:\tools\agent.exe"));
        assert_eq!(command.get_program(), r"C:\tools\agent.exe");
    }

    #[cfg(windows)]
    #[test]
    fn custom_pathext_drives_gate_membership() {
        let _guard = pathext_guard().lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("runtime.ps1");
        std::fs::write(&runtime, b"script").unwrap();
        let previous = env::var_os("PATHEXT");
        env::set_var("PATHEXT", ".EXE");
        assert!(!is_executable_file(&runtime));
        env::set_var("PATHEXT", ".PS1");
        assert!(is_executable_file(&runtime));
        match previous {
            Some(value) => env::set_var("PATHEXT", value),
            None => env::remove_var("PATHEXT"),
        }
    }
}
