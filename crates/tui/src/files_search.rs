//! Off-thread fuzzy file search, structurally a sibling to
//! `search_panel.rs` (background thread + generation-tagged channel,
//! polled once per frame) wrapping `ide_core::fuzzy_match_files` instead
//! of `ide_core::search_tree` -- see `docs/features/
//! tui-go-to-file-and-symbol.md` §2.1 for why this is a sibling module
//! rather than a shared generic with `search_panel.rs`.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

#[derive(Default)]
pub(crate) struct FilesSearchPanel {
    pub(crate) results: Option<ide_core::FuzzyFileResults>,
    pub(crate) searching: bool,
    generation: u64,
    rx: Option<Receiver<(u64, ide_core::FuzzyFileResults)>>,
}

impl FilesSearchPanel {
    /// No-op if a search is already running. Otherwise spawns a
    /// background thread running `ide_core::fuzzy_match_files(&tree,
    /// &query)`, same generation-tagging discipline as
    /// `search_panel::SearchPanel::run`.
    pub(crate) fn run(&mut self, tree: ide_core::DirEntry, query: String) {
        if self.searching {
            return;
        }
        self.searching = true;
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let results = ide_core::fuzzy_match_files(&tree, &query);
            let _ = tx.send((generation, results));
        });
        self.rx = Some(rx);
    }

    /// Same shape as `search_panel::SearchPanel::poll`.
    pub(crate) fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        match rx.try_recv() {
            Ok((generation, results)) => {
                self.rx = None;
                self.searching = false;
                if generation == self.generation {
                    self.results = Some(results);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::{DirEntry, DirEntryKind, FuzzyFileResults};
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

    fn dummy_results() -> FuzzyFileResults {
        FuzzyFileResults {
            matches: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn run_while_searching_is_a_noop() {
        let mut panel = FilesSearchPanel {
            results: None,
            searching: true,
            generation: 5,
            rx: None,
        };
        panel.run(empty_tree(), "x".to_string());
        assert_eq!(panel.generation, 5);
        assert!(panel.rx.is_none());
        assert!(panel.searching);
    }

    #[test]
    fn poll_with_nothing_running_returns_false() {
        let mut panel = FilesSearchPanel::default();
        assert!(!panel.poll());
    }

    #[test]
    fn run_and_poll_eventually_yields_matching_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("needle.txt"), "").unwrap();
        let project = ide_core::Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let mut panel = FilesSearchPanel::default();
        panel.run(tree, "needle".to_string());
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
        tx.send((1, dummy_results())).unwrap();
        let mut panel = FilesSearchPanel {
            results: None,
            searching: true,
            generation: 1,
            rx: Some(rx),
        };

        assert!(panel.poll());
        assert!(!panel.searching);
        assert!(panel.results.is_some());
    }

    #[test]
    fn poll_drops_a_stale_generation_result_but_still_clears_searching() {
        let (tx, rx) = mpsc::channel();
        tx.send((1, dummy_results())).unwrap();
        let mut panel = FilesSearchPanel {
            results: None,
            searching: true,
            generation: 2,
            rx: Some(rx),
        };

        assert!(panel.poll());
        assert!(!panel.searching);
        assert!(panel.results.is_none());
    }

    #[test]
    fn poll_on_a_disconnected_channel_clears_searching_without_setting_results() {
        let (tx, rx) = mpsc::channel::<(u64, FuzzyFileResults)>();
        drop(tx);
        let mut panel = FilesSearchPanel {
            results: None,
            searching: true,
            generation: 1,
            rx: Some(rx),
        };

        assert!(panel.poll());
        assert!(!panel.searching);
        assert!(panel.results.is_none());
    }
}
