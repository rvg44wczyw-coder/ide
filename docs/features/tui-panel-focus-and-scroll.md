# TUI dock-panel click-to-focus + wheel-scroll, and the Git-screen click bug

## 1. Purpose

Follow-up to `tui-mouse-support.md` (T33 revision note 3) and T50, per a
direct user request: "all tui panel must be clickable for focus and
scrollable" / "same for popups etc". Investigation (re-reading
`handle_mouse_click`/`handle_mouse_scroll`/`any_popup_open`/
`render_left_dock`/`render_bottom_dock` against current source, not
memory) found three real, independent gaps:

1. **A click bug, not a scroll gap.** `any_popup_open()` folds in
   `self.active_screen == AppScreen::Git` (so wheel-scroll over the Git
   screen already gets routed correctly through the popup-priority
   synthetic-key path). But `handle_mouse_click`'s very first line is
   `if self.any_popup_open() { return; }`, unconditional — so **every
   mouse click is silently dropped while the Git screen is active**,
   including a click on the screen-tab bar itself. There is currently no
   way to leave the Git screen with the mouse once you're on it (`Esc` or
   a keybinding still work). This is an unintended interaction between
   T44's tab-bar click support and the Git-screen popup-priority trick,
   not a deliberate design choice — nothing in `tui-screen-navigation.md`
   or `tui-mouse-support.md` says screen-tab clicks should stop working
   once you're already on the target screen they'd otherwise switch away
   from.
2. **Dock body content has no click or wheel-scroll support at all.**
   Confirmed by reading `handle_mouse_click`'s full body: only
   `hits.left_dock_tabs`/`hits.bottom_dock_tabs` (the one-row tab strips)
   are clickable — nothing routes clicks into the content below them
   (Files tree is the one exception, since `hits.tree_area` predates dock
   tabs). Confirmed by reading `handle_mouse_scroll`: only `tree_area` and
   `editor_text_area` are position-tested outside the popup/Keys-screen
   branches — wheel-scrolling over Todos/Actions (left dock) or
   Docker/AI/Kubernetes/Cargo/Custom Actions/Problems/Git Log (bottom
   dock) is currently a silent no-op.
3. **Most of those dock tabs already have working keyboard Up/Down
   selection logic that a wheel event has no way to reach.** Re-verified
   directly against source, not assumed: `handle_docker_panel_key`,
   `handle_k8s_panel_key`, `handle_custom_actions_panel_key`,
   `handle_problems_key`, `handle_todo_panel_key`,
   `handle_tree_actions_tab_key`, and `handle_git_log_dock_key` all have
   real `KeyCode::Up`/`KeyCode::Down` arms already (several with existing
   regression tests, e.g. `handle_docker_panel_key_up_down_clamp_the_
   selection`). The fix for these seven tabs is a **routing** addition
   only — reuse the exact same "synthesize an arrow key, feed it to the
   existing per-tab handler" mechanism `tui-mouse-support.md` §3.3 already
   established for popups and the tree, not new scroll state.

### Confirmed non-goals (verified in code, not carried over from stale doc text)

- **Cargo panel output and the Claude chat panel remain non-scrollable.**
  Re-checked directly: `handle_cargo_panel_key` has no `KeyCode::Up`/
  `Down` arm at all (only single-letter subcommand triggers), and
  `handle_claude_chat_key` has no `Up`/`Down` arm either (its `_ => {}`
  catches them). Both would need a genuinely new scroll-offset field plus
  a rendering change to respect it — `tui-mouse-support.md`'s original
  exclusion list called both out for the same reason, and that is still
  accurate today. Out of scope here; a future feature, not a routing fix.
- **Notifications panel, Hover popup, Rename preview** — re-checked:
  still exactly `_open: bool`/`Option` flags with `Esc`-only key handling,
  no list or scroll state to hook a wheel event into. Unchanged from
  `tui-mouse-support.md` §4's own exclusion list.
