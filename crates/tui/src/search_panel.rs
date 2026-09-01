//! Off-thread global search, ported near-verbatim from `crates/ui/src/
//! search_panel.rs` -- same "spawn a thread, poll a channel once per
//! frame" shape `CargoPanel`/`LspBridge` already use in this crate. See
//! `docs/features/tui-find-in-path.md` §2.1 for why `discard_in_flight`
//! isn't ported: `ide-tui` has no project-switch feature, the only
//! reason `ide-ui` ever needs it.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

#[derive(Default)]
pub(crate) struct SearchPanel {
    pub(crate) results: Option<ide_core::SearchResults>,
    pub(crate) searching: bool,
    /// Tags each spawned search; bumped only by `run`. A result is only
    /// written into `results` if its tag still matches -- with no
    /// `discard_in_flight` in this crate, this only ever protects against
    /// a result that arrives after `run` itself was never called again
    /// (i.e. it's always still current by the time it lands), but kept
    /// for parity with `ide-ui`'s own state machine rather than trimmed
    /// away as apparently-dead.
    generation: u64,
    rx: Option<Receiver<(u64, ide_core::SearchResults)>>,
}

impl SearchPanel {
    /// No-op if a search is already running (v1 runs at most one search
    /// at a time) -- this no-op path leaves everything, including the
    /// generation counter, untouched; the already-running search
    /// continues uninterrupted. Otherwise spawns a background thread
    /// running `ide_core::search_tree(&tree, &query)`, sets `searching =
    /// true`, and increments the generation counter, tagging this search
    /// with the new value.
    pub(crate) fn run(&mut self, tree: ide_core::DirEntry, query: String) {
        if self.searching {
            return;
        }
        self.searching = true;
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let results = ide_core::search_tree(&tree, &query);
            let _ = tx.send((generation, results));
        });
        self.rx = Some(rx);
    }

    /// Drains the result channel if the background search has finished:
    /// clears `searching` unconditionally (the background work is done
    /// either way), and sets `results` only if the result's generation
    /// still matches the current one. Returns `true` if anything changed.
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
    use ide_core::{DirEntry, DirEntryKind, SearchResults};
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

    fn dummy_results() -> SearchResults {
        SearchResults {
            matches: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn run_while_searching_is_a_noop() {
        let mut panel = SearchPanel {
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
        let mut panel = SearchPanel {
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
        // A hypothetical superseded generation -- exercised directly here
        // since nothing in this crate's own call sites can produce this
        // case without `discard_in_flight` (not ported, §2.1), but the
        // underlying `poll` logic still guards against it structurally.
        tx.send((1, dummy_results())).unwrap();
        let mut panel = SearchPanel {
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
        let (tx, rx) = mpsc::channel::<(u64, SearchResults)>();
        drop(tx);
        let mut panel = SearchPanel {
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
