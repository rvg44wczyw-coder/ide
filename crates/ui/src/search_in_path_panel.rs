//! Off-thread Search/Replace in Path (`docs/features/search-in-path-v2.md`
//! §2.2/§3.1), the C7 upgrade of the Find in Path tool window. Deliberately
//! a sibling module to `search_panel.rs`, not a rework of it in place --
//! `search_panel::SearchPanel` is still used unchanged by the Search
//! Everywhere popup's Text tab (`IdeApp::search_everywhere_text`), which
//! stays on `ide_core::search_tree`/`SearchResults` per this doc's own §1
//! ("the existing `ide_core::search_tree`/... stay exactly as they are").
//! Reworking `SearchPanel` in place would have changed that unrelated
//! feature's types too, so this module follows the same "sibling module,
//! not a shared generic" precedent `files_search.rs`'s own header comment
//! already establishes for `FilesSearchPanel` vs. `SearchPanel`.
//!
//! Two independent instances of the same generation-counter state machine
//! `search_panel::SearchPanel` established (§3.1): one for
//! `search_tree_advanced`, one for `replace_in_path`, each with its own
//! single-in-flight-at-a-time + stale-result-discard contract.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use ide_core::{PathSearchError, PathSearchOptions, PathSearchResults, ReplaceInPathResult};

#[derive(Default)]
pub struct PathSearchPanel {
    pub results: Option<PathSearchResults>,
    /// Set instead of `results` when the last search failed to compile
    /// (bad regex or bad glob, doc §3.5) -- mutually exclusive with a
    /// non-empty `results`, mirroring `FindBar::error`'s convention.
    pub error: Option<PathSearchError>,
    pub searching: bool,
    /// Per-file collapse state (doc §3.4): tracks *collapsed-out*
    /// exclusion by presence -- a path present in this set has been
    /// explicitly collapsed; a path absent (the default, before any click)
    /// is expanded, matching the old panel's always-expanded behavior for
    /// anyone who never interacts with the new control. `toggle_expanded`
    /// flips membership rather than storing an `expanded: bool` per path.
    pub expanded: HashSet<PathBuf>,
    generation: u64,
    rx: Option<Receiver<(u64, Result<PathSearchResults, PathSearchError>)>>,

    pub replace_preview: Option<ReplaceInPathResult>,
    pub replace_error: Option<PathSearchError>,
    pub replacing: bool,
    replace_generation: u64,
    replace_rx: Option<Receiver<(u64, Result<ReplaceInPathResult, PathSearchError>)>>,
}