- **No click-to-select-a-specific-row inside dock body content.** The
  literal request is "clickable for focus and scrollable" — this doc
  gives every dock body click-to-focus (sets `Focus::LeftDock`/
  `Focus::BottomDock`, mirroring what a tab-strip click already does) and
  wheel-to-arrow-key routing, but does not add per-row hit regions inside
  Docker/K8s/Custom Actions/Problems/Todos/Git Log content — that would
  mean seven separate new row-geometry implementations, one per panel's
  own render function, which is a materially bigger feature than what was
  asked. Users still select a row with the keyboard (`Up`/`Down`, which
  wheel-scroll now also drives) after clicking to focus the dock.
- **No click support inside the Git screen's own body** (branches,
  commits, diff, staging, conflicts). Fixing the click-blocks-everything
  bug (item 1) only restores the screen-tab bar's click; it does not add
  click regions for the Git panel's internal views, which is a
  substantially larger feature (five distinct sub-views, per
  `tui-screen-navigation.md`) than the narrow bug this doc fixes.
- **`AppScreen::Run`'s own wheel-scroll routing gap, found during the same
  audit, is not worth fixing on its own.** `handle_cargo_panel_key` (the
  handler the Keys-screen pattern would route into) has no `Up`/`Down`
  arm at all (see above) — adding the routing without also adding Cargo
  output scroll state would be a no-op change with no observable
  behaviour difference. Left alone; revisit only alongside the Cargo/
  Claude scroll-state feature above, if that's ever built.

## 2. Interface

### 2.1 `crates/tui/src/ui.rs` — `HitMap`

Two new fields, same shape as the existing `tree_area`/`editor_text_area`:

```rust
pub struct HitMap {
    // ...existing fields unchanged...
    /// The left dock's active-tab body area (`rows[1]` in
    /// `render_left_dock`, below its one-row tab strip) -- populated
    /// whenever the left dock renders at all, regardless of which tab is
    /// active (`docs/features/tui-panel-focus-and-scroll.md` §2.1).
    pub left_dock_body: Option<Rect>,
    /// Mirrors `left_dock_body` for the bottom dock (`rows[1]` in
    /// `render_bottom_dock`).
    pub bottom_dock_body: Option<Rect>,
}
```

Populated in `render_left_dock`/`render_bottom_dock`, right after each
function computes its own `rows` split (both already compute exactly this
`Rect` today to pass to their per-tab render call — this only stores the
same value into `hits` as well, no new geometry).

### 2.2 `crates/tui/src/app.rs` — `any_popup_open` split

```rust
/// Every condition `any_popup_open` checks EXCEPT `active_screen ==
/// AppScreen::Git` -- see `any_popup_open`'s own doc comment for why the
/// two are kept separate now (§3.1 of this doc).
fn any_true_popup_open(&self) -> bool {
    // ...exact same body `any_popup_open` has today, minus the
    // `|| self.active_screen == AppScreen::Git` line...
}

