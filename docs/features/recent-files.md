# C4 — Recent Files & Recent Locations (`ide-ui`)

## 1. Purpose

Closes `docs/roadmap.md` §2.2's `❌ → C4` row for `ide-ui`: two navigation
popups matching real JetBrains macOS defaults —

- **Recent Files** (`⌘E`) — the MRU list of files this project's tabs have
  been opened/focused in, fuzzy-filterable by typing, persisted across
  sessions.
- **Recent Locations** (`⌘⇧E`) — a reverse-chronological list of the
  caret-position jumps already tracked by `nav_history.rs` (`NavHistory`,
  built for **C1**'s back/forward), each row showing a one-line text
  preview of that location, session-only (see §1.1).

Both are read-only *lists of somewhere to go*, distinct from **C2**'s
`⇧⇧` Search Everywhere (which searches the whole project by name/symbol/
text/action) — real JetBrains keeps these as separate dedicated popups
too, not extra Search Everywhere tabs, and `ide-ui` follows that shape.

This is `ide-ui`-only (`docs/roadmap.md` §6 sizes C4 as `ui`, `S`) — no
`ide-core`/`ide-lsp` change. `ide-tui` already has its own, independently
designed equivalent (`docs/features/tui-recent-files-and-bookmarks.md`,
T17); that doc explicitly cut Recent Locations because *neither* frontend
had navigation-history infrastructure at the time. `ide-ui` already does
(`nav_history.rs`, merged for C1) — this doc's Recent Locations exists
specifically because that gap doesn't apply here.

### 1.1 Scope cuts

- **Recent Locations does not persist across sessions.** `docs/roadmap.md`
  §6's C4 row states "персист между сессиями" (persist across sessions)
  for the feature line as a whole; read narrowly against real JetBrains
  behaviour and this codebase's own existing infrastructure, that
  requirement is naturally Recent Files' (a JetBrains restart still shows
  your last several opened files) — `NavHistory` itself is session-only,
  in-memory, with no `serde` derives today, built purely for the current
  session's back/forward. Retrofitting persistence onto it is a
  `nav_history.rs`-widening change disproportionate to an `S`-sized phase,
  and would need its own answer to "does a stale on-disk caret offset from
  last session still make sense to jump back to" that real JetBrains
  itself answers by pruning aggressively — deferred, not silently dropped:
  flagged here as a known, deliberate v1 cut rather than a gap this doc
  pretends doesn't exist.
- **No typing/fuzzy-filter in the Recent Locations popup.** Real JetBrains
  supports it; cut here the same "cut convenience, keep the core action"
  way `tui-recent-files-and-bookmarks.md` cut numbered bookmarks — the
  list is already bounded (§3.2) and Up/Down plus Enter cover the
  essential jump-back workflow.
- **No text preview in the Recent Files popup** (only in Recent Locations)
  — matches real JetBrains: Recent Files shows plain filenames, Recent
  Locations is the one with code-snippet previews.
- **Recent Locations entries aren't deduplicated beyond what `NavHistory`
  already collapses** (consecutive same-file jumps coalesce to one entry,
  `nav_history.rs`'s own existing `push` behaviour, §2.1) — two separate,
  non-consecutive visits to the same line each get their own row. Real
  JetBrains groups/merges more aggressively; this is a v1 cut in the same
  spirit as the ones above, not a bug.

## 2. Interface

### 2.1 `crates/ui/src/nav_history.rs`: one new accessor

```rust
impl NavHistory {
    /// Every entry, most-recently-pushed first -- `Recent Locations`'
    /// only read of `NavHistory`'s internal list (`entries` itself stays
    /// private). Independent of `current`/back-forward position: shows
    /// the full visited history, not "from here backward".
    pub fn recent_locations(&self) -> impl Iterator<Item = &NavLocation> {
        self.entries.iter().rev()
    }
}
```

No other change to this module — `push`/`go_back`/`go_forward`/
`can_go_back`/`can_go_forward` are untouched.

### 2.2 `crates/ui/src/app.rs`: `WorkspaceState` gains a third field

```rust
const MAX_RECENT_FILES: usize = 20; // matches tui-recent-files-and-bookmarks.md's own cap
const MAX_RECENT_LOCATIONS_SHOWN: usize = 50; // display cap only, not a persisted cap (§1.1)

struct WorkspaceState {
    open_tabs: Vec<OpenTabState>,
    active_path: Option<PathBuf>,
    /// Root-relative paths, most-recently-used first, deduplicated (a
    /// re-open moves the existing entry to the front rather than adding a
    /// second one) -- same convention `OpenTabState.path` already uses
    /// for on-disk storage. `#[serde(default)]` so an older
    /// `workspace.json` without this field just starts empty.
    #[serde(default, deserialize_with = "deserialize_bounded_recent_files")]
    recent_files: Vec<PathBuf>,
}
```

`deserialize_bounded_recent_files` follows `deserialize_bounded_custom_
languages`/`deserialize_bounded_dismissed_suggestions`'s exact existing
pattern (a `Vec<PathBuf>`-typed `Visitor` that stops collecting at
`MAX_RECENT_FILES` and discards the remainder of an oversized array
without allocating for it) — `workspace.json` is untrusted per its own
existing doc comment, and an unbounded `Vec<PathBuf>` here would be the
same latent gap the custom-languages/dismissed-suggestions risk-fix pass
already closed for `ProjectPreferences`'s two array fields.

### 2.3 `crates/ui/src/app.rs`: `IdeApp` new fields

```rust
recent_files_open: bool,
recent_files_query: String,
recent_files_selected: usize,
/// Mirrors `pending_search_everywhere_focus` -- consumed (`mem::take`)
/// by `render_recent_files_popup` on the frame it's `true`, to focus the
/// query text box the moment the popup opens rather than requiring a
/// click first.
pending_recent_files_focus: bool,