impl PathSearchPanel {
    /// No-op if a search is already running (v1 runs at most one search at
    /// a time), same convention `SearchPanel::run` established. Otherwise
    /// spawns a background thread running `ide_core::search_tree_advanced`,
    /// sets `searching = true`, and increments the generation counter.
    pub fn run(&mut self, tree: ide_core::DirEntry, query: String, options: PathSearchOptions) {
        if self.searching {
            return;
        }
        self.searching = true;
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = ide_core::search_tree_advanced(&tree, &query, &options);
            let _ = tx.send((generation, result));
        });
        self.rx = Some(rx);
    }

    /// Drains the result channel if the background search has finished.
    /// On `Ok`, sets `results` and clears `error`; on `Err`, sets `error`
    /// and clears `results` (an error replaces stale content rather than
    /// leaving a previous successful result showing behind it). Returns
    /// `true` if anything changed. See `SearchPanel::poll`'s doc comment
    /// for the stale-generation-discard behavior this mirrors exactly.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        match rx.try_recv() {
            Ok((generation, result)) => {
                self.rx = None;
                self.searching = false;
                if generation == self.generation {
                    match result {
                        Ok(results) => {
                            self.results = Some(results);
                            self.error = None;
                        }
                        Err(e) => {
                            self.error = Some(e);
                            self.results = None;
                        }
                    }
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                self.searching = false;
                true
            }
        }
    }

    /// Bumps the generation counter without starting a new search, so a
    /// currently in-flight search's eventual result is discarded by `poll`.
    /// Mirrors `SearchPanel::discard_in_flight` exactly.
    pub fn discard_in_flight(&mut self) {
        self.generation += 1;
    }

    /// The replace-preview op's `run`, independent of the search op above
    /// (doc §3.1 -- a user could adjust the query while a stale
    /// replace-preview computation is still in flight).
    pub fn run_replace(
        &mut self,
        tree: ide_core::DirEntry,
        query: String,
        replacement: String,
        options: PathSearchOptions,
    ) {
        if self.replacing {
            return;
        }
        self.replacing = true;
        self.replace_generation += 1;
        let generation = self.replace_generation;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = ide_core::replace_in_path(&tree, &query, &replacement, &options);
            let _ = tx.send((generation, result));
        });
        self.replace_rx = Some(rx);
    }

    pub fn poll_replace(&mut self) -> bool {
        let Some(rx) = &self.replace_rx else {
            return false;
        };
        match rx.try_recv() {
            Ok((generation, result)) => {
                self.replace_rx = None;
                self.replacing = false;
                if generation == self.replace_generation {
                    match result {
                        Ok(result) => {
                            self.replace_preview = Some(result);
                            self.replace_error = None;
                        }
                        Err(e) => {
                            self.replace_error = Some(e);
                            self.replace_preview = None;
                        }
                    }
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.replace_rx = None;
                self.replacing = false;
                true
            }
        }
    }

    pub fn discard_replace_in_flight(&mut self) {
        self.replace_generation += 1;
    }

    pub fn toggle_expanded(&mut self, path: &Path) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::buffer_search::SearchOptions;
    use ide_core::{DirEntry, DirEntryKind};
    use std::time::{Duration, Instant};

    fn wait_until<F: FnMut() -> bool>(mut condition: F) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "condition did not become true in time"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn empty_tree() -> DirEntry {
        DirEntry {
            name: "root".to_string(),
            path: PathBuf::from("/root"),
            kind: DirEntryKind::Dir,
            children: Vec::new(),
        }
    }

    fn opts() -> PathSearchOptions {
        PathSearchOptions {
            search: SearchOptions::default(),
            include: Vec::new(),
            exclude: Vec::new(),
            respect_gitignore: true,
        }
    }

    fn dummy_results() -> PathSearchResults {
        PathSearchResults {
            matches: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn run_while_searching_is_a_noop() {
        let mut panel = PathSearchPanel {
            searching: true,
            generation: 5,
            ..Default::default()
        };
        panel.run(empty_tree(), "x".to_string(), opts());
        assert_eq!(panel.generation, 5);
        assert!(panel.rx.is_none());
        assert!(panel.searching);
    }

    #[test]
    fn poll_with_nothing_running_returns_false() {
        let mut panel = PathSearchPanel::default();
        assert!(!panel.poll());
    }

    #[test]
    fn run_and_poll_eventually_yields_matching_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle here").unwrap();
        let project = ide_core::Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let mut panel = PathSearchPanel::default();
        panel.run(tree, "needle".to_string(), opts());
        assert!(panel.searching);

        wait_until(|| {
            panel.poll();
            !panel.searching
        });

        assert_eq!(panel.results.unwrap().matches.len(), 1);
        assert!(panel.error.is_none());
    }

    #[test]
    fn run_with_an_invalid_regex_surfaces_an_error_not_empty_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle here").unwrap();
        let project = ide_core::Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let mut panel = PathSearchPanel::default();
        let mut options = opts();
        options.search.regex = true;
        panel.run(tree, "(".to_string(), options);

        wait_until(|| {
            panel.poll();
            !panel.searching
        });

        assert!(panel.results.is_none());
        assert!(panel.error.is_some());
    }

    #[test]
    fn poll_accepts_a_result_matching_the_current_generation() {
        let (tx, rx) = mpsc::channel();
        tx.send((1, Ok(dummy_results()))).unwrap();
        let mut panel = PathSearchPanel {
            searching: true,
            generation: 1,
            rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll());
        assert!(!panel.searching);
        assert!(panel.results.is_some());
    }

    #[test]
    fn poll_drops_a_stale_generation_result_but_still_clears_searching() {
        let (tx, rx) = mpsc::channel();
        tx.send((1, Ok(dummy_results()))).unwrap();
        let mut panel = PathSearchPanel {
            searching: true,
            generation: 2,
            rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll());
        assert!(!panel.searching);
        assert!(panel.results.is_none());
    }

    #[test]
    fn discard_in_flight_bumps_generation_and_leaves_searching_untouched() {
        let mut panel = PathSearchPanel {
            searching: true,
            generation: 3,
            rx: Some(mpsc::channel().1),
            ..Default::default()
        };
        panel.discard_in_flight();
        assert_eq!(panel.generation, 4);
        assert!(panel.searching);

        let mut idle = PathSearchPanel::default();
        idle.discard_in_flight();
        assert_eq!(idle.generation, 1);
        assert!(!idle.searching);
    }

    #[test]
    fn run_replace_while_replacing_is_a_noop() {
        let mut panel = PathSearchPanel {
            replacing: true,
            replace_generation: 5,
            ..Default::default()
        };
        panel.run_replace(empty_tree(), "x".to_string(), "y".to_string(), opts());
        assert_eq!(panel.replace_generation, 5);
        assert!(panel.replace_rx.is_none());
        assert!(panel.replacing);
    }

    #[test]
    fn poll_replace_with_nothing_running_returns_false() {
        let mut panel = PathSearchPanel::default();
        assert!(!panel.poll_replace());
    }

    #[test]
    fn run_replace_and_poll_replace_eventually_yields_a_workspace_edit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle here").unwrap();
        let project = ide_core::Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let mut panel = PathSearchPanel::default();
        panel.run_replace(tree, "needle".to_string(), "found".to_string(), opts());
        assert!(panel.replacing);

        wait_until(|| {
            panel.poll_replace();
            !panel.replacing
        });

        let result = panel.replace_preview.unwrap();
        assert_eq!(result.edit.edits.len(), 1);
        assert!(panel.replace_error.is_none());
    }

    #[test]
    fn run_replace_with_an_invalid_glob_surfaces_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle here").unwrap();
        let project = ide_core::Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let mut panel = PathSearchPanel::default();
        let mut options = opts();
        options.include = vec!["[".to_string()];
        panel.run_replace(tree, "needle".to_string(), "found".to_string(), options);

        wait_until(|| {
            panel.poll_replace();
            !panel.replacing
        });

        assert!(panel.replace_preview.is_none());
        assert!(panel.replace_error.is_some());
    }

    #[test]
    fn poll_replace_accepts_a_result_matching_the_current_generation() {
        let (tx, rx) = mpsc::channel();
        let result = ReplaceInPathResult {
            edit: ide_core::WorkspaceEdit { edits: Vec::new() },
            truncated: false,
        };
        tx.send((1, Ok(result))).unwrap();
        let mut panel = PathSearchPanel {
            replacing: true,
            replace_generation: 1,
            replace_rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll_replace());
        assert!(!panel.replacing);
        assert!(panel.replace_preview.is_some());
    }

    #[test]
    fn discard_replace_in_flight_bumps_generation_and_leaves_replacing_untouched() {
        let mut panel = PathSearchPanel {
            replacing: true,
            replace_generation: 3,
            replace_rx: Some(mpsc::channel().1),
            ..Default::default()
        };
        panel.discard_replace_in_flight();
        assert_eq!(panel.replace_generation, 4);
        assert!(panel.replacing);
    }

    #[test]
    fn toggle_expanded_flips_membership() {
        let mut panel = PathSearchPanel::default();
        let path = PathBuf::from("/root/f.txt");
        assert!(!panel.expanded.contains(&path));
        panel.toggle_expanded(&path);
        assert!(panel.expanded.contains(&path));
        panel.toggle_expanded(&path);
        assert!(!panel.expanded.contains(&path));
    }
}
