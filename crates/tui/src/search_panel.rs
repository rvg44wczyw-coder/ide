//! Off-thread Search and Replace in Path (`docs/features/tui-search-and-
//! replace-in-path.md`), the T37 upgrade of T15's Find in Path panel.
//! Reworked in place rather than added as a new sibling module the way
//! `ide-ui`'s own C7 phase did (`search-in-path-v2.md` §2.2) -- this
//! crate's `SearchPanel` has no second consumer to preserve (`ide-tui`'s
//! `todo_panel.rs`, T24, depends on `ide_core::search_tree` the free
//! function directly, never this struct), so a parallel module would be
//! immediately dead code. Two independent generation-counter state
//! machines, one per op (§3.1 of the doc above), same "spawn a thread,
//! poll a channel once per frame" shape `CargoPanel`/`LspBridge` already
//! use in this crate. `discard_in_flight` is still not ported for either
//! op -- `ide-tui` has no project-switch feature, the only reason
//! `ide-ui` ever needs it.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use ide_core::{
    DirEntry, PathSearchError, PathSearchOptions, PathSearchResults, ReplaceInPathResult,
};

#[derive(Default)]
pub(crate) struct SearchPanel {
    pub(crate) results: Option<PathSearchResults>,
    /// Mutually exclusive with `results` -- an arriving payload always
    /// replaces exactly one of the two, matching `render_find_bar`'s
    /// existing "error replaces content" convention.
    pub(crate) error: Option<PathSearchError>,
    pub(crate) searching: bool,
    generation: u64,
    rx: Option<Receiver<(u64, Result<PathSearchResults, PathSearchError>)>>,

    pub(crate) replace_preview: Option<ReplaceInPathResult>,
    pub(crate) replace_error: Option<PathSearchError>,
    pub(crate) replacing: bool,
    replace_generation: u64,
    replace_rx: Option<Receiver<(u64, Result<ReplaceInPathResult, PathSearchError>)>>,
}

