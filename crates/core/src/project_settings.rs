//! Per-project settings storage (`docs/features/project-settings.md`): a
//! generic, project-root-scoped JSON read/write helper mirroring
//! IntelliJ-based IDEs' `.idea/` directory. Generic over the payload type
//! (`T: Serialize`/`DeserializeOwned`) so this module never needs to know
//! about `ide-ui`-only types (`Theme`, `KeymapOverlay`) -- it only
//! round-trips whatever the caller gives it under one of two named slots.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const SETTINGS_DIR_NAME: &str = ".ide";

/// The two settings categories this feature defines (§3.4 of the doc: a
/// future feature needing its own project-scoped state adds a new
/// variant/file here rather than growing `Preferences`/`Workspace`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSettingsFile {
    /// Stable preferences: theme, keymap overrides, custom language
    /// configs, format-on-save.
    Preferences,
    /// Volatile session state: open tabs, active tab, cursor offsets.
    Workspace,
    /// Recent-files/bookmarks-style navigation aids
    /// (`docs/features/tui-recent-files-and-bookmarks.md` §2.1) -- a slot
    /// content-named, not frontend-named, the same way `Preferences`/
    /// `Workspace` are. `ide-tui` is this slot's first user; nothing about
    /// the name or file ties it to one frontend.
    Navigation,
}

impl ProjectSettingsFile {
    fn file_name(self) -> &'static str {
        match self {
            ProjectSettingsFile::Preferences => "preferences.json",
            ProjectSettingsFile::Workspace => "workspace.json",
            ProjectSettingsFile::Navigation => "navigation.json",
        }
    }
}

