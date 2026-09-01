# TUI Code Folding (T19)

## 1. Purpose

Ports `docs/features/code-folding.md` (roadmap phase A6) to `ide-tui`:
Collapse/Expand/Collapse All/Expand All, plus the visual-row mapping that
lets scrolling, rendering, and caret motion treat a collapsed range's
interior as if it weren't there. **Zero new `ide-core` API** —
`TextBuffer::fold_ranges` (`FoldKind`/`FoldRange`) is already merged and
already used by `ide-ui`'s own A6 implementation. This is a
`crates/tui/**`-only diff.

Nothing from `ide-lsp` is involved, matching `code-folding.md` §1's own
scoping (LSP's `textDocument/foldingRange` stays future work).

### Scope cuts specific to porting

- **No gutter, so no fold arrows and no click handling.** `ide-ui`'s
  gutter reserves a marker lane for a clickable triangle; `ide-tui`
  renders one `ratatui::text::Line` per row with no gutter/line-number
  column at all (confirmed by reading `render_editor` — `styled_line`'s
  output is the whole visible line, nothing left of it). This port is
  keyboard-only: `CollapseFold`/`ExpandFold`/`CollapseAllFolds`/
  `ExpandAllFolds`, no gutter arrow, no placeholder click. The collapsed
  row's `" ⋯"` marker (§3.4) is still rendered — it's just not clickable.
- **No mouse at all**, so none of `ide-ui`'s click-math
  (`handle_mouse`/`offset_at`) has an equivalent to update — confirmed by
  grepping this crate for any mouse-event handling (none exists).
- **Only two caret-motion granularities exist: character (`Left`/
  `Right`) and single-line (`Up`/`Down`).** `ide-tui` has no
  `Granularity::Word`/`Document`/`Page` for the editor caret (confirmed by
  reading `editor.rs`'s `Direction`/`move_cursor` and every `KeyCode`
  arm `handle_editor_key` matches for the four arrow keys) — so none of
  `code-folding.md` §2.6's `Document`-granularity fix
  (`⌘↑`/`⌘↓`/`⌘⇧Home`/`⌘⇧End`) applies here; there is no such binding to
  fix. `PageUp`/`PageDown` exist only for the Git panel's diff view
  (`handle_git_panel_key`), an unrelated widget this phase doesn't touch.
- **`Up`/`Down` are ported as genuine row-stepping, not a post-step
  correction.** `ide-ui`'s `vertical_step` already worked in rows for
  `Line`/`Page` granularity before this phase, so `code-folding.md` §2.6
  only had to swap what feeds it. `ide-tui`'s `move_cursor` steps by raw
  buffer line (`line ± 1`) with no row concept at all — so this port
  changes the *call site* (`handle_editor_key`'s direction-handling
  block) to compute the target *row* first (`VisualLines::row_of`/
  `buffer_line`) and only then convert to an offset, rather than
  stepping the raw line and correcting afterward. Row-stepping can never
  land on a hidden line by construction, so no correction step is needed
  for `Up`/`Down` at all — simpler than `code-folding.md`'s own approach,
  because this crate never had `Down`'s raw-line-stepping to begin with.
- **`Left`/`Right` keep `code-folding.md`'s post-step-correction shape.**
  A character step can validly cross a line boundary the same way it
  always could, so (like the source doc) this needs a check-and-redirect
  after `move_cursor` returns, not a rewrite of `move_cursor` itself
  (§3.6).
- **Three genuine jump sites, not `ide-ui`'s two.** `open_location`
  (Goto Declaration/Find Usages/notification-panel opens),
  `open_search_result` (Find in Path), and `jump_to_match` (this crate's
  own Find/Replace, `tui-find.md`) — all three share the exact
  `let (line, _) = cursor_line_column(...); buf.scroll = line...;`
  pattern (confirmed by reading all three directly), all three need
  `reveal_line` before computing `scroll` (§3.5). `ide-ui`'s equivalent
  list is `open_at`/`open_search_result`/`nav_back`/`nav_forward` — this
  crate has no back/forward navigation history (not yet ported, `T17`),
  so no fourth site.
- **Undo/redo are not made fold-aware, matching `code-folding.md`'s own
  scope.** `ide-core`'s `Buffer::undo`/`redo` restore whatever
  `Selections` were recorded at that point, which could in principle now
  sit inside a range collapsed *after* that point. `code-folding.md`
  doesn't list undo/redo among its jump sites either (§7's list is
  `open_at`/`open_search_result`/`nav_back`/`nav_forward` only) — this is
  a documented limitation inherited from the source phase, not a new gap
  introduced by porting it, so it isn't fixed here.