impl SearchPanel {
    /// No-op if a search is already running. Otherwise spawns a background
    /// thread running `ide_core::search_tree_advanced(&tree, &query,
    /// &options)`, sets `searching = true`, and increments the generation
    /// counter, tagging this search with the new value.
    pub(crate) fn run(&mut self, tree: DirEntry, query: String, options: PathSearchOptions) {
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

    /// Drains the search result channel if the background search has
    /// finished: clears `searching` unconditionally, and writes the
    /// arriving payload into `results` (clearing `error`) or `error`
    /// (clearing `results`) only if its generation still matches current.
    /// Returns `true` if anything changed.
    pub(crate) fn poll(&mut self) -> bool {
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
                        Err(err) => {
                            self.error = Some(err);
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

    /// Second, fully independent instance of `run`'s exact shape, calling
    /// `ide_core::replace_in_path` instead.
    pub(crate) fn run_replace(
        &mut self,
        tree: DirEntry,
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

    /// Second, fully independent instance of `poll`'s exact shape.
    pub(crate) fn poll_replace(&mut self) -> bool {
        let Some(rx) = &self.replace_rx else {
            return false;
        };
        match rx.try_recv() {
            Ok((generation, result)) => {
                self.replace_rx = None;
                self.replacing = false;
                if generation == self.replace_generation {
                    match result {
                        Ok(preview) => {
                            self.replace_preview = Some(preview);
                            self.replace_error = None;
                        }
                        Err(err) => {
                            self.replace_error = Some(err);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::{DirEntryKind, PathSearchMatch};
    use std::path::PathBuf;
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

    fn default_options() -> PathSearchOptions {
        PathSearchOptions {
            search: Default::default(),
            include: Vec::new(),
            exclude: Vec::new(),
            respect_gitignore: false,
        }
    }

    fn dummy_results() -> PathSearchResults {
        PathSearchResults {
            matches: Vec::new(),
            truncated: false,
        }
    }

    fn dummy_error() -> PathSearchError {
        ide_core::search_tree_advanced(
            &empty_tree(),
            "x",
            &PathSearchOptions {
                search: Default::default(),
                include: vec!["[".to_string()],
                exclude: Vec::new(),
                respect_gitignore: false,
            },
        )
        .expect_err("an unclosed glob bracket must fail to compile")
    }

    #[test]
    fn run_while_searching_is_a_noop() {
        let mut panel = SearchPanel {
            searching: true,
            generation: 5,
            ..Default::default()
        };
        panel.run(empty_tree(), "x".to_string(), default_options());
        assert_eq!(panel.generation, 5);
        assert!(panel.rx.is_none());
        assert!(panel.searching);
    }

    #[test]
    fn poll_with_nothing_running_returns_false() {
        let mut panel = SearchPanel::default();
        assert!(!panel.poll());
    }

    #[test]
    fn run_and_poll_eventually_yields_matching_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle here").unwrap();
        let project = ide_core::Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let mut panel = SearchPanel::default();
        panel.run(tree, "needle".to_string(), default_options());
        assert!(panel.searching);

        wait_until(|| {
            panel.poll();
            !panel.searching
        });

        assert_eq!(panel.results.unwrap().matches.len(), 1);
    }

    #[test]
    fn poll_accepts_a_result_matching_the_current_generation() {
        let (tx, rx) = mpsc::channel();
        tx.send((1, Ok(dummy_results()))).unwrap();
        let mut panel = SearchPanel {
            generation: 1,
            searching: true,
            rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll());
        assert!(!panel.searching);
        assert!(panel.results.is_some());
        assert!(panel.error.is_none());
    }

    #[test]
    fn poll_accepts_an_error_matching_the_current_generation_and_clears_results() {
        let (tx, rx) = mpsc::channel();
        tx.send((1, Err(dummy_error()))).unwrap();
        let mut panel = SearchPanel {
            generation: 1,
            searching: true,
            rx: Some(rx),
            results: Some(dummy_results()),
            ..Default::default()
        };

        assert!(panel.poll());
        assert!(panel.results.is_none());
        assert!(panel.error.is_some());
    }

    #[test]
    fn poll_drops_a_stale_generation_result_but_still_clears_searching() {
        let (tx, rx) = mpsc::channel();
        tx.send((1, Ok(dummy_results()))).unwrap();
        let mut panel = SearchPanel {
            generation: 2,
            searching: true,
            rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll());
        assert!(!panel.searching);
        assert!(panel.results.is_none());
        assert!(panel.error.is_none());
    }

    #[test]
    fn poll_on_a_disconnected_channel_clears_searching_without_setting_results() {
        let (tx, rx) = mpsc::channel::<(u64, Result<PathSearchResults, PathSearchError>)>();
        drop(tx);
        let mut panel = SearchPanel {
            generation: 1,
            searching: true,
            rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll());
        assert!(!panel.searching);
        assert!(panel.results.is_none());
    }

    #[test]
    fn run_replace_while_replacing_is_a_noop() {
        let mut panel = SearchPanel {
            replacing: true,
            replace_generation: 5,
            ..Default::default()
        };
        panel.run_replace(
            empty_tree(),
            "x".to_string(),
            "y".to_string(),
            default_options(),
        );
        assert_eq!(panel.replace_generation, 5);
        assert!(panel.replace_rx.is_none());
        assert!(panel.replacing);
    }

    #[test]
    fn poll_replace_with_nothing_running_returns_false() {
        let mut panel = SearchPanel::default();
        assert!(!panel.poll_replace());
    }

    #[test]
    fn run_replace_and_poll_replace_eventually_yields_a_preview() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle here").unwrap();
        let project = ide_core::Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let mut panel = SearchPanel::default();
        panel.run_replace(
            tree,
            "needle".to_string(),
            "gone".to_string(),
            default_options(),
        );
        assert!(panel.replacing);

        wait_until(|| {
            panel.poll_replace();
            !panel.replacing
        });

        assert_eq!(panel.replace_preview.unwrap().edit.edits.len(), 1);
    }

    #[test]
    fn poll_replace_accepts_a_result_matching_the_current_generation() {
        let preview = ReplaceInPathResult {
            edit: ide_core::WorkspaceEdit { edits: Vec::new() },
            truncated: false,
        };
        let (tx, rx) = mpsc::channel();
        tx.send((1, Ok(preview))).unwrap();
        let mut panel = SearchPanel {
            replace_generation: 1,
            replacing: true,
            replace_rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll_replace());
        assert!(!panel.replacing);
        assert!(panel.replace_preview.is_some());
        assert!(panel.replace_error.is_none());
    }

    #[test]
    fn poll_replace_accepts_an_error_matching_the_current_generation() {
        let (tx, rx) = mpsc::channel();
        tx.send((1, Err(dummy_error()))).unwrap();
        let mut panel = SearchPanel {
            replace_generation: 1,
            replacing: true,
            replace_rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll_replace());
        assert!(panel.replace_preview.is_none());
        assert!(panel.replace_error.is_some());
    }

    #[test]
    fn poll_replace_drops_a_stale_generation_result_but_still_clears_replacing() {
        let preview = ReplaceInPathResult {
            edit: ide_core::WorkspaceEdit { edits: Vec::new() },
            truncated: false,
        };
        let (tx, rx) = mpsc::channel();
        tx.send((1, Ok(preview))).unwrap();
        let mut panel = SearchPanel {
            replace_generation: 2,
            replacing: true,
            replace_rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll_replace());
        assert!(!panel.replacing);
        assert!(panel.replace_preview.is_none());
    }

    #[test]
    fn poll_replace_on_a_disconnected_channel_clears_replacing() {
        let (tx, rx) = mpsc::channel::<(u64, Result<ReplaceInPathResult, PathSearchError>)>();
        drop(tx);
        let mut panel = SearchPanel {
            replace_generation: 1,
            replacing: true,
            replace_rx: Some(rx),
            ..Default::default()
        };

        assert!(panel.poll_replace());
        assert!(!panel.replacing);
        assert!(panel.replace_preview.is_none());
    }

    #[test]
    fn dummy_match_field_shape_sanity() {
        // Not a behavior test -- just confirms the imported `PathSearchMatch`
        // shape used implicitly by `search_tree_advanced` above still
        // matches what this module expects, so a core-side rename would
        // fail this file's compilation rather than silently drifting.
        let _: fn(PathSearchMatch) = |_| {};
    }
}
