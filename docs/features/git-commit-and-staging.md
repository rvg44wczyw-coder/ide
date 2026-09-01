# Git Commit & Staging (E1)

## 1. Purpose

The Source Control view (`git-support.md`) can already show a commit
graph, diff a single file, and resolve a merge conflict — but there is no
way to actually *make* a commit from inside the app. This phase closes
that gap: a working-tree status list (staged / unstaged / untracked),
stage/unstage/discard **per file**, and commit creation (with amend),
surfaced as a Fleet-style Changes panel above the existing commit graph.

**Scope cut, stated up front:** `docs/roadmap.md`'s own E1 line asks for
staging "по файлам **и хункам**" (files *and* hunks). This phase ships
**file-level** staging only. Hunk-level staging needs applying a partial
patch to the git index (either `git2::Patch`/`Diff::apply` restricted to
selected hunks, or hand-building a blob from selectively-applied hunks)
— a meaningfully larger, mostly independent subsystem on top of
everything this doc already specifies, the same shape of deferral
`semantic-highlighting.md` §4 made for `semanticTokens/full/delta` and
`editor-git-gutter.md` §3.5 made for EOF deletion markers: a documented v1
cut, not an oversight. Revisit as a fast-follow (`E1b` or folded into a
later VCS phase) once file-level staging is in use. `docs/roadmap.md` is
updated to reflect this split when this phase merges.

## 2. Interface

### 2.1 `ide-core` (`crates/core/src/git/mod.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: PathBuf, // repo-relative
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingTreeStatus {
    pub staged: Vec<StatusEntry>,
    pub unstaged: Vec<StatusEntry>,
}

