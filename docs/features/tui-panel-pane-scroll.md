# TUI position-based scroll for multi-pane panels

## 1. Purpose

Direct response to a user report: "git panel from top menu on tui is not
scrollable. recheck ALL panels docker k8s etc and fix it." Re-reading the
render layout code (not memory, not `tui-panel-focus-and-scroll.md`'s own
prior conclusion) found the Git screen's "wheel scroll already works"
claim from T51 was incomplete, and found a second, distinct gap in three
bottom-dock tabs while auditing "ALL panels" as instructed.

**Gap 1 -- Git screen, position-independent scroll.** `render_git_log_view`
renders up to three simultaneously-visible sub-panes (Conflicts + Graph
stacked in a 30%-wide left column, Diff in the 70%-wide right column); the
Changes view renders Staged and Unstaged as two stacked lists. But
`handle_git_panel_key`'s `Up`/`Down` handling -- and therefore wheel-scroll,
which T51 routed through the same `any_popup_open()` → synthesize-a-key →
`handle_key` path -- is keyed entirely off `state.focus`/`state.changes_
focus`, never off where the mouse actually is. Default focus is `Graph`,
so scrolling the wheel anywhere on the Git screen, including over the
visually dominant Diff pane, moved `graph_selected` instead of `diff_
scroll` -- no visible effect in the pane the user was looking at. The
bottom dock's Git Log tab (`BottomDockTab::GitLog`) reuses `render_git_
log_view` verbatim and has the identical problem via its own `handle_git_
log_dock_key`.

**Gap 2 -- Docker/Kubernetes/Custom Actions, no scroll state at all.**
These three bottom-dock tabs each render a list pane alongside an output/
logs pane, but unlike Cargo/Claude/AI (`tui-panel-history-scroll.md`, T52)
their output/logs panes have **no scroll field whatsoever** -- `DockerPanel.
logs`, `K8sPanel.logs`/`describe_output`, and `CustomActionsPanel.output`
were all rendered as a fixed unclamped tail window, mouse or keyboard. This
is the exact gap class T52 closed for Cargo/Claude/AI, missed for these
three at the time.

## 2. Interface

### 2.1 `crates/tui/src/ui.rs` -- new `HitMap` fields

```rust
pub struct HitMap {
    // ... existing fields unchanged ...

    /// The Git panel's commit graph list, populated by `render_git_left_
    /// column` whenever the Log view renders -- also reused verbatim by
    /// the bottom-dock Git Log tab's own graph pane, since both share
    /// that one render function and only one of the two is ever on
    /// screen in a given frame.
    pub git_graph_area: Option<Rect>,
    /// The Git panel's Conflicts list, populated only while `!app.git.
    /// conflicts.is_empty()`. Not reused by the dock's Git Log tab --
    /// `GitLogDockState` has no `conflicts_selected` field to scroll, so
    /// wheel-scroll over this area in that context is deliberately left
    /// unhandled.
    pub git_conflicts_area: Option<Rect>,
    /// The Git panel's Diff pane, populated only when a diff (not
    /// conflict resolution) is showing -- reused by the dock's Git Log
    /// tab like `git_graph_area`.
    pub git_diff_area: Option<Rect>,
    /// The Git panel's Changes-view Staged list.
    pub git_staged_area: Option<Rect>,
    /// The Git panel's Changes-view Unstaged list.
    pub git_unstaged_area: Option<Rect>,
    /// The bottom dock's "secondary" pane -- Docker/Kubernetes' logs
    /// column, Custom Actions' output row -- for whichever tab is
    /// active. One generic field, not one per tab, since only one
    /// bottom-dock tab is ever rendered at a time; dispatch reads
    /// `bottom_dock.as_ref().map(|d| d.tab)` to know which panel's
    /// scroll field to mutate.
    pub dock_secondary_area: Option<Rect>,
}
```

`render_git_panel`, `render_git_log_view`, `render_git_left_column`,
`render_git_changes`, `render_docker_panel`, `render_k8s_panel`, and
`render_custom_actions_panel` each gain a trailing `hits: &mut HitMap`
parameter (matching the shape `render_tree`/`render_left_dock`/`render_
bottom_dock` already have) and populate the fields above at the same
points they compute the corresponding `Rect`.

### 2.2 New scroll-state fields

Same tail-anchored `u16` shape T52 established for `CargoPanel::output_
scroll`:

```rust
// crates/tui/src/docker_panel.rs
pub(crate) struct DockerPanel {
    // ...
    pub(crate) logs_scroll: u16,
}

