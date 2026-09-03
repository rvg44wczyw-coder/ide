//! Background-thread-plus-`mpsc`-channel clone state for the standalone
//! Clone Repository popup (`docs/features/tui-git-clone.md` §2.1/§3.1).
//! Mirrors `crates/ui/src/clone_panel.rs`'s `CloneState` almost verbatim
//! -- same background-thread + `mpsc` poll-once-per-frame shape,
//! duplicated rather than shared, since `ide-tui` has no dependency on
//! `ide-ui`. Differs from that module in two ways the doc calls out: no
//! native folder picker (`destination` is a typed field, not a
//! `PathBuf`), and a successful clone has nothing to hand its resulting
//! path off to (`ide-tui` has no runtime project-switch capability), so
//! `done` persists on `ClonePanel` itself instead of being consumed once
//! by the caller.

use ide_core::git;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// Mirrors `ide_core::git::CloneProgress`, trimmed to the two fields the
/// popup actually displays -- same shape as `crates/ui/src/clone_panel.rs`'s
/// own `CloneProgress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CloneProgress {
    pub received_objects: usize,
    pub total_objects: usize,
}

impl From<git::CloneProgress> for CloneProgress {
    fn from(p: git::CloneProgress) -> Self {
        Self {
            received_objects: p.received_objects,
            total_objects: p.total_objects,
        }
    }
}

enum CloneEvent {
    Progress(CloneProgress),
    Done(Result<PathBuf, String>),
}

/// What `poll` hands back the one frame something changed -- same richer-
/// than-`bool` shape as `ide-ui`'s `ClonePollResult`, for the same reason
/// (the caller needs to tell a progress tick apart from the terminal
/// frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClonePollResult {
    Progress,
    Succeeded(PathBuf),
    Failed,
}

/// Which of the popup's two text fields `Tab`/`BackTab` currently target --
/// a 2-way instance of `git_panel.rs`'s existing `WorktreeAddField` shape
/// (`next`/`prev`, wrapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClonePanelField {
    #[default]
    Url,
    Destination,
}

impl ClonePanelField {
    pub fn next(self) -> Self {
        match self {
            Self::Url => Self::Destination,
            Self::Destination => Self::Url,
        }
    }
    pub fn prev(self) -> Self {
        self.next() // 2-way toggle: `Tab` and `Shift+Tab` both flip it.
    }
}

/// Always alive on `App` (`app.clone: ClonePanel`), visibility gated by
/// `App.clone_panel_open: bool` -- the same "always-alive `T`, separate
/// visibility flag" split `docs/features/tui-tool-window-docking.md` (T33)
/// established for the bottom-dock panels, applied here to a standalone
/// modal for the same underlying reason: a background clone must keep
/// running, and its progress must stay readable, across the popup being
/// closed and reopened (`docs/features/tui-git-clone.md` §3.4/§4).
#[derive(Default)]
pub struct ClonePanel {
    pub url: String,
    pub destination: String,
    pub field: ClonePanelField,
    pub progress: Option<CloneProgress>,
    pub error: Option<String>,
    /// Set on the frame a clone finishes successfully; cleared only when
    /// a new clone is started. Unlike `ide-ui`'s `ClonePollResult::
    /// Succeeded(PathBuf)` (consumed once, immediately, by `open_project`),
    /// `ide-tui` has nothing to hand this off to -- it has to persist
    /// somewhere the popup can keep displaying it.
    pub done: Option<PathBuf>,
    rx: Option<Receiver<CloneEvent>>,
}

impl ClonePanel {
    /// Distinct from `self.progress.is_some()` for the same reason
    /// `ide-ui`'s `CloneState::is_running` documents: a same-filesystem
    /// (local) clone can complete without ever driving the indexer
    /// progress callback.
    pub fn is_running(&self) -> bool {
        self.rx.is_some()
    }