recent_locations_open: bool,
recent_locations_selected: usize,
```

Both popups follow the existing `show_refactor_menu_popup`/
`show_generate_menu_popup` shape (a plain `bool` plus whatever selection
state a filterable list needs) rather than `search_everywhere`'s
multi-tab machinery — these are single-list, non-tabbed popups.
`recent_locations_open` needs no focus field: it has no text input (§1.1).

### 2.4 `crates/ui/src/app.rs`: new methods

```rust
impl IdeApp {
    /// Called from both of `open_file`'s branches (already-open-tab
    /// refocus and freshly-opened) -- every successful open or refocus
    /// counts as "recently used" (real JetBrains tracks visits, not just
    /// first-opens, `tui-recent-files-and-bookmarks.md` §2.3 established
    /// the same call site shape for `ide-tui`'s `open_or_focus_tab`).
    /// Moves `path` to the front if already present, inserts at the front
    /// otherwise, truncates to `MAX_RECENT_FILES`. In-memory only -- does
    /// not itself write `workspace.json` (that still only happens from
    /// `flush_project_settings`, matching every other workspace-state
    /// field's existing persistence cadence: on save/project-switch, not
    /// on every mutation).
    fn record_recent_file(&mut self, path: PathBuf);

    /// `⌘E`'s entry point. No-op-with-nothing-to-show is not an error
    /// state here (unlike `trigger_refactor_this`/`trigger_generate_menu`)
    /// -- an empty Recent Files list on a freshly opened project is
    /// completely normal, so the popup opens either way and shows "No
    /// recent files." itself (§3.3), matching `render_generate_menu_
    /// popup`'s own empty-state-inside-the-window precedent rather than
    /// bouncing an error through `self.error`.
    fn trigger_recent_files(&mut self) {
        self.recent_files_open = true;
        self.recent_locations_open = false;
        self.recent_files_query.clear();
        self.recent_files_selected = 0;
        self.pending_recent_files_focus = true;
    }

    /// Empty query: `recent_files` verbatim (MRU order). Non-empty query:
    /// every entry scored via `ide_core::fuzzy_score(query, &display)`
    /// where `display` is the path *relative to the project root*, not
    /// the absolute canonical path stored internally -- scoring the
    /// absolute path would let an unrelated segment of a canonicalized
    /// temp/home directory spuriously match every entry, the exact bug
    /// `tui-recent-files-and-bookmarks.md` §7.1 already hit and fixed for
    /// the identical shape of query; this doc adopts that fix from the
    /// start rather than re-discovering it. Dropped on `None`, sorted by
    /// score descending. Synchronous, no background thread -- the list is
    /// bounded by `MAX_RECENT_FILES`, cheap enough to score every
    /// keystroke (same reasoning as the `ide-tui` precedent).
    fn recent_files_rows(&self) -> Vec<PathBuf>;

