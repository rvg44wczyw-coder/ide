# TUI Screen Navigation (T44)

## 1. Purpose

Introduces a persistent, always-visible **screen switcher** to `ide-tui`:
one full-screen view active at a time (`Editor`/`Git`/`Run`/`Keys`),
selected by a permanent top tab bar. This is the foundational piece of
`docs/roadmap.md` §11's reversal of §9 for `ide-tui` — adopting the
`Orbit TUI.dc.html` mockup's actual navigation model (full-screen
exclusive views) in place of the IntelliJ-New-UI-style tool-window
docking §9 mandated. `ide-ui` is unaffected; this crate's docking
primitives (`LeftDock`/`BottomDock`, most popups) are **not removed** —
only `Git` and `Run` are promoted out of their current popup/dock-tab
homes into top-level screens. See `docs/roadmap.md` §11 for the full
rationale and the T44–T48 series this opens.

**Non-goal, explicitly**: every other existing tool window (Docker,
Kubernetes, Problems, Claude, Debug, TODO, Search-in-Path, Scratch
Files, Clone panel, Worktrees, Blame, Keymap/Theme settings, GitLog dock,
Custom Actions) keeps working exactly as today, reachable via the
existing command palette (`Ctrl+Shift+A`) entry and keybinding. None of
them gain a tab-bar entry in this run.

## 2. Interface

### 2.1 `app.rs`

```rust
/// Which top-level screen is currently showing in the main body
/// (`docs/features/tui-screen-navigation.md` §2.1, T44). Independent of
/// `Focus` (which still governs Tab/arrow routing *within* the Editor
/// screen's Tree/Editor/BottomDock split) and independent of every popup
/// overlay (a popup can be open on top of any screen -- see §3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AppScreen {
    #[default]
    Editor,
    Git,
    Run,
    Keys,
}
```

New `App` field: `pub(crate) active_screen: AppScreen` (initialized to
`AppScreen::default()` in `App::new`, alongside the other `Default`-able
fields).

No new field replaces `git_panel: Option<GitPanelState>` or
`bottom_dock`/`cargo: CargoPanel` — both keep their current shape and
meaning (§3.2/§3.3 below specify exactly how `active_screen` interacts
with each, since neither is a drop-in "is this screen active" boolean).

### 2.2 `commands.rs`

Four new `Action` variants and `Command` entries, all **palette-only, no
default binding** — this concept (a top-level screen switcher) has no
JetBrains-macOS-keymap precedent to translate, and per `CLAUDE.md`'s
"never invent a binding" rule that means no default key, not an invented
one:

```rust
Command {
    id: "GoToEditorScreen",
    title: "Editor",
    binding: None,
    action: Action::GoToEditorScreen,
},
Command {
    id: "GoToGitScreen",
    title: "Git",
    binding: None,
    action: Action::GoToGitScreen,
},
Command {
    id: "GoToRunScreen",
    title: "Run",
    binding: None,
    action: Action::GoToRunScreen,
},
Command {
    id: "GoToKeysScreen",
    title: "Keys",
    binding: None,
    action: Action::GoToKeysScreen,
},
```

`ToggleGitPanel`'s and `ToggleCargoPanel`'s existing `Command` entries
are **repointed**, not removed: their `action` becomes
`Action::GoToGitScreen`/`Action::GoToRunScreen` respectively (title/id
unchanged, so existing palette muscle-memory and any doc referencing
them by id still resolves) — see §3.2/§3.3 for why "toggle" becomes "go
to" (screens don't toggle, they're mutually exclusive; the old
open/close pair collapses into one).

**Explicit rejection, stated in the doc since it's the obvious first
instinct and the wrong one**: do *not* bind bare digit keys (`1`/`2`/
`3`/`4`) to screen switching, even though the mockup's own JS does
exactly that. The mockup is a chrome-only static sample with no real
text-editing surface; `ide-tui`'s Editor screen is a real text buffer
where `1234` are ordinary characters a user types constantly. A bare
digit binding would silently eat every digit keystroke typed into the
buffer. The mockup's own keybinding list is not a source of truth here —
this is exactly the class of literal-macro-mockup detail earlier T-runs
(T41/T42) already learned not to copy verbatim.

