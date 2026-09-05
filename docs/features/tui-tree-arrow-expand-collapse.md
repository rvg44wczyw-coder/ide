# TUI: Directory Tree Left/Right Arrow Expand/Collapse (T40)

## 1. Purpose

`ide-tui`'s Project tree (`crates/tui/src/tree.rs` + `handle_tree_key` in
`crates/tui/src/app.rs`) currently only expands/collapses a directory via
`Enter` (`handle_tree_enter`, toggling), with `Up`/`Down` moving the
selection. This leaves the tree's `Left`/`Right` arrow keys doing nothing
while the Project tree has focus — a gap against every reference file-tree
widget (the reference desktop-IDE's own Project view, VS Code's Explorer,
Windows/macOS file browsers), all of which use `Right` to expand-or-descend
and `Left` to collapse-or-ascend.

This is tree-widget-local navigation behavior, the same category `Up`/
`Down`/`Enter` already are in `handle_tree_key` — it is not a
keymap-registered, user-rebindable command (`commands.rs`/`Action`), so the
root `CLAUDE.md`'s "never invent a binding" keyboard-shortcuts section does
not apply here in the rebindable-command sense; it applies in spirit,
satisfied by matching the reference IDE's own Project-view arrow-key
convention exactly rather than inventing new semantics.

## 2. Interface / API

### 2.1 `ide-core`

None. This is pure `ide-tui` UI-navigation state.

### 2.2 `ide-lsp`

None.

### 2.3 `ide-tui`

**`crates/tui/src/tree.rs`** — two new methods on `TreeState`, alongside
the existing `toggle_expand_selected`:

```rust
/// `Right` arrow's tree-navigation primitive: if the selected row is a
/// collapsed directory, expands it (selection unchanged). If the selected
/// row is a directory that's already expanded, moves the selection to its
/// first child -- but only after confirming the immediately-following row
/// in `visible_rows` is actually a child (`rows[selected + 1].depth ==
/// row.depth + 1`), not a sibling. That check matters specifically for an
/// **empty, already-expanded** directory: `push_rows` only appends rows
/// for children that exist, so with none, the row at `selected + 1` (if
/// any) is the directory's own next sibling (same depth) or an ancestor's
/// next sibling (shallower depth) -- never a child. No-op on a file row or
/// an expanded directory whose next row fails that depth check (nothing
/// to descend into).
pub fn expand_or_descend_selected(&mut self, root: &DirEntry);