    /// `↑`/`↓` while the popup is open, **clamped** to the current
    /// filtered row count -- not wrapping like `search_everywhere_move_
    /// selection`/`file_structure_move_selection`, since this MRU-ordered
    /// list has a meaningful "top". `render_recent_files_popup` resets
    /// `recent_files_selected` to `0` whenever its query text changes
    /// (the same place -- not a key handler -- `render_file_structure_
    /// popup` already does this for its own filtered list, since a
    /// filtered list can shrink on every keystroke).
    fn recent_files_move_selection(&mut self, delta: isize);

    /// `Enter`, or clicking a row: opens/refocuses the selected row via
    /// `open_file` -- deliberately does **not** set `pending_cursor_
    /// offset`, unlike a Go to Definition/Find Usages jump: Recent Files'
    /// whole point is "go back to where I already was", so it leaves the
    /// reopened tab's own last-known cursor position alone rather than
    /// forcing offset 0. No-op if the selection is out of range for the
    /// current (possibly filtered) row list. No new request to any LSP
    /// client.
    fn recent_files_confirm(&mut self);

    fn close_recent_files(&mut self);

    /// `true` exactly while the popup is open -- checked in
    /// `handle_shortcuts`'s escape-arbitration chain alongside
    /// `file_structure_owns_escape` (§4).
    fn recent_files_owns_escape(&self) -> bool;

    /// `⌘⇧E`'s entry point. Same open-regardless-of-emptiness posture as
    /// `trigger_recent_files`.
    fn trigger_recent_locations(&mut self) {
        self.recent_locations_open = true;
        self.recent_files_open = false;
        self.recent_locations_selected = 0;
    }

    /// `self.nav.recent_locations()` (§2.1), taken up to
    /// `MAX_RECENT_LOCATIONS_SHOWN`, paired with a 1-based display line
    /// (`ide_lsp::byte_offset_to_position`'s 0-based `Position.line`,
    /// `+1`) and a one-line preview: the trimmed text of the line
    /// containing `offset`, read from the open tab's live buffer if that
    /// path is currently open (so an unsaved edit's preview reflects
    /// what's actually on screen), else a best-effort `std::fs::
    /// read_to_string` of the file on disk (the same read-only,
    /// no-validation-needed fallback `apply_workspace_edit` already uses
    /// for an unopened file's *old* text -- these paths only ever
    /// originate from `self.nav`, itself only ever pushed from already-
    /// validated jump sites, `push_nav_location`'s own doc comment). Both
    /// `None` (row still shown, rendered as "(unavailable)") if the read
    /// fails or `offset` no longer lands inside the file's current length
    /// (the file shrank since the visit was recorded) -- permissive,
    /// matching `confirm_recent_file`'s stale-entry precedent in the
    /// `ide-tui` doc rather than pruning the list.
    fn recent_locations_rows(&self) -> Vec<(NavLocation, Option<u32>, Option<String>)>;

    /// `↑`/`↓` while the popup is open, clamped -- same rationale as
    /// `recent_files_move_selection`.
    fn recent_locations_move_selection(&mut self, delta: isize);

    /// `Enter`, or clicking a row: opens the location's file and sets
    /// `pending_cursor_offset` to its `offset` -- the exact `nav_back`/
    /// `nav_forward` mechanism, deliberately **not** calling
    /// `push_nav_location` for the same "don't let Back/jump-back-history
    /// itself grow history" invariant §4 states. No-op if the selection
    /// is out of range. No query field (§1.1).
    fn recent_locations_confirm(&mut self);

    fn close_recent_locations(&mut self);

    fn recent_locations_owns_escape(&self) -> bool;
}
```

All six list-navigation methods above share one shape with every other
list popup this crate already has (`search_everywhere_move_selection`/
`_confirm`, `file_structure_move_selection`/`_confirm`/`_owns_escape`):
`*_move_selection`/`*_confirm`/`*_owns_escape`, centrally polled once per
frame inside `handle_shortcuts` (`app/render.rs`) via
`ctx.input(|i| i.key_pressed(...))`, with `*_owns_escape` checked in that
function's shared escape-arbitration priority chain alongside
`file_structure_owns_escape`. This crate has no per-popup
`handle_..._key(event: &egui::Event) -> bool` convention anywhere --
verified by reading `file_structure_move_selection`/`_confirm`/
`_owns_escape` and their `handle_shortcuts` call sites directly before
adopting this shape here rather than the ad hoc one an earlier draft of
this doc specified.
```

