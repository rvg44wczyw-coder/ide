# T17 — Recent Files & Bookmarks (`ide-tui`)

## 1. Purpose

Ports the navigation half of `docs/roadmap.md`'s **C4**
(`recent-files.md`) and **C5** (`bookmarks.md`) tracks to `ide-tui`. Both
are still `❌` in `ide-ui` itself (`docs/roadmap.md` §2.2) — there is no
existing `ide-ui` implementation to port pixel-for-pixel the way most
prior `T`-items ported one; this doc designs the `ide-tui`-scoped v1
directly from the roadmap's own C4/C5 one-line descriptions and the real
JetBrains macOS/other keymap defaults, the same way `T18a`/`T18b` designed
straight from a roadmap line when no earlier phase had built the feature
first.

### 1.1 Scope cuts (explicit, matching this crate's established discipline)

- **Recent Locations** (`⌘⇧E` / `Ctrl+Shift+E`) — cut. JetBrains' Recent
  Locations tracks caret-position history with line-preview snippets; that
  needs a navigation-history mechanism (back/forward through visited
  positions) that doesn't exist in *either* frontend yet — `ide-ui`'s own
  C1 (`goto-definition.md`, "плюс история навигации (назад/вперёд)") is
  still `❌` too (`docs/roadmap.md` §2.2). Building history tracking as a
  side effect of this doc would be scope creep into C1's job. Deferred
  until that infra lands.
- **Numbered bookmarks** (JetBrains' `F1`–`F9` mnemonic assign/jump) — cut.
  Convenience over the basic toggle+list, the same "cut convenience, keep
  the core action" call `T16` made for Go to Class over Go to Symbol.
- **Gutter bookmark markers** — cut. `render_editor` (`ui.rs`) has no
  line-number/gutter column at all yet — confirmed by reading it before
  writing this doc, not assumed. There is nothing to attach a marker glyph
  to; this is a pre-existing gap this phase doesn't own, not a regression
  it introduces. Bookmarked lines are only visible via Show Bookmarks'
  popup for now.
- **Removing a bookmark from the Show Bookmarks popup** (e.g. `Delete`) —
  cut for v1. Toggle Bookmark at the source line is the only way to
  remove one; the popup is read/jump-only. A future revision can add this
  without changing the popup's shape.
- **Line-text preview in either popup** — cut. Both popups show only
  `path`/`path:line`, matching this crate's existing single-line-per-row
  popup convention (`render_go_to_file_popup` et al.) rather than growing
  a two-line-per-row layout for this feature alone.

### 1.2 Bindings

| Action | mac | other | Source |
|---|---|---|---|
| Recent Files | `Ctrl+E` (Ctrl-translated) | `Ctrl+E` | real JetBrains default is `⌘E`/`Ctrl+E` — same letter both platforms, `Cmd`→`Ctrl` translation only |
| Toggle Bookmark | `F3` (literal) | `F3` | genuine `{mac: F3, other: F11}` split in real JetBrains keymaps — same "no single mac binding to translate" case `QuickDocumentation`'s `{F1, Ctrl+Q}` already established (`commands.rs`'s module doc); `F3` used literally, `F11` not modeled since this crate has one binding table, not a `{mac, other}` pair, and `F3` needs no `Ctrl`-masking on any terminal |
| Show Bookmarks | `Ctrl+F3` (Ctrl-translated) | `Ctrl+F3` | real JetBrains mac default is `⌘F3`; `Cmd`→`Ctrl`, `F3` stays literal |

All three verified non-colliding against the full `commands.rs` binding
table before adoption (`Ctrl+E`, bare `F3`, `Ctrl+F3` — none previously
assigned).

## 2. Interface

### 2.1 `ide-core`: `project_settings.rs`'s third slot

`crates/core/src/project_settings.rs`'s `ProjectSettingsFile` enum gains a
third variant:

```rust
pub enum ProjectSettingsFile {
    Preferences,
    Workspace,
    /// Recent-files/bookmarks-style navigation aids -- a slot content-named
    /// (not frontend-named) the same way `Preferences`/`Workspace` are, per
    /// this module's own stated extension mechanism ("a future feature
    /// needing its own project-scoped state adds a new variant/file here
    /// rather than growing `Preferences`/`Workspace`"). `ide-tui` is this
    /// slot's first user; nothing about the name or file ties it to one
    /// frontend, so a future `ide-ui` C4/C5 implementation could read the
    /// same `navigation.json` and share history across frontends on one
    /// project -- not required by this phase, just not precluded by it.
    Navigation,
}
```

