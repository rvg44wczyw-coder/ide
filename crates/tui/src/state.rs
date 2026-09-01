//! Global, per-user persisted state -- today just the last successfully
//! opened project root (`docs/features/tui-persist-last-project.md`).
//! `ide-tui` has no `eframe`/`egui` dependency, so there's no
//! `eframe::Storage` to reuse the way `ide-ui` does for the same fact;
//! this is the same *kind* of storage (global, keyed by application
//! identity, survives across every project opened), just a small JSON
//! file at a per-user config path instead.

use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedState {
    pub last_project: Option<PathBuf>,
}

/// Best-effort load: a missing file, malformed JSON, or an unresolvable
/// home directory all yield [`PersistedState::default`] rather than an
/// error -- there is no user-facing error channel this early (the
/// terminal isn't even set up yet), and a broken state file must never
/// block startup.
pub fn load() -> PersistedState {
    match state_file_path() {
        Some(path) => load_from(&path),
        None => PersistedState::default(),
    }
}

/// Best-effort save: creates the parent directory if needed. Any failure
/// (permission denied, read-only filesystem, no resolvable home
/// directory, a serialization error that can't actually happen for this
/// plain-data type) is silently swallowed -- persistence is a
/// convenience, never a requirement for `ide-tui` to run.
pub fn save(state: &PersistedState) {
    if let Some(path) = state_file_path() {
        save_to(&path, state);
    }
}

/// Split out from [`load`] so tests can point it at a tempdir-backed file
/// directly, without mutating any real process environment variable.
fn load_from(path: &Path) -> PersistedState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

/// Split out from [`save`] for the same reason as [`load_from`].
fn save_to(path: &Path, state: &PersistedState) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, json);
    }
}

fn state_file_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(state_file_path_from_home(Path::new(&home)))
}

/// Split out from [`state_file_path`] so the actual join logic is
/// testable without reading (or having to fake) a real environment
/// variable.
fn state_file_path_from_home(home: &Path) -> PathBuf {
    home.join(".config/ide-tui/state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_from_a_fresh_directory_returns_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let state = load_from(&dir.path().join("state.json"));
        assert_eq!(state, PersistedState::default());
    }

    #[test]
    fn save_then_load_round_trips_a_remembered_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state.json");
        let remembered = PersistedState {
            last_project: Some(PathBuf::from("/tmp/some-project")),
        };
        save_to(&path, &remembered);
        assert_eq!(load_from(&path), remembered);
    }

    #[test]
    fn load_on_malformed_json_returns_the_default_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load_from(&path), PersistedState::default());
    }

    #[test]
    fn save_creates_the_parent_directory_if_it_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/state.json");
        assert!(!path.parent().unwrap().exists());
        save_to(
            &path,
            &PersistedState {
                last_project: Some(PathBuf::from("/tmp/x")),
            },
        );
        assert!(path.exists());
    }

    #[test]
    fn save_on_an_unresolvable_parent_is_a_silent_no_op() {
        // A path with no parent component at all (rare in practice, but
        // `Path::parent()` on e.g. `/` returns `None`) must not panic.
        save_to(Path::new("/"), &PersistedState::default());
    }

    #[test]
    fn state_file_path_from_home_joins_the_expected_relative_path() {
        let path = state_file_path_from_home(Path::new("/home/someone"));
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.config/ide-tui/state.json")
        );
    }

    #[test]
    fn state_file_path_resolves_to_some_path_in_this_test_environment() {
        // Reads the real `$HOME`/`$USERPROFILE` but performs no I/O -- safe
        // to call for real, unlike `load`/`save` which touch disk.
        assert!(state_file_path().is_some());
    }

    #[test]
    fn load_against_the_real_environment_never_panics() {
        // Read-only: exercises `load`'s own `state_file_path()` branch
        // against whatever the real environment/filesystem happens to
        // have, without ever writing to it.
        let _ = load();
    }
}