`open_file` gains one call to `self.record_recent_file(path.to_path_buf())`
in each of its two existing branches (the early-return already-open-tab
branch, and the freshly-opened `Ok(buffer)` branch) — mirrors
`tui-recent-files-and-bookmarks.md` §2.3's two-call-site shape, collapsed
here into one function since `ide-ui`'s `open_file` (unlike `ide-tui`'s
`open_or_focus_tab`) already has both branches in one place.

`flush_project_settings` gains `recent_files: self.recent_files.clone()`
in the `WorkspaceState` it builds (root-relative, same `strip_prefix(root)`
treatment `open_tabs`/`active_path` already get — an entry that fails to
strip, e.g. one pointing outside the project root via a Find-Usages jump
into a dependency, is silently dropped from the *persisted* list only,
same as `active_path`'s existing `.ok()` shape; the in-memory MRU keeps
the absolute path for the current session).

`load_project_settings` restores `recent_files` **before** the existing
`open_tabs` restoration loop (each restored path re-validated through the
already-existing `resolve_restorable_tab_path`, silently dropping any
entry that fails — deleted file, path traversal, symlink escape outside
root, exactly `open_tabs`' own restore-time validation), so that the
tab-restoration loop's own `open_file` calls naturally re-order the list
around whichever tabs actually got restored, without a second, separate
recency computation.

`ide-ui` has no shared cross-popup exclusivity sweep today — unlike
`ide-tui`'s `close_all_overlays`, each existing popup here
(`show_refactor_menu_popup`, `show_generate_menu_popup`,
`search_everywhere_open`, `show_code_actions_popup`, ...) is closed only
by its own trigger/select/Esc logic, confirmed by reading
`trigger_refactor_this`/`trigger_generate_menu`/`close_search_everywhere`
directly — none of them close each other. This doc does **not** invent
that mechanism. It scopes exclusivity to just the two popups it adds:
`trigger_recent_files` sets `self.recent_locations_open = false` and
`trigger_recent_locations` sets `self.recent_files_open = false`, each in
addition to opening its own flag — a two-line addition in each trigger,
not a new shared framework. Neither new popup closes, or is closed by,
any *other* pre-existing popup (Search Everywhere, Generate menu, Refactor
menu, code actions popup), matching this crate's existing convention
exactly rather than extending it.

### 2.5 `crates/ui/src/command.rs`: two new commands

```rust
Command {
    id: "RecentFiles",
    title: "Recent Files",
    category: "Navigate",
    binding: Some(Binding::same(KeyChord::new(Key::E).command())),
    action: CommandAction::RecentFiles,
},
Command {
    id: "RecentLocations",
    title: "Recent Locations",
    category: "Navigate",
    binding: Some(Binding::same(KeyChord::new(Key::E).command().shift())),
    action: CommandAction::RecentLocations,
},
```

Both are a plain `Cmd`→`Ctrl` letter substitution (real JetBrains uses the
same `⌘E`/`Ctrl+E` and `⌘⇧E`/`Ctrl+Shift+E` letter on every platform,
verified via `WebSearch` against JetBrains' own reference-keymap
documentation before adoption — this doc's own §6 lists the sources), so
both are `Binding::same`, not a two-chord split like `GenerateMenu`'s.
Checked against the full existing registry: no other command uses
`Key::E` in any modifier combination — no collision.

### 2.6 `crates/ui/src/app/render.rs`: two new popups

`render_recent_files_popup` — structurally `render_search_everywhere_
popup`'s Files-tab row rendering (a text-input line, then a scrollable
selectable-label list, `REVERSED`-equivalent highlight on
`recent_files_selected`) minus the tab bar, mirroring the single-list
shape `render_generate_menu_popup`/`render_refactor_menu_popup` already
established for a non-tabbed popup. Empty-list state: label "No recent
files." (§2.4).

`render_recent_locations_popup` — same list-popup shape, no text input
(§1.1), each row `path:line` (1-based line: `ide_lsp::byte_offset_to_
position`'s 0-based `Position.line`, `+1` for display — the same
0-based-to-1-based conversion this crate already applies at its other
line-number-in-UI call sites) followed by the preview text in a
dimmed/secondary style, or "(unavailable)" per §2.4's fallback. Empty-list
state: label "No recent locations."

Both wired into `eframe::App::update`'s popup-render dispatch list
alongside `render_refactor_menu_popup`/`render_generate_menu_popup`.

## 3. Behaviour

### 3.1 Recording

Every `open_file` call — from the project tree, Go to File/Class/Symbol/
Line, Go to Definition, Find Usages, a search result, the Refactor
Preview's "open file" action, or Recent Files/Recent Locations themselves
opening their own selected row — records that path into `recent_files`
(MRU, deduplicated, capped at `MAX_RECENT_FILES`). `NavHistory` already
records the *positional* history for Recent Locations independently, via
`push_nav_location`'s own existing call sites (§2.1) — this doc adds no
new call to `nav.push`.

### 3.2 Recent Files (`⌘E`)

Opens showing the full MRU list, empty query, selection on the first row.
Typing filters by fuzzy score against each entry's project-relative
display path; `Up`/`Down` move the selection (wrapping is **not** applied
— clamped at both ends, matching `render_generate_menu_popup`'s row list
rather than `search_everywhere_move_selection`'s wrap-around, since this
is a flat single list a user scans top-to-bottom, not a tabbed multi-
section view). `Enter` opens/refocuses the selected row's file, preserving
whatever cursor position that tab already has. `Esc` closes without
acting.

### 3.3 Recent Locations (`⌘⇧E`)

Opens showing up to `MAX_RECENT_LOCATIONS_SHOWN` entries from `nav.
recent_locations()`, most-recent-first, each with its one-line preview.
`Up`/`Down`/`Enter`/`Esc` as in §2.4. `Enter` opens the file and jumps the
caret to the recorded offset (top-aligned reveal, matching `nav_back`/
`nav_forward`'s own existing scroll behaviour) — it does **not** push a
new `NavHistory` entry (§4).

## 4. Constraints / invariants

- `recent_files_open`/`recent_locations_open` are mutually exclusive with
  **each other** (opening one closes the other, §2.4) but not with any
  other pre-existing popup — matching this crate's existing convention,
  where no popup closes any other today (`ide-ui` has no `ide-tui`-style
  shared `close_all_overlays` sweep).
- `recent_files` (in-memory and persisted) never exceeds `MAX_RECENT_
  FILES` (20) entries.
- Jumping via Recent Locations' `Enter` never calls `push_nav_location` —
  otherwise every jump-back-into-history press would immediately grow a
  new forward-erasing entry, the identical invariant `nav_back`/
  `nav_forward` already state for themselves.
- A malformed or missing `workspace.json` never blocks project load or
  any other feature — `recent_files` simply defaults to empty, the same
  fail-open posture `load_project_settings` already has for `open_tabs`/
  `active_path`.
- Recent Locations shows only the current session's navigation history;
  it starts empty on every fresh launch (§1.1) — this is a documented v1
  cut, not a bug to file against this phase.

## 5. Examples

Opening `main.rs`, then `lib.rs`, then re-opening `main.rs`: `⌘E` shows
`main.rs` first (most-recently-used, not most-recently-*first*-opened),
then `lib.rs` — two entries, not three, `main.rs`'s second open moved it
rather than duplicating it. Quitting and reopening the project still shows
both, in the same order, restored from `workspace.json`.

Using Find Usages to jump through four different call sites across two
files, then pressing `⌘⇧E`, shows four rows in reverse-visit order, each
with a snippet of that call site's line — closing the project and
reopening it clears this list (§1.1), unlike Recent Files.

## 6. Dependencies / integration / tests

No new external dependency. `git diff --name-only main` after this phase:
`crates/ui/src/nav_history.rs`, `crates/ui/src/app.rs`,
`crates/ui/src/app/render.rs`, `crates/ui/src/command.rs`, this doc,
`docs/roadmap.md`. No `crates/core/**`/`crates/lsp/**` change — this
phase's role is `ui` only, matching `docs/roadmap.md` §6's sizing.

Not on `CLAUDE.md`'s security-sensitive-path list, and this diff doesn't
touch any path that is (no subprocess, no `lsp_bridge.rs` change, no
project-root path validation logic beyond reusing the already-hardened
`resolve_restorable_tab_path`) — `hacker` is not expected to trigger for
this phase; confirmed against the actual diff before merge, not assumed
from this plan alone.

Tests required, ≥80% line coverage on every touched non-rendering file:
- `nav_history.rs`: `recent_locations()` ordering (most-recent-first) and
  its interaction with same-file coalescing (§1.1's third bullet).
- `app.rs`: `record_recent_file` (dedup-and-move-to-front, cap at
  `MAX_RECENT_FILES`), `deserialize_bounded_recent_files` (mirrors the
  existing two bounded-array tests' shape), `recent_files_rows` (empty
  query = MRU verbatim, non-empty query scores against relative path —
  regression-test the temp-directory-spoofing case §2.4 calls out by
  name), `recent_files_move_selection` (clamp not wrap),
  `recent_files_confirm` (opens without setting `pending_cursor_offset`,
  no-op out of range), `recent_locations_rows` (preview from open buffer
  vs. disk fallback vs. unavailable), `recent_locations_confirm` (sets
  `pending_cursor_offset` and does not call `push_nav_location`),
  `flush_project_settings`/`load_project_settings` round-trip for the new
  field, `resolve_restorable_tab_path` reuse on restore (deleted/
  traversal/symlink entries dropped, matching its own existing test
  suite's cases).
- `command.rs`: the two new bindings' non-collision (already covered by
  the registry's existing generic `no_two_commands_in_the_registry_share_
  the_same_default_mac_chord` test) plus a dedicated chord-value assertion
  for each, matching every prior phase's own per-command test convention.
- `app/render.rs`: excluded from the coverage target (rendering-only,
  matching every other popup in this file).

## 7. Sources

Keybindings verified via `WebSearch` against JetBrains' own documentation
before adoption (§2.5):
- [Recent Locations - JetBrains Guide](https://www.jetbrains.com/guide/go/tips/recent-locations/) — confirms `⌘⇧E` and that Recent Locations shows code snippets, Recent Files does not.
- [Predefined macOS keymap | IntelliJ IDEA Documentation](https://www.jetbrains.com/help/idea/reference-keymap-mac-default.html) — confirms `⌘E` for the Recent Files popup.

## 8. Revision notes

1. §2.4/§4 originally described a `close_all_overlays`-based mutual-
   exclusivity mechanism, copied by mistake from `ide-tui`'s design
   (`tui-recent-files-and-bookmarks.md` §2.3) — `ide-ui` has no such
   function or shared closing sweep at all (confirmed by grepping
   `crates/ui/src/`: zero hits, and by reading `trigger_refactor_this`/
   `trigger_generate_menu`/`close_search_everywhere` directly, each of
   which only ever touches its own flag). Fixed by scoping exclusivity to
   just the two new popups directly in their own trigger functions
   (`trigger_recent_files`/`trigger_recent_locations` each clear the
   other's `_open` flag), matching this crate's actual existing
   convention (no popup closes any *other* pre-existing popup) rather
   than inventing a new shared mechanism.
2. §2.6's justification for the Recent Locations popup's 1-based line
   number cited `find_usages_target` as precedent for "UI-facing 1-based
   displays" -- inaccurate, `find_usages_target`'s `Position` is 0-based
   LSP-wire data for `find_references`, never displayed to the user.
   Reworded to cite the actual mechanism (`byte_offset_to_position` +
   `1`) without the incorrect precedent; the specified behavior itself
   was already correct.
3. Added `pending_recent_files_focus` (§2.3), missing from the original
   draft, so the Recent Files query box auto-focuses on open the same way
   `pending_search_everywhere_focus` already does for Search Everywhere,
   instead of silently requiring a click first.
4. (Post-implementation, `rust-ui-dev` code review round 1) §2.4/§6
   originally specified `handle_recent_files_key(&mut self, key: &egui::
   Event) -> bool` / `handle_recent_locations_key(...)` -- a shape that
   turned out not to exist anywhere in this crate. While implementing,
   `rust-ui-dev` found via direct inspection of `file_structure_move_
   selection`/`_confirm`/`_owns_escape` and their `handle_shortcuts` call
   sites that every existing list popup here instead uses a `*_move_
   selection(delta)`/`*_confirm()`/`*_owns_escape()` triple, centrally
   polled once per frame in `handle_shortcuts` (`app/render.rs`) via
   `ctx.input(|i| i.key_pressed(...))`, with escape handled in that
   function's shared arbitration chain -- not a per-popup raw-`egui::
   Event` handler. Implemented that real convention instead of the
   originally-specified one; this doc's §2.4/§6 are now updated to match
   what was actually built, including `recent_locations_rows`'s actual
   3-tuple return type (`Vec<(NavLocation, Option<u32>, Option<String>)>`,
   adding the 1-based display line §2.6 needed) instead of the originally
   -specified 2-tuple.