/// Resolves `project_root/.ide`, rejecting one that already exists and
/// resolves (following a symlink, if it is one) outside `project_root` --
/// mirrors the symlink-escape rejection `project.rs`'s directory-tree scan
/// already applies to the project root itself, which nothing in this
/// module carried over to `.ide/`'s own resolution before a hacker pass
/// found it live (`docs/security-findings/
/// rust-core-dev-project-settings-2026-08-25.md`, finding 2). A `.ide/`
/// that doesn't exist yet can't escape anything, so this only rejects an
/// already-existing one -- it never blocks the ordinary first-write case.
fn settings_dir(project_root: &Path) -> Result<PathBuf, ProjectSettingsError> {
    let dir = project_root.join(SETTINGS_DIR_NAME);
    if let Ok(canonical_dir) = dir.canonicalize() {
        let canonical_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        if !canonical_dir.starts_with(&canonical_root) {
            return Err(ProjectSettingsError::Io(std::io::Error::other(format!(
                "{} resolves to {}, outside the project root",
                dir.display(),
                canonical_dir.display()
            ))));
        }
    }
    Ok(dir)
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectSettingsError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed settings file: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// Reads and deserializes `project_root/.ide/<file>`. `Ok(None)` if the
/// file doesn't exist yet -- the expected case for a project's first
/// session with this settings category, not a failure. `Malformed` if it
/// exists but doesn't parse (hand-edited, truncated by a crash outside
/// this module's own atomic `write`) -- callers fall back to defaults
/// exactly as they would for `Ok(None)`.
pub fn read<T: serde::de::DeserializeOwned>(
    project_root: &Path,
    file: ProjectSettingsFile,
) -> Result<Option<T>, ProjectSettingsError> {
    let dir = settings_dir(project_root)?;
    let path = dir.join(file.file_name());
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// Serializes `value` and writes it to `project_root/.ide/<file>`,
/// pretty-printed. Creates `.ide/` if it doesn't exist yet, calling
/// [`ensure_gitignored`] on that first creation. Writes to a uniquely-
/// named temp file in the same directory first, then atomically persists
/// it over the real target -- a crash mid-write can never leave a
/// truncated/corrupt file at `path`. The temp name is collision-proof
/// (via [`tempfile::Builder`], the same fix already applied to
/// `git::Repository`'s conflict-resolution write for the identical
/// reason) rather than the fixed `<file>.tmp` literal an earlier version
/// used: two concurrent `write()` calls sharing one temp filename can
/// interleave their `open(O_TRUNC)`+`write` calls, and because `O_TRUNC`
/// only truncates once, at `open()` time, a shorter payload's write does
/// not shrink a longer payload's leftover trailing bytes -- confirmed
/// live to corrupt the final file (docs/security-findings/
/// rust-core-dev-project-settings-2026-08-25.md, finding 1).
pub fn write<T: serde::Serialize>(
    project_root: &Path,
    file: ProjectSettingsFile,
    value: &T,
) -> Result<(), ProjectSettingsError> {
    let dir = settings_dir(project_root)?;
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
        ensure_gitignored(project_root)?;
    }
    let json = serde_json::to_vec_pretty(value)?;
    let target = dir.join(file.file_name());
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{}-", file.file_name()))
        .tempfile_in(&dir)?;
    tmp.write_all(&json)?;
    tmp.persist(&target).map_err(|e| e.error)?;
    Ok(())
}

/// Appends a `.ide/` ignore entry to `project_root/.gitignore`, creating
/// that file if it doesn't exist. No-ops if a line already covers it
/// (`.ide/`, `.ide`, or `/.ide/`, checked textually -- not full gitignore
/// pattern semantics, out of scope for one fixed directory name).
///
/// Computes the desired final content and replaces the whole file
/// atomically (via [`tempfile::Builder`]+`persist`, same as [`write`])
/// rather than a read-then-append -- two threads racing this function
/// both read the same pre-append content and both compute the *same*
/// deterministic final content (this function only ever appends one
/// fixed literal line), so whichever one's atomic replace lands last
/// still converges on a single correct entry instead of a duplicate.
/// The read-then-append version had exactly this race, confirmed live to
/// produce duplicate `.ide/` lines under concurrent first-writes
/// (docs/security-findings/rust-core-dev-project-settings-2026-08-25.md,
/// finding 1).
pub fn ensure_gitignored(project_root: &Path) -> std::io::Result<()> {
    let gitignore_path = project_root.join(".gitignore");
    let existing = fs::read_to_string(&gitignore_path).unwrap_or_default();
    let already_ignored = existing
        .lines()
        .any(|line| matches!(line.trim(), ".ide/" | ".ide" | "/.ide/"));
    if already_ignored {
        return Ok(());
    }
    let mut new_content = existing.clone();
    if !existing.is_empty() && !existing.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(SETTINGS_DIR_NAME);
    new_content.push_str("/\n");

    let dir = project_root;
    let mut tmp = tempfile::Builder::new()
        .prefix(".gitignore-")
        .tempfile_in(dir)?;
    tmp.write_all(new_content.as_bytes())?;
    tmp.persist(&gitignore_path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Example {
        count: u32,
    }

    #[test]
    fn read_with_no_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read::<Example>(dir.path(), ProjectSettingsFile::Preferences).unwrap(),
            None
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ProjectSettingsFile::Preferences,
            &Example { count: 3 },
        )
        .unwrap();

        let got = read::<Example>(dir.path(), ProjectSettingsFile::Preferences).unwrap();
        assert_eq!(got, Some(Example { count: 3 }));
    }

    #[test]
    fn preferences_and_workspace_are_independent_slots() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ProjectSettingsFile::Preferences,
            &Example { count: 1 },
        )
        .unwrap();
        write(
            dir.path(),
            ProjectSettingsFile::Workspace,
            &Example { count: 2 },
        )
        .unwrap();

        assert_eq!(
            read::<Example>(dir.path(), ProjectSettingsFile::Preferences).unwrap(),
            Some(Example { count: 1 })
        );
        assert_eq!(
            read::<Example>(dir.path(), ProjectSettingsFile::Workspace).unwrap(),
            Some(Example { count: 2 })
        );
        assert!(dir
            .path()
            .join(SETTINGS_DIR_NAME)
            .join("preferences.json")
            .exists());
        assert!(dir
            .path()
            .join(SETTINGS_DIR_NAME)
            .join("workspace.json")
            .exists());
    }

    #[test]
    fn navigation_is_a_third_slot_independent_of_the_other_two() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ProjectSettingsFile::Navigation,
            &Example { count: 42 },
        )
        .unwrap();

        assert_eq!(
            read::<Example>(dir.path(), ProjectSettingsFile::Navigation).unwrap(),
            Some(Example { count: 42 })
        );
        assert_eq!(
            read::<Example>(dir.path(), ProjectSettingsFile::Preferences).unwrap(),
            None
        );
        assert!(dir
            .path()
            .join(SETTINGS_DIR_NAME)
            .join("navigation.json")
            .exists());
    }

    #[test]
    fn write_creates_gitignore_entry_on_first_creation_only() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ProjectSettingsFile::Preferences,
            &Example { count: 1 },
        )
        .unwrap();

        let gitignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(gitignore.matches(".ide/").count(), 1);
    }

    #[test]
    fn write_leaves_no_temp_file_behind_on_success() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ProjectSettingsFile::Preferences,
            &Example { count: 1 },
        )
        .unwrap();

        let entries: Vec<_> = fs::read_dir(dir.path().join(SETTINGS_DIR_NAME))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("preferences.json")]);
    }

    #[test]
    fn read_on_malformed_json_returns_malformed_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let settings_dir = dir.path().join(SETTINGS_DIR_NAME);
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(settings_dir.join("preferences.json"), b"{ not json").unwrap();

        let result = read::<Example>(dir.path(), ProjectSettingsFile::Preferences);
        assert!(matches!(result, Err(ProjectSettingsError::Malformed(_))));
    }

    #[test]
    fn ensure_gitignored_never_duplicates_the_entry_under_concurrency() {
        use std::sync::Arc;
        use std::thread;

        for _round in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let root: Arc<std::path::PathBuf> = Arc::new(dir.path().to_path_buf());
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let root = Arc::clone(&root);
                    thread::spawn(move || ensure_gitignored(&root))
                })
                .collect();
            for h in handles {
                h.join().unwrap().unwrap();
            }

            let gitignore = fs::read_to_string(root.join(".gitignore")).unwrap();
            assert_eq!(
                gitignore.matches(".ide/").count(),
                1,
                "expected exactly one .ide/ entry, got: {gitignore:?}"
            );
        }
    }

    #[test]
    fn ensure_gitignored_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        ensure_gitignored(dir.path()).unwrap();
        ensure_gitignored(dir.path()).unwrap();

        let gitignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(gitignore.matches(".ide/").count(), 1);
    }

    #[test]
    fn ensure_gitignored_appends_to_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();

        ensure_gitignored(dir.path()).unwrap();

        let gitignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gitignore.contains("target/"));
        assert!(gitignore.contains(".ide/"));
    }

    #[test]
    fn ensure_gitignored_recognizes_an_existing_entry_in_any_accepted_form() {
        for existing in [".ide/", ".ide", "/.ide/"] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(".gitignore"), format!("{existing}\n")).unwrap();

            ensure_gitignored(dir.path()).unwrap();

            let gitignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
            assert_eq!(gitignore.trim(), existing);
        }
    }

    #[test]
    fn ensure_gitignored_appends_newline_before_entry_when_file_lacks_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "target/").unwrap();

        ensure_gitignored(dir.path()).unwrap();

        let gitignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(gitignore, "target/\n.ide/\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_rejects_a_dot_ide_symlink_escaping_the_project_root() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), project.path().join(SETTINGS_DIR_NAME)).unwrap();

        let result = write(
            project.path(),
            ProjectSettingsFile::Preferences,
            &Example { count: 42 },
        );

        assert!(result.is_err());
        assert!(!outside.path().join("preferences.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn read_rejects_a_dot_ide_symlink_escaping_the_project_root() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(
            outside.path().join("preferences.json"),
            serde_json::to_vec(&Example { count: 1 }).unwrap(),
        )
        .unwrap();
        symlink(outside.path(), project.path().join(SETTINGS_DIR_NAME)).unwrap();

        let result = read::<Example>(project.path(), ProjectSettingsFile::Preferences);

        assert!(result.is_err());
    }

    #[test]
    fn write_still_succeeds_when_dot_ide_does_not_exist_yet() {
        // The escape check must never block the ordinary first-write case,
        // where `.ide/` legitimately doesn't exist yet.
        let dir = tempfile::tempdir().unwrap();
        assert!(write(
            dir.path(),
            ProjectSettingsFile::Preferences,
            &Example { count: 1 },
        )
        .is_ok());
    }

    #[test]
    fn concurrent_writes_to_the_same_slot_never_corrupt_the_final_file() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let root: Arc<std::path::PathBuf> = Arc::new(dir.path().to_path_buf());

        for round in 0..20 {
            let handles: Vec<_> = (0..16u32)
                .map(|i| {
                    let root = Arc::clone(&root);
                    thread::spawn(move || {
                        write(
                            &root,
                            ProjectSettingsFile::Preferences,
                            &Example { count: i },
                        )
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap().unwrap();
            }

            let got = read::<Example>(&root, ProjectSettingsFile::Preferences);
            assert!(
                matches!(got, Ok(Some(_))),
                "round {round}: final file failed to parse: {got:?}"
            );
            assert!(!root
                .join(SETTINGS_DIR_NAME)
                .join("preferences.json.tmp")
                .exists());
        }
    }
}