- **`apply_workspace_edit`/rename-preview-apply are not made fold-aware,
  for the same reason.** Their resulting caret position is `TextBuffer`'s
  own edit-position-mapping outcome (like `insert_char`/line ops, §3.3),
  not an externally-chosen jump target — `code-folding.md` doesn't route
  its own equivalent (`apply_workspace_edit` in `ide-ui`) through
  `reveal_line` either.
- **No `Buffer`-vs-`TextBuffer` split concern.** `code-folding.md`'s
  `reveal_caret_after_collapse` takes `&mut Buffer` specifically because
  `ide-ui`'s `Buffer::set_selections` path is dirty-flag-aware there.
  Confirmed by reading `crates/core/src/buffer.rs`: `set_selections` lives
  on `TextBuffer` directly and never touches the dirty flag (moving the
  caret is not an edit) — every trigger method already in this crate
  (`trigger_extend_selection`/`trigger_shrink_selection`,
  `tui-line-commands-and-editorconfig.md` §3.3) calls
  `buf.buffer.text_buffer_mut().set_selections(...)` directly. This
  port's caret-reveal-after-collapse logic therefore needs only
  `&mut OpenBuffer`, no `Buffer`-specific plumbing.

## 2. Interface

### 2.1 `crates/tui/src/folding.rs` (new)

```rust
use std::collections::BTreeSet;
use ide_core::FoldRange;

/// Maps buffer lines to visual rows, hiding every line inside a collapsed
/// fold range except the range's own `start_line` — the same mapping
/// `code-folding.md` §2.2 describes, minus the click-facing pieces this
/// crate has no gutter/mouse to drive.
pub struct VisualLines {
    rows: Vec<usize>, // buffer line index for each visual row, ascending
}

impl VisualLines {
    /// `folded` is the set of currently-collapsed `start_line`s. When two
    /// or more of `ranges` share a `start_line` and more than one is
    /// collapsed, the one with the largest `end_line` (outermost)
    /// determines how much is hidden -- same tie-break `code-folding.md`
    /// §2.2 uses.
    pub fn build(line_count: usize, ranges: &[FoldRange], folded: &BTreeSet<usize>) -> Self;

    pub fn row_count(&self) -> usize;

    /// The buffer line for visual `row`, clamped to the last row.
    pub fn buffer_line(&self, row: usize) -> usize;

    /// The visual row for `buffer_line` -- its own row if visible,
    /// otherwise the row of the collapsed fold that hides it. `O(log R)`
    /// via `partition_point` over the sorted row list.
    pub fn row_of(&self, buffer_line: usize) -> usize;
}

/// The innermost uncollapsed range in `ranges` containing `caret_line`,
/// collapsed into `folded` (`CollapseFold`, §3.2). No-op if none does.
pub fn collapse_at_caret(folded: &mut BTreeSet<usize>, ranges: &[FoldRange], caret_line: usize);

/// Uncollapses the range whose `start_line` is `caret_line`, if one is
/// currently collapsed there (`ExpandFold`). No-op otherwise.
pub fn expand_at_caret(folded: &mut BTreeSet<usize>, caret_line: usize);

pub fn collapse_all(folded: &mut BTreeSet<usize>, ranges: &[FoldRange]);
pub fn expand_all(folded: &mut BTreeSet<usize>);

/// Uncollapses every currently-collapsed range whose
/// `start_line..=end_line` contains `line` -- used before a jump so it
/// always reveals its target instead of landing on some unrelated fold's
/// `start_line` (§3.5).
pub fn reveal_line(folded: &mut BTreeSet<usize>, ranges: &[FoldRange], line: usize);
```

