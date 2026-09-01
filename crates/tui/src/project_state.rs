//! Per-project navigation state -- recent files and bookmarks
//! (`docs/features/tui-recent-files-and-bookmarks.md` §2.2). Persisted via
//! `ide_core::project_settings`'s `Navigation` slot (`.ide/
//! navigation.json`), the per-project counterpart to `state.rs`'s global
//! `~/.config/ide-tui/state.json` -- that file remembers *which* project
//! to reopen, this one remembers navigation state *within* a project.

use std::path::{Path, PathBuf};

use ide_core::project_settings::{self, ProjectSettingsFile};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_RECENT_FILES: usize = 20;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProjectNavigationState {
    pub(crate) recent_files: Vec<PathBuf>,
    pub(crate) bookmarks: Vec<Bookmark>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Bookmark {
    pub(crate) path: PathBuf,
    pub(crate) line: usize,
}

impl ProjectNavigationState {
    /// Moves `path` to the front (dedup, not a duplicate entry) if
    /// already present, inserts at the front otherwise, then truncates to
    /// `MAX_RECENT_FILES` -- MRU order, capped.
    pub(crate) fn record_recent_file(&mut self, path: PathBuf) {
        self.recent_files.retain(|p| p != &path);
        self.recent_files.insert(0, path);
        self.recent_files.truncate(MAX_RECENT_FILES);
    }

    /// Toggles a bookmark at `(path, line)`: removes it if present
    /// (returns `false`), appends it otherwise (returns `true`).
    pub(crate) fn toggle_bookmark(&mut self, path: PathBuf, line: usize) -> bool {
        if let Some(pos) = self
            .bookmarks
            .iter()
            .position(|b| b.path == path && b.line == line)
        {
            self.bookmarks.remove(pos);
            false
        } else {
            self.bookmarks.push(Bookmark { path, line });
            true
        }
    }
}

/// Every failure mode (no `.ide/` yet, malformed JSON, an unreadable
/// directory) collapses to the default -- a fresh/broken navigation file
/// must never block startup, the same fail-open posture `state.rs`'s
/// `load` already established for the global last-project file.
pub(crate) fn load(project_root: &Path) -> ProjectNavigationState {
    project_settings::read(project_root, ProjectSettingsFile::Navigation)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Best-effort: a failed write (permissions, a read-only project root)
/// must never crash an editing session over a convenience feature, the
/// same reasoning `state.rs`'s `save` already documents.
pub(crate) fn save(project_root: &Path, state: &ProjectNavigationState) {
    let _ = project_settings::write(project_root, ProjectSettingsFile::Navigation, state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_recent_file_on_an_empty_list_inserts_it() {
        let mut state = ProjectNavigationState::default();
        state.record_recent_file(PathBuf::from("a.rs"));
        assert_eq!(state.recent_files, vec![PathBuf::from("a.rs")]);
    }

    #[test]
    fn record_recent_file_moves_an_existing_entry_to_the_front_without_duplicating() {
        let mut state = ProjectNavigationState::default();
        state.record_recent_file(PathBuf::from("a.rs"));
        state.record_recent_file(PathBuf::from("b.rs"));
        state.record_recent_file(PathBuf::from("a.rs"));

        assert_eq!(
            state.recent_files,
            vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]
        );
    }

    #[test]
    fn record_recent_file_caps_at_max_recent_files() {
        let mut state = ProjectNavigationState::default();
        for i in 0..(MAX_RECENT_FILES + 5) {
            state.record_recent_file(PathBuf::from(format!("{i}.rs")));
        }
        assert_eq!(state.recent_files.len(), MAX_RECENT_FILES);
        // Most recently inserted survives at the front.
        assert_eq!(
            state.recent_files[0],
            PathBuf::from(format!("{}.rs", MAX_RECENT_FILES + 4))
        );
    }

    #[test]
    fn toggle_bookmark_adds_then_removes() {
        let mut state = ProjectNavigationState::default();
        let path = PathBuf::from("a.rs");

        assert!(state.toggle_bookmark(path.clone(), 3));
        assert_eq!(
            state.bookmarks,
            vec![Bookmark {
                path: path.clone(),
                line: 3
            }]
        );

        assert!(!state.toggle_bookmark(path.clone(), 3));
        assert!(state.bookmarks.is_empty());
    }

    #[test]
    fn toggle_bookmark_treats_different_lines_in_the_same_file_independently() {
        let mut state = ProjectNavigationState::default();
        let path = PathBuf::from("a.rs");
        state.toggle_bookmark(path.clone(), 1);
        state.toggle_bookmark(path.clone(), 2);

        assert_eq!(state.bookmarks.len(), 2);
        state.toggle_bookmark(path.clone(), 1);
        assert_eq!(state.bookmarks, vec![Bookmark { path, line: 2 }]);
    }

    #[test]
    fn load_from_a_fresh_project_returns_the_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), ProjectNavigationState::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = ProjectNavigationState::default();
        state.record_recent_file(PathBuf::from("a.rs"));
        state.toggle_bookmark(PathBuf::from("b.rs"), 7);

        save(dir.path(), &state);

        assert_eq!(load(dir.path()), state);
    }

    #[test]
    fn load_on_malformed_json_returns_the_default_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ide")).unwrap();
        std::fs::write(dir.path().join(".ide/navigation.json"), b"{ not json").unwrap();

        assert_eq!(load(dir.path()), ProjectNavigationState::default());
    }

    #[test]
    fn save_on_an_unresolvable_parent_is_a_silent_no_op() {
        let missing = PathBuf::from("/definitely/does/not/exist/ide-tui-test-project");
        // Must not panic.
        save(&missing, &ProjectNavigationState::default());
    }
}