    /// No-op if a clone is already in flight (`ide-ui`'s own `CloneState::
    /// start` convention) or if `url`/`destination` (trimmed) is empty --
    /// the latter is a purely local, no-thread-spawned short-circuit
    /// avoiding a guaranteed-immediate `GitError::EmptyUrl` round trip
    /// through a background thread for a case the popup can already see
    /// from its own text fields.
    pub fn start(&mut self) {
        if self.rx.is_some() {
            return;
        }
        let url = self.url.trim().to_string();
        let dest = self.destination.trim().to_string();
        if url.is_empty() || dest.is_empty() {
            return;
        }
        self.error = None;
        self.progress = None;
        self.done = None;

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let dest = PathBuf::from(dest);
        thread::spawn(move || {
            let result = git::clone_repo(&url, &dest, |p| {
                let _ = tx.send(CloneEvent::Progress(p.into()));
            });
            let done = match result {
                Ok(repo) => Ok(repo.workdir().to_path_buf()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(CloneEvent::Done(done));
        });
    }

    /// Call once per frame while `is_running()` (`App`'s per-frame poll
    /// block, alongside `poll_docker`/`poll_k8s`/etc.) -- drains via
    /// `try_recv()` in a loop, same as every other channel-backed panel in
    /// this crate.
    pub fn poll(&mut self) -> Option<ClonePollResult> {
        let rx = self.rx.as_ref()?;
        let mut result = None;
        loop {
            match rx.try_recv() {
                Ok(CloneEvent::Progress(p)) => {
                    self.progress = Some(p);
                    result = Some(ClonePollResult::Progress);
                }
                Ok(CloneEvent::Done(Ok(path))) => {
                    self.rx = None;
                    self.progress = None;
                    self.done = Some(path.clone());
                    return Some(ClonePollResult::Succeeded(path));
                }
                Ok(CloneEvent::Done(Err(e))) => {
                    self.rx = None;
                    self.progress = None;
                    self.error = Some(e);
                    return Some(ClonePollResult::Failed);
                }
                Err(_) => break,
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_with_no_clone_started_is_a_noop() {
        let mut panel = ClonePanel::default();
        assert_eq!(panel.poll(), None);
    }

    #[test]
    fn start_is_a_noop_for_an_empty_url_or_destination() {
        let mut panel = ClonePanel {
            destination: "/tmp/somewhere".to_string(),
            ..ClonePanel::default()
        };
        panel.start();
        assert!(!panel.is_running());

        let mut panel = ClonePanel {
            url: "https://example.com/repo.git".to_string(),
            ..ClonePanel::default()
        };
        panel.start();
        assert!(!panel.is_running());
    }

    #[test]
    fn start_trims_whitespace_before_checking_emptiness() {
        let mut panel = ClonePanel {
            url: "   ".to_string(),
            destination: "/tmp/somewhere".to_string(),
            ..ClonePanel::default()
        };
        panel.start();
        assert!(!panel.is_running());
    }

    #[test]
    fn start_is_a_noop_while_a_clone_is_already_in_flight() {
        // Both calls use an empty destination on a real-looking URL so the
        // background thread fails fast (`GitError::Io`/`Git2` opening a
        // nonexistent relative path) without real network I/O; the guard
        // under test (`self.rx.is_some()`) runs before either background
        // thread would be spawned a second time.
        let mut panel = ClonePanel {
            url: "https://example.com/repo.git".to_string(),
            destination: "does-not-matter-one".to_string(),
            ..ClonePanel::default()
        };
        panel.start();
        assert!(panel.is_running());

        panel.destination = "does-not-matter-two".to_string();
        panel.start();
        assert!(panel.is_running());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = panel.poll() {
                assert_eq!(result, ClonePollResult::Failed);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no terminal event arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!panel.is_running());
        for _ in 0..5 {
            assert_eq!(panel.poll(), None);
        }
    }

    #[test]
    fn clone_progress_from_ide_core_progress_converts_relevant_fields() {
        let core = git::CloneProgress {
            received_objects: 3,
            total_objects: 10,
            indexed_objects: 2,
            indexed_deltas: 1,
            total_deltas: 4,
            received_bytes: 999,
        };
        let ui: CloneProgress = core.into();
        assert_eq!(ui.received_objects, 3);
        assert_eq!(ui.total_objects, 10);
    }

    #[test]
    fn field_next_and_prev_both_flip_between_the_two_fields() {
        assert_eq!(ClonePanelField::Url.next(), ClonePanelField::Destination);
        assert_eq!(ClonePanelField::Destination.next(), ClonePanelField::Url);
        assert_eq!(ClonePanelField::Url.prev(), ClonePanelField::Destination);
        assert_eq!(ClonePanelField::Destination.prev(), ClonePanelField::Url);
    }

    #[test]
    fn start_then_poll_eventually_reports_failure_for_a_bad_destination() {
        let mut panel = ClonePanel {
            url: "https://example.com/repo.git".to_string(),
            destination: "/nonexistent-root-does-not-exist-abc123/x".to_string(),
            ..ClonePanel::default()
        };
        panel.start();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = panel.poll() {
                assert_eq!(result, ClonePollResult::Failed);
                assert!(panel.error.is_some());
                assert!(!panel.is_running());
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "clone_repo never reported failure"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
