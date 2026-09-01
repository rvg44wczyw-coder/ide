# T25 — File Watcher (`ide-tui`)

## 1. Purpose

Ports **G6** (`docs/features/file-watcher.md`) to `ide-tui`. Unlike
`T17`/`T24`, G6's `ide-core` half (`crates/core/src/file_watcher.rs`,
`FileWatcher`/`WatchEvent`/`WatchError`) and its `ide-ui` half are both
**already fully merged and ✅** — this phase is a straight port of
`ide-ui`'s already-shipped UI wiring onto `ide-tui`'s own state shape, the
same relationship every other T-item has to its `ide-ui` original where
one already exists. No new `ide-core` API.

Today `ide-tui`'s tree is scanned exactly once, at `App::new`, and never
again — there is no "Refresh" action at all (confirmed by grepping for one
before writing this doc). A file created/removed/renamed outside the app,
or a tab's file changed or deleted underneath it, is invisible until the
whole app restarts. This phase closes both gaps.

### 1.1 Scope cut

- **Per-tab banner UI** — cut. `ide-ui`'s `ExternalChange` renders as an
  inline banner above the affected tab's editor (an `egui` widget); this
  crate has no equivalent per-tab chrome. `ide-tui`'s v1 instead: (a)
  appends a bracketed marker to that tab's entry in the tab strip (`" [modified
  on disk]"`/`" [deleted on disk]"`, next to the existing dirty `*`
  marker) so the state is visible without opening anything, and (b) posts
  a one-line `notify()` the moment the change is detected — this crate's
  existing substitute for a transient UI notice (`docs/features/
  tui-goto-and-usages.md` already established `notify` for exactly this
  "something happened, no popup needed" case).
- **Reload / Keep Mine as click targets** — cut, because there's nothing
  to click. `ide-ui` itself registers *no command-palette entry* for
  either action (confirmed by grepping `crates/ui/src/command.rs` — zero
  matches for "Reload"/`dismiss_external_change`), so there is no existing
  binding or palette entry to translate. Two new palette-only commands,
  `ReloadFromDisk`/`DismissExternalChange`, fill the same role here (no
  default binding, joining `ToggleGitPanel`/`JumpToMatchingBracket`/
  `ToggleTodoPanel` in that category — CLAUDE.md's "never invent a
  binding" rule leaves no other option when the thing being ported has no
  binding to begin with).

## 2. Interface

### 2.1 `ide_core::file_watcher` — unchanged

`FileWatcher::new(root) -> Result<Self, WatchError>`, `poll(&mut self) ->
Vec<WatchEvent>`, `suppress(&mut self, path: &Path)`. See
`docs/features/file-watcher.md` §2.1 for the full existing contract
(debounce/coalescing/suppression semantics, already implemented and
hacker-reviewed there — nothing here changes any of it).

