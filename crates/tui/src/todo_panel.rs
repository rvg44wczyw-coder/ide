//! TODO/FIXME/HACK panel (`docs/features/tui-todo-panel.md`) -- built
//! directly on `ide_core::search_tree`, one call per literal pattern,
//! merged and sorted by `(path, line, column)`. Same "spawn a thread, poll
//! a channel once per frame" shape `search_panel.rs`/`files_search.rs`
//! already establish for a whole-project scan.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use ide_core::{search_tree, DirEntry, SearchMatch};

/// Literal, non-configurable in v1 (`docs/features/tui-todo-panel.md`
/// §1.1) -- G5's "настраиваемые паттерны" needs a settings UI this crate
/// doesn't have yet.
pub(crate) const TODO_PATTERNS: [&str; 3] = ["TODO", "FIXME", "HACK"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TodoMatch {
    pub(crate) pattern: &'static str,
    pub(crate) inner: SearchMatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TodoResults {
    pub(crate) matches: Vec<TodoMatch>,
    /// `true` if *any* of the three per-pattern `search_tree` calls hit
    /// `MAX_SEARCH_RESULTS` and stopped early.
    pub(crate) truncated: bool,
}

#[derive(Default)]
pub(crate) struct TodoPanel {
    pub(crate) results: Option<TodoResults>,
    pub(crate) searching: bool,
    /// Tags each spawned scan; bumped only by `run`. Mirrors
    /// `SearchPanel`'s own generation counter for parity, even though
    /// (like that one) nothing in this crate currently calls `run` a
    /// second time before the first finishes.
    generation: u64,
    rx: Option<Receiver<(u64, TodoResults)>>,
}

impl TodoPanel {
    /// No-op if a scan is already running (v1 runs at most one at a time).
    /// Otherwise spawns a background thread calling `search_tree` once per
    /// `TODO_PATTERNS` entry, tagging each resulting match with which
    /// pattern produced it, then sorting the combined list by `(path,
    /// line, column)` -- the same three-key sort `App::
    /// flattened_diagnostics` already uses for Problems.
    pub(crate) fn run(&mut self, tree: DirEntry) {
        if self.searching {
            return;
        }
        self.searching = true;
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut matches = Vec::new();
            let mut truncated = false;
            for pattern in TODO_PATTERNS {
                let found = search_tree(&tree, pattern);
                truncated |= found.truncated;
                matches.extend(
                    found
                        .matches
                        .into_iter()
                        .map(|inner| TodoMatch { pattern, inner }),
                );
            }
            matches.sort_by(|a, b| {
                a.inner
                    .path
                    .cmp(&b.inner.path)
                    .then(a.inner.line.cmp(&b.inner.line))
                    .then(a.inner.column.cmp(&b.inner.column))
            });
            let _ = tx.send((generation, TodoResults { matches, truncated }));
        });
        self.rx = Some(rx);
    }

    /// Drains the result channel if the background scan has finished:
    /// clears `searching` unconditionally, sets `results` only if the
    /// result's generation still matches the current one. Returns `true`
    /// if anything changed.
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
    use ide_core::DirEntryKind;
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

    #[test]
    fn run_while_searching_is_a_noop() {
        let mut panel = TodoPanel {
            results: None,
            searching: true,
            generation: 5,
            rx: None,
        };
        panel.run(empty_tree());
        assert_eq!(panel.generation, 5);
        assert!(panel.rx.is_none());
        assert!(panel.searching);
    }

    #[test]
    fn poll_with_nothing_running_returns_false() {
        let mut panel = TodoPanel::default();
        assert!(!panel.poll());
    }

    #[test]
    fn run_and_poll_merges_matches_from_every_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "// TODO: fix this\nfn f() {}").unwrap();
        std::fs::write(dir.path().join("b.py"), "# FIXME later\n# also a HACK here").unwrap();
        let project = ide_core::Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let mut panel = TodoPanel::default();
        panel.run(tree);
        assert!(panel.searching);

        wait_until(|| {
            panel.poll();
            !panel.searching
        });

        let results = panel.results.unwrap();
        assert!(!results.truncated);
        assert_eq!(results.matches.len(), 3);
        // Sorted by path -- every `a.rs` row before every `b.py` row.
        assert_eq!(results.matches[0].pattern, "TODO");
        assert_eq!(
            results.matches[0].inner.path,
            dir.path().canonicalize().unwrap().join("a.rs")
        );
        let patterns_in_b: Vec<_> = results.matches[1..].iter().map(|m| m.pattern).collect();
        assert_eq!(patterns_in_b, vec!["FIXME", "HACK"]);
    }

    #[test]
    fn poll_accepts_a_result_matching_the_current_generation() {
        let (tx, rx) = mpsc::channel();
        tx.send((1, TodoResults::default())).unwrap();
        let mut panel = TodoPanel {
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
        tx.send((1, TodoResults::default())).unwrap();
        let mut panel = TodoPanel {
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
        let (tx, rx) = mpsc::channel::<(u64, TodoResults)>();
        drop(tx);
        let mut panel = TodoPanel {
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
