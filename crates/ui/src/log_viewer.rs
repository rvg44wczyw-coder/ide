use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use ide_core::git::{CommitDetail, CommitLogFilter, CommitNode, FileDiff, GitRepo};

const LOG_LIMIT: usize = 500;

#[derive(Default)]
pub(crate) struct LogViewerPanel {
    pub(crate) commits: Vec<CommitNode>,
    pub(crate) selected: Option<usize>,
    pub(crate) detail: Option<CommitDetail>,
    pub(crate) diff: Option<Vec<FileDiff>>,
    pub(crate) filter: CommitLogFilter,
    pub(crate) loading: bool,
    generation: u64,
    rx: Option<Receiver<(u64, Vec<CommitNode>)>>,
    repo_root: Option<PathBuf>,
}

impl LogViewerPanel {
    pub(crate) fn run(&mut self, root: PathBuf, filter: CommitLogFilter) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = mpsc::channel();
        self.repo_root = Some(root.clone());
        thread::spawn(move || {
            let result = GitRepo::open(&root)
                .ok()
                .and_then(|repo| repo.commit_graph(LOG_LIMIT, &filter).ok())
                .unwrap_or_default();
            let _ = tx.send((generation, result));
        });
        self.rx = Some(rx);
    }

    pub(crate) fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        match rx.try_recv() {
            Ok((generation, commits)) => {
                self.rx = None;
                self.loading = false;
                if generation == self.generation {
                    self.commits = commits;
                    self.selected = None;
                    self.detail = None;
                    self.diff = None;
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                self.loading = false;
                true
            }
        }
    }

    pub(crate) fn select_commit(&mut self, index: usize) {
        if index >= self.commits.len() {
            return;
        }
        self.selected = Some(index);
        let commit_id = self.commits[index].id.clone();
        if let Some(root) = &self.repo_root {
            if let Ok(repo) = GitRepo::open(root) {
                self.detail = repo.commit_detail(&commit_id).ok();
                self.diff = repo.diff_commit(&commit_id).ok();
            }
        }
    }

    pub(crate) fn has_repo(&self) -> bool {
        self.repo_root.is_some()
    }
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

    fn init_git_repo(dir: &std::path::Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("file.txt"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn run_while_loading_is_a_noop() {
        let mut panel = LogViewerPanel {
            loading: true,
            generation: 3,
            ..Default::default()
        };
        panel.run(PathBuf::from("/nonexistent"), CommitLogFilter::default());
        assert_eq!(panel.generation, 3);
        assert!(panel.rx.is_none());
    }

    #[test]
    fn poll_with_nothing_running_returns_false() {
        let mut panel = LogViewerPanel::default();
        assert!(!panel.poll());
    }

    #[test]
    fn run_and_poll_on_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());

        let project = ide_core::Project::open(dir.path()).unwrap();
        let root = project.root().to_path_buf();

        let mut panel = LogViewerPanel::default();
        panel.run(root, CommitLogFilter::default());
        assert!(panel.loading);

        wait_until(|| {
            panel.poll();
            !panel.loading
        });

        assert!(!panel.commits.is_empty());
        assert_eq!(panel.commits[0].summary, "initial");
    }

    #[test]
    fn select_commit_loads_detail_and_diff() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());

        let project = ide_core::Project::open(dir.path()).unwrap();
        let root = project.root().to_path_buf();

        let mut panel = LogViewerPanel::default();
        panel.run(root, CommitLogFilter::default());

        wait_until(|| {
            panel.poll();
            !panel.loading
        });

        panel.select_commit(0);
        assert!(panel.detail.is_some());
        assert_eq!(panel.detail.as_ref().unwrap().summary, "initial");
    }
}