// crates/tui/src/k8s_panel.rs
pub(crate) struct K8sPanel {
    // ...
    /// Shared by `logs` and `describe_output` -- `render_k8s_panel` only
    /// ever shows one of the two at a time (mutually exclusive by `logs_
    /// for`/`describe_for`), so one field suffices.
    pub(crate) output_scroll: u16,
}

// crates/tui/src/custom_actions.rs
pub(crate) struct CustomActionsPanel {
    // ...
    pub(crate) output_scroll: u16,
}
```

`DockerPanel::fetch_logs`, `K8sPanel::fetch_logs`, `K8sPanel::fetch_
describe`, and `CustomActionsPanel::run` each gain one line resetting
their scroll field to `0`, right next to the existing `.clear()` on the
content they're about to replace -- the same "reset on an unrelated
content swap" reasoning `CargoPanel::run` already applies to `output_
scroll`.

`render_docker_panel`/`render_k8s_panel`/`render_custom_actions_panel`
replace their previous fixed-tail slicing with a call to the existing
`tail_window` helper (`tui-panel-history-scroll.md` §2.1), passing the new
scroll field.

### 2.3 `crates/tui/src/app.rs` -- dispatch

Three new private `App` methods:

```rust
fn scroll_git_log_pane(&mut self, pane: GitPanelFocus, direction: KeyCode);
fn scroll_git_changes_pane(&mut self, pane: ChangesFocus, direction: KeyCode);
fn scroll_dock_secondary_pane(&mut self, direction: KeyCode);
```

`scroll_git_log_pane`/`scroll_git_changes_pane` mutate exactly the field
`pane` names (`graph_selected`/`conflicts_selected`/`diff_scroll` or
`staged_selected`/`unstaged_selected`), with clamping identical to
`handle_git_log_key`/`handle_git_changes_key`'s existing `Up`/`Down` arms
-- never `state.focus`/`state.changes_focus` itself, matching `tui-mouse-
support.md` §3.3's "wheel scroll never changes focus" rule. `scroll_dock_
secondary_pane` reads `self.bottom_dock.as_ref().map(|d| d.tab)` and
mutates `self.docker.logs_scroll`/`self.k8s.output_scroll`/`self.custom_
actions.output_scroll` accordingly; a no-op for every other tab.

`handle_mouse_scroll` gains position-based branches, described in full in
§3.

## 3. Behaviour

### 3.1 The full Git screen

A new block runs **before** the existing `any_popup_open()` check (which
folds `active_screen == AppScreen::Git` in unconditionally and would
otherwise always win):

```
if active_screen == Git and neither branches_popup nor worktrees_popup is open:
    match state.view:
        Log:    git_diff_area hit?      -> scroll_git_log_pane(Diff)
                git_conflicts_area hit? -> scroll_git_log_pane(Conflicts)
                git_graph_area hit?     -> scroll_git_log_pane(Graph)
        Changes (and pending_discard is None):
                git_staged_area hit?    -> scroll_git_changes_pane(Staged)
                git_unstaged_area hit?  -> scroll_git_changes_pane(Unstaged)
    (any match returns immediately)