### 2.3 `ui.rs`

New `fn render_screen_tabs(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap)`
— a permanent one-row strip (`Constraint::Length(1)`) rendered as the
*first* row of `render()`'s outer vertical `Layout`, above today's
`body`/`status_area` split (`ui.rs`'s existing `render()`, currently
`Layout::default().constraints([Constraint::Min(1), Constraint::Length(1)])`
for body/status — gains a third row on top:
`[Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]` for
tabs/body/status). Four labels ("Editor"/"Git"/"Run"/"Keys"), the active
one highlighted via `theme.tokens()`'s existing focus/accent color
(mirror whatever `render_tab_strip`, `ui.rs`'s existing *editor-buffer*
tab strip, already uses for its own active-tab highlight — reuse the
same token, don't invent a new one). Each label registers a click region
in `hits` the same way `render_tab_strip`'s own per-tab regions do
(`HitMap`'s existing registration call — mirror it exactly), mapped to
the corresponding `Action::GoToXScreen`.

`render()`'s body dispatch changes from unconditionally rendering the
Editor layout to matching on `app.active_screen`:

- `Editor` — exactly today's body (`render_left_dock`/`render_editor`/
  `render_bottom_dock` unchanged, verbatim).
- `Git` — `render_git_panel(frame, app, body)` **full-bleed** (`body`,
  not `size` — today's call passes `size` because it's a full-screen
  popup already; same function, different-shaped argument since the tab
  bar now permanently occupies one row above it).
- `Run` — a new small wrapper, `render_run_screen(frame, app, body)`,
  that renders `render_cargo_panel`'s existing row/output content into
  the full `body` rect instead of the bottom-dock's partial-height
  rect. Do not duplicate `render_cargo_panel`'s drawing logic — either
  call it directly with `body` in place of its usual dock-sized `area`
  argument (check its current signature takes a plain `Rect`; if so this
  is a one-line change of what's passed, not a new function), or extract
  a shared inner helper if its current implementation assumes dock-tab
  chrome (tab-strip-relative borders) that doesn't fit full-screen
  framing.
- `Keys` — new `render_keys_screen(frame, app, body)`: reuses
  `render_keymap_popup`'s existing binding-table content (same rows,
  same columns) as a **read-only reference list** in this run — no edit
  UI, no per-pane-edge bind slots yet (T47).

All of today's popup-overlay `if app.<popup>.is_some() { render_x(...) }`
calls (palette, code actions, rename, blame, go-to-file, etc.) stay
**exactly as they are**, rendered on top of whichever screen's body just
got drawn, at the very end of `render()`, unchanged — see §3.6.

## 3. Behaviour

### 3.1 Switching screens

Three ways to change `active_screen`, all producing identical end state:

1. Click a tab bar label (mouse).
2. Run `GoToEditorScreen`/`GoToGitScreen`/`GoToRunScreen`/
   `GoToKeysScreen` from the command palette.
3. `Esc`, when no popup is open (`!self.any_popup_open()`) and
   `active_screen != Editor`, returns to `Editor` (§3.5) — mirrors the
   mockup's own `Escape -> go("editor")`, and is safe unlike the digit
   keys (§2.2) because `Esc` is never a character a user types into the
   buffer.

Switching is idempotent and has no "already there, so close" toggle
behavior (unlike the old `toggle_git_panel`) — screens are mutually
exclusive positions, not overlays you dismiss. Switching *away* from a
screen never discards that screen's own state (§3.2/§3.3) — only popups
(§3.6) have discard-on-close semantics.

### 3.2 Git screen state

`git_panel: Option<GitPanelState>` keeps its existing shape and every
existing field/behavior (`GitPanelView`, `GitPanelFocus`, branches/
worktrees/conflicts popups nested inside it, etc.) untouched.
`GoToGitScreen`'s handler:

```rust
fn go_to_git_screen(&mut self) {
    self.git_panel.get_or_insert_with(GitPanelState::default);
    self.active_screen = AppScreen::Git;
}
```

`git_panel` is **never** set back to `None` by a screen switch — only by
an explicit future "reset" affordance if one is ever added (none exists
today; today's only way `git_panel` became `None` was closing the old
popup, which no longer happens). This is a deliberate behavior change
from popup semantics (state reset on every open) to screen semantics
(state persists while you're elsewhere) — switching to Run to check a
build and back to Git leaves you on the same diff/commit-message draft
you had. State this explicitly in review: it is not a bug that
`GitPanelState::default()` no longer runs on every visit, it is the
point.

`handle_git_panel_key`'s existing `Esc` branch currently ends with
`self.git_panel = None;` (its "close the whole overlay" fallback, after
the Filter/log-history special cases). That line changes to
`self.active_screen = AppScreen::Editor;` — same "leave the Git surface"
intent, adapted to the new model: leaves `git_panel`'s state intact,
just navigates back to Editor. Every other branch of that function
(Tab/`g`/`s`/`b` sub-navigation, log/changes/conflicts handling) is
untouched.

`handle_key`'s existing dispatch already checks `if self.git_panel.is_some() { return self.handle_git_panel_key(key); }`
near the top of its popup-priority chain (verify the exact current
position against source — it sits alongside the other `Option`-gated
popup checks). Since `git_panel` is now `Some` for the entire time
`active_screen == Git` (not just while a popup is open), this existing
check continues to route every key on the Git screen to
`handle_git_panel_key` with **no change to that check itself** — it was
already exactly the right shape, because "is `git_panel` populated" was
always the real gate, "is a popup open" was just what that used to mean.

### 3.3 Run screen state

`self.cargo: CargoPanel` is already an always-alive struct (per T33's
"always-alive struct + derived dock visibility" convention, same as
Docker/Kubernetes) — nothing new to construct. `GoToRunScreen`'s handler
is just `self.active_screen = AppScreen::Run;`.

**Dispatch rank — verified against actual source, not assumed**: unlike
every popup check above it, `BottomDockTab`/Cargo key handling today is
*not* an early, unconditional intercept — it's reached only via
`handle_key`'s final `match self.focus { ... Focus::BottomDock =>
self.handle_bottom_dock_key(key) }` fallback, which itself only runs
*after* the existing global-keybinding lookup
(`if let Some(action) = self.keymap.action_for(key.modifiers, key.code) { return self.run_action(action); }`)
has already failed to match. That means today, while the Cargo dock tab
is focused, every global binding (`Ctrl+Shift+A`/OpenPalette included)
still fires normally — only genuinely unbound keys fall through to
Cargo's own `b`/`r`/`t`/`c`/`l`/`f` handling. The new `active_screen ==
Run` check must sit at that **same rank** — immediately after the
`keymap.action_for` lookup, not before it — or every global binding
would break on the Run screen, a real regression from today's dock-tab
behavior:

```rust
if let Some(action) = self.keymap.action_for(key.modifiers, key.code) {
    return self.run_action(action);
}
if self.active_screen == AppScreen::Run {
    return self.handle_cargo_panel_key(key);
}
if self.active_screen == AppScreen::Keys {
    return LoopSignal::Continue;
}
match self.focus { ... }
```

(The `Keys` branch above is specified fully in §3.4; shown here together
since both sit at the same rank, right before the existing `Focus`
match, and both must exist for that match to now only ever actually
apply to the Editor screen in practice.)

Existing bottom-dock behavior is otherwise **fully preserved**:
`BottomDockTab::Cargo` still exists, still reachable inside the bottom
dock the normal way if a user opens it while on the Editor screen (e.g.
to see build output alongside the code, side by side, without leaving
the Editor screen) — the Run *screen* is an additional, full-screen way
to reach the same always-alive `self.cargo`, not a replacement for the
dock tab.

`handle_cargo_panel_key` has an unconditional `_ => {}` catch-all and
never itself recognizes `Esc` (verified:
`handle_cargo_panel_key_esc_does_not_close_the_panel_or_stop_a_running_command`)
— it does not need to. §3.5's generic Esc rule is placed *before* this
check specifically so it intercepts `Esc` first; `handle_cargo_panel_key`
itself never sees an `Esc` keypress while `active_screen == Run` and
never needs an arm for it. State this explicitly rather than leaving an
implementer to wonder why the Cargo handler has no Esc case: ownership of
`Esc` differs per screen (Git owns it internally, §3.2; Run and Keys have
it intercepted upstream by §3.5's rule before their own handling ever
runs).

### 3.4 Keys screen

Read-only in this run: renders the same binding table
`render_keymap_popup` already builds (grouped by category, current
binding shown, rebind-in-place editing **stays exclusive to the actual
Keymap Settings popup** — this screen does not duplicate or replace
that editing UI, it's a second, always-reachable read surface over the
same data). `GoToKeysScreen`'s handler is just
`self.active_screen = AppScreen::Keys;` — no state to construct.

**This screen needs an explicit dispatch guard, and its absence would be
a real bug, not just an omission.** Switching screens never touches
`Focus` (§4) — `Focus` stays whatever it last was, almost always
`Focus::Editor`. Without a guard, an unmatched keystroke on the Keys
screen falls all the way to `handle_key`'s final `match self.focus {
Focus::Editor => self.handle_editor_key(key), ... }` and silently edits
whatever file was open *before* the user switched to Keys — no visible
cursor, no feedback, just a hidden buffer mutation while the user thinks
they're reading a read-only reference screen. The guard (already shown
in §3.3's snippet, same dispatch rank as the Run-screen check, i.e.
after the global keymap lookup so ordinary bindings like `Ctrl+S` still
work normally while this screen is showing):

```rust
if self.active_screen == AppScreen::Keys {
    return LoopSignal::Continue;
}
```

`Esc` never reaches this guard — §3.5's rule, placed earlier in the
chain, already routes it back to `Editor` first.

### 3.5 The generic Esc-returns-to-Editor rule

**Must be placed *before* the existing global-keybinding lookup
(`keymap.action_for`), not after it — this is the one placement detail
in this whole doc most likely to be gotten wrong by copying the pattern
of "goes low in the chain" from elsewhere.** `Esc` is *already* a global
binding today (`commands.rs`, `CollapseSelections`, bound to plain
`Esc` with no modifier). If this rule sits after `keymap.action_for`,
`CollapseSelections` intercepts every Esc press first and this rule
never fires at all on the Run or Keys screens. Placed *before* that
lookup (but after every existing modal-popup check, so an open popup's
own Esc handling — closing the palette, canceling a rename preview —
still wins first, completely unchanged, exactly as today):

```rust
if key.code == KeyCode::Esc && self.active_screen != AppScreen::Editor && !self.any_popup_open() {
    self.active_screen = AppScreen::Editor;
    return LoopSignal::Continue;
}
```

Verified this doesn't lose real functionality: `CollapseSelections`
collapses multi-cursor selections in the editor buffer, which is
meaningless to run while that buffer isn't even the visible screen, so
stealing `Esc` away from it specifically while `active_screen !=
Editor` is safe.

This rule is placed *after* the existing `git_panel` check in the
dispatch chain (that check is itself very early, well before this rule
and before the global keymap lookup too), so it never actually fires for
the Git screen at all — `handle_git_panel_key` already returns
unconditionally on `KeyCode::Esc` per §3.2's fix before control ever
reaches this rule. It also never fires while `any_popup_open()` is true
for an unrelated reason (e.g. a Docker confirm dialog happens to be open
on top of the Run screen) — that guard defers to whichever popup's own
check already ran earlier in the chain. In practice this rule is what
actually moves `active_screen` back to `Editor` for the Run and Keys
screens (§3.3/§3.4); for Git it's dead code in the sense that
`git_panel`'s own handling always wins first, which is fine and expected.

### 3.6 Popups on top of each screen

`any_popup_open()` and `close_all_overlays()` are **unchanged** — every
existing `Option`/`bool` popup field they already check/reset keeps
being checked/reset exactly as today, regardless of `active_screen`.
`render()`'s popup-overlay block at the end (palette, code actions,
rename, blame, go-to-file, keymap settings, theme settings, etc.) is
unchanged and keeps rendering on top of whatever screen's body was just
drawn.

**What's actually true, verified against the corrected dispatch order
above — narrower than "identically on every screen"**: on the **Editor**
and **Keys** screens, every popup opens exactly as it does today (the
global keymap lookup, and therefore `Action::OpenPalette` and everything
else, is reached normally on both). On the **Run** screen, the same
holds once §3.3's corrected rank (global keymap lookup *before* the
`active_screen == Run` check) is implemented — this is a real
requirement, not a bonus, since getting that rank backwards would
silently break every global binding on this one screen only. On the
**Git** screen, the *existing, pre-existing* limitation carries forward
unchanged: `git_panel.is_some()` causes `handle_git_panel_key` to
intercept every key before the global lookup ever runs, exactly as it
already does today for the modal Git Panel popup — a user must return to
`Editor` (or another screen) before opening most other popups. This is
not a regression T44 introduces; T44 only extends how long that
already-true constraint applies (from "while the popup happens to be
open" to "the whole time you're viewing the Git screen"). Do not write a
test claiming the palette opens on top of the Git screen — it doesn't,
today or after this change, and a doc claiming otherwise (an earlier
draft of this one did) would just describe a test that fails. Do write
`palette_opens_on_top_of_the_run_screen` and
`palette_opens_on_top_of_the_keys_screen` — those are the two
interactions this run genuinely must get right and could plausibly
regress if the dispatch rank in §3.3/§3.4 is implemented in the wrong
order.

## 4. Constraints & invariants

- `active_screen` is orthogonal to `Focus` (`Editor`/`LeftDock`/
  `BottomDock`) — `Focus` still governs Tab-cycling and resize
  (`GrowFocusedDock`/`ShrinkFocusedDock`, unchanged, still Editor-screen
  -only concepts) *within* the Editor screen's own three-way split.
  Switching to Git/Run/Keys does not need to touch or reset `Focus` at
  all — it's simply not consulted while those screens are active. Do
  not add an `AppScreen`-keyed `Focus` variant; keep the two concepts
  separate, since Git/Run/Keys have no internal dock split to focus
  between (yet — that's T47's per-edge model).
- `git_panel`/`self.cargo` are never `None`/reset by navigation alone,
  only by their own internal logic (currently: never, for either) — see
  §3.2/§3.3's explicit statement that this is intended, not a leak.
- The new tab bar's one row is **unconditional** screen real estate
  (`Constraint::Length(1)`, always present) — do not make it
  conditionally hidden; a screen switcher that disappears defeats the
  point, and a fixed row avoids the same `EDITOR_CHROME_ROWS`
  recompute-desync class of bug T36's breadcrumbs row fix (§4 of that
  doc) already had to fix once for exactly this "conditional chrome row"
  mistake. **Confirm this while implementing**: `lib.rs`'s main loop
  pre-computes `set_editor_viewport_rows` based on a fixed chrome-row
  count before `Layout::split` runs each frame — grep for
  `EDITOR_CHROME_ROWS` (or whatever constant/expression currently plays
  that role) and increment it by exactly one for the new permanent tab
  row, the same fix T36 made for its breadcrumbs row.
- No new `ide-core`/`ide-lsp` API. Entirely `crates/tui/src/{app.rs,
  commands.rs,ui.rs}`.
- Not security-sensitive by `CLAUDE.md`'s declared list: touches
  `app.rs`/`ui.rs`/`commands.rs`'s navigation/rendering only, no new
  subprocess, no new file I/O, no new untrusted-input parsing.
  `git_panel.rs` itself is on the declared list (its own write paths),
  but this doc changes none of `git_panel.rs`'s write logic — only when/
  how its already-hardened rendering and key handling are invoked.
  `hacker` should be skipped; re-confirm against the actual declared
  list text at review time rather than trusting this restated summary.

## 5. Examples

```text
Launch ide-tui on a Rust project.
  -> Tab bar shows: [Editor] Git  Run  Keys      (Editor highlighted)
  -> Body: today's Tree + Editor + (BottomDock if any), unchanged.

Click "Git" in the tab bar (or run GoToGitScreen from the palette).
  -> Tab bar: Editor [Git] Run  Keys
  -> Body: full-screen Git Panel (Log/Changes view, same as opening the
     old ToggleGitPanel popup used to show) -- borders now flush against
     the tab bar above, no popup margin.
  -> Press `g`/`s`/`b` etc.: identical to today's Git Panel behavior.

Press Esc.
  -> Tab bar: [Editor] Git  Run  Keys
  -> Body: back to Tree + Editor + BottomDock.
  -> `git_panel`'s Log/Changes selection and any in-progress commit
     message draft are exactly as left -- switching back to Git shows
     the same state, not a fresh popup.

Run GoToRunScreen from the palette while a Cargo build is streaming in
the bottom dock (opened separately, on the Editor screen).
  -> Tab bar: Editor  [Run] Keys
  -> Body: the same running/streamed `self.cargo` output, now full-
     screen -- switching screens never interrupts or restarts it.
  -> Press Ctrl+Shift+A: the command palette opens normally, on top of
     the Run screen -- global bindings are unaffected by being on this
     screen (see 3.3/3.6).

While on the Git screen, press Ctrl+Shift+A.
  -> Nothing opens -- exactly like today's modal Git Panel popup, every
     key is claimed by Git's own handler first. Press Esc (handled
     inside `handle_git_panel_key` itself, back to Editor), then
     Ctrl+Shift+A -- the palette opens normally.
