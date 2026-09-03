# TUI Back/Forward Navigation (T31)

## 1. Purpose

Ports the navigation-history half of `nav_history.rs`/`fleet-shell.md`
(`ide-ui`'s C1) to `ide-tui`: a Back/Forward stack over file+offset
locations, so jumping somewhere (Go to Definition, a search result, a
debugger stack frame, Go to File, a bookmark, a tree click, a scratch
file) can be undone with one keystroke, the same way a web browser's
Back button undoes a link click.

`ide-tui` has no top bar for a literal back-arrow button, so this port is
keyboard-only: `Ctrl+Alt+Left`/`Ctrl+Alt+Right`, `ide-ui`'s own
already-shipped binding on non-mac platforms (`crates/ui/src/command.rs`
lines 607–622, `NavigateBack`/`NavigateForward`, `⌘⌥←`/`⌘⌥→` on mac
substituted to `Ctrl+Alt+Left`/`Ctrl+Alt+Right` per `CLAUDE.md`'s stated
"`other` defaults to `mac` with modifiers substituted" rule — this
happens to be JetBrains' genuine native Windows/Linux binding too, no
divergence to special-case).

**Explicitly out of scope**: everything else in `fleet-shell.md` (top-bar
redesign, Smart Mode, slim tree, thin tabs, tool-window edge icons,
Zen/Distraction-Free mode) is GUI chrome with no terminal analog —
deliberately excluded from TUI parity, the same treatment already given
to `native-menu-bar.md` and `themes.md` (`docs/roadmap.md` §10). Recent
Locations (`recent-files.md`, a *consumer* of this same history, listing
`nav.recent_locations()`) is also out of scope — `ide-tui` cut Recent
Locations already (`tui-recent-files-and-bookmarks.md`, noted at the
time as a possible future follow-up); this doc only ports the history
mechanism itself and the jump sites that feed it, not that separate
browsing UI.

## 2. Interface

### 2.1 `crates/tui/src/nav_history.rs` (new)

A near-verbatim port of `crates/ui/src/nav_history.rs` — pure logic, zero
`egui` dependency, so the port changes nothing but `use` paths:

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct NavLocation {
    pub path: PathBuf,
    pub offset: usize,
}

#[derive(Default)]
pub struct NavHistory {
    entries: Vec<NavLocation>,
    current: Option<usize>,
}

impl NavHistory {
    // Constructed via `NavHistory::default()` -- no explicit `new()`,
    // matching `crates/ui/src/nav_history.rs` exactly.

    /// Pushes `location`. If the current entry is for the same `path`,
    /// replaces it in place rather than growing the list (repeated jumps
    /// within one file coalesce into the most recent one). Otherwise
    /// truncates any forward history past `current` (pushing from the
    /// middle of the stack discards the abandoned forward branch, same as
    /// a browser) and appends.
    pub fn push(&mut self, location: NavLocation);
    pub fn can_go_back(&self) -> bool;
    pub fn can_go_forward(&self) -> bool;
    pub fn go_back(&mut self) -> Option<NavLocation>;
    pub fn go_forward(&mut self) -> Option<NavLocation>;
}
```

Carries over the 7 tests from `crates/ui/src/nav_history.rs`'s
`#[cfg(test)] mod tests` that don't exercise `recent_locations()`
unchanged (the other 4 are dropped along with that method -- see below).
`recent_locations()` (the
most-recent-first iterator `ide-ui`'s Recent Locations popup reads) is
**not** ported — no consumer for it in `ide-tui` per §1's scope note;
`ide-tui`'s copy of the struct is otherwise identical so a future Recent
Locations port (if one is ever done) can add it back without touching
anything else.

### 2.2 `App` additions (`crates/tui/src/app.rs`)

```rust
struct App {
    // ...
    nav_history: NavHistory,
    // ...
}
```

Named `nav_history` rather than `ide-ui`'s own bare `nav` field name,
specifically to stay visually distinct from the pre-existing `nav_state`
(`tui-recent-files-and-bookmarks.md`'s persisted Recent-Files/Bookmarks
state) sitting right next to it in the struct — an unrelated field with a
confusingly similar name; the more distinct name costs nothing and removes
the at-a-glance ambiguity a matching `nav`/`nav_state` pair would invite.

```rust
impl App {
    /// Records **the tab that is active at call time** (path + `offset`)
    /// as the new current entry in `nav_history`. No-op with no active tab
    /// or an untitled one (no path to return to). Every call site opens
    /// (or focuses) its destination tab *first*, then calls this -- it
    /// always records the destination just jumped to, never the origin
    /// the jump started from (identical to `ide-ui`'s own
    /// `push_nav_location`, which reads `self.active_tab` after its
    /// caller's own `open_file` already switched it). `ide-ui`'s version
    /// reads a deferred `pending_cursor_offset` field for the offset
    /// instead of taking a parameter -- `ide-tui` has no such field (it's
    /// fully synchronous, unlike egui's frame-deferred consumption), so
    /// callers pass the offset directly -- see §2.3 on where that offset
    /// comes from at each call site.
    fn push_nav_location(&mut self, offset: usize);

    /// The active buffer's actual current caret offset, or `0` with no
    /// active tab (`docs/features/tui-back-forward-navigation.md` §2.3's
    /// fix-round finding). Reading this live, rather than assuming a
    /// jump's own nominal target offset, is what keeps `push_nav_location`
    /// accurate when `open_or_focus_tab` refocuses a tab that was already
    /// open -- that branch never touches the tab's live caret.
    fn active_caret_offset(&self) -> usize;

    /// `NavigateBack` command: opens the previous location's file (via
    /// `open_or_focus_tab`, switching tabs if already open) and places
    /// the caret at its offset via `scroll_to_and_reveal` + a direct
    /// `Selections::single(Selection::caret(offset))` set -- the same
    /// two-step `open_or_focus_tab` + caret-placement shape every other
    /// jump site in this file already uses. No-op at the oldest entry.
    /// Deliberately never calls `push_nav_location` itself -- every
    /// Back/Forward press would otherwise immediately push a new
    /// forward-erasing entry (§3's invariant).
    fn nav_back(&mut self);

    /// `NavigateForward` command: same shape as `nav_back`, opposite
    /// direction. No-op at the newest entry.
    fn nav_forward(&mut self);
}
```

`run_action` (`app.rs:4425`, the `Action` dispatch match every existing
command already routes through, e.g. `Action::ToggleBlockComment =>
self.run_line_op(...)` at line 4506) gets two new arms:

```rust
Action::NavigateBack => self.nav_back(),
Action::NavigateForward => self.nav_forward(),
```

### 2.3 Call sites: where `push_nav_location` is called

Enumerated by reading every function in `ide-tui`'s `app.rs` that changes
the active tab/caret as a result of a jump (not a plain typed edit), and
cross-checking each against `ide-ui`'s own 5 `push_nav_location` call
sites (`open_at`/`open_stack_frame`/`open_search_result`/the tree-click
handler in `crates/ui/src/app/render.rs:4249`/a Go-to-Line site not
applicable here per `tui-go-to-file-and-symbol.md`'s T16 entry, which cut
Go to Line for a binding conflict) for precedent:

| `ide-tui` function | Push? | Reasoning |
|---|---|---|
| `open_location` | **Yes** | Direct match for `ide-ui`'s `open_at`. `ide-tui` already consolidates what `ide-ui` keeps as two separate functions (`open_at` for LSP jumps, `open_stack_frame` for debugger jumps) into this one function (`open_stack_frame` here just calls `open_location` internally) -- adding the push here alone covers **both** cases, a TUI-specific simplification worth noting for anyone diffing against `ide-ui`'s two call sites. |
| `open_search_result` | **Yes** | Direct match for `ide-ui`'s `open_search_result`. |
| `handle_tree_enter` | **Yes** | Direct match for `ide-ui`'s tree-click handler (`render.rs:4249`, `open_file` then `push_nav_location`). Push after a successful `open_or_focus_tab`, not on the directory-toggle branch. |
| `confirm_go_to_file` | **Yes** | Same "pick a file from a list, open it" shape as the tree click; `ide-ui` has no separate Go to File precedent to mirror, but the underlying action (open a new file, land at a new location) is identical in kind. Push *after* the successful `open_or_focus_tab` **and** after its own explicit offset-0 caret reset (unlike every other "Yes" row, which pushes before any caret placement) -- pushing first would read whatever stale caret the buffer had before this jump, since the reset is what actually sets it to `0`. |
| `confirm_bookmark_jump` | **Yes** | `ide-ui` has no Bookmarks feature at all (`ide-tui`-only), so no direct precedent -- judgment call, reasoned from `ide-ui`'s own stated invariant (§2.1 above): Bookmarks is an independent list, not itself sourced from `nav_history` (unlike Recent Locations, which `ide-ui`'s `recent_locations_confirm` deliberately excludes for exactly that self-referential-growth reason -- `crates/ui/src/app.rs:2259-2273`). Jumping from an independent list to a new caret position is exactly the shape every other "yes" row already has. Push after the successful `open_or_focus_tab`, before the best-effort caret placement. |
| `confirm_new_scratch_file` | **Yes** | Creates and opens a brand-new file -- changes the active tab exactly like any other open. `ide-ui` has no scratch-files feature to mirror; reasoned the same way as `confirm_go_to_file`. Push after the successful `open_or_focus_tab`. |
| `confirm_scratch_file` | **Yes** | Opens an existing scratch file chosen from a list -- same "pick from a list, open it" shape as `confirm_go_to_file`. Push after the successful `open_or_focus_tab`. |
| `confirm_recent_file` | **No** | Direct match for `ide-ui`'s `recent_files_confirm` (`crates/ui/src/app.rs:2176-2183`), which deliberately does not call `push_nav_location` -- Recent Files' whole point is "go back to where you already were" (it doesn't move the caret at all), so pushing a nav entry here would be pushing a location the user didn't actually navigate *to* in the offset sense. Same reasoning `ide-tui`'s own doc comment on `confirm_recent_file` already gives for not touching the caret. |

Every "Yes" row passes the target offset explicitly (`push_nav_location`
takes it as a parameter, §2.2) rather than relying on a deferred field:
`open_location`/`open_search_result` pass the offset they're about to
place the caret at; `confirm_bookmark_jump` passes the resolved line-start
offset (or skips the push entirely if the bookmarked line no longer
resolves, mirroring its own existing permissive-`None` handling).

`confirm_go_to_file`/`confirm_new_scratch_file`/`confirm_scratch_file`/
`handle_tree_enter` instead pass `App::active_caret_offset()` — the active
buffer's actual current-caret offset, read *after* `open_or_focus_tab`
returns (and, for `confirm_go_to_file` specifically, after its own
explicit offset-0 caret reset). This is a deliberate deviation from a
literal port of `ide-ui`'s own `pending_cursor_offset` mechanism: `ide-ui`
records `0` unconditionally for these cases (via a `None` deferred field
that defaults to `0`), which under-reports the real position whenever
`open_or_focus_tab`/`open_file`'s already-open branch refocuses a tab
without touching its live caret. `ide-tui` fixes this rather than
reproducing it — reading the live caret costs one extra call and is
correct in every case (a freshly-opened tab's caret is `0` by construction,
so the two approaches agree there; they only diverge on the already-open
case, where the live read is simply right). `crates/tui/src/app.rs`'s
`reopening_an_already_open_tab_pushes_its_real_caret_not_zero` test is the
regression test for this.

### 2.4 Keybinding (`crates/tui/src/commands.rs`)

Two new `Action` variants and two new entries in the command list, in the
same `Command { id, title, binding: Some((modifiers, keycode)), action }`
shape every existing entry already uses (mirroring `ToggleBlockComment`'s
own entry, the direct precedent for the `CONTROL.union(ALT)` combo):

```rust
Command {
    id: "NavigateBack",
    title: "Navigate Back",
    // `⌘⌥←` translated -- ide-ui's own binding for the same action.
    binding: Some((
        KeyModifiers::CONTROL.union(KeyModifiers::ALT),
        KeyCode::Left,
    )),
    action: Action::NavigateBack,
},
Command {
    id: "NavigateForward",
    title: "Navigate Forward",
    // `⌘⌥→` translated.
    binding: Some((
        KeyModifiers::CONTROL.union(KeyModifiers::ALT),
        KeyCode::Right,
    )),
    action: Action::NavigateForward,
},
```

Verified collision-free: no existing binding in `commands.rs` occupies
`KeyCode::Left`/`KeyCode::Right` with any modifier combination, and
`CONTROL.union(ALT)` is already a proven-reachable combo in this terminal
environment (`ToggleBlockComment`, `Ctrl+Alt+/`). Verified
precedence-safe: `handle_key`'s dispatch (`app.rs`, ~line 4112) checks
`self.keymap.action_for(key.modifiers, key.code)` — the command
registry — **before** falling through to `handle_editor_key`'s raw
`KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL)` word-navigation
match arms; since `.contains(CONTROL)` would otherwise also match a
`Ctrl+Alt+Left` chord, this registry-first ordering is what prevents the
two from colliding (same precedent as T18b's word-navigation-vs-registry
finding).

## 3. Behaviour

- **Coalescing**: pushing a second location in the same file as the
  current entry replaces it in place rather than growing the stack —
  moving around within one file via several jump sites in a row doesn't
  bloat history with intermediate stops in that file.
- **Forward-branch truncation**: pushing a new location while not at the
  newest entry (i.e. after having gone Back at least once) discards
  every entry after the current position before appending — going Back
  then jumping somewhere new abandons the old forward branch, exactly
  like a browser tab.
- **Back/Forward never push**: `nav_back`/`nav_forward` reuse the same
  `open_or_focus_tab` + caret-placement mechanism every jump site uses,
  but never call `push_nav_location` themselves — otherwise every Back
  press would immediately push a new entry that erases the forward
  history it just uncovered.
- **No-op at the ends**: `nav_back` at the oldest entry and `nav_forward`
  at the newest are both no-ops (`can_go_back`/`can_go_forward` gate
  this) — no wraparound, no panic.
- **No-op with nothing to push**: `push_nav_location` with no active tab,
  or an active tab with no path (an unsaved scratch buffer that was never
  written to disk, if that state is ever reachable — currently
  `confirm_new_scratch_file` always writes the file before opening it, so
  this branch is defensive rather than reachable today), is a silent
  no-op.

## 4. Constraints & invariants

- `nav_back`/`nav_forward` must never call `push_nav_location` (§3).
- Every jump site in §2.3's "Yes" column must push *after* confirming the
  open succeeded (an `open_or_focus_tab` failure — e.g. a since-deleted
  file — must not pollute history with a location that was never
  actually reached) **and after** the active tab has already switched to
  the destination — `push_nav_location` always records whichever tab is
  active *at call time*, so calling it before `open_or_focus_tab` would
  record the origin under the destination's intended entry.
- A location's recorded `offset` is a snapshot from whenever *that*
  location was itself pushed — going Back to it lands at that recorded
  offset, not necessarily wherever the caret happened to be immediately
  before the *next* jump away from it (e.g. plain scrolling/arrow-key
  movement never updates a pushed entry). This is not a TUI-specific gap;
  `ide-ui`'s own `pending_cursor_offset` mechanism has the identical
  limitation.
- `nav: NavHistory` is in-memory only, not persisted across restarts —
  `ide-ui`'s own `NavHistory` isn't persisted either (`recent_files`/
  `nav_state`'s bookmarks are, via `project_settings`, but the back/
  forward stack itself is session-scoped in both frontends).

## 5. Example

```
1. User opens main.rs via a tree click (handle_tree_enter).
   -> push_nav_location(0) records NavLocation { path: "main.rs", offset: 0 }
      as entries[0], current = 0.
2. Cursor is in main.rs; user runs Go to Definition on a call to `foo`,
   which lives in lib.rs.
   -> open_location switches the active tab to lib.rs, places the caret
      at `foo`'s definition, then push_nav_location(<foo's offset>)
      records NavLocation { path: "lib.rs", offset: <foo's offset> } as
      entries[1], current = 1. main.rs's entry from step 1 is unchanged,
      one step back.
3. User presses Ctrl+Alt+Left (NavigateBack).
   -> nav_back() moves current to 0, reopens main.rs, places the caret at
      offset 0 -- main.rs's *recorded* offset from step 1, not whatever
      the live caret happened to be immediately before step 2's jump
      (§4's snapshot-offset invariant).
4. User presses Ctrl+Alt+Right (NavigateForward).
   -> nav_forward() moves current to 1, returns to lib.rs at `foo`'s
      definition.
```

## 6. Dependencies & integration points

- `crates/tui/src/app.rs`: new `nav_history` field, `push_nav_location`/
  `nav_back`/`nav_forward`/`active_caret_offset` methods, and the 7
  call-site edits in §2.3.
- `crates/tui/src/commands.rs`: two new `Action` variants and their
  bindings (§2.4).
- No `ide-core`/`ide-lsp`/`ide-dap` changes — this is pure `ide-tui`
  state built on already-existing `open_or_focus_tab`/
  `scroll_to_and_reveal` primitives.
- **Not security-sensitive**: no subprocess, no untrusted external input
  beyond the same already-validated file paths every other jump site in
  this file already handles (all of which go through `open_or_focus_tab`,
  which itself resolves via `Self::canonicalize_best_effort` — unchanged
  by this doc). Not on `CLAUDE.md`'s declared security-sensitive-paths
  list; `hacker` is skipped for this run per the `dev-chain` skill's own
  rule.

## Revision notes

Post-merge fix round, addressing two `rev` controversial findings the
user asked to act on rather than leave as accepted tradeoffs:

- Renamed the `nav` field to `nav_history` (§2.2) to remove the
  at-a-glance confusability with the pre-existing, unrelated `nav_state`
  field sitting next to it in `App`.
- Fixed the offset-0 imprecision at `confirm_go_to_file`/
  `confirm_new_scratch_file`/`confirm_scratch_file`/`handle_tree_enter`
  (§2.3) instead of accepting it as a faithfully-ported `ide-ui`
  limitation: these now push `App::active_caret_offset()` (a new method,
  §2.2) — the buffer's real live caret — rather than a hardcoded `0`,
  which was wrong whenever `open_or_focus_tab` refocused a tab that was
  already open. `confirm_go_to_file`'s push also moved to *after* its own
  explicit offset-0 caret reset, since reading the caret before that reset
  would read the stale pre-jump value instead.