`file_name()` maps it to `"navigation.json"`. No other change to this
module — `settings_dir`, `read`, `write`, `ensure_gitignored` are already
generic over the stored type and the slot name; adding a third named file
introduces no new security surface (the already-hardened symlink-escape
rejection and atomic-write-via-tempfile logic apply unconditionally to
every slot, this one included). Per `CLAUDE.md`'s security-sensitive-path
list, this file isn't itself named there, and this specific change is a
one-`match`-arm, non-behavioral addition to already-audited machinery — no
`hacker` pass triggered by this diff alone (verified against the full diff
scope in §6).

### 2.2 `ide-tui`: `crates/tui/src/project_state.rs` (new)

```rust
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProjectNavigationState {
    pub(crate) recent_files: Vec<PathBuf>,
    pub(crate) bookmarks: Vec<Bookmark>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Bookmark {
    pub(crate) path: PathBuf,
    pub(crate) line: usize,
}

pub(crate) const MAX_RECENT_FILES: usize = 20;

impl ProjectNavigationState {
    /// Moves `path` to the front if already present (dedup, not duplicate
    /// entries), inserts at the front otherwise, then truncates to
    /// `MAX_RECENT_FILES` -- MRU order, capped.
    pub(crate) fn record_recent_file(&mut self, path: PathBuf);

    /// Toggles a bookmark at `(path, line)`: removes it if present, adds
    /// it (appended, insertion order) otherwise. Returns `true` if the
    /// bookmark was just added, `false` if it was just removed -- the
    /// caller uses this only to word its notification, not for control
    /// flow.
    pub(crate) fn toggle_bookmark(&mut self, path: PathBuf, line: usize) -> bool;
}

pub(crate) fn load(project_root: &Path) -> ProjectNavigationState;
pub(crate) fn save(project_root: &Path, state: &ProjectNavigationState);
```

`load`/`save` wrap `ide_core::project_settings::{read, write}` with
`ProjectSettingsFile::Navigation`, following `state.rs`'s (`T21`) own
established convention exactly: `load` collapses every failure mode
(missing file, malformed JSON, an unreadable `.ide/`) to
`ProjectNavigationState::default()` rather than surfacing an error nobody
downstream could act on; `save` is a best-effort fire-and-forget (`let _
= ...`) for the identical reason `state.rs`'s `save` already documents —
a failed write here must never crash an editing session over a
convenience feature.

### 2.3 `ide-tui`: `app.rs` additions

```rust
#[derive(Default)]
pub(crate) struct RecentFilesState {
    pub(crate) query: String,
    pub(crate) selected: usize,
}

#[derive(Default)]
pub(crate) struct BookmarksPopupState {
    pub(crate) selected: usize,
}
```

`App` gains:
- `pub(crate) nav_state: ProjectNavigationState` — loaded once in
  `App::new` via `project_state::load(project.root())`.
- `pub(crate) recent_files: Option<RecentFilesState>`
- `pub(crate) bookmarks_popup: Option<BookmarksPopupState>`

New methods (all `fn`, private except where `ui.rs` needs read access):
- `fn toggle_recent_files(&mut self)` / `fn toggle_bookmarks_popup(&mut self)`
  — same open/close-via-`close_all_overlays` shape `toggle_go_to_file`
  already establishes.
- `fn handle_recent_files_key`/`handle_bookmarks_popup_key(&mut self, key)
  -> LoopSignal` — same per-key shape as `handle_go_to_file_key`.