```

## 6. Dependencies & integration points

- `ui.rs`'s existing `render()`, `render_git_panel`, `render_cargo_panel`,
  `render_keymap_popup`, `HitMap`'s click-region registration (mirrored
  by the new tab bar), `render_tab_strip` (its active-tab highlight
  token, mirrored not duplicated).
- `app.rs`'s existing `git_panel`/`self.cargo`/`any_popup_open`/
  `close_all_overlays`/`handle_git_panel_key`/`handle_cargo_panel_key`/
  `handle_bottom_dock_key`, all reused, only `handle_git_panel_key`'s
  final Esc line actually changes.
- `commands.rs`'s `Command`/`Action` registry and the
  `no_two_bound_commands_share_the_same_chord` test (all four new
  commands are `binding: None`, so this test is unaffected but must
  still pass).
- `lib.rs`'s main loop constant governing pre-split chrome-row count
  (§4's `EDITOR_CHROME_ROWS`-equivalent bullet).

## Revision notes

Round 1 `rev` found the dispatch-order design in §3.3/§3.4/§3.5 as
originally drafted was wrong in three load-bearing ways, verified
directly against `app.rs` source rather than assumed:

1. `Esc` is already a global binding (`CollapseSelections`, plain `Esc`,
   `commands.rs`). The original §3.5 rule was specified as sitting after
   the global keymap lookup, which would make it unreachable on the Run
   and Keys screens (`CollapseSelections` would always win first). Fixed:
   the rule now sits before that lookup.
2. §3.3 originally claimed the new Run-screen check belonged "at the
   same rank the bottom_dock check already occupies" — but there is no
   such early-rank check; `BottomDockTab`/Cargo handling is reached only
   through the final `Focus::BottomDock` fallback, *after* the global
   keymap lookup. As originally specified, the Run-screen check would
   have sat *before* that lookup, breaking every global binding
   (including the palette) on the Run screen. Fixed: the check now sits
   after the lookup, matching the existing dock-tab precedent exactly.
3. §3.4 originally specified no dispatch guard at all for the Keys
   screen, which — since switching screens never touches `Focus` — would
   have let unmatched keystrokes fall through to `handle_editor_key` and
   silently edit whichever file was open before the user switched there.
   Fixed: added an explicit consume-and-no-op guard at the same rank as
   the Run-screen check.

§3.6 was also corrected: an earlier draft claimed popups open
"identically on every screen," including an example of the command
palette opening on top of the Git screen — untrue both before and after
this doc, since `handle_git_panel_key` already intercepts every key
before the global lookup runs (the same way it already does for today's
modal popup). The corrected §3.6 states this as a carried-forward,
pre-existing limitation specific to Git, not a regression, and points
implementation at the two interactions (Run/Keys + palette) that
genuinely need a passing test.