```

If a branches/worktrees popup is open, or `pending_discard` is set, or the
click position matches none of the above (e.g. the branch-name row), the
code falls through to the pre-existing `any_popup_open()` → synthetic-key
→ `handle_git_panel_key` path -- whose full priority chain (popups,
discard confirm, conflict resolution) already handles those states
correctly; this feature does not change any of that.

### 3.2 The bottom dock's Git Log / Docker / Kubernetes / Custom Actions tabs

Two new checks in `handle_mouse_scroll`, positioned before the pre-existing
generic `bottom_dock_body` check (they're more specific sub-regions of it):

- `git_diff_area`/`git_graph_area` hit (only reachable here when `active_
  screen != Git`, since that case already returned in §3.1) -> mutate
  `self.git_log_dock.diff_scroll`/`graph_selected` directly, clamped the
  same way `handle_git_log_dock_key` already does. `Conflicts` is not
  handled here -- `GitLogDockState` has no field for it.
- `dock_secondary_area` hit -> `scroll_dock_secondary_pane`.

Reusing `git_diff_area`/`git_graph_area` for both the full Git screen and
the dock tab is safe because only one of the two ever renders in a given
frame (`AppScreen::Git` replaces the dock/editor body entirely), and
`HitMap` is reset to `default()` at the start of every `render()` call, so
a field can never carry a stale value from the other context into the
current frame's dispatch.

### 3.3 Fallback, not full coverage

The Git screen's left column (Conflicts + Graph stacked) is dispatched at
column granularity, not sub-row granularity: `git_conflicts_area` and
`git_graph_area` are two separate rects, so scrolling over either moves
that specific list -- this *is* precise, not a "whichever has focus"
compromise. The one accepted gap is symmetrical with T51's own scope cut:
if the click position matches nothing (branch-name row, borders), the
existing focus-based fallback runs, exactly as it did before this feature.

## 4. Constraints & invariants

- No new field ever causes a panic: every new dispatch method clamps
  against the same length reads (`self.git.graph.len()`, `self.git.status.
  staged.len()`, etc.) the existing keyboard handlers already use.
- `HitMap::default()` at the top of every `render()` call means a pane
  that didn't render this frame (e.g. the Diff pane during conflict
  resolution) has its area `None`, and dispatch simply doesn't match it --
  no explicit "is this pane visible" check needed anywhere else.
- `logs_scroll`/`output_scroll` are `u16`, matching `diff_scroll`'s and
  `CargoPanel::output_scroll`'s existing type.
- Wheel-scroll over a pane never changes keyboard focus (`tui-mouse-
  support.md` §3.3) -- verified directly by `wheel_scroll_over_git_diff_
  pane_moves_diff_scroll_regardless_of_focus` and its Graph-focus
  counterpart.

## 5. Examples

Scrolling the mouse wheel over the Diff pane on the full Git screen while
focus defaults to Graph: `git_diff_area` matches, `scroll_git_log_pane
(Diff, ScrollDown)` runs, `diff_scroll` increments, `state.focus` stays
`Graph` -- before this feature, the same scroll moved `graph_selected`
instead, invisible in the pane under the cursor.

Scrolling over the Docker tab's Logs column: `dock_secondary_area` matches
(populated by `render_docker_panel`), `scroll_dock_secondary_pane` reads
`bottom_dock.tab == Docker` and increments `self.docker.logs_scroll`;
`render_docker_panel`'s next frame calls `tail_window(&panel.logs,
visible_rows, panel.logs_scroll)` and the window slides back one line.

## 6. Dependencies & integration points

Extends `tui-panel-focus-and-scroll.md` (T51, whose `bottom_dock_body`/
`any_popup_open()` routing this feature adds more-specific checks in front
of, without changing either) and reuses `tui-panel-history-scroll.md`'s
(T52) `tail_window` helper verbatim for Docker/Kubernetes/Custom Actions.
Entirely `crates/tui/src/{app.rs,ui.rs,docker_panel.rs,k8s_panel.rs,
custom_actions.rs}`; no `ide-core`/`ide-lsp`/`ide-dap` changes.