### 2.2 `ide-tui`: `app.rs` additions

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalChange {
    Modified,
    Deleted,
}
```

`OpenBuffer` gains `pub(crate) external_change: Option<ExternalChange>`
(`None` at construction, mirroring `ide-ui`'s `Tab::external_change`).
`App` gains `watcher: Option<FileWatcher>`, started in `App::new`
alongside the existing `git.refresh(project.root())` call — failure
(`WatchError::Start`) is surfaced via `self.notify(...)` rather than
`ide-ui`'s single `self.error` slot (this crate has no equivalent slot;
the notification log is the existing substitute), leaving `watcher =
None` — the app degrades to "no automatic refresh," not a failure to
open the project, exactly matching `docs/features/file-watcher.md` §3.6's
existing degrade behaviour.

New methods on `App`:
- `pub fn poll_watcher(&mut self)` — called once per frame from `lib.rs`'s
  loop, same unconditional-every-frame shape `poll_search`/`poll_todo`
  already use. No-op if `watcher` is `None`. Drains `watcher.poll()` and
  dispatches each `WatchEvent` (§3.1 below).
- `fn refresh_tree(&mut self)` — `ide-tui`'s first tree-refresh action of
  any kind (there was none before this phase): re-opens `project_root`
  via `Project::open` and replaces `self.tree` with a fresh
  `scan_tree()`. Silently no-ops if `Project::open` fails (the root
  itself disappeared — an edge case with no good recovery here; the next
  successful event still retries).
- `fn handle_external_modification(&mut self, path: &Path)` / `fn
  handle_external_removal(&mut self, path: &Path)` — same dispatch shape
  `ide-ui`'s own §3.4/§3.5 already establish, adapted to this crate's
  `notify`-instead-of-banner surface (§1.1).
- `fn reload_tab_from_disk(&mut self, idx: usize)` — re-reads the tab's
  file via `Buffer::open`, replacing its buffer and clearing
  `external_change`; a read failure notifies rather than panicking.
- `fn reload_active_from_disk(&mut self)` / `fn
  dismiss_external_change(&mut self)` — the `ReloadFromDisk`/
  `DismissExternalChange` command targets, both acting on the active tab
  only (mirrors `ide-ui`'s own "always the active tab" scope for the
  explicit user action, as opposed to `handle_external_modification`'s
  any-tab dispatch).

`open_or_focus_tab` gains a `path` canonicalization at its very start
(`Self::canonicalize_best_effort`, ported near-verbatim from `ide-ui`'s
own helper of the same name) — the "Path identity" invariant
`file-watcher.md` §3.4 requires: every `OpenBuffer::path` must be
canonical so a `WatchEvent`'s already-canonical path matches it by plain
equality. This also fixes a latent, pre-existing dedup gap: a path handed
to `open_or_focus_tab` from anywhere other than the tree scan (e.g. a
test constructing `dir.path().join(...)` directly) previously would not
match an already-open tab whose path came from the (already-canonical)
tree, opening a spurious duplicate instead of refocusing — the identical
gap `file-watcher.md` §3.4 already documented `ide-ui`'s own `open_file`
as having before this same fix.

`trigger_save_active` gains a `watcher.suppress(&path)` call immediately
before `buf.buffer.save_with(charset)` — this crate's only disk-write
call site (no Save As / untitled-buffer support exists yet, so there is
exactly one place to suppress, unlike `ide-ui`'s two).

`commands.rs` gains `Action::ReloadFromDisk`/`Action::DismissExternalChange`,
both bound to `None` (§1.1).

### 2.3 Rendering

`render_tab_strip` gains a bracketed suffix per tab, after the existing
dirty `*` marker: `" [modified on disk]"` for `ExternalChange::Modified`,
`" [deleted on disk]"` for `ExternalChange::Deleted`, nothing otherwise.

## 3. Behaviour

### 3.1 Dispatch

`poll_watcher` maps `WatchEvent::TreeChanged` to `refresh_tree`,
`WatchEvent::FileModified(path)`/`WatchEvent::FileRemoved(path)` to the
tab (if any) whose `path == path` — same three-way dispatch
`file-watcher.md` §2.3/§3.4/§3.5 already specifies, unchanged.

### 3.2 A tab's file changes externally

Clean tab (`!buffer.is_dirty()`): silently reloaded through
`reload_tab_from_disk`, plus a `notify` ("{path} reloaded (changed on
disk).") — `ide-ui`'s own behaviour has no equivalent notice since its
banner mechanism doesn't apply to the silent-reload case either; this
crate adds one anyway since a silent, invisible content swap under a
terminal editor (no diff gutter animation, no visual "this changed" cue
`ide-ui`'s IDE-style UI incidentally provides) would otherwise be
genuinely surprising.

Dirty tab: `external_change = Some(Modified)`, `notify`'s the user, and
waits for `ReloadFromDisk`/`DismissExternalChange` from the palette — same
semantics as `ide-ui`'s Reload/Keep Mine banner buttons.

### 3.3 A tab's file is deleted externally

`external_change = Some(Deleted)` regardless of dirty state (§3.5's own
reasoning: there's no "nothing to lose" case for a file that's simply
gone), `notify`'s the user. Saving afterward recreates the file at that
path (`Buffer::save_with`'s existing behaviour, untouched) — the tab-strip
marker and `external_change` only clear via an explicit
`DismissExternalChange`, not automatically on save, matching `file-watcher.md`
§3.5's own "don't silently disappear the notice" reasoning.

## 4. Constraints / invariants

All of `docs/features/file-watcher.md` §4's seven invariants apply
unchanged (they're properties of the already-merged `ide_core::
FileWatcher`, not of either frontend's wiring). Additionally:
- Exactly one watcher, tied to `App`'s single project root for its whole
  process lifetime — `ide-tui` has no project-switch feature (confirmed
  by `T21`'s own doc, `tui-persist-last-project.md`), so unlike `ide-ui`'s
  `load_project`-driven replace-on-switch, this crate's watcher is
  started once in `App::new` and lives until the process exits.
- `open_or_focus_tab`'s canonicalization is best-effort: a path that
  cannot be canonicalized (nor its parent) is used as-is, letting the
  normal `Buffer::open` failure path surface instead of this silently
  swallowing the problem — identical fallback contract to `ide-ui`'s own
  `canonicalize_best_effort`.

## 5. Examples

Editing a file outside the terminal (another editor, `git checkout`)
while it's open in a clean `ide-tui` tab reloads it automatically, with a
notification recording that it happened. The same edit against a *dirty*
tab instead shows `" [modified on disk]"` in the tab strip until
`ReloadFromDisk`/`DismissExternalChange` is invoked from the palette.
Creating a new file in the project directory from another terminal
refreshes the tree without any user action.

## 6. Dependencies / integration / tests

No new dependency (`notify` is already an `ide-core` dependency, unused by
`ide-tui` until now per the roadmap's own T25 row). Diff scope:
`crates/tui/src/app.rs`, `crates/tui/src/commands.rs`, `crates/tui/src/
lib.rs` (`app.poll_watcher()` in the main loop), `crates/tui/src/ui.rs`,
this doc, `docs/roadmap.md`. No `ide-core`/`ide-lsp` change; the only
new dependency relationship is `ide-tui` now calling an already-existing
`ide-core` type it didn't before — not a security-sensitive path per
`CLAUDE.md`'s list (the watcher itself was already reviewed under G6;
nothing about *how* a second frontend calls the same already-audited
`FileWatcher::new`/`poll`/`suppress` surface introduces a new one).

Tests: `App::new` starts a watcher for a real temp project;
`poll_watcher`'s three dispatch branches (tree refresh picks up an
externally created file, a clean tab silently reloads, a dirty tab sets
`external_change` and does *not* reload); `ReloadFromDisk`/
`DismissExternalChange` each clear `external_change` (one replacing the
buffer, one not); `FileRemoved` sets `Deleted` regardless of dirty state;
`trigger_save_active`'s own write doesn't spuriously set `external_change`
on itself (suppression working end-to-end); `open_or_focus_tab`'s
canonicalization fixes the pre-existing dedup gap (a non-canonical path
targeting an already-open canonical tab refocuses instead of duplicating).

## 7. Revision notes

1. `open_or_focus_tab`'s new canonicalization broke one pre-existing `T17`
   test (`confirm_recent_file_opens_the_selected_row`), which compared an
   opened tab's `path` against a raw, non-canonical `TempDir` path — fixed
   by comparing against the canonicalized form instead, the same fix
   already applied to every other path-comparing test in this file during
   `T17`. No behaviour change; the test's assertion was simply written
   before this phase's canonicalization existed.
2. `refresh_tree`'s `Project::open` re-scan is synchronous on the main
   thread, unlike `ide-ui`'s async tree-scan (`docs/features/
   async-tree-scan.md`, a `ide-ui`-only batch this crate never picked up).
   Acceptable here: `scan_tree()` is already memoized/cheap per its own
   doc comment, and `ide-tui` has no other async-scan precedent to match —
   flagged for awareness, not treated as a gap to fix in this phase.
