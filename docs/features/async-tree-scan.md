# Async directory tree scan

## 1. Purpose

`IdeApp::load_project` and `IdeApp::refresh_tree` (`crates/ui/src/app.rs`)
call `Project::scan_tree()` (`crates/core/src/project.rs`) synchronously on
the UI thread. `scan_tree` is already algorithmically efficient (memoized
against symlink blowup — see that module's own doc comment), but on a
large real-world directory (thousands of files, deep nesting) the
recursive `fs::read_dir` walk still takes long enough in wall-clock time to
freeze `eframe`'s frame loop: the whole window stops repainting and
responding to input until the scan returns. User feedback (item #5 of the
post-B2c batch): "dirs are freezing sometimes on opening. I want all
indexations to be running in parallel."

Fix: move the scan itself to a background thread and poll for its result
once per frame, using the exact background-thread + `mpsc` + poll pattern
`ClaudePanel` (`crates/ui/src/claude_panel.rs`) and `CargoPanel`
(`crates/ui/src/cargo_panel.rs`) already establish in this crate — no new
concurrency primitive, no new dependency.

This is Batch B of the post-B2c feedback batch (see the user's approved
plan); Batch A (stripe width, remembered last project) is already merged.

## 2. Interface / API

### 2.1 New module `crates/ui/src/tree_scan.rs`

```rust
type Runner = fn(&Path) -> Option<DirEntry>;

pub struct TreeScan {
    rx: Option<Receiver<Option<DirEntry>>>,
    runner: Runner,
}

impl Default for TreeScan {
    fn default() -> Self { Self::with_runner(scan_project_root) }
}

impl TreeScan {
    fn with_runner(runner: Runner) -> Self { .. } // test-only constructor, same shape as ClaudePanel::with_runner

    /// Starts scanning `root` on a background thread. If a scan is
    /// already in flight, its `Receiver` is dropped (the old thread's
    /// eventual `send` silently fails and is ignored) and this call's
    /// scan supersedes it -- see §3.2.
    pub fn start(&mut self, root: PathBuf);

    /// Call once per frame. Returns `Some(tree)` the frame a scan
    /// finishes (whether or not it found the directory still there --
    /// `None` in that case, see §3.3), `None` on every other frame
    /// (nothing in flight, or in flight but not finished yet).
    pub fn poll(&mut self) -> Option<Option<DirEntry>>;

    pub fn is_scanning(&self) -> bool;
}

/// Default `Runner`: reopens `root` as a `Project` and scans it.
/// `Project` isn't `Clone` (`crates/core/src/project.rs`), so the thread
/// gets an owned `PathBuf` and reconstructs the handle instead of moving
/// a borrowed `&Project` across the thread boundary; `Project::open` on
/// an already-canonical, already-validated directory is just
/// `fs::canonicalize` + an `is_dir` check, not a second scan.
fn scan_project_root(root: &Path) -> Option<DirEntry> {
    ide_core::Project::open(root).ok().map(|p| p.scan_tree())
}
```

### 2.2 `IdeApp` (`crates/ui/src/app.rs`)

```rust
tree_scan: TreeScan,
pending_tree_scan_kind: Option<TreeScanKind>,
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeScanKind { Load, Refresh }
```

New method, called once per frame from `render.rs`'s per-frame poll block
(alongside `self.lsp.poll()`, `self.cargo.poll()`, `self.search.poll()`,
§2.3):

```rust
/// Drains a finished background scan, if any, and runs the follow-up
/// work that depends on the new tree (§3.1). Returns `true` if
/// state changed (caller should request a repaint), same contract as
/// every other `.poll()` in this crate.
fn poll_tree_scan(&mut self) -> bool
```

### 2.3 Call site (`crates/ui/src/app/render.rs`, per-frame poll block)

```rust
if self.poll_tree_scan() {
    changed = true; // or ctx.request_repaint(), matching the existing block's convention
}
```
placed alongside the existing `self.lsp.poll()` / `self.cargo.poll()` /
`self.search.poll()` calls at `render.rs:2501-2522`.

## 3. Behaviour

### 3.1 What moves to the background, what stays synchronous

Only the disk walk (`Project::scan_tree()`) and the tree-dependent
follow-up work move off the immediate call path. Everything else
`load_project`/`refresh_tree` already do keeps running synchronously, at
the same point it runs today, with the same latency users see today (git
status/branch refresh, file-watcher setup are comparatively cheap and were
never the source of the freeze; user feedback item #5 was specifically
about directory scanning):

`load_project` (unchanged, synchronous):
- `self.git.refresh(project.root())`
- file-watcher (re)creation (`FileWatcher::new`)
- `self.project = Some(project)`, `self.error = watcher_error`
- `self.pending_create_parent = None`, `self.create_project_name.clear()`
- `self.search.discard_in_flight()`

`load_project` (new): `self.tree = None` (clears any previous project's
tree immediately -- a stale, wrong-project tree must never be visible
while the new one scans), then `self.tree_scan.start(root)` and
`self.pending_tree_scan_kind = Some(TreeScanKind::Load)`.

`refresh_tree` (new): does **not** clear `self.tree` -- the previously
scanned tree (same project) stays visible, avoiding a flicker to empty,
until the refreshed one arrives. Calls `self.tree_scan.start(root)` and
sets `self.pending_tree_scan_kind = Some(TreeScanKind::Refresh)`.

`poll_tree_scan` (new, runs the deferred half, once the background scan
completes):
- `Some(new_tree) => self.tree = Some(new_tree)`, `None => {}` (§3.3 --
  leaves `self.tree` as it already was: `None` for a failed `Load`,
  unchanged for a failed `Refresh`).
- Takes `self.pending_tree_scan_kind`:
  - `Load` → calls `self.redetect_language()` (unconditional
    detect-and-restart, exactly `load_project`'s existing end-of-method
    behaviour today).
  - `Refresh` → runs `refresh_tree`'s existing inline language-detection
    block verbatim: `self.active_language =
    self.tree.as_ref().and_then(|t| detect_language(t,
    &self.custom_languages))`, then start the LSP only `if
    !self.lsp.is_running()`, else stop it if no language matched. (Kept
    distinct from `Load`'s unconditional restart for the same reason it
    is today -- a background refresh shouldn't bounce an already-running
    language server.)
  - `None` (nothing pending -- e.g. `poll_tree_scan` called on a frame
    where `TreeScan::poll()` returned `None`) → no-op, this branch is
    only reached together with a `Some(new_tree)`/`None` result from
    `TreeScan::poll()`, never spuriously.

### 3.2 Superseding scans

If `load_project`/`refresh_tree` is called again while a scan is already
in flight (e.g. the file watcher fires a second `TreeChanged` before the
first refresh finished, or the user opens a new project before the
previous one's initial scan completed), `TreeScan::start` simply replaces
`self.rx` with a new `Receiver` for the new scan. The superseded
background thread keeps running to completion (there's no cancellation
mechanism, `std::thread` doesn't support it) but its eventual `tx.send(..)`
targets a `Sender` whose paired `Receiver` has already been dropped, so the
send returns `Err` and is ignored -- its result is silently discarded.
`self.pending_tree_scan_kind` is likewise overwritten to the new call's
kind. Net effect: only the most recently requested scan's result is ever
applied, and a project switch mid-scan can never have a stale, wrong-root
tree land in `self.tree` after the fact.

### 3.3 Failure

`scan_project_root` returns `None` if `Project::open(root)` fails --
practically only a race where the directory was deleted/moved between
`start()` being called and the background thread running (`root` was
already validated once by the caller of `load_project`/`refresh_tree`
moments earlier). `poll_tree_scan` treats this exactly like
`restore_last_project`'s existing silent-failure posture (`app.rs`,
Batch A): no error is surfaced, `self.tree` is simply left as it was.

## 4. Constraints & invariants

- No new external dependency -- `std::thread`/`std::sync::mpsc` only, same
  as `ClaudePanel`/`CargoPanel`.
- Not security-sensitive: `tree_scan.rs`'s only input is a `PathBuf` that
  already passed through `load_project`/`refresh_tree`'s existing,
  already-validated project-root path (nothing new is read from
  user-controlled data); `crates/ui/src/app.rs` isn't on CLAUDE.md's
  declared security-sensitive list either. `Project::open`/`scan_tree`
  themselves are unchanged (still `ide-core`'s existing, already-reviewed
  symlink-escape and permission-error handling).
- Coverage: `tree_scan.rs` is not pure-rendering (real branching: start,
  poll, supersede, failure) and needs its own `#[cfg(test)] mod tests` at
  ≥80% line coverage, using a fake `Runner` (a plain `fn(&Path) ->
  Option<DirEntry>` that returns a fixed value with no real disk I/O) the
  same way `ClaudePanel`'s tests inject a fake CLI runner
  (`claude_panel.rs`'s `with_runner`/`Runner` type alias), and the same
  shape of `wait_until(|| ...)` millisecond-spin-loop helper
  `claude_panel.rs`'s tests define for the identical purpose --
  `crates/ui/src/app.rs`'s own test module already has its own copy of
  this helper (`app.rs:6372`, already used at `app.rs:6466` for
  `search_everywhere_files` polling); reuse that existing one directly
  rather than defining a second copy or reaching into `claude_panel`'s
  private one.
- The `load_project`/`refresh_tree`/`poll_tree_scan` restructuring in
  `app.rs` is non-rendering decision logic and needs its own coverage.
  Concretely: every existing test in `crates/ui/src/app.rs`'s test module
  that currently calls `open_project`/`create_project`/`refresh_tree` and
  then immediately asserts on `app.tree`, `app.active_language`, or LSP
  running/stopped state (roughly two dozen call sites) must insert a
  `wait_until(|| app.poll_tree_scan())` between the call and the
  assertion -- the tree is no longer available synchronously the instant
  `open_project` returns. Tests that only assert on `app.project`/
  `app.error` (tree-independent) need no change (e.g. Batch A's
  `restore_last_project_*` tests, which never inspect `app.tree`).
- `render.rs`'s tree panel (`crates/ui/src/app/render.rs:2251-2263`): when
  `self.tree` is `None` but a project is open and `self.tree_scan.
  is_scanning()`, render a lightweight "Scanning project…" label in place
  of the (currently silently-absent) tree, instead of rendering nothing.
  Pure rendering, exempt from the coverage target like the rest of this
  file's changes.
- `TreeScan` does not debounce or coalesce a burst of `start()` calls
  (e.g. many rapid filesystem events each triggering `WatchEvent::
  TreeChanged` → `refresh_tree`) beyond "the latest supersedes the rest"
  (§3.2) -- each call still spawns a new thread. This is acceptable at v1
  scope (matches the user's ask: "run in parallel," not "debounce
  filesystem churn") and each thread is short-lived and exits on its own
  once it sends or fails to send; a debounce layer is future work if it
  ever proves necessary in practice, not part of this doc.

## 5. Examples

**Opening a project** (`load_project`, abbreviated):

```rust
fn load_project(&mut self, project: Project) {
    self.tree = None;
    self.git.refresh(project.root());
    // ...watcher setup, self.project = Some(project), etc., unchanged...
    self.tree_scan.start(self.project.as_ref().unwrap().root().to_path_buf());
    self.pending_tree_scan_kind = Some(TreeScanKind::Load);
    self.search.discard_in_flight();
}
```

**Per-frame completion handling**:

```rust
fn poll_tree_scan(&mut self) -> bool {
    let Some(result) = self.tree_scan.poll() else { return false };
    self.tree = result;
    match self.pending_tree_scan_kind.take() {
        Some(TreeScanKind::Load) => self.redetect_language(),
        Some(TreeScanKind::Refresh) => {
            // existing refresh_tree language-detect-and-LSP-start block
        }
        None => {}
    }
    true
}
```

**A representative test after this change**:

```rust
#[test]
fn load_project_scans_tree_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "").unwrap();
    let mut app = app_without_gui();
    app.open_project(dir.path());
    assert!(app.tree.is_none()); // not yet -- scan is in flight
    wait_until(|| app.poll_tree_scan());
    assert!(app.tree.is_some());
}
```

## 6. Dependencies & integration points

- Single role: `rust-ui-dev`, `crates/ui/**` only. No `ide-core` change --
  `Project::open`/`Project::scan_tree`/`DirEntry` are already public and
  used exactly as before, just called from a background thread instead of
  the UI thread.
- Depends on Batch A (merged) only incidentally (same file,
  `crates/ui/src/app.rs`); no functional dependency.
- No `/design` mockup -- backend/concurrency fix, one new small rendering
  state (a "Scanning project…" label) with no new visual language.