fn any_popup_open(&self) -> bool {
    self.any_true_popup_open() || self.active_screen == AppScreen::Git
}
```

Every existing caller of `any_popup_open()` (`handle_key`'s Esc/`:`
guards, `handle_mouse_scroll`) is unchanged and keeps exactly its current
behaviour, since `any_popup_open()`'s own return value is unchanged bit
-for-bit. Only `handle_mouse_click` switches to calling
`any_true_popup_open()` instead (§3.1).

### 2.3 `crates/tui/src/app.rs` — `handle_mouse_click`

Gate changed from `any_popup_open()` to `any_true_popup_open()`. Two new
branches added after the existing `left_dock_tabs`/`bottom_dock_tabs`
loops, before `tree_area`:

```rust
if let Some(area) = hits.left_dock_body {
    if area.contains(point.into()) {
        self.focus = Focus::LeftDock;
        return;
    }
}
if let Some(area) = hits.bottom_dock_body {
    if area.contains(point.into()) {
        self.focus = Focus::BottomDock;
        return;
    }
}
```

And, immediately after the existing `screen_tabs` loop (so a screen-tab
click is checked before this executes; ordering matters, see §3.1): a
short-circuit for the Git screen, since none of the branches below it
apply there (§1's "no click support inside the Git screen's own body"
non-goal):

```rust
if self.active_screen == AppScreen::Git {
    return;
}
```

### 2.4 `crates/tui/src/app.rs` — `handle_mouse_scroll`

Two new position-based branches, after the existing `tree_area` check and
before the existing `editor_text_area` check:

```rust
if hits.left_dock_body.is_some_and(|r| r.contains(point.into())) {
    self.handle_left_dock_key(synthetic);
    return;
}
if hits.bottom_dock_body.is_some_and(|r| r.contains(point.into())) {
    self.handle_bottom_dock_key(synthetic);
    return;
}
```

No change to the popup-open branch, the `AppScreen::Keys` branch, or the
`tree_area`/`editor_text_area` branches.

## 3. Behaviour

### 3.1 Why the Git-screen click fix needs `any_true_popup_open`, not a one-line special case

The naive fix — move the `screen_tabs` loop above the `any_popup_open()`
gate entirely — would also let screen-tab clicks through while a genuine
modal popup is open elsewhere (e.g. the command palette), which an
existing test explicitly asserts must stay blocked
(`handle_mouse_click_on_a_screen_tab_is_ignored_while_a_popup_is_open`,
`app.open_palette()` scenario). The three-way requirement is:

| State | Screen-tab click | Everything else |
|---|---|---|
| Real popup open (palette, etc.), any screen | blocked | blocked |
| `AppScreen::Git`, nothing else open | **allowed** (the fix) | blocked (§1 non-goal) |
| `AppScreen::Git` **and** a real popup open | blocked | blocked |

`any_true_popup_open()` (everything `any_popup_open()` already checks,
minus the Git-screen line) is exactly the "real popup open" predicate row
1/3 need. `handle_mouse_click` therefore: gates on
`any_true_popup_open()` first (covers rows 1 and 3 uniformly, screen or
not), then checks `screen_tabs` (now reachable on the Git screen when no
real popup is open — row 2), then short-circuits the rest of the function
for `AppScreen::Git` (nothing past the tab bar is clickable there yet,
per §1).

`any_popup_open()` itself is kept as a thin wrapper so every other
existing caller (`handle_key`'s Esc-closes-non-Editor-screen guard, the
`:`-opens-colon-command guard, `handle_mouse_scroll`'s popup branch) sees
byte-for-byte the same boolean it always has — none of them are changed
by this doc.

### 3.2 Dock-body click-to-focus

Clicking anywhere in `left_dock_body`/`bottom_dock_body` (i.e. anywhere in
the active tab's rendered content, below its tab strip) sets `self.focus`
to `Focus::LeftDock`/`Focus::BottomDock` and returns — the same effect a
tab-strip click already has, extended to the much larger area below it.
This does not change `tree_area`'s own existing behaviour (select row +
open + focus): `tree_area` is checked later in the function and is a
strict subset of `left_dock_body` when the Files tab is active, but since
`left_dock_body` is checked first and both regions overlap for Files, the
dock-body branch would win and only set focus, silently dropping the
existing select-and-open behaviour.

To avoid that regression, `tree_area` is checked *before*
`left_dock_body` in `handle_mouse_click` (see §2.3's exact insertion
point relative to the existing `tree_area` check) — so a click on a tree
row still selects and opens it (unchanged), and `left_dock_body` only
fires for the two tabs with no existing click behaviour of their own
(Todos, Actions) or as the fallback for empty space within the Files tab
below the tree's own content.

### 3.3 Dock-body wheel-scroll

One wheel notch = one synthetic `Up`/`Down` key fed directly into
`handle_left_dock_key`/`handle_bottom_dock_key` — the same functions
`Tab`/`Focus`-based keyboard navigation already calls, bypassing
`self.focus` entirely (matching `tree_area`'s existing wheel-scroll
branch, which likewise calls `handle_tree_key` directly regardless of
current focus). Both functions already fully delegate `Up`/`Down` to
whichever tab is currently active (`dock.tab`) with no per-tab wheel code
needed:

- Left dock: Files → `handle_tree_key` (already wheel-scrollable via the
  pre-existing `tree_area` branch, so `left_dock_body`'s routing for this
  tab is only reached for the rare corner case in §3.2). Todos →
  `handle_todo_panel_key` (existing `Up`/`Down` arms). Actions →
  `handle_tree_actions_tab_key` (existing `Up`/`Down` arms).
- Bottom dock: Docker → `handle_docker_panel_key`. Kubernetes →
  `handle_k8s_panel_key`. Custom Actions → `handle_custom_actions_panel_
  key`. Problems → `handle_problems_key`. Git Log →
  `handle_git_log_dock_key`. All five already have working `Up`/`Down`
  clamp logic, several with existing regression tests (§1, item 3).
- Bottom dock: AI → `handle_ai_panel_key` → (no match) `handle_claude_
  chat_key`'s catch-all does nothing for `Up`/`Down` (confirmed non-goal,
  §1). Cargo → `handle_cargo_panel_key`, same non-goal.

  For these two tabs, wheel-scroll over the dock body is now routed
  correctly but has nothing to do once it arrives — a silent no-op,
  identical in observable behaviour to before this doc, not a regression.

### 3.4 Interaction with the existing popup-priority wheel-scroll branch

Unchanged. `handle_mouse_scroll` still checks `any_popup_open()` first —
while the Git screen is active, `any_popup_open()` is still `true` (the
wrapper is unchanged, §2.2), so wheel-scroll there still routes through
the synthetic-key-to-`handle_key` path exactly as it does today (into
`handle_git_panel_key`), never reaching the new dock-body branches. The
new branches are position-based fallbacks for the *non-popup* base view
only, same as the existing `tree_area`/`editor_text_area` branches.

## 4. Constraints & invariants

- `left_dock_body`/`bottom_dock_body` are `None` whenever their dock
  isn't rendered that frame (mirrors every other `HitMap` field's
  existing "reflects only what was actually drawn" contract, `tui-mouse-
  support.md` §4).
- No new per-panel scroll or selection state is introduced anywhere —
  every dock tab this doc makes scrollable already owned its `Up`/`Down`
  logic before this change; this doc only adds a way for a wheel event to
  reach it.
- `any_popup_open()`'s return value is bit-for-bit unchanged for every
  existing caller — only `handle_mouse_click` is repointed to the new,
  narrower `any_true_popup_open()`.
- The Git-screen click fix restores exactly the screen-tab bar's click
  and nothing else — clicking anywhere else on the Git screen remains a
  no-op, unchanged from today.

## 5. Examples

Clicking into the Todos tab's body while the Files tab was previously
focused: `left_dock_body.contains(point)` is true (Todos tab is active,
same `rows[1]` region regardless of tab) → `self.focus =
Focus::LeftDock`. A subsequent wheel-down over the same area synthesizes
`KeyCode::Down` → `handle_left_dock_key` → `handle_todo_panel_key` moves
the Todos selection down by one, clamped.

Being on the Git screen and clicking the "Editor" label in the screen-tab
bar: `any_true_popup_open()` is `false` (no real popup, and the Git
-screen line is excluded from this predicate) → the `screen_tabs` loop
runs → matches → `go_to_editor_screen()` → returns. Previously this click
was dropped at the function's first line.

Clicking inside the Git screen's diff view (not the tab bar): `any_true_
popup_open()` is `false` → `screen_tabs` loop doesn't match (click wasn't
on the tab bar) → the new `active_screen == AppScreen::Git` short-circuit
returns before reaching any dock/tree/editor branch. Unchanged from
today: still a no-op.

## 6. Dependencies & integration points

Extends `tui-mouse-support.md` (T33 revision note 3) and depends on
`tui-tool-window-docking.md` (T33, `render_left_dock`/`render_bottom_dock`
and `handle_left_dock_key`/`handle_bottom_dock_key`) and
`tui-screen-navigation.md` (T44, `screen_tabs`/`AppScreen::Git`). No
`ide-core`/`ide-lsp`/`ide-dap` changes; entirely `crates/tui/src/{app.rs,
ui.rs}`.
