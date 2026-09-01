//! `notify`-backed filesystem watcher rooted at a project directory. Raw OS
//! events are folded and debounced on a background thread into a small
//! `WatchEvent` set the UI drains once per frame via `poll` — see
//! `docs/features/file-watcher.md`.
//!
//! Every event path is canonicalized and checked against the watched
//! root before it is ever placed in the pending state a caller can
//! observe (§4.2 of the doc): the same rule `project::scan_tree`'s
//! `classify` enforces during a directory scan, applied here to watch
//! events instead. A symlink inside the project whose target resolves
//! outside the root produces no event for that path.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::{ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Largest number of distinct paths coalesced into one flush before the
/// watcher gives up naming them individually and reports `TreeChanged`
/// instead — bounds memory and event-handling cost during a burst like
/// `cargo clean` or a branch switch touching thousands of files.
pub const MAX_COALESCED_PATHS: usize = 512;

/// Bound on the raw OS-event channel between notify's callback thread and
/// the debounce loop. The callback must never block (it runs on notify's
/// own watcher thread), so a full channel drops the individual raw event
/// via `try_send` rather than blocking -- but never drops the *fact* that
/// something happened: see the `overflow` flag in `FileWatcher::new`,
/// which forces `tree_changed` on the next flush instead, the same
/// "give up enumerating, just say something changed" fallback
/// `MAX_COALESCED_PATHS` already uses for the folded-event sets.
const RAW_EVENT_CHANNEL_CAPACITY: usize = 4096;

/// How long a burst of raw OS events is allowed to keep arriving before the
/// watcher treats it as settled and flushes. Chosen well above a single
/// `write()`+`rename()` save sequence's inter-event gap and well below "the
/// user would notice a delay."
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);

/// How long a path stays suppressed after `FileWatcher::suppress` — covers
/// a slow disk or a save that itself re-enters (an `EditorConfig` pre-write
/// transaction followed immediately by the actual write) without also
/// swallowing a genuine external edit that happens to land in the same
/// second.
pub const SUPPRESS_WINDOW: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// Something was created, removed, or renamed somewhere under the
    /// watched root, or a burst exceeded `MAX_COALESCED_PATHS`. The UI's
    /// answer is always the same: re-run `Project::scan_tree`.
    TreeChanged,
    /// An existing file's content changed on disk. Canonicalized, and
    /// already filtered through `SUPPRESS_WINDOW` and the root check —
    /// every path a caller sees here is real, in-root, external.
    FileModified(PathBuf),
    /// A file was removed (or renamed away from) this exact path. The
    /// canonical form of the **parent directory**, joined lexically with
    /// the removed entry's file name — never a re-`canonicalize()` of the
    /// removed path itself, which no longer exists by the time this event
    /// is built.
    FileRemoved(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("failed to start watching {path}: {source}")]
    Start {
        path: PathBuf,
        #[source]
        source: notify::Error,
    },
}

/// Pending state folded from raw OS events before any of it reaches a
/// caller of `poll` — see the doc's §3.2 for the fold/flush rules this
/// implements verbatim.
#[derive(Default)]
struct Pending {
    tree_changed: bool,
    modified: HashSet<PathBuf>,
    removed: HashSet<PathBuf>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        !self.tree_changed && self.modified.is_empty() && self.removed.is_empty()
    }
}

pub struct FileWatcher {
    // `Option` so `Drop` can tear the OS watch down (dropping the closure
    // that owns the raw-event sender, which disconnects the debounce
    // thread's receiver) before joining that thread, rather than racing
    // the two in field-drop order.
    watcher: Option<RecommendedWatcher>,
    events_rx: Receiver<WatchEvent>,
    suppress_map: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    thread: Option<JoinHandle<()>>,
}

impl FileWatcher {
    /// Starts watching `root` recursively on a background thread. Returns
    /// once the OS watch is registered; events arrive asynchronously,
    /// drained via `poll`. `root` is canonicalized internally — every
    /// event this watcher ever produces is checked against that canonical
    /// root.
    pub fn new(root: &Path) -> Result<Self, WatchError> {
        Self::with_raw_channel_capacity(root, RAW_EVENT_CHANNEL_CAPACITY)
    }

