# `ide-tui`: scroll follows cursor

## 1. Purpose

`ide-tui`'s editor pane had no scroll-follows-cursor logic anywhere: the
only thing that ever changed `OpenBuffer::scroll` was `tui-find.md`'s
`jump_to_match` (an unconditional top-align on a find/replace jump).
Ordinary cursor movement -- arrow keys, typing past the bottom of the
visible area, undo/redo -- never adjusted `scroll` at all, so once the
cursor moved outside the currently-visible rows the editor pane appeared
frozen: the user could keep typing or pressing arrows, the buffer was
genuinely changing, but nothing on screen moved. Discovered live by a user
("editor panel in not scrolling") immediately after using per-buffer
find/replace, in the same session that fixed two other real-terminal
reachability bugs (`tui-shell-and-editor.md`'s and `tui-find.md`'s
Post-merge correction notes on `Ctrl+1`/`Ctrl+Shift+*`).

This batch adds a minimal-scroll viewport-follow: after any key that can
move the cursor, `scroll` is adjusted only enough to bring the cursor's
line back into the visible window, never jumping further than necessary
and never moving when the cursor is already visible.

## 2. Interface / API

### 2.1 `src/editor.rs`

```rust
pub fn scroll_to_keep_visible(scroll: u16, cursor_line: usize, viewport_rows: u16) -> u16;
```