- `pub(crate) fn recent_files_rows(&self) -> Vec<PathBuf>` — empty query:
  `nav_state.recent_files` verbatim (MRU order preserved). Non-empty
  query: every recent path scored via `ide_core::fuzzy_score(query, &path.
  display().to_string())`, dropped on `None`, sorted by score descending.
  No background thread (`files_search.rs`'s own machinery) — the list is
  bounded by `MAX_RECENT_FILES`, cheap enough to score synchronously every
  keystroke, unlike a whole-project file scan.
- `fn confirm_recent_file(&mut self)` — opens the selected row via
  `open_or_focus_tab` (re-focuses without moving the caret if already
  open, unlike Go to File's forced jump-to-offset-0 — Recent Files' whole
  point is "go back to where you were", not "go to the top of this file").
- `fn toggle_bookmark_at_cursor(&mut self)` — no active tab: notifies
  `"No file open to bookmark."` and no-ops. Otherwise toggles at the
  active buffer's primary caret's current line
  (`cursor_line_column(...).0`), persists via `project_state::save`, and
  notifies `"Bookmark added at line N."`/`"Bookmark removed at line N."`
  (1-based for the human-facing message, matching every other 1-based
  line number this crate already surfaces, e.g. Go to Line's absence
  notwithstanding — see `editor.rs`'s own 1-based column-in-status
  convention).
- `fn confirm_bookmark_jump(&mut self)` — opens the bookmark's file, then
  best-effort places the caret at that line's start via
  `text_buffer.lines().line_start(line)` (`None` — the file shrank since
  the bookmark was recorded — silently closes the popup without moving
  the caret, the same permissive shape `open_location` already uses for
  its own "line vanished" case) and `Self::scroll_to_and_reveal`.

`open_or_focus_tab` gains one call each in its two branches (already-open:
right before its early `return Ok(())`; freshly-opened: right after
`self.active_tab = Some(...)`) to a new private `fn record_recent_file
(&mut self, path: PathBuf)` that updates `nav_state` and persists it —
every successful open or refocus counts as "recently used", matching real
JetBrains Recent Files semantics (it tracks visits, not just first-opens).

`handle_key`'s interception chain gains, right after the existing
`go_to_symbol` check:
```rust
if self.recent_files.is_some() { return self.handle_recent_files_key(key); }
if self.bookmarks_popup.is_some() { return self.handle_bookmarks_popup_key(key); }
```

`close_all_overlays` gains `self.recent_files = None; self.bookmarks_popup
= None;`. `run_action` gains three arms before `Exit`:
`Action::RecentFiles => self.toggle_recent_files()`,
`Action::ToggleBookmark => self.toggle_bookmark_at_cursor()`,
`Action::ShowBookmarks => self.toggle_bookmarks_popup()`.

## 3. Behaviour

### 3.1 Recent Files (`Ctrl+E`)

Opens a popup seeded with the full MRU list (most-recent-first), no query.
Typing filters via fuzzy score against the list only (never a project-wide
scan — this is not Go to File). `Up`/`Down` move the selection, clamped to
the current filtered row count; typing or `Backspace` resets `selected` to
`0` (same stale-selection-avoidance rule `T16`'s self-review fix
established for Go to File/Go to Symbol — a filtered list can shrink on
every keystroke here just as easily as there). `Enter` opens/refocuses the
selected row without moving its caret. `Esc` closes without acting.

### 3.2 Toggle Bookmark (`F3`) / Show Bookmarks (`Ctrl+F3`)

`F3` with no active tab notifies and no-ops; otherwise toggles a bookmark
at the caret's current line and persists immediately (`project_state::
save`, matching `T21`'s "persist on success, not speculatively" posture —
here there's no failure mode to gate on, so it's simply unconditional).

`Ctrl+F3` opens a popup listing every bookmark across the project
(insertion order — the order bookmarks were toggled on, not sorted by path
or recency), one row per `path:line` (1-based line number, path relative
display exactly as `Path::display()` renders it — the same untruncated
convention `render_go_to_file_popup` uses for `m.relative`). `Enter` opens
that file and jumps to the line (top-aligned via `scroll_to_and_reveal`,
same as `open_location`/`jump_to_match`). `Esc` closes.

### 3.3 Rendering

Two new popups, `render_recent_files_popup`/`render_bookmarks_popup`,
structurally identical to `render_go_to_file_popup`/
`render_go_to_symbol_popup` (centered `Clear`+bordered `List`, title
embeds the query/instructions, `REVERSED` style on the selected row).
Wired into `render()`'s dispatch list alongside the existing `go_to_file`/
`go_to_symbol` checks.

## 4. Constraints / invariants

- `recent_files`/`bookmarks_popup` participate in the same overlay-
  exclusivity `close_all_overlays` already enforces for every other
  overlay in this crate — opening either closes any other open overlay
  first, and neither can be open while the other is.
- `nav_state.recent_files` never exceeds `MAX_RECENT_FILES` (20) entries.
- A malformed or missing `navigation.json` never blocks startup or any
  other feature — `project_state::load` always returns a usable (possibly
  empty) `ProjectNavigationState`, the identical fail-open posture
  `state.rs`'s `load`/`load_from` already established for the global
  last-project file.
- `record_recent_file`/`toggle_bookmark_at_cursor` write synchronously on
  the calling thread (no background thread, matching `state.rs`'s own
  choice) — a `navigation.json` write is small and infrequent enough
  (once per tab focus / explicit bookmark toggle, not once per frame or
  per keystroke) that this doesn't warrant the channel-plus-poll machinery
  `cargo_panel.rs`/`files_search.rs` use for genuinely slow operations.

## 5. Examples

Toggling a bookmark, closing the file, reopening the project later, and
pressing `Ctrl+F3` still shows that bookmark — `navigation.json` persists
it exactly like `.ide/preferences.json`/`workspace.json` already persist
`ide-ui`'s own project-scoped state.

Opening five different files via the tree, then pressing `Ctrl+E`, shows
all five in reverse-open order; typing part of the third file's name
filters the popup down to just that match (scored, not substring-only, so
`"mai"` matches `main.rs` even with an intervening directory-name
character mismatch elsewhere in the path).

## 6. Dependencies / integration / tests

No new external dependency (`serde`/`serde_json` already present in
`crates/tui/Cargo.toml` since `T21`). `git diff --name-only main` after
this phase: `crates/core/src/project_settings.rs`,
`crates/tui/src/project_state.rs` (new), `crates/tui/src/app.rs`,
`crates/tui/src/commands.rs`, `crates/tui/src/lib.rs` (`mod
project_state;`), `crates/tui/src/ui.rs`, this doc, `docs/roadmap.md`.
`crates/core/src/project_settings.rs` is touched but, per §2.1, introduces
no new security surface — confirmed by re-reading the diff before merge
rather than assumed.

Tests required: `project_state.rs` (record/dedup/cap, toggle add/remove
round-trip, load/save round-trip, load-on-malformed-json-returns-default,
load-on-fresh-dir-returns-default) at ≥80% coverage; `app.rs`'s new
methods (open/close/mutual-exclusivity with other overlays, query
filtering and its selection-reset, Enter/Esc behavior for both popups,
`toggle_bookmark_at_cursor`'s no-active-tab case, `open_or_focus_tab`'s
recent-file recording on both its branches); `commands.rs`'s three new
binding lookups plus non-collision.

## 7. Revision notes

1. `recent_files_rows`' fuzzy filter originally scored `ide_core::
   fuzzy_score` against each recorded path's *full absolute* display
   string. Caught during implementation (a test wrote to a canonicalized
   macOS temp directory and every entry matched every query): a subsequence
   fuzzy match over a whole absolute path lets an unrelated segment (a
   temp/home-directory component that happens to contain the query's
   letters in order) spuriously match every entry, not just the intended
   one. Fixed by scoring against the path stripped of `project_root`
   instead (falling back to the full path if `strip_prefix` fails, e.g. a
   file opened from outside the project root via a Goto/Find Usages jump)
   — the same project-relative-display convention `files_search.rs`'s
   `FuzzyFileMatch.relative` already established for Go to File.
2. A recent-files or bookmark entry pointing at a file since deleted from
   disk isn't pruned automatically — `confirm_recent_file`/
   `confirm_bookmark_jump` surface the resulting `Buffer::open` error via
   `notify` and leave the popup open with the stale entry still listed,
   the same permissive behavior `confirm_go_to_file` already has for an
   unopenable match. Accepted as consistent with existing precedent rather
   than a gap this phase needs to close.
3. `crates/core/src/project_settings.rs` is touched by this phase (a third
   `ProjectSettingsFile::Navigation` variant) but isn't itself named on
   `CLAUDE.md`'s security-sensitive-path list, and the change is a single
   non-behavioral `match`-arm addition to already-hardened, generic
   read/write/symlink-escape-rejection machinery — no `hacker` pass
   triggered, confirmed against the actual diff (`git diff --name-only
   main`) rather than assumed from the plan alone.