    /// Same as `new`, with the raw-channel bound (normally
    /// `RAW_EVENT_CHANNEL_CAPACITY`) as a parameter -- only so a test can
    /// shrink it to reliably drive the overflow path with a small burst
    /// instead of needing a real flood large enough to fill 4096 slots.
    fn with_raw_channel_capacity(root: &Path, raw_capacity: usize) -> Result<Self, WatchError> {
        let canonical_root = fs::canonicalize(root).map_err(|e| WatchError::Start {
            path: root.to_path_buf(),
            source: notify::Error::from(e),
        })?;

        let (raw_tx, raw_rx) = sync_channel::<notify::Result<Event>>(raw_capacity);
        // The callback runs on notify's own watcher thread and must never
        // block; a full channel means the debounce loop can't keep up, so
        // the individual raw event is dropped via `try_send` -- but the
        // `overflow` flag it sets is picked up by `debounce_loop` and
        // forces `tree_changed`, so the *fact* that something happened is
        // never lost, only the fine-grained detail of what.
        let overflow = Arc::new(AtomicBool::new(false));
        let callback_overflow = Arc::clone(&overflow);
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                if raw_tx.try_send(res).is_err() {
                    callback_overflow.store(true, Ordering::Relaxed);
                }
            },
            notify::Config::default(),
        )
        .map_err(|e| WatchError::Start {
            path: canonical_root.clone(),
            source: e,
        })?;

        watcher
            .watch(&canonical_root, RecursiveMode::Recursive)
            .map_err(|e| WatchError::Start {
                path: canonical_root.clone(),
                source: e,
            })?;

        let (events_tx, events_rx) = channel();
        let suppress_map = Arc::new(Mutex::new(HashMap::new()));
        let thread_suppress = Arc::clone(&suppress_map);
        let thread = thread::spawn(move || {
            debounce_loop(canonical_root, raw_rx, events_tx, thread_suppress, overflow);
        });

        Ok(Self {
            watcher: Some(watcher),
            events_rx,
            suppress_map,
            thread: Some(thread),
        })
    }

    /// Non-blocking drain of every `WatchEvent` flushed since the last
    /// call. Empty on a frame with nothing new. Never blocks — safe to
    /// call once per UI frame.
    pub fn poll(&mut self) -> Vec<WatchEvent> {
        self.events_rx.try_iter().collect()
    }

    /// Marks `path` as "we just wrote this" for `SUPPRESS_WINDOW`: the
    /// next raw OS event(s) for it are dropped rather than turned into
    /// `FileModified`. Call this immediately around the app's own write.
    /// Never suppresses a `TreeChanged` from an unrelated path in the same
    /// burst.
    pub fn suppress(&mut self, path: &Path) {
        // A brand-new file (Save As to a path that doesn't exist yet)
        // can't be canonicalized before the write creates it; fall back to
        // the same parent-canonicalize-and-join technique the debounce
        // loop uses for a removed path, so the suppression key matches
        // whatever the fold-time canonicalization of the subsequent write
        // event will produce.
        let canonical = fs::canonicalize(path).ok().or_else(|| {
            let parent = path.parent()?;
            let file_name = path.file_name()?;
            fs::canonicalize(parent).ok().map(|p| p.join(file_name))
        });
        let Some(canonical) = canonical else { return };
        if let Ok(mut guard) = self.suppress_map.lock() {
            guard.insert(canonical, Instant::now());
        }
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        // Drop the watcher (and the closure/sender it owns) first so the
        // debounce thread's `recv` observes disconnection and exits,
        // rather than blocking `join` indefinitely.
        drop(self.watcher.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn debounce_loop(
    canonical_root: PathBuf,
    raw_rx: Receiver<notify::Result<Event>>,
    events_tx: Sender<WatchEvent>,
    suppress_map: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    overflow: Arc<AtomicBool>,
) {
    let mut pending = Pending::default();
    let mut last_activity = Instant::now();

    loop {
        // Checked every iteration, not only after a successful `recv` --
        // the loop can be sitting in a blocking `raw_rx.recv()` (pending
        // empty) when the callback thread hits a full channel, and this is
        // the only place that would notice.
        if overflow.swap(false, Ordering::Relaxed) {
            pending.tree_changed = true;
            last_activity = Instant::now();
        }

        let wait = if pending.is_empty() {
            None
        } else {
            let elapsed = last_activity.elapsed();
            if elapsed >= DEBOUNCE_WINDOW {
                for event in flush(&mut pending, &suppress_map) {
                    if events_tx.send(event).is_err() {
                        return;
                    }
                }
                continue;
            }
            Some(DEBOUNCE_WINDOW - elapsed)
        };

        let received = match wait {
            Some(timeout) => raw_rx.recv_timeout(timeout),
            None => raw_rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };

        match received {
            Ok(Ok(event)) => {
                fold_event(&canonical_root, event, &mut pending);
                last_activity = Instant::now();
            }
            Ok(Err(_)) => {
                // A single raw event failed inside notify's own backend;
                // nothing actionable per-event, keep watching.
            }
            Err(RecvTimeoutError::Timeout) => {
                // Loop back around; the top-of-loop elapsed check flushes
                // if the window has now closed.
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn fold_event(canonical_root: &Path, event: Event, pending: &mut Pending) {
    match event.kind {
        EventKind::Create(_) => {
            pending.tree_changed = true;
        }
        EventKind::Remove(kind) => {
            pending.tree_changed = true;
            // A folder removal (rather than a file) never produces a
            // `FileRemoved` on its own -- deleting `sub/` wholesale is not
            // "file `sub` was removed", it's the tree structurally
            // changing; any files that were under it are covered by the
            // same `tree_changed` signal, not enumerated individually.
            if kind != RemoveKind::Folder {
                if let Some(path) = event.paths.first() {
                    fold_removal(canonical_root, path, pending);
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(mode)) => {
            pending.tree_changed = true;
            match mode {
                RenameMode::From => {
                    if let Some(path) = event.paths.first() {
                        fold_removal(canonical_root, path, pending);
                    }
                }
                RenameMode::Both => {
                    if let Some(from) = event.paths.first() {
                        fold_removal(canonical_root, from, pending);
                    }
                    // The "to" half is create-shaped; `tree_changed` above
                    // already covers it.
                }
                // `To`, `Any`, and any future variant: create-like or
                // ambiguous. `tree_changed` alone is the safe signal.
                _ => {}
            }
        }
        EventKind::Modify(_) => {
            if let Some(path) = event.paths.first() {
                fold_modification(canonical_root, path, pending);
            }
        }
        EventKind::Access(_) | EventKind::Other | EventKind::Any => {}
    }
}

fn fold_removal(canonical_root: &Path, path: &Path, pending: &mut Pending) {
    let Some(file_name) = path.file_name() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    // The removed entry itself can never be canonicalized again -- by the
    // time this fires it no longer exists. If the parent is gone too (a
    // whole subtree removed in the same burst), there's nothing reliable
    // to join against: `tree_changed` (already set by the caller) carries
    // the refresh alone, with no per-file `FileRemoved`.
    let Ok(canonical_parent) = fs::canonicalize(parent) else {
        return;
    };
    if !canonical_parent.starts_with(canonical_root) {
        return;
    }
    let candidate = canonical_parent.join(file_name);
    pending.modified.remove(&candidate);
    pending.removed.insert(candidate);
}

fn fold_modification(canonical_root: &Path, path: &Path, pending: &mut Pending) {
    let Ok(canonical_path) = fs::canonicalize(path) else {
        return;
    };
    if !canonical_path.starts_with(canonical_root) {
        return;
    }
    let Ok(metadata) = fs::metadata(&canonical_path) else {
        return;
    };
    if !metadata.is_file() {
        return;
    }
    if !pending.removed.contains(&canonical_path) {
        pending.modified.insert(canonical_path);
    }
}

fn flush(
    pending: &mut Pending,
    suppress_map: &Mutex<HashMap<PathBuf, Instant>>,
) -> Vec<WatchEvent> {
    if pending.modified.len() + pending.removed.len() > MAX_COALESCED_PATHS {
        pending.tree_changed = true;
        pending.modified.clear();
        pending.removed.clear();
    }

    let now = Instant::now();
    let mut suppressed = suppress_map.lock().unwrap_or_else(|e| e.into_inner());
    suppressed.retain(|_, ts| now.duration_since(*ts) < SUPPRESS_WINDOW);

    let mut events = Vec::new();
    if pending.tree_changed {
        events.push(WatchEvent::TreeChanged);
    }
    for path in pending.removed.drain() {
        events.push(WatchEvent::FileRemoved(path));
    }
    for path in pending.modified.drain() {
        if suppressed.remove(&path).is_none() {
            events.push(WatchEvent::FileModified(path));
        }
    }
    pending.tree_changed = false;
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    /// Real notify backends deliver events asynchronously with timing
    /// that varies by platform and machine load; poll repeatedly up to a
    /// generous deadline instead of a single fixed sleep+poll; that's less
    /// flaky than betting on one exact sleep duration.
    fn poll_until(
        watcher: &mut FileWatcher,
        deadline: Duration,
        pred: impl Fn(&[WatchEvent]) -> bool,
    ) -> Vec<WatchEvent> {
        let start = Instant::now();
        let mut collected = Vec::new();
        loop {
            collected.extend(watcher.poll());
            if pred(&collected) || start.elapsed() >= deadline {
                return collected;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    const TEST_DEADLINE: Duration = Duration::from_secs(5);

    #[test]
    fn tree_changed_on_create() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher = FileWatcher::new(dir.path()).unwrap();

        fs::write(dir.path().join("new.txt"), "hi").unwrap();

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events.contains(&WatchEvent::TreeChanged)
        });
        assert!(events.contains(&WatchEvent::TreeChanged));
    }

    #[test]
    fn tree_changed_on_remove() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "hi").unwrap();

        let mut watcher = FileWatcher::new(dir.path()).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the create

        fs::remove_file(&file).unwrap();

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events.contains(&WatchEvent::TreeChanged)
        });
        assert!(events.contains(&WatchEvent::TreeChanged));
    }

    #[test]
    fn tree_changed_on_rename() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, "hi").unwrap();

        let mut watcher = FileWatcher::new(dir.path()).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the create

        fs::rename(&file, dir.path().join("b.txt")).unwrap();

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events.contains(&WatchEvent::TreeChanged)
        });
        assert!(events.contains(&WatchEvent::TreeChanged));
    }

    #[test]
    fn file_modified_on_content_change_coalesces_rapid_writes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "v0").unwrap();

        let mut watcher = FileWatcher::new(dir.path()).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the create

        let canonical = file.canonicalize().unwrap();
        for i in 1..=5 {
            fs::write(&file, format!("v{i}")).unwrap();
        }

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events.contains(&WatchEvent::FileModified(canonical.clone()))
        });
        let modified_count = events
            .iter()
            .filter(|e| matches!(e, WatchEvent::FileModified(p) if p == &canonical))
            .count();
        assert_eq!(
            modified_count, 1,
            "five rapid writes to the same path must coalesce into one FileModified"
        );
    }

    #[test]
    fn suppress_drops_next_modification_but_not_tree_changed_or_other_paths() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f.txt");
        let other = dir.path().join("other.txt");
        fs::write(&target, "v0").unwrap();
        fs::write(&other, "v0").unwrap();

        let mut watcher = FileWatcher::new(dir.path()).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the creates

        let canonical_target = target.canonicalize().unwrap();
        let canonical_other = other.canonicalize().unwrap();

        watcher.suppress(&target);
        fs::write(&target, "v1 -- app's own write").unwrap();
        fs::write(&other, "v1 -- external, unrelated").unwrap();
        fs::create_dir(dir.path().join("new_dir")).unwrap(); // also sets tree_changed

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events.contains(&WatchEvent::FileModified(canonical_other.clone()))
                && events.contains(&WatchEvent::TreeChanged)
        });

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WatchEvent::FileModified(p) if p == &canonical_target)),
            "suppressed path must not produce FileModified: {events:?}"
        );
        assert!(events.contains(&WatchEvent::TreeChanged));
        assert!(events.contains(&WatchEvent::FileModified(canonical_other)));
    }

    #[test]
    fn burst_exceeding_cap_produces_tree_changed_instead_of_individual_events() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_COALESCED_PATHS + 10) {
            fs::write(dir.path().join(format!("f{i}.txt")), "v0").unwrap();
        }

        let mut watcher = FileWatcher::new(dir.path()).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the creates

        for i in 0..(MAX_COALESCED_PATHS + 10) {
            fs::write(dir.path().join(format!("f{i}.txt")), "v1").unwrap();
        }

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events.contains(&WatchEvent::TreeChanged)
        });

        assert!(events.contains(&WatchEvent::TreeChanged));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WatchEvent::FileModified(_))),
            "a burst over the cap must not also emit individual FileModified events: {events:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_root_produces_no_event_for_paths_reached_through_it() {
        let project_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("secret.txt");
        fs::write(&outside_file, "shh").unwrap();

        symlink(outside_dir.path(), project_dir.path().join("escape")).unwrap();

        let mut watcher = FileWatcher::new(project_dir.path()).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the symlink create

        fs::write(&outside_file, "still shh, but changed").unwrap();

        // Give the watcher every chance to (incorrectly) report something,
        // then assert nothing named the escaped path arrived.
        let events = poll_until(&mut watcher, Duration::from_millis(800), |_| false);

        assert!(
            !events.iter().any(|e| matches!(
                e,
                WatchEvent::FileModified(p) | WatchEvent::FileRemoved(p)
                    if p.starts_with(outside_dir.path().canonicalize().unwrap())
            )),
            "an event must never name a path outside the watched root: {events:?}"
        );
    }

    #[test]
    fn dropping_the_watcher_stops_the_background_thread() {
        let dir = tempfile::tempdir().unwrap();
        let watcher = FileWatcher::new(dir.path()).unwrap();
        drop(watcher); // blocks until the debounce thread has actually exited

        fs::write(dir.path().join("after_drop.txt"), "hi").unwrap();
        thread::sleep(DEBOUNCE_WINDOW * 2);

        // There is no watcher left to poll -- the only observable proof is
        // that no channel/thread is left running, which `drop` above
        // already waited out synchronously via `JoinHandle::join`. This
        // test exists to document and lock in that guarantee.
    }

    #[test]
    fn file_removed_path_is_parent_canonical_joined_with_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "hi").unwrap();
        let expected_parent = dir.path().canonicalize().unwrap();

        let mut watcher = FileWatcher::new(dir.path()).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the create

        fs::remove_file(&file).unwrap();

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events
                .iter()
                .any(|e| matches!(e, WatchEvent::FileRemoved(_)))
        });

        let removed = events
            .iter()
            .find_map(|e| match e {
                WatchEvent::FileRemoved(p) => Some(p.clone()),
                _ => None,
            })
            .expect("expected a FileRemoved event");
        assert_eq!(removed, expected_parent.join("f.txt"));
    }

    #[test]
    fn removing_a_subtree_produces_tree_changed_only_no_file_removed() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("inner.txt"), "hi").unwrap();

        let mut watcher = FileWatcher::new(dir.path()).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the creates

        fs::remove_dir_all(&sub).unwrap();

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events.contains(&WatchEvent::TreeChanged)
        });

        assert!(events.contains(&WatchEvent::TreeChanged));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WatchEvent::FileRemoved(_))),
            "a whole-subtree removal must not produce a per-file FileRemoved: {events:?}"
        );
    }

    #[test]
    fn modify_then_remove_same_path_in_one_window_flushes_as_removed_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "v0").unwrap();
        let expected = dir.path().canonicalize().unwrap().join("f.txt");

        let mut watcher = FileWatcher::new(dir.path()).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the create

        fs::write(&file, "v1").unwrap();
        fs::remove_file(&file).unwrap();

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events
                .iter()
                .any(|e| matches!(e, WatchEvent::FileRemoved(_) | WatchEvent::FileModified(_)))
        });

        assert!(events.contains(&WatchEvent::FileRemoved(expected.clone())));
        assert!(
            !events.contains(&WatchEvent::FileModified(expected)),
            "same-window modify+remove of one path must never also emit FileModified: {events:?}"
        );
    }

    #[test]
    fn raw_channel_overflow_still_produces_tree_changed() {
        // A raw-channel capacity of 1 (rather than the real
        // RAW_EVENT_CHANNEL_CAPACITY, which a modest test burst couldn't
        // realistically fill) makes a rapid-write burst reliably outrun
        // the single debounce thread's one-canonicalize-syscall-per-event
        // drain and forces the overflow path -- proving it degrades to
        // "definitely stale, refresh" (TreeChanged) rather than silently
        // dropping the burst. Only this test uses the shrunk capacity;
        // every other test in this module still exercises the real one via
        // plain `FileWatcher::new`.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "v0").unwrap();

        let mut watcher = FileWatcher::with_raw_channel_capacity(dir.path(), 1).unwrap();
        poll_until(&mut watcher, TEST_DEADLINE, |_| false); // drain the create

        for i in 0..500 {
            fs::write(&file, format!("v{i}")).unwrap();
        }

        let events = poll_until(&mut watcher, TEST_DEADLINE, |events| {
            events.contains(&WatchEvent::TreeChanged)
        });

        assert!(
            events.contains(&WatchEvent::TreeChanged),
            "a raw-channel overflow must still surface as TreeChanged, never silence: {events:?}"
        );
    }
}
