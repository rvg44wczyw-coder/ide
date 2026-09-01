//! Background-thread-plus-`mpsc`-channel clone state for the compact
//! launcher's "Clone Repository" flow (`docs/features/git-remote.md`
//! §2.2/§3.1). Mirrors `CargoPanel`'s existing pattern: `start` spawns a
//! thread that calls the blocking `ide_core::git::clone_repo`, `poll`
//! drains whatever the thread has sent so far via `try_recv()`.

use ide_core::git;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
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

/// What `poll` hands back the one frame a clone finishes, so `IdeApp` can
/// react (open the freshly cloned project) without `CloneState` itself
/// knowing about `IdeApp`/`egui::Context`.
#[derive(Debug, Clone, PartialEq)]
pub enum ClonePollResult {
    Progress,
    Succeeded(PathBuf),
    Failed,
}

#[derive(Default)]
pub struct CloneState {
    pub url: String,
    pub destination: Option<PathBuf>,
    pub progress: Option<CloneProgress>,
    pub error: Option<String>,
    rx: Option<Receiver<CloneEvent>>,
}

impl CloneState {
    /// Whether a clone is currently running. Distinct from
    /// `self.progress.is_some()` -- a same-filesystem (local) clone can
    /// complete without ever driving the indexer progress callback (no
    /// packfile transfer to report on), so `progress` alone can't tell
    /// the caller whether a clone is in flight.
    pub fn is_running(&self) -> bool {
        self.rx.is_some()
    }

    /// No-op if a clone is already in flight -- v1 runs at most one at a
    /// time, same convention `CargoPanel::run` already uses.
    pub fn start(&mut self, url: String, dest: PathBuf) {
        if self.rx.is_some() {
            return;
        }
        self.error = None;
        self.progress = None;

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
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

    /// Call once per frame. `Some(_)` means `progress`/`error` changed or
    /// the clone completed (caller should request a repaint); richer
    /// than the crate's usual bare-`bool` `.poll()` convention because
    /// the caller needs to know *which* kind of change happened (a
    /// mid-flight progress tick vs. the one frame the clone actually
    /// finishes) to react correctly. `None` means nothing changed this
    /// frame.
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
        let mut state = CloneState::default();
        assert_eq!(state.poll(), None);
    }

    #[test]
    fn start_is_a_noop_while_a_clone_is_already_in_flight() {
        let mut state = CloneState::default();
        // Both calls use an empty URL (fails fast with `GitError::EmptyUrl`,
        // no filesystem/network I/O) so the test stays fast and
        // deterministic; the guard under test (`self.rx.is_some()`) runs
        // before either background thread is ever spawned.
        state.start(String::new(), PathBuf::from("/tmp/one"));
        assert!(state.rx.is_some());
        state.start(String::new(), PathBuf::from("/tmp/two"));
        assert!(state.rx.is_some());

        // If the second `start` had (incorrectly) spawned its own thread,
        // draining to completion would eventually surface two terminal
        // events instead of one -- this only ever sees one, then `rx`
        // goes back to `None` and every further `poll` is `None`.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = state.poll() {
                assert_eq!(result, ClonePollResult::Failed);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no terminal event arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(state.rx.is_none());
        for _ in 0..5 {
            assert_eq!(state.poll(), None);
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
    fn start_then_poll_eventually_reports_failure_for_an_empty_url() {
        let mut state = CloneState::default();
        state.start(String::new(), PathBuf::from("/tmp/does-not-matter"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = state.poll() {
                assert_eq!(result, ClonePollResult::Failed);
                assert!(state.error.is_some());
                assert!(state.rx.is_none());
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "clone_repo never reported EmptyUrl"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