No `App`/`ratatui` dependency — a state-free sibling of `editor.rs`,
operating on plain `BTreeSet<usize>`/`&[FoldRange]` the same way
`editor.rs`'s own functions operate on plain `&TextBuffer`.

### 2.2 `crates/tui/src/app.rs`

`OpenBuffer` gains one field:

```rust
pub(crate) struct OpenBuffer {
    // ... existing fields ...
    /// `start_line` of every currently-collapsed fold, this tab's
    /// session-only view state -- reset (empty) whenever a tab is
    /// (re)opened, the same category `shrink_stack`/`auto_closed` are
    /// already in (`tui-line-commands-and-editorconfig.md` §2.1,
    /// `tui-smart-editing.md` §2.1).
    pub(crate) folded: std::collections::BTreeSet<usize>,
}
```

`run_action` gains four arms (§2.3's `Action` variants), each a one-line
call into a new trigger method (§3.2). `handle_editor_key`'s `Left`/
`Right`/`Up`/`Down` handling is rewritten to route through `VisualLines`
(§3.3/§3.6). `sync_editor_scroll`, `open_location`, `open_search_result`,
and `jump_to_match` are updated to compute a row instead of a raw buffer
line (§3.5).

### 2.3 `crates/tui/src/commands.rs`

Four new `Action`/`Command` entries, bindings straight from
`docs/roadmap.md`'s Code Folding row (`⌘−`/`⌘+`/`⌘⇧−`/`⌘⇧+`),
`Ctrl`-translated the same way every prior binding in this file is —
nothing invented:

| `Action` | Binding | Note |
|---|---|---|
| `CollapseFold` | `Ctrl+-` | `⌘−` translated |
| `ExpandFold` | `Ctrl++` | `⌘+` translated -- the shifted symbol itself, no separate `SHIFT` bit (same convention `ToggleLineComment`'s `Ctrl+/` already established for a symbol key) |
| `CollapseAllFolds` | `Ctrl+Shift+-` | `⌘⇧−` -- explicit `SHIFT` bit alongside the literal char, same shape `JoinLines`/`ToggleCase` already use |
| `ExpandAllFolds` | `Ctrl+Shift++` | `⌘⇧+` |

All four are enabled unconditionally (a no-op when there's nothing to
collapse/expand costs the same `fold_ranges()` call gating it would, per
`code-folding.md` §2.4's own reasoning) — no new `is_command_enabled`-style
gating exists in this crate to begin with.

## 3. Behaviour

### 3.1 `VisualLines::build`, `row_of`, `buffer_line`

Identical algorithm to `code-folding.md` §3.4's first paragraph: walk
buffer lines `0..line_count`; a line is skipped exactly when it falls
inside a currently-collapsed range's `start_line+1..=end_line` (using the
outermost range sharing a `start_line` when more than one is collapsed
there); every other line gets the next row in order. Line `0` can never
be hidden (the smallest possible hidden line is `start_line + 1 ≥ 1`), so
`VisualLines` always has at least one row, matching this crate's own
"scroll math assumes at least one visible line" convention already
established by `scroll_to_keep_visible`.

`row_of(buffer_line)` is a `partition_point` over the ascending `rows`
list for the last row whose buffer line is `≤ buffer_line` — for a
visible line this is that line's own row; for a hidden line this
necessarily lands on the enclosing fold's `start_line` row, since hidden
ranges are contiguous immediately after a `start_line` row with no
visible rows in between.

### 3.2 Collapse / Expand / Collapse All / Expand All

`CollapseFold` (`Ctrl+-`): reads the active buffer's caret line, calls
`folding::collapse_at_caret` (innermost uncollapsed containing range,
same tie-break as `code-folding.md` §3.5), then reveals the caret if that
collapse just hid it (§3.4). `ExpandFold` (`Ctrl++`): calls
`folding::expand_at_caret` — thanks to the same invariant §3.4 states, a
caret sitting on any collapsed fold is necessarily on that fold's own
`start_line` (the interior is unreachable), so no containment search is
needed the way `collapse_at_caret` needs one. `CollapseAllFolds`/
`ExpandAllFolds` (`Ctrl+Shift+-`/`Ctrl+Shift++`) call
`folding::collapse_all`/`expand_all` directly, followed (collapse only)
by the same caret-reveal check.

None of the four mark the buffer dirty (folding is view state, not an
edit) and none go through `run_line_op`
(`tui-line-commands-and-editorconfig.md` §3.4) — they have their own
small trigger methods, since `run_line_op`'s dirty-marking/shrink-stack-
clearing contract doesn't apply to a fold toggle.

### 3.3 The caret-hidden-by-its-own-collapse fix

```rust
fn reveal_caret_if_hidden(buf: &mut OpenBuffer) {
    // ranges = buf.buffer.text_buffer().fold_ranges();
    // visual = VisualLines::build(line_count, &ranges, &buf.folded);
    // (line, column) = cursor_line_column(...) of the current caret;
    // visible_line = visual.buffer_line(visual.row_of(line));
    // no-op if visible_line == line (still visible);
    // otherwise: new caret = offset_for_line_column(text_buffer, visible_line, column)
}
```

Called after every `CollapseFold`/`CollapseAllFolds` (never after
`ExpandFold`/`ExpandAllFolds` — expanding can only ever reveal more text,
never hide the caret's line). For a range collapsed around the caret,
`visual.row_of(line)` resolves to that range's own `start_line` row, so
`buffer_line` of it is exactly `start_line` — matching `code-folding.md`
§3.4's "moves the caret onto the nearest still-visible line at or before
it... exactly that range's `start_line`" behaviour, reusing this crate's
existing `editor::offset_for_line_column` (already `pub`, already used by
`move_cursor`) rather than inventing a second column-clamping helper.

### 3.4 Rendering: rows instead of raw lines, and the collapsed marker

`render_editor` builds one `VisualLines` per frame (fresh, no cache —
same convention `bracket_pair`/semantic tokens already follow, per
`tui-smart-editing.md` §2.4/`tui-semantic-highlighting.md` §3.3) from
`buf.buffer.text_buffer().fold_ranges()` and `buf.folded`, and iterates
*rows* instead of raw buffer lines:

```rust
let total_rows = visual.row_count();
let visible_start = (buf.scroll as usize).min(total_rows);
let visible_end = (visible_start + text_area.height as usize).min(total_rows);
let lines: Vec<Line> = (visible_start..visible_end)
    .map(|row| {
        let line = visual.buffer_line(row);
        let mut styled = styled_line(text_buffer, line, &overlays);
        if buf.folded.contains(&line) && fold_ranges.iter().any(|r| r.start_line == line) {
            styled.push_span(Span::styled(" ⋯", Style::default().fg(Color::DarkGray)));
        }
        styled
    })
    .collect();
```

`highlight.rs` is unmodified — the marker is appended in `ui.rs` after
`styled_line` returns, not inside it, keeping folding a concern `ui.rs`
alone knows about (`styled_line`'s signature and `LineOverlays` gain
nothing new). `Color::DarkGray` matches this crate's existing muted-text
convention (`chip_spans_at`'s inlay-hint labels already use exactly this
style, `highlight.rs` — reused, not a new token). The cursor-position
calculation at the end of `render_editor` (`frame.set_cursor_position`)
switches from `line.checked_sub(buf.scroll as usize)` to
`visual.row_of(line).checked_sub(buf.scroll as usize)` — the caret's own
line is always visible by the invariant (§3.6), so `row_of` here always
returns that line's own true row, never a substituted one.

A collapsed row's height contribution is exactly one row regardless of
how much it hides — the hidden lines simply never appear in `visual.rows`
in the first place, so no separate "don't count hidden lines toward
height" logic is needed anywhere.

### 3.5 The three jump sites: reveal before scrolling

`open_location`, `open_search_result`, and `jump_to_match` each replace
their `buf.scroll = line.min(u16::MAX as usize) as u16;` line with a call
to a new shared helper:

```rust
fn scroll_to_and_reveal(buf: &mut OpenBuffer, line: usize) {
    let text_buffer = buf.buffer.text_buffer();
    let ranges = text_buffer.fold_ranges();
    folding::reveal_line(&mut buf.folded, &ranges, line);
    let line_count = text_buffer.lines().line_count();
    let visual = VisualLines::build(line_count, &ranges, &buf.folded);
    buf.scroll = visual.row_of(line).min(u16::MAX as usize) as u16;
}
```

`reveal_line` runs first so `line` is guaranteed visible by the time
`row_of` computes its row — after the reveal, `row_of(line)` always
returns `line`'s own true row (never a substituted enclosing-fold row),
matching `code-folding.md`'s "the jump unfolds whatever was hiding its
target rather than silently landing somewhere else" behaviour (§3.4).
Each call site's existing "unconditional top-align" comment
(`tui-find.md` §2.2/§4.3) is otherwise unchanged — this only changes
*what* row gets top-aligned, not whether the scroll is conditional.

### 3.6 Caret motion: rows for vertical, post-step correction for horizontal

`handle_editor_key`'s direction-handling block builds one `VisualLines`
(same per-frame-fresh convention as §3.4) before dispatching on
`direction`:

- **`Up`/`Down`** step the *row*, not the line: `row_of(current_line) ± 1`,
  clamped to `0..row_count` (a clamp failure — `Up` at row `0` or `Down`
  at the last row — is a no-op, exactly `move_cursor`'s own existing
  edge behaviour, just re-expressed in row space), then
  `buffer_line(target_row)` for the new caret line, then
  `offset_for_line_column` at the carried `desired_column` — the same
  column-carrying contract `move_cursor`'s own `Up`/`Down` branch already
  has, just fed a row-derived line instead of `line ± 1`. Because
  `buffer_line` only ever returns rows that exist, this can never land on
  a hidden line by construction — no post-step check needed, unlike
  `Left`/`Right` below.
- **`Left`/`Right`** call `move_cursor` unchanged first (a raw
  character-boundary step, exactly as before this phase), then check
  whether the resulting offset's line is hidden
  (`visual.buffer_line(visual.row_of(raw_line)) != raw_line`). If so,
  redirect: `Right` (forward) lands at the start of the row right after
  the fold — `offset_for_line_column(text_buffer,
  visual.buffer_line(visual.row_of(raw_line) + 1), 0)`; `Left` (backward)
  lands at the end of the fold's own `start_line` text —
  `offset_for_line_column(text_buffer, visual.buffer_line(visual.row_of(raw_line)), usize::MAX)`
  (`offset_for_line_column` already clamps an out-of-range column to the
  line's end, per its own doc comment — reused rather than a second
  line-end lookup). Both directions clear `desired_column`, matching
  `move_cursor`'s own existing `Left`/`Right` contract.

**`Backspace` and `Delete`'s plain-character branches need the same
correction, not just the pure caret-motion block.** Found during
self-review, not in the original design: `delete_backward`'s no-adjacent-
bracket-pair fallback and `KeyCode::Delete`'s handler each call
`move_cursor` directly with `Direction::Left`/`Right` respectively (their
own call sites, separate from the direction-handling block above) to find
what to delete. Left as raw calls, `Delete` at the end of a collapsed
fold's `start_line` would silently delete the hidden newline (and,
character by character, work its way through the entire hidden interior)
with no visible change on screen; `Backspace` at the start of the row
right after a fold has the mirror-image risk stepping `Left`. Both are
fixed the same way: they call `move_caret_with_folds` instead of
`move_cursor` directly, so a redirect across a fold boundary happens here
too. One direct consequence, not originally intended but a reasonable
reading of "the interior is opaque": `Delete`/`Backspace` right at a fold
boundary now delete the *entire* hidden interior as a single unit (the
same target `Right`/`Left`'s own redirect already computes), rather than
one hidden character at a time — matching the common "a folded region
behaves as one line" editor convention this doc already invokes for
vertical motion (§3.4), extended here to forward/backward delete as well.

One documented edge case: if a collapsed fold's `end_line` is the
buffer's own last line and the caret sits at the end of that fold's
`start_line`, pressing `Right` computes a redirect row of
`row_of(raw_line) + 1`, which is past the last row —
`VisualLines::buffer_line` clamps this to the last row, which (since
nothing after the fold exists) is the fold's own `start_line` again. The
net effect is the caret lands at column `0` of the line it started on
rather than visibly moving — never wrong (the invariant still holds, and
nothing panics), just a minor cosmetic no-progress outcome in a rare
corner case, the same "documented limitation, not a bug" category
`code-folding.md` §4 already uses for its own constraint 9.

### 3.7 `sync_editor_scroll`

Recomputes `VisualLines` and clamps against the caret's *row* instead of
its raw line:

```rust
fn sync_editor_scroll(buf: &mut OpenBuffer, viewport_rows: u16) {
    let text_buffer = buf.buffer.text_buffer();
    let offset = text_buffer.selections().primary().start();
    let (line, _) = cursor_line_column(text_buffer, offset);
    let ranges = text_buffer.fold_ranges();
    let line_count = text_buffer.lines().line_count();
    let visual = VisualLines::build(line_count, &ranges, &buf.folded);
    buf.scroll = scroll_to_keep_visible(buf.scroll, visual.row_of(line), viewport_rows);
}
```

No `reveal_line` call here — every call site (every edit, `run_line_op`,
`trigger_extend_selection`/`shrink_selection`, the new `Up`/`Down`/`Left`/
`Right` handling in §3.6) only ever leaves the caret on a line that was
already visible before the operation ran (an edit only touches the
caret's own current line; row-stepping can't produce a hidden line by
construction; the horizontal post-step correction in §3.6 already
guarantees a visible result) — so `line` here is always already visible,
and `row_of` simply returns its own true row.

## 4. Constraints

1. `folding.rs` has no `App`/`ratatui` dependency — pure functions over
   `BTreeSet<usize>`/`&[FoldRange]`, the same boundary `editor.rs`
   already holds relative to `app.rs` (§2.1).
2. `VisualLines` is **not cached** — rebuilt fresh every frame in
   `render_editor` and on every keystroke that needs it, cheap enough at
   `O(line_count)` (§3.1/§3.4/§3.6/§3.7), matching this crate's existing
   "recompute per-frame, don't invalidate a cache" convention for
   `bracket_pair`/semantic tokens/document highlights.
3. `OpenBuffer::folded` is reset to empty whenever a tab is (re)opened —
   `open_or_focus_tab` initializes it fresh, the same lifecycle
   `shrink_stack`/`auto_closed` already have.
4. The caret can never be placed on a buffer line hidden by a collapsed
   fold — held by: row-based `Up`/`Down` (§3.6, by construction), a
   post-step redirect for `Left`/`Right` (§3.6), `reveal_line` before the
   three jump sites (§3.5), and `reveal_caret_if_hidden` after
   `CollapseFold`/`CollapseAllFolds` (§3.3). Undo/redo and
   `apply_workspace_edit` are explicitly *not* covered, matching
   `code-folding.md`'s own scope (§1's "Scope cuts" section).
5. A stale `folded` entry (a `start_line` no longer matching any range
   `fold_ranges()` currently reports, because an edit shifted what used
   to be there) is inert: `VisualLines::build` only honors entries that
   still match a real range, so the line just renders normally again — no
   fold-anchor tracking through edits, matching `code-folding.md`
   constraint 5.
6. No new `Cargo.toml` dependency — the collapsed-row marker reuses
   `ratatui::text::Span`/`Style` and this crate's existing `Color::DarkGray`
   convention.
7. Selection Expand/Shrink (`Alt+Up`/`Alt+Down`,
   `tui-line-commands-and-editorconfig.md` §3.3) is not made fold-aware,
   matching `code-folding.md` constraint 9's identical scope cut.

## 5. Examples

**Collapsing a Rust function body:** caret inside `fn foo() { ... }`.
`Ctrl+-` finds the innermost uncollapsed range containing the caret's
line (the function's own `{}` pair) and collapses it. The signature line
renders `fn foo() { ⋯` and every row between `{` and `}` disappears from
`VisualLines` — the widget's content shrinks by exactly that many rows.

**Arrowing past a collapsed fold:** caret at the very end of `fn foo() {
⋯`'s visible text. `Right` computes the raw next-character boundary
(crossing into the hidden interior); the post-step check redirects to the
start of the row right after the fold's `end_line`.

**Find Usages into a folded function:** the function's body is currently
collapsed and the caret is elsewhere. Selecting a usage result inside
that body calls `open_location`, which now calls `scroll_to_and_reveal` —
`reveal_line` unfolds the function before the row is computed, so the
caret lands on the exact usage line, not the function's signature line.

**Collapsing mid-block:** caret sits well inside a function's body (not
its signature line). `Ctrl+-` collapses the function's own braces, hiding
the caret's own line; `reveal_caret_if_hidden` immediately moves the caret
onto the signature line.

## 6. Dependencies & integration points

- Depends on T18a/T18b (`OpenBuffer`'s current shape, `run_line_op`'s
  precedent for a small per-tab trigger method) — must merge after both
  (already true).
- `crates/tui/src/folding.rs` (new).
- `crates/tui/src/app.rs` — `OpenBuffer::folded`; four new trigger
  methods; `handle_editor_key`'s direction block; `sync_editor_scroll`;
  `open_location`/`open_search_result`/`jump_to_match` (via
  `scroll_to_and_reveal`).
- `crates/tui/src/commands.rs` — four new registry entries.
- `crates/tui/src/ui.rs` — `render_editor`'s row iteration and cursor
  positioning; the collapsed-row `" ⋯"` marker.
- `crates/tui/src/editor.rs` — no signature changes; `move_cursor` is
  called exactly as it was for `Left`/`Right`, with a correction layered
  on at the call site (§3.6) rather than inside `move_cursor` itself.
- `crates/tui/src/highlight.rs` — no changes; the marker is appended
  outside `styled_line` (§3.4).
- Does not touch `ide-lsp` or `ide-core`.

## Revision notes

Self-review round (inline, no `hacker` pass per §1's reasoning — zero new
`ide-core` API, `crates/core/src/text/folding.rs` unmodified):

1. §3.4/§3.6 — found two real gaps by re-reading `handle_editor_key`'s
   full body rather than trusting the direction-handling block alone:
   `KeyCode::Delete`'s handler and `delete_backward`'s no-adjacent-
   bracket-pair fallback each call `move_cursor` directly at their own,
   separate call sites (not through the direction-handling block this doc
   originally scoped `move_caret_with_folds` to) — left unfixed, `Delete`
   at the end of a collapsed fold's `start_line` would have silently
   deleted into the hidden interior one character at a time, and
   `Backspace` at the start of the row right after a fold had the mirror-
   image risk. Both now call `move_caret_with_folds` instead, with the
   consequence that a fold boundary is deleted as a whole unit in one
   keystroke, documented in §3.6. Regression tests: `delete_at_the_end_of
   _a_collapsed_start_line_does_not_eat_the_hidden_newline`,
   `backspace_at_the_start_of_the_row_after_a_fold_does_not_eat_the_hidden
   _newline`.
2. §3.4 — the collapsed-row `" ⋯"` marker originally checked only
   `buf.folded.contains(&line)`, not whether a matching range still
   exists in `fold_ranges()`. A stale `folded` entry (constraint 5) is
   correctly inert for *hiding* purposes (`VisualLines::build` already
   ignores it), but the marker would still have rendered, showing a
   misleading "collapsed" glyph on a line where every line under it is
   already fully visible. Fixed by requiring both conditions.
3. Naming: §2.1/§2.2/§3.5 originally described the tab-open-time config
   step and the caret-reveal helper slightly differently from their final
   names in code (`resolve_editor_config`/`indent_unit_for`,
   `reveal_caret_if_hidden`) — corrected to match, the same discipline
   `tui-line-commands-and-editorconfig.md`'s own self-review applied to
   its `apply_editor_config` naming mismatch.
4. Confirmed via `git diff --name-only main` that the diff stays
   `crates/tui/**`-only (plus this doc); confirmed no binding collisions
   against the other 33 registered commands.