Pure function, no `App`/`TextBuffer` dependency -- matches this file's
existing `cursor_line_column`/`move_cursor` shape. Given the current
`scroll` (the viewport's first visible line), the cursor's absolute line,
and how many text rows are currently visible:

- `viewport_rows == 0` returns `scroll` unchanged (nothing is visible to
  clamp into -- see §4).
- `cursor_line` is capped to `u16::MAX` before arithmetic, the same
  defensive cap `jump_to_match` already applies.
- If `cursor_line < scroll`, returns `cursor_line` (top-align -- the
  cursor moved above the visible window).
- If `cursor_line >= scroll + viewport_rows`, returns
  `cursor_line + 1 - viewport_rows` (scroll down the minimum amount that
  puts `cursor_line` on the viewport's last visible row).
- Otherwise returns `scroll` unchanged (cursor already visible).

### 2.2 `src/app.rs`

```rust
impl App {
    pub(crate) fn set_editor_viewport_rows(&mut self, rows: u16);
}
```

`App` gains a private `editor_viewport_rows: u16` field, defaulting to
`u16::MAX` in `App::new` (see §4 for why). `set_editor_viewport_rows` is
called once per frame by `main.rs`, before `handle_key`, with the editor
pane's current text-row count.

A new private helper:

```rust
fn sync_editor_scroll(buf: &mut OpenBuffer, viewport_rows: u16);
```

reads `buf`'s current cursor line and applies `scroll_to_keep_visible`.
It's a function taking `buf: &mut OpenBuffer` explicitly, not a `&mut
self` method, so it can run from inside `handle_editor_key` after
`active_buffer_mut()`'s mutable borrow of `self.tabs` is already held --
a second call through `self` there would conflict with that borrow.
Called from:

- `handle_editor_key`'s arrow-key branch, right before its early `return`.
- `handle_editor_key`'s end, after the `match key.code` block (covers
  typed characters, `Enter`, `Tab`, `Backspace`, `Delete`).
- `run_action`'s `Action::Undo`/`Action::Redo` arms, right after
  `buffer.undo()`/`buffer.redo()`.

`jump_to_match` (`tui-find.md` §2.2) is **not** changed and does not call
`sync_editor_scroll` -- it keeps its own unconditional top-align, which
already satisfies `scroll_to_keep_visible`'s invariant trivially (setting
`scroll = line` always leaves the cursor at the viewport's very first
row, which is visible by construction), so the two never conflict.

### 2.3 `src/ui.rs`

```rust
pub const EDITOR_CHROME_ROWS: u16 = 4;
```

The editor pane's non-text-row count: the status bar (`render`'s own
vertical split, 1 row) + the editor `Block`'s top/bottom borders (2 rows)
+ the tab strip (1 row). `ui.rs` mutates nothing and contains no logic
per its own module doc comment, so it cannot compute
`editor_viewport_rows` itself and hand it to `App` -- `main.rs` derives
the value from this constant instead of running a real `Layout` pass
before every keystroke.

### 2.4 `src/main.rs`

`run`'s loop gains, before polling for input:

```rust
let rows = terminal.size()?.height;
app.set_editor_viewport_rows(rows.saturating_sub(ui::EDITOR_CHROME_ROWS));
```

Queried every iteration (not just once at startup), so a terminal resize
is picked up before the next keystroke's scroll-follow calculation.

## 3. Behaviour

1. User presses `Down` repeatedly past the bottom of the visible window:
   `handle_editor_key` moves the cursor via the existing `move_cursor`,
   then `sync_editor_scroll` scrolls down by the minimum needed to keep
   the new cursor line on the viewport's last row.
2. User presses `Up` back above the top of the window: same call scrolls
   up, top-aligning the cursor's line.
3. User types past the bottom (e.g. many `Enter`s): the same end-of-
   function `sync_editor_scroll` call covers this, since it runs after
   every branch of the `match key.code` block, not just the arrow-key
   path.
4. `Ctrl+Z`/`Ctrl+Shift+Z` (undo/redo): the cursor can jump to wherever
   the inverted transaction restores it, potentially far from the current
   scroll position -- `run_action`'s `Undo`/`Redo` arms re-sync the
   scroll immediately after, so the edit site is always back in view.
5. `Ctrl+G`/`Ctrl+Shift+G`/find-jumps: unaffected, `jump_to_match` keeps
   its own top-align behavior (§2.2).

## 4. Constraints & invariants

- **`viewport_rows == 0` is deliberately a safe no-op, not an error.**
  Before `main.rs`'s first loop iteration ever runs -- and in every
  existing test that constructs an `App` directly without calling
  `set_editor_viewport_rows` -- there is no real viewport height to clamp
  into. `App::new` initializes `editor_viewport_rows` to `u16::MAX`
  instead of `0` specifically so the clamp is *always* a no-op by
  default (an effectively-infinite viewport never triggers either scroll
  branch), preserving every pre-existing test's `scroll == 0` assertions
  exactly, rather than requiring every one of them to be updated to call
  a new setter they don't care about.
- **`EDITOR_CHROME_ROWS` is a second, independent copy of a layout fact
  `ui.rs`'s real `Layout` calls already know.** There is no single source
  of truth shared between the two -- `ui.rs`'s own doc comment explains
  why (it must stay mutation/logic-free), and `EDITOR_CHROME_ROWS`'s own
  doc comment flags that a future change to `render`'s or `render_editor`'s
  vertical splits must also update this constant, or the scroll-follow
  clamp will use a viewport height that's off by however many rows the
  layout changed by. This is a real, accepted coupling, not an oversight.
- **Minimal scroll, not re-centering or top-align-on-every-move.** The
  clamp only moves `scroll` far enough to restore visibility, matching
  ordinary editor/terminal-pane behavior (e.g. `less`, `vim`) rather than
  jumping the cursor to a fixed relative row on every keystroke.
- **`jump_to_match`'s existing top-align is untouched.** `tui-find.md`
  §4.3 already reviewed and locked in that behavior for the find/replace
  jump case specifically; this batch does not revisit that decision.

## 5. Examples

```
$ ide-tui ~/code/my-project
```

Opens a 60-line file with a 10-row visible editor pane. `Down` pressed 15
times moves the cursor to line 15; without this batch, the view would
stay frozen showing lines 0-9 with no visible cursor. With it, `scroll`
becomes 6, so the visible window is lines 6-15 and the cursor sits on the
window's last row. Pressing `Up` 15 times back to line 0 scrolls `scroll`
back to 0, top-aligning.

## 6. Dependencies & integration points

No new crate dependencies. Touches `crates/tui/src/{app,editor,main,ui}.rs`
only -- not on `CLAUDE.md`'s security-sensitive path list, no `hacker`
pass required.

## 7. Diagrams

None -- the behavior is a small, linear per-keystroke calculation
(`scroll_to_keep_visible`'s own doc comment and §2/§3 above already state
it precisely); a sequence diagram would restate the same three lines of
arithmetic without adding information a diagram is actually good at
conveying (branching control flow, component boundaries). Skipped rather
than added for convention's own sake.

## Revision notes

Implemented directly in response to a live user report ("editor panel in
not scrolling") in the same session as two other real-terminal
reachability fixes; this doc was written after implementation and testing
rather than before, given the bug was actively blocking basic editor use.
Self-reviewed (code-review checklist + devil's-advocate pass) before
merge: no controversial findings -- the `EDITOR_CHROME_ROWS` duplication
(§4) was considered and is the accepted, documented trade-off given
`ui.rs`'s own no-logic invariant, not an oversight to fix later.