impl GitRepo {
    pub fn status(&self) -> Result<WorkingTreeStatus, GitError>;
    pub fn stage_path(&self, path: impl AsRef<Path>) -> Result<(), GitError>;
    pub fn unstage_path(&self, path: impl AsRef<Path>) -> Result<(), GitError>;
    pub fn discard_path(&self, path: impl AsRef<Path>) -> Result<(), GitError>;
    /// Returns the new commit's full id (hex).
    pub fn commit(&self, message: &str, amend: bool) -> Result<String, GitError>;
}
```

`ChangeKind` deliberately folds git's richer per-path flag set (rename/
typechange/copied) down to five buckets — same "own flattened summary
enum" precedent `ide_lsp::SymbolKind`/`DiagnosticSeverity` already
established for a wire protocol's own richer type: a rename shows as
`Deleted` (old path) + `Added` (new path) rather than a dedicated
`Renamed` variant with an old-path field. `Conflicted` reuses the same
signal `conflicts()` already exposes (a path in `WorkingTreeStatus` with
kind `Conflicted` and a path in `conflicts()` describe the same set) —
kept as its own enum member here (not cross-referenced) so a single
`WorkingTreeStatus` fully describes what the Changes panel renders
without a second query.

### 2.2 `ide-ui` (`crates/ui/src/git_panel.rs`)

`GitPanel` gains:

```rust
pub status: WorkingTreeStatus,
pub commit_message: String,
pub amend: bool,
/// A path awaiting a user's confirm/cancel on Discard -- the Commit
/// panel's own small modal, distinct from the editor's unrelated
/// "discard unsaved tab changes" modal (`IdeApp::pending_confirm`, §3.5).
pub pending_discard: Option<PathBuf>,
```

New methods:

```rust
pub fn sync_status(&mut self);
pub fn stage(&mut self, path: &Path) -> Result<(), String>;
pub fn unstage(&mut self, path: &Path) -> Result<(), String>;
pub fn request_discard(&mut self, path: &Path);
pub fn cancel_discard(&mut self);
pub fn confirm_discard(&mut self) -> Result<(), String>;
pub fn commit(&mut self) -> Result<(), String>;
```

`refresh()` gains a `self.status = repo.status().unwrap_or_default();`
call alongside its existing `graph`/`conflicts`/`current_branch` loads,
and now also resets `commit_message`/`amend`/`pending_discard` to their
defaults (a project switch abandoning an in-progress, unsent commit
message is the right default — same "reset everything on refresh"
precedent the existing `active_conflict`/`binary_conflict` reset already
sets).

### 2.3 `ide-ui` (`crates/ui/src/app/render.rs`)

`render_source_control` calls `self.git.sync_status()` once at its own
top, every frame the view renders — same per-frame-recompute cost profile
already accepted for `show_working_tree_diff` (called every frame for the
active tab) and `editor-git-gutter.md`'s git-gutter marks (§3.1 there).
No new call site elsewhere in `app.rs`/`render.rs` — this is the *only*
place status becomes stale-then-fresh, so nothing else needs to know
about it.

A new section, "Changes", renders between the conflict-resolution UI
(unchanged) and the existing "Commits" heading: a commit-message
`egui::TextEdit::multiline`, an "Amend" checkbox, a "Commit" button
(disabled when `commit_message` is empty and `amend` is false — an empty
message is only meaningful for `amend`, which can keep the previous
commit's own message by leaving the field as `sync_status` leaves it,
§3.4), then two `egui::ScrollArea`s — "Staged Changes"
(`status.staged`) and "Changes" (`status.unstaged`) — each row showing
`path` + a one-letter `kind` badge (`A`/`M`/`D`/`U`/`C`) and a
stage/unstage button (Staged rows get "Unstage", unstaged rows get
"Stage"); unstaged rows additionally get a "Discard" button, wired to
`request_discard`, not `confirm_discard` directly (§3.5's modal).

A small `egui::Window` "Discard changes?" renders whenever
`self.git.pending_discard.is_some()` (same shape `render_git_gutter_popup`
already established for a small transient confirm-style popup): "Discard
changes to `<path>`? This cannot be undone." plus Discard/Cancel buttons
calling `self.git.confirm_discard()`/`self.git.cancel_discard()`.

## 3. Behaviour

### 3.1 `status()`

Built from `git2::Repository::statuses` with `include_untracked(true)`,
`recurse_untracked_dirs(true)`, `include_ignored(false)`. Each
`git2::Status` entry's flags are inspected in this priority order to
produce zero, one, or two `StatusEntry`s (a path can be both staged *and*
further modified unstaged at once — real git status semantics, and
exactly what "partially staged" means at file granularity):

- `INDEX_NEW`/`INDEX_MODIFIED`/`INDEX_RENAMED`/`INDEX_TYPECHANGE` →
  `staged` entry, kind `Added` for `INDEX_NEW`, else `Modified`.
- `INDEX_DELETED` → `staged` entry, kind `Deleted`.
- `WT_NEW` → `unstaged` entry, kind `Untracked` (an untracked file has no
  staged counterpart by definition, so it can only ever appear here).
- `WT_MODIFIED`/`WT_RENAMED`/`WT_TYPECHANGE` → `unstaged` entry, kind
  `Modified`.
- `WT_DELETED` → `unstaged` entry, kind `Deleted`.
- `CONFLICTED` → **both** a `staged` and an `unstaged` entry are
  suppressed for this path in favor of one `Conflicted` entry, pushed to
  `unstaged` only — a conflicted path isn't meaningfully "partially
  staged", and `conflicts()`/the Conflicts UI already owns its resolution
  flow; showing it a second time in the plain stage/unstage list would
  invite double-resolving the same conflict two different ways.

Ordering within `staged`/`unstaged` is whatever `git2::Statuses` iterates
in (already path-sorted by libgit2) — no client-side sort needed.

### 3.2 `stage_path` / `unstage_path`

`stage_path` mirrors `resolve_conflict`'s existing path-escape check
(canonicalize, `starts_with(workdir)`) before touching the index — even
though `path` here is expected to already be repo-relative from
`status()`'s own output, the same discipline every other write path in
this module already applies to input it receives, not input it trusts by
provenance alone. If the target file exists on disk, `index.add_path`
(covers new/modified — matches `git add <path>`); if it doesn't (staging
a working-tree deletion), `index.remove_path` (matches `git add <path>`
on a deleted file, which stages the removal). `index.write()` after
either.

`unstage_path` is `git2::Repository::reset_default(Some(&head_commit),
[path])` when `HEAD` resolves, which resets only that path's index entry
back to `HEAD`'s tree without touching the working directory (`git
restore --staged <path>`'s exact semantics) — the untracked-when-committed
case (nothing at `HEAD` for this path, e.g. unstaging a brand-new file)
is `reset_default`'s own well-defined behavior (removes it from the
index, matching real `git restore --staged` on a newly-added file). When
`HEAD` doesn't resolve yet (a brand-new repo, no commits — same "unborn
branch" case `head_tree`/`commit_graph` already special-case), unstaging
means "remove from the index, there is no older version to reset to" —
`index.remove_path` directly, `index.write()`.

### 3.3 `discard_path`

Two cases, dispatched on the path's own current status (the caller —
`GitPanel::confirm_discard` — always knows which, from `status.unstaged`):

- **Tracked, modified/deleted** (`WT_MODIFIED`/`WT_DELETED`): `git2::
  Repository::checkout_head` with a `CheckoutBuilder` restricted to this
  one path and `.force()` — overwrites the working-tree file with
  `HEAD`'s content (or restores a deleted file), matching `git checkout
  -- <path>` / `git restore <path>`.
- **Untracked** (`WT_NEW`): there is nothing in `HEAD` to check out —
  discard means delete the file from disk. Same escape check as
  `resolve_conflict`/`stage_path` (canonicalize, `starts_with(workdir)`)
  before the `fs::remove_file` call — this is the one path in this whole
  module that **deletes** user content rather than overwriting it, so the
  check is load-bearing in a way none of the write paths' checks
  previously were (a symlink or `..`-relative path slipping past it here
  doesn't overwrite the wrong file, it destroys one). No trash/undo — the
  doc's own §4 states this plainly, and the UI's confirm modal (§2.3)
  says so too.

Never called for a `Conflicted` path — `discard_path` doesn't attempt to
special-case conflict markers; the Conflicts UI's existing Accept Ours/
Accept Theirs/Mark Resolved flow is the only supported way to leave that
state (§3.1's suppression already keeps conflicted paths out of the
plain unstaged Discard button in the first place).

### 3.4 `commit`

`index.write_tree()` → the tree to commit. Signature via `git2::
Repository::signature()` (reads `user.name`/`user.email` from git config
exactly the way a real `git commit` would — never hardcoded, never
prompted for in-app; a repo/global config with neither set is a genuine
`GitError::Git2` the UI surfaces as-is, same as any other libgit2
failure). Non-amend: `repo.commit(Some("HEAD"), &sig, &sig, message,
&tree, &parents)` where `parents` is `[]` for the unborn-`HEAD` case
(first commit in a fresh repo) or `[head_commit]` otherwise — the same
`self.repo.head()`-resolves-or-not branch `head_tree`/`commit_graph`
already established. Amend: `head_commit.amend(Some("HEAD"), Some(&sig),
Some(&sig), None, Some(message), Some(&tree))` — keeps the original
commit's parents automatically (`amend`'s own `None` for that argument
means "unchanged"), replaces tree and message. An empty `message` on a
non-amend commit is rejected before calling into `git2` at all
(`GitError::Git2` built from `git2::Error::from_str`, matching
`conflict_sides`' own "manufacture a `git2::Error` for a v1-invalid input"
precedent) — an amend with an empty message instead means "keep the
previous commit's message", so `GitPanel::commit` (§3.5) pre-fills
`commit_message` from the selected-for-amend commit rather than the core
layer trying to distinguish "empty on purpose" from "empty because
nothing was typed yet".

### 3.5 `GitPanel`'s own layer

- `sync_status` — no-op if no repo open; else `self.status =
  repo.status().unwrap_or_default()`, same permissive-on-error convention
  `refresh()`'s own graph/conflicts loads already use (a transient git
  error mid-frame degrades to "nothing changed", not a crash or an error
  banner spamming every frame).
- `stage`/`unstage` — call the matching `GitRepo` method, map `GitError`
  to `String` (`mark_resolved`'s exact shape), re-run `sync_status` on
  success so the panel reflects the move immediately rather than waiting
  for next frame's own `sync_status` call to catch up (avoids a
  one-frame-stale list right after a click).
- `request_discard(path)` — sets `pending_discard = Some(path)`, no git
  call yet.
- `cancel_discard` — clears `pending_discard`.
- `confirm_discard` — no-op `Ok(())` if `pending_discard` is `None`
  (mirrors `mark_resolved`'s "no active target" no-op); else calls
  `discard_path`, clears `pending_discard` either way (an error still
  closes the modal — retrying a failed discard is a fresh click, not a
  modal that lingers on failure), `sync_status` on success.
- `commit` — no-op `Ok(())` if `commit_message.trim().is_empty() &&
  !amend` (mirrors the core layer's own rejection, checked here too so
  the UI can simply grey out the button rather than surface a
  round-tripped `GitError`); else calls `GitRepo::commit`, and on success
  clears `commit_message`, resets `amend` to `false`, re-runs
  `sync_status`, and reloads `graph` (`repo.commit_graph(COMMIT_GRAPH_LIMIT)`)
  so the new commit appears in the graph immediately, same "the write
  succeeded, now make every cached view of it fresh" pattern
  `mark_resolved` already sets for `conflicts()`.

## 4. Constraints

**Security-sensitive** — `crates/core/src/git/**` is unconditionally on
`CLAUDE.md`'s list, and this phase adds real writes to the index and
working tree (including a genuine file **deletion** path,
`discard_path`'s untracked case). **`hacker` pass is mandatory** before
merge.

- No trash/undo for `discard_path` — the working directory's version of
  the discarded content is gone once the call returns. The UI's own
  confirm modal (§2.3) is the only safety net; there is no server-side or
  core-layer "are you sure" beyond that.
- `commit`'s signature always comes from git config (`git2::Repository::
  signature()`) — never a user-typed name/email field in this phase's UI,
  so there is no "type an arbitrary author identity" surface to worry
  about.
- Every path this phase's methods accept is expected to already be
  repo-relative from `status()`'s own output, but every write path still
  independently re-validates via the canonicalize+`starts_with(workdir)`
  check `resolve_conflict` already established — no method in this
  module trusts a caller's path by provenance alone.

## 5. Examples

```rust
let repo = GitRepo::open(&root)?;
let status = repo.status()?;
// status.unstaged might contain StatusEntry { path: "src/main.rs", kind: Modified }

repo.stage_path("src/main.rs")?;
let status = repo.status()?;
// now in status.staged instead

let id = repo.commit("Fix the thing", false)?;
// id is the new commit's full hex id; status.staged is now empty
```

Discarding an untracked file:

```rust
// status.unstaged contains StatusEntry { path: "scratch.txt", kind: Untracked }
repo.discard_path("scratch.txt")?;
// scratch.txt no longer exists on disk
```

## 6. Dependencies / integration

No new external dependency — everything here is `git2` surface already in
use elsewhere in this module (`Repository::statuses`, `Index`,
`Repository::reset_default`, `Repository::checkout_head`,
`Repository::signature`, `Repository::commit`, `Commit::amend`). Touches
`crates/core/src/git/mod.rs`, `crates/ui/src/git_panel.rs`,
`crates/ui/src/app/render.rs` — two roles, `rust-core-dev` then
`rust-ui-dev`, plus a mandatory `hacker` pass on the `rust-core-dev` half
before `rust-ui-dev` starts building on it (same ordering
`git-remote.md`'s own E6 chain used for its own mandatory-`hacker`
core-layer work).

## Revision notes

`ide-core` half implemented and hacker-pass-clean as of 2026-08-28
(commit history: initial implementation, then a fix round for the
findings below). Findings doc:
`docs/security-findings/rust-core-dev-git-commit-and-staging-2026-08-28.md`.

- **git2 API shapes discovered while implementing** (not obvious from the
  crate docs, only from reading the vendored `git2` 0.21.0 source):
  `StatusEntry::path()` returns `Result<&str, Error>`, not `Option<&str>`;
  `Commit::summary()` returns `Result<Option<&str>, Error>`.
- **`discard_path`'s original branch condition was backwards.** Branching
  on `full_path.exists()` (checkout-if-exists, else no-op) is exactly
  wrong: a tracked-then-deleted file doesn't exist on disk but needs
  `checkout_head` to restore it, while an untracked file exists on disk
  but has nothing in `HEAD` to check out. Fixed by branching on whether
  `HEAD`'s tree actually has an entry for the path
  (`tree.get_path(path).is_ok()`) instead.
- **Hacker-pass finding (Medium): `CheckoutBuilder::path()` is a pathspec
  pattern, not a literal path.** Without `disable_pathspec_match(true)`,
  discarding a tracked file whose name contains a glob-special character
  could revert an unrelated sibling file matching the same pattern. Fixed;
  see findings doc §1.
- **Hacker-pass finding (Medium): an empty repo-relative path bypassed
  path validation entirely and made `unstage_path("")` unstage every
  staged file**, because `Path::new("").components()` is empty (so the
  component-scan escape check never runs) and `workdir.join("")` resolves
  to `workdir` itself, which `Repository::reset_default` then treats as
  "match everything" for its pathspec. Fixed by rejecting an empty path
  explicitly in `validate_repo_relative_path`; see findings doc §2.
- **Stale `cargo llvm-cov` cache gotcha**: a coverage run mid-implementation
  showed `git/mod.rs` at an implausible 78.50% with old, already-tested
  lines listed as uncovered. `cargo llvm-cov clean --workspace` fixed it —
  the real number was 94.97% at that point, 95.06% after the hacker-pass
  fix round's added tests. Worth remembering for future phases: an
  inconsistent-looking coverage number is worth a cache-clean retry before
  treating it as real.

`ide-ui` half implemented as of 2026-08-28: `GitPanel` gained `status`/
`commit_message`/`amend`/`pending_discard` plus `sync_status`/`stage`/
`unstage`/`request_discard`/`cancel_discard`/`confirm_discard`/`commit`,
all per §2.2/§3.5 exactly as specified. `render_source_control` calls
`sync_status()` at its own top every frame it renders (§2.3); a new
`render_changes_section` (commit message box, Amend checkbox, Commit
button, Staged/Unstaged scroll lists with per-row Stage/Unstage/Discard
buttons) sits between the conflict UI and the "Commits" heading; a new
`render_discard_confirm_popup` — following the exact small-`egui::Window`
shape `render_git_gutter_popup` already established — gates on
`pending_discard.is_some()`. No deviations from the doc's specified
interface. `GitPanel::commit`'s implementation needed one adjustment not
worth a design change: the initial draft re-borrowed `self.repo`
immutably across an intervening `self.sync_status()` mutable call (a
borrow-checker error, not a logic bug) — fixed by re-fetching the `&self.repo`
reference after the mutable calls instead of holding one borrow across
both. `cargo clippy`'s `field_reassign_with_default` lint also required
one test to use struct-update syntax (`GitPanel { commit_message: ...,
..GitPanel::default() }`) instead of assigning fields one at a time after
`GitPanel::default()`.

Coverage: `git_panel.rs` 97.79% line coverage (cargo llvm-cov, cache
cleaned before measuring); rendering additions in `app/render.rs`
(`render_changes_section`, `render_change_row`,
`render_discard_confirm_popup`) are pure-rendering and excluded from the
floor per this project's established convention, same as every other
panel's render function. Full workspace `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo build --workspace --all-targets`, and `cargo test --workspace`
(497 + 765 relevant crate totals, 2292 tests workspace-wide) all green.
E1 is done; both halves and the hacker pass are merged into `main`.
