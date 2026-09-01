//! Background directory-tree scanning off the UI thread, mirroring
//! `claude_panel.rs`/`cargo_panel.rs`'s thread + `mpsc` + poll-once-per-
//! frame pattern (`async-tree-scan.md`).

use ide_core::{DirEntry, Project};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

type Runner = fn(&Path) -> Option<DirEntry>;

pub struct TreeScan {
    rx: Option<Receiver<Option<DirEntry>>>,
    runner: Runner,
}

impl Default for TreeScan {
    fn default() -> Self {
        Self::with_runner(scan_project_root)
    }
}

impl TreeScan {
    fn with_runner(runner: Runner) -> Self {
        Self { rx: None, runner }
    }

    /// Starts scanning `root` on a background thread. Replacing `self.rx`
    /// drops any previous `Receiver` -- a still-running superseded scan's
    /// eventual `send` then fails silently and is ignored (`doc §3.2`).
    pub fn start(&mut self, root: PathBuf) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let runner = self.runner;
        thread::spawn(move || {
            let _ = tx.send(runner(&root));
        });
    }

    pub fn is_scanning(&self) -> bool {
        self.rx.is_some()
    }

    /// Call once per frame. `Some(_)` the frame a scan finishes (whether
    /// or not the directory was still there, `doc §3.3`), `None` on every
    /// other frame.
    pub fn poll(&mut self) -> Option<Option<DirEntry>> {
        let Some(rx) = &self.rx else {
            return None;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.rx = None;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                Some(None)
            }
        }
    }
}

fn scan_project_root(root: &Path) -> Option<DirEntry> {
    Project::open(root).ok().map(|p| p.scan_tree())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn fake_tree(path: &Path) -> Option<DirEntry> {
        Some(DirEntry {
            name: "root".to_string(),
            path: path.to_path_buf(),
            kind: ide_core::DirEntryKind::Dir,
            children: Vec::new(),
        })
    }

    fn fake_none(_path: &Path) -> Option<DirEntry> {
        None
    }

    #[test]
    fn is_scanning_is_false_before_a_scan_starts() {
        let scan = TreeScan::with_runner(fake_tree);
        assert!(!scan.is_scanning());
    }

    #[test]
    fn poll_returns_none_when_nothing_is_in_flight() {
        let mut scan = TreeScan::with_runner(fake_tree);
        assert_eq!(scan.poll(), None);
    }

    #[test]
    fn start_then_poll_eventually_returns_the_scanned_tree() {
        let mut scan = TreeScan::with_runner(fake_tree);
        scan.start(PathBuf::from("/tmp/whatever"));
        assert!(scan.is_scanning());
        let mut result = None;
        wait_until(|| {
            result = scan.poll();
            result.is_some()
        });
        let tree = result.unwrap().expect("fake_tree always returns Some");
        assert_eq!(tree.name, "root");
        assert!(!scan.is_scanning());
    }

    #[test]
    fn a_failed_scan_yields_some_none() {
        let mut scan = TreeScan::with_runner(fake_none);
        scan.start(PathBuf::from("/no/such/dir"));
        let mut result = None;
        wait_until(|| {
            result = scan.poll();
            result.is_some()
        });
        assert_eq!(result, Some(None));
    }

    #[test]
    fn a_second_start_supersedes_the_first_in_flight_scan() {
        let mut scan = TreeScan::with_runner(fake_tree);
        scan.start(PathBuf::from("/first"));
        scan.start(PathBuf::from("/second"));
        let mut result = None;
        wait_until(|| {
            result = scan.poll();
            result.is_some()
        });
        let tree = result.unwrap().expect("fake_tree always returns Some");
        assert_eq!(tree.path, PathBuf::from("/second"));
        assert!(!scan.is_scanning());
    }

    #[test]
    fn default_runner_reopens_and_scans_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let tree = scan_project_root(dir.path()).unwrap();
        assert_eq!(tree.children.len(), 1);
    }

    #[test]
    fn default_runner_returns_none_for_a_missing_directory() {
        assert_eq!(
            scan_project_root(Path::new("/no/such/directory/ide-test-missing")),
            None
        );
    }
}
