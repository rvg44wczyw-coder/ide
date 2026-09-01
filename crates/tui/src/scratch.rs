//! Scratch files (`docs/features/tui-scratch-files.md`, `T23`): real,
//! persisted files under a fixed per-user directory, opened through the
//! same `open_or_focus_tab` path every other file already goes through --
//! see that doc's §1.1 for why this doesn't use `ide_core::Buffer::
//! untitled()`'s in-memory, path-less form instead.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScratchNameError {
    #[error("name cannot be empty")]
    Empty,
    #[error("name cannot contain a path separator")]
    PathSeparator,
    #[error("name cannot be \".\" or \"..\"")]
    DotOrDotDot,
}

/// Trims whitespace, then rejects empty, `.`/`..` exactly, and any `/`
/// or `\` -- both, regardless of host OS, so a name typed on any
/// platform can't escape `scratch_dir()` on any other (`docs/features/
/// tui-scratch-files.md` §2.1).
pub fn validate_scratch_name(name: &str) -> Result<String, ScratchNameError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ScratchNameError::Empty);
    }
    if trimmed == "." || trimmed == ".." {
        return Err(ScratchNameError::DotOrDotDot);
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(ScratchNameError::PathSeparator);
    }
    Ok(trimmed.to_string())
}

/// `~/.config/ide-tui/scratch/` -- global, per-user, independent of
/// whatever project is currently open.
pub fn scratch_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(scratch_dir_from_home(Path::new(&home)))
}

fn scratch_dir_from_home(home: &Path) -> PathBuf {
    home.join(".config/ide-tui/scratch")
}

/// Validates `name`, then joins it onto `scratch_dir()`, creating that
/// directory (not the file itself) if missing. `Ok(None)` if
/// `scratch_dir()` can't be resolved (unresolvable `$HOME`/
/// `$USERPROFILE`) -- distinct from `Err`, which means the *name* itself
/// was rejected.
pub fn new_scratch_path(name: &str) -> Result<Option<PathBuf>, ScratchNameError> {
    let Some(dir) = scratch_dir() else {
        return Ok(None);
    };
    new_scratch_path_in(&dir, name).map(Some)
}

/// Split out from [`new_scratch_path`] so tests can point it at a
/// tempdir-backed directory directly, mirroring `state.rs`'s own
/// `load_from`/`save_to` split.
fn new_scratch_path_in(dir: &Path, name: &str) -> Result<PathBuf, ScratchNameError> {
    let name = validate_scratch_name(name)?;
    let _ = std::fs::create_dir_all(dir);
    Ok(dir.join(name))
}

/// Every regular file directly inside `scratch_dir()`, sorted by file
/// name. Empty if the directory doesn't exist yet, can't be resolved, or
/// can't be read.
pub fn list_scratch_files() -> Vec<PathBuf> {
    let Some(dir) = scratch_dir() else {
        return Vec::new();
    };
    list_scratch_files_in(&dir)
}

fn list_scratch_files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_scratch_name_accepts_an_ordinary_name() {
        assert_eq!(
            validate_scratch_name("notes.md"),
            Ok("notes.md".to_string())
        );
    }

    #[test]
    fn validate_scratch_name_trims_surrounding_whitespace() {
        assert_eq!(
            validate_scratch_name("  notes.md  "),
            Ok("notes.md".to_string())
        );
    }

    #[test]
    fn validate_scratch_name_rejects_empty_and_whitespace_only() {
        assert_eq!(validate_scratch_name(""), Err(ScratchNameError::Empty));
        assert_eq!(validate_scratch_name("   "), Err(ScratchNameError::Empty));
    }

    #[test]
    fn validate_scratch_name_rejects_dot_and_dot_dot() {
        assert_eq!(
            validate_scratch_name("."),
            Err(ScratchNameError::DotOrDotDot)
        );
        assert_eq!(
            validate_scratch_name(".."),
            Err(ScratchNameError::DotOrDotDot)
        );
    }

    #[test]
    fn validate_scratch_name_rejects_an_embedded_forward_slash() {
        assert_eq!(
            validate_scratch_name("../../etc/passwd"),
            Err(ScratchNameError::PathSeparator)
        );
        assert_eq!(
            validate_scratch_name("sub/dir.txt"),
            Err(ScratchNameError::PathSeparator)
        );
    }

    #[test]
    fn validate_scratch_name_rejects_an_embedded_backslash_on_any_host_os() {
        assert_eq!(
            validate_scratch_name("sub\\dir.txt"),
            Err(ScratchNameError::PathSeparator)
        );
    }

    #[test]
    fn scratch_dir_from_home_joins_the_expected_relative_path() {
        assert_eq!(
            scratch_dir_from_home(Path::new("/home/someone")),
            PathBuf::from("/home/someone/.config/ide-tui/scratch")
        );
    }

    #[test]
    fn scratch_dir_resolves_to_some_path_in_this_test_environment() {
        assert!(scratch_dir().is_some());
    }

    #[test]
    fn new_scratch_path_rejects_an_invalid_name_without_creating_anything() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        let result = new_scratch_path_in(&scratch, "../escape");
        assert_eq!(result, Err(ScratchNameError::PathSeparator));
        assert!(!scratch.exists());
    }

    #[test]
    fn new_scratch_path_in_creates_the_directory_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("nested/scratch");
        assert!(!scratch.exists());

        let path = new_scratch_path_in(&scratch, "notes.md").unwrap();

        assert_eq!(path, scratch.join("notes.md"));
        assert!(scratch.is_dir());
        // The file itself is not created by this function -- only the
        // directory it will live in.
        assert!(!path.exists());
    }

    #[test]
    fn new_scratch_path_against_a_real_environment_never_panics() {
        // Read/create-dir-only (never writes a file) -- safe to call for
        // real, unlike a test that actually needs a controlled directory.
        let _ = new_scratch_path("tui-scratch-files-doctest-probe.txt");
    }

    #[test]
    fn list_scratch_files_in_a_missing_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert_eq!(list_scratch_files_in(&missing), Vec::<PathBuf>::new());
    }

    #[test]
    fn list_scratch_files_in_sorts_by_name_and_skips_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let files = list_scratch_files_in(dir.path());

        assert_eq!(
            files,
            vec![dir.path().join("a.rs"), dir.path().join("b.txt")]
        );
    }

    #[test]
    fn list_scratch_files_against_a_real_environment_never_panics() {
        let _ = list_scratch_files();
    }
}