/// `Left` arrow's tree-navigation primitive: if the selected row is an
/// expanded directory, collapses it (selection unchanged). Otherwise
/// (a file row, or an already-collapsed directory) moves the selection to
/// its parent row -- the nearest earlier row in `visible_rows` whose
/// `depth` is exactly one less than the selected row's. No-op at depth 0
/// (a top-level row has no parent row to move to).
pub fn collapse_or_ascend_selected(&mut self, root: &DirEntry);
```

Both are no-ops on an empty tree (mirroring `move_selection`/
`toggle_expand_selected`'s existing convention) and never panic on an
out-of-range `selected` (same `rows.get(self.selected)`-based guard
`toggle_expand_selected` already uses).

**`crates/tui/src/app.rs`** — `handle_tree_key` gains two new arms,
alongside the existing `Up`/`Down`/`Enter`:

```rust
fn handle_tree_key(&mut self, key: KeyEvent) {
    match key.code {
        KeyCode::Up => self.tree_state.move_selection(&self.tree, -1),
        KeyCode::Down => self.tree_state.move_selection(&self.tree, 1),
        KeyCode::Left => self.tree_state.collapse_or_ascend_selected(&self.tree),
        KeyCode::Right => self.tree_state.expand_or_descend_selected(&self.tree),
        KeyCode::Enter => self.handle_tree_enter(),
        _ => {}
    }
}
```

No new `App` fields, no new `Action`/`Command` entries — this is
`handle_tree_key`'s own direct `KeyEvent` match, the same non-keymap,
non-rebindable shape `Up`/`Down`/`Enter` already have there (the tree
already never routes through `commands()`/`Action` for these three; `Left`/
`Right` join them, not the keymap registry).

## 3. Behaviour

### 3.1 `Right` — expand or descend

- Selected row is a **collapsed directory** → it becomes expanded;
  selection stays on the same row (same path). Its children become visible
  at `depth + 1` on the next `visible_rows()` call, exactly as `Enter`'s
  existing toggle-to-expand already does.
- Selected row is an **already-expanded directory with at least one
  child** → selection moves to that first child (`selected += 1`; the
  child is always the immediately-following row in a depth-first listing,
  so this is a plain index increment, not a fresh tree walk).
- Selected row is an **already-expanded directory with no children** (an
  empty directory) → no-op; there is nothing to descend into and nothing
  useful for "expand" to change (it's already expanded).
- Selected row is a **file** → no-op. `Right` never opens a file (that
  stays `Enter`'s job) and a file has no expand state.

### 3.2 `Left` — collapse or ascend

- Selected row is an **expanded directory** → it becomes collapsed;
  selection stays on the same row. Its descendants disappear from
  `visible_rows()`, exactly as `Enter`'s existing toggle-to-collapse
  already does.
- Selected row is a **collapsed directory** or a **file**, and it is not
  at `depth == 0` → selection moves to its parent row: the nearest row
  *before* it in `visible_rows()` whose `depth` is exactly
  `row.depth - 1`. Depth-first ordering guarantees such a row exists and
  is unique whenever `depth > 0`.
- Selected row is at **`depth == 0`** (a top-level entry, collapsed or a
  file) → no-op; there is no parent row in the tree to move to (the
  project root itself is never a row — `tree.rs`'s own `visible_rows` doc
  comment).

### 3.3 Interaction with existing tree state

Neither method touches anything outside `TreeState`'s own two fields
(`expanded`, `selected`) — no `App` state, no LSP requests, no file I/O.
`move_selection`/`toggle_expand_selected`/`select`/`selected_row`/
`visible_rows` are all unchanged; the two new methods are additive,
composed from the same `visible_rows()` + `expanded.insert`/`remove`
primitives those already use.

## 4. Constraints & invariants

- Both new methods take `&DirEntry` (the current root), matching every
  existing `TreeState` method's signature shape — no new state threading.
- `expand_or_descend_selected`'s "descend to first child" branch never
  re-scans the tree structurally; it relies on `visible_rows()`'s
  depth-first ordering guarantee (already relied on by
  `toggle_expand_selected`'s own tests, e.g.
  `expanding_a_directory_reveals_its_children_at_the_next_depth`) that
  *when a child exists*, it is always at `selected + 1` immediately after
  the parent's own row. That guarantee does not by itself prove a child
  exists — an empty, already-expanded directory has no row at all past
  itself that belongs to it. The implementation must therefore check
  `rows.get(selected + 1).is_some_and(|next| next.depth == row.depth + 1)`
  before moving the selection there; failing that check (row missing, or
  present but at `depth <= row.depth`, meaning it's a sibling/uncle, not a
  child) is the empty-directory no-op case.
- `collapse_or_ascend_selected`'s "ascend to parent" branch walks
  `visible_rows()` backward from `selected` looking for the first row with
  `depth == row.depth - 1` — this is `O(depth)` in the worst case (shallow
  trees in practice; no perf concern for a UI-scale directory tree).
- Both are total functions over any `selected` value (in range or not,
  since `rows.get(self.selected)` returns `None` and both methods return
  immediately in that case) and over an empty tree.

## 5. Examples

**Expanding a collapsed directory, then descending into it:**

```rust
let mut state = TreeState::new();
// tree: src/ (collapsed), Cargo.toml
state.expand_or_descend_selected(&root); // src/ becomes expanded, selection stays on src/
state.expand_or_descend_selected(&root); // src/ already expanded -> selection moves to src/main.rs
```

**Collapsing back up, then ascending to the parent:**

```rust
// selection is on src/main.rs, src/ is expanded
state.collapse_or_ascend_selected(&root); // main.rs isn't a dir -> ascends: selection moves to src/
state.collapse_or_ascend_selected(&root); // src/ is expanded -> collapses it, selection stays on src/
```

**No-ops:**

```rust
// selection on Cargo.toml (a file) at depth 0
state.collapse_or_ascend_selected(&root); // depth 0, no parent -> no-op
// selection on an empty, expanded directory
state.expand_or_descend_selected(&root); // already expanded, no children -> no-op
```

## 6. Dependencies

None beyond what `crates/tui/src/tree.rs` already imports
(`ide_core::{DirEntry, DirEntryKind}`, `std::collections::HashSet`).

## 7. Diagram

Skipped — the change is small and fully described by §3's behavior table;
a diagram wouldn't add clarity beyond it.

## Revision notes

- `rev` (doc review, round 1) found that §2.3/§4's description of the
  "descend to first child" branch didn't specify the guard needed to
  distinguish a real child row from an empty expanded directory's next
  *sibling* row at `selected + 1` — `visible_rows()`'s depth-first order
  only guarantees a child is at `selected + 1` when one exists; it says
  nothing about what's there when it doesn't. Fixed by adding the explicit
  `rows[selected + 1].depth == row.depth + 1` guard to both sections.
