//! Off-thread fuzzy file search for the Search Everywhere `Files` tab --
//! structurally a straight copy of `search_panel.rs` with
//! `ide_core::fuzzy_match_files` swapped in for `ide_core::search_tree`
//! (deliberately a sibling module, not a shared generic — see
//! `docs/features/search-everywhere.md` §4). See that same doc's §2.2/§3.2
//! for the generation-counter state machine this implements.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

#[derive(Default)]
pub struct FilesSearchPanel {
    pub results: Option<ide_core::FuzzyFileResults>,
    pub searching: bool,
    /// Tags each spawned search; bumped by `run` (when it actually starts
    /// one) and `discard_in_flight` -- the only two increment points. A
    /// result is only written into `results` if its tag still matches.
    generation: u64,
    rx: Option<Receiver<(u64, ide_core::FuzzyFileResults)>>,
}

impl FilesSearchPanel {
    /// No-op if a search is already running (v1 runs at most one search at
    /// a time) -- this no-op path leaves everything, including the
    /// generation counter, untouched; the already-running search continues
    /// uninterrupted. Otherwise spawns a background thread running
    /// `ide_core::fuzzy_match_files(&tree, &query)`, sets `searching =
    /// true`, and increments the generation counter, tagging this search
    /// with the new value.
    pub fn run(&mut self, tree: ide_core::DirEntry, query: String) {
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

    /// Drains the result channel if the background search has finished:
    /// clears `searching` unconditionally (the background work is done
    /// either way), and sets `results` only if the result's generation
    /// still matches the current one -- a result superseded by
    /// `discard_in_flight` is dropped without touching `results`, but
    /// still clears `searching` so a later `run` isn't blocked forever by
    /// a stale in-flight search that nothing will ever supersede again.
    /// Returns `true` if anything changed.
    pub fn poll(&mut self) -> bool {
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

    /// Bumps the generation counter without starting a new search, so a
    /// currently in-flight search's eventual result is discarded by `poll`
    /// rather than overwriting `results` with a stale project's matches.
    /// Does **not** stop the background thread (mirrors
    /// `SearchPanel::discard_in_flight`'s own precedent) -- the thread
    /// runs to completion and its result is simply ignored on arrival.
    /// Leaves `searching` untouched either way.
    pub fn discard_in_flight(&mut self) {
        self.generation += 1;
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
        std::fs::write(dir.path().join("needle.txt"), "x").unwrap();
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
        // Generation 1 was superseded by a `discard_in_flight` bump to 2
        // before this (still in-flight) result arrived.
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
    fn discard_in_flight_bumps_generation_and_leaves_searching_untouched() {
        let mut panel = FilesSearchPanel {
            results: None,
            searching: true,
            generation: 3,
            rx: Some(mpsc::channel().1),
        };
        panel.discard_in_flight();
        assert_eq!(panel.generation, 4);
        assert!(panel.searching);

        let mut idle = FilesSearchPanel::default();
        idle.discard_in_flight();
        assert_eq!(idle.generation, 1);
        assert!(!idle.searching);
    }
}
