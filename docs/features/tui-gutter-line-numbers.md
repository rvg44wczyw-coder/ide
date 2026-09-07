# T50: TUI Line-Number Gutter + Right-Click Context Menu

## 1. Purpose

`ide-tui`'s editor has never rendered line numbers, confirmed by direct
inspection: `ui.rs`'s per-line render loop only ever prepends the blame
lane (`blame_lane_width`) and the git-gutter lane (`git_gutter_lane_width`)
ahead of the text — there is no line-number column at all, and a
breakpointed line washes its whole background instead of getting a gutter
marker, per `ui.rs`'s own comment: "`ide-tui` has no gutter to paint a
breakpoint marker into" (`docs/features/tui-debugger.md` §2.4). `ide-ui`,
by contrast, got a real gutter — line numbers, current-line band,
breakpoint-click-to-toggle — in phase **A2** (`crates/ui/src/editor/`).
This run closes that specific `ide-tui` gap and, on top of it, adds the
first right-click / context-menu interaction anywhere in this codebase
(verified: neither crate has any `MouseButton::Right` handling today).

Two additive pieces:

1. A **line-number lane**, rightmost of the gutter lanes (blame, then
   git-gutter, then line numbers, then text) — mirrors `ide-ui`'s own
   documented lane ordering ("JetBrains/VS Code both put blame left of
   line numbers", `crates/ui/src/editor/geometry.rs:78`).
2. A **right-click context menu** on that lane: Toggle Line Breakpoint,
   Toggle Bookmark, Show Bookmarks, Toggle Blame Annotations. All four
   already exist as commands (`ToggleLineBreakpoint`/`ToggleBookmark`/
   `ShowBookmarks`/`ToggleBlameAnnotations`) — this run's only job is
   giving them a gutter entry point, plus generalizing the first two from
   "act on the caret's line" to "act on an arbitrary clicked line". A
   **left**-click on the lane toggles a breakpoint directly (no menu),
   mirroring `ide-ui`'s real gutter behavior (`docs/features/debugger.md`
   §347: "clicking a line's line-number digits... toggles a breakpoint").

Confirmed non-goals: no current-line highlight band (a separate,
unrequested feature — `ide-tui` has no current-line highlighting of any
kind today, and this run doesn't add one), no active-line-number color
distinction (`ide-ui`'s `gutter_fg_active` token has no `ide-tui`
equivalent and none is added here — plain `theme.gutter_fg` for every
line number, uniformly), and no context menu anywhere else in the editor
(scoped to the line-number lane only, matching exactly what was asked).

## 2. Interface

### 2.1 `App::line_number_lane_width` (`app.rs`, new, next to
`blame_lane_width`/`git_gutter_lane_width` at `app.rs:4025`/`4071`)

```rust
/// `0` with no active tab; else the decimal digit width of the active
/// buffer's total line count, plus one column for a trailing space --
/// mirrors `ide-ui`'s `Metrics::gutter_width` stepping at line-count
/// digit boundaries (`code-editor-widget.md` §3.3), the same "grows only
/// when an extra digit is actually needed" rule, not a fixed budget.
pub(crate) fn line_number_lane_width(&self) -> u16;
```

### 2.2 `App::editor_lane_width` (existing, `app.rs:4088`) — widened

```rust
pub(crate) fn editor_lane_width(&self) -> u16 {
    self.blame_lane_width() + self.git_gutter_lane_width() + self.line_number_lane_width()
}
```

Every existing caller (`render_editor`'s native-cursor-position fix,
`ui.rs:703`; `handle_mouse_click`'s text-vs-lane split, `app.rs:6522`)
keeps working unchanged — both already treat this as one opaque combined
offset, never assuming it's exactly two lanes.

### 2.3 `App::resolve_gutter_line` (new, private)

```rust
/// Row is relative to the editor's text area's top-left corner. Same
/// bounds-check shape `click_blame_lane`/`click_git_gutter_lane` already
/// establish for mapping a click row to a buffer line (`VisualLines::
/// build` + `buf.scroll` offset + `row_count()` bounds check), just also
/// returning the active tab's path since both of this run's new callers
/// need it. `None` with no active tab or a row past the buffer's last
/// visible row.
fn resolve_gutter_line(&self, area_row: u16) -> Option<(PathBuf, usize)>;
```

### 2.4 `App::click_line_number_lane` (new, mirrors `click_git_gutter_lane`)

```rust
/// Left-click on the line-number lane: toggles a breakpoint on the
/// clicked line via `resolve_gutter_line` + `toggle_breakpoint_at_line`
/// (§2.6) -- independent of wherever the caret currently sits.
fn click_line_number_lane(&mut self, area_row: u16);
```

Wired into `handle_mouse_click`'s existing three-way lane split
(`app.rs:6511-6526`), which gains a fourth branch between git-gutter and
text:

```rust
let blame_w = self.blame_lane_width();
let git_w = self.git_gutter_lane_width();
let lane = self.editor_lane_width(); // blame + git + line-number, unchanged call
if (col as usize) < blame_w as usize {
    self.click_blame_lane(row);
} else if (col as usize) < (blame_w + git_w) as usize {
    self.click_git_gutter_lane(row);
} else if (col as usize) < lane as usize {
    self.click_line_number_lane(row);
} else {
    self.click_editor_at(col - lane, row);
}
```

(The middle branch's bound changes from `< lane` to `< blame_w + git_w`
— a required change, not incidental, since `lane` now spans three lanes
instead of two.)

### 2.5 `GutterContextMenuState` (new) + `App::gutter_context_menu` field

```rust
/// The line-number gutter's right-click menu -- `path`/`line` name the
/// buffer line the right-click landed on (0-based, same convention
/// `click_git_gutter_lane`/`click_blame_lane` already use), independent
/// of wherever the caret happens to be, matching JetBrains' own gutter
/// context menu (it acts on the clicked line, not the caret's line).
pub(crate) struct GutterContextMenuState {
    pub(crate) path: PathBuf,
    pub(crate) line: usize,
    pub(crate) selected: usize,
}
```

`gutter_context_menu: Option<GutterContextMenuState>` is added to `App`
next to `git_gutter_popup_line` (`app.rs:1164`), defaulting to `None`.

### 2.6 New `App` methods

```rust
/// Right-click anywhere in the editor text area (new `MouseEventKind::
/// Down(MouseButton::Right)` arm in `handle_mouse`, §2.8). A no-op unless
/// the click lands specifically inside the line-number lane's column
/// range -- right-clicking the blame/git-gutter lanes or the text itself
/// does nothing in this run (scoped exactly to what was asked: a context
/// menu for bookmarks/breakpoints on the line-number gutter, not a
/// general editor-body context menu).
fn handle_mouse_right_click(&mut self, event: MouseEvent, hits: &crate::ui::HitMap);

/// `path`/`line` become the caret-based versions' shared implementation
/// (§2.7) -- both existing `toggle_breakpoint_at_caret`/
/// `toggle_bookmark_at_cursor` are refactored to resolve their line from
/// the caret and delegate here, so the context menu's "act on the
/// clicked line, not the caret" requirement doesn't fork the underlying
/// toggle logic into two copies.
fn toggle_breakpoint_at_line(&mut self, path: PathBuf, line: usize);
fn toggle_bookmark_at_line(&mut self, path: PathBuf, line: usize);

/// Navigates/confirms the open menu -- `Up`/`Down` clamp against the
/// fixed 4-item list, `Enter` calls `confirm_gutter_context_menu`, `Esc`
/// closes, every other key is a no-op. Checked in `handle_key`'s popup-
/// priority chain immediately after `git_gutter_popup_line` (`app.rs:
/// 6202-6204`), same position, same "owns all input while open" effect.
fn handle_gutter_context_menu_key(&mut self, key: KeyEvent) -> LoopSignal;

/// Takes `gutter_context_menu` (closing it unconditionally -- every one
/// of the four actions is a plain toggle/open with nothing that would
/// ever need to keep the menu open) and dispatches on `.selected`:
/// 0 -> `toggle_breakpoint_at_line`, 1 -> `toggle_bookmark_at_line`,
/// 2 -> `toggle_bookmarks_popup` (existing, `Ctrl+F3`'s target),
/// 3 -> `toggle_blame_annotations` (existing, palette-only today).
fn confirm_gutter_context_menu(&mut self);
```

### 2.7 Existing methods, refactored to delegate (behavior-preserving)

```rust
fn toggle_breakpoint_at_caret(&mut self) {
    let Some(buf) = self.active_buffer() else { return; };
    let path = buf.path.clone();
    let line = cursor_line_column(buf.buffer.text_buffer(), buf.buffer.text_buffer().selections().primary().head).0;
    self.toggle_breakpoint_at_line(path, line);
}

fn toggle_bookmark_at_cursor(&mut self) {
    let Some(buf) = self.active_buffer() else {
        self.notify("No file open to bookmark.");
        return;
    };
    let path = buf.path.clone();
    let line = cursor_line_column(buf.buffer.text_buffer(), buf.buffer.text_buffer().selections().primary().head).0;
    self.toggle_bookmark_at_line(path, line);
}
```

Every existing test against these two caret-based entry points keeps
passing unchanged — this is a pure extract, not a behavior change (`debug
.toggle_breakpoint(path, line as u32 + 1)` and `nav_state.toggle_bookmark
(path, line)` + `project_state::save` + `self.notify(...)` move into the
new `_at_line` methods verbatim).

### 2.8 `handle_mouse` (existing, `app.rs:6427-6432`) — new arm

```rust
match event.kind {
    MouseEventKind::Down(MouseButton::Left) => self.handle_mouse_click(event, hits),
    MouseEventKind::Down(MouseButton::Right) => self.handle_mouse_right_click(event, hits),
    MouseEventKind::ScrollUp => self.handle_mouse_scroll(event, hits, KeyCode::Up),
    MouseEventKind::ScrollDown => self.handle_mouse_scroll(event, hits, KeyCode::Down),
    _ => {}
}
```

### 2.9 `ui.rs` changes

- `render_editor`'s per-line loop (`ui.rs:611-656`) gains a new prepend
  step for line numbers, inserted **between** the fold-marker append and
  the existing git-gutter prepend (so the final left-to-right order is
  blame, git-gutter, line-number, text — matching §2.2's lane order):

  ```rust
  if app.line_number_lane_width() > 0 {
      let digits = app.line_number_lane_width() as usize - 1;
      let number = format!("{:>digits$} ", line + 1, digits = digits);
      let mut spans = vec![Span::styled(number, Style::default().fg(theme.gutter_fg))];
      spans.extend(styled.spans);
      styled = Line::from(spans);
  }
  ```

- New `render_gutter_context_menu(frame, app, area)`, same small-fixed-
  popup shape `render_git_gutter_popup` uses (`ui.rs:3318`), but list-
  selectable via the existing `render_scrollable_list` helper
  (`ui.rs:320`) since it has four items, not two mnemonic single keys —
  mirrors `render_bookmarks_popup`'s exact list-with-`REVERSED`-highlight
  shape (`ui.rs:1410`). Wired into the per-frame popup dispatch right
  after `render_git_gutter_popup`'s own call (`ui.rs:238-240`).

## 3. Behaviour

### 3.1 Lane width and ordering

`line_number_lane_width()` is `0` only with no active tab; otherwise it's
always shown, unconditionally (unlike blame/git-gutter, which are `0`
until explicitly toggled on / inside a clean repo respectively) — line
numbers are useful independent of git state, matching `ide-ui`'s A2
gutter, which is likewise always present. It grows by one column exactly
when the buffer's line count crosses a power-of-ten boundary (9→10 lines,
99→100, etc.) — computed fresh every frame from `to_string().len()` on
the current total line count, not cached, since `App::line_number_lane_
width` is already called once per frame the same way its two sibling
lane-width methods are.

### 3.2 Left-click vs. right-click on the line-number lane

- **Left-click** (`click_line_number_lane`): toggles a breakpoint on the
  clicked line, unconditionally — no confirmation, no menu. Matches
  `ide-ui`'s real behavior exactly (`debugger.md` §347).
- **Right-click** (`handle_mouse_right_click`): opens the four-item menu
  at `GutterContextMenuState { path, line, selected: 0 }`, `selected`
  always starting at the first item (Toggle Line Breakpoint) — no attempt
  to pre-select based on whether a breakpoint/bookmark already exists on
  that line, matching this crate's existing "menus don't reflect current
  state in their initial selection" convention (`render_git_gutter_popup`
  doesn't either).

### 3.3 Menu item behavior

1. **Toggle Line Breakpoint** — `toggle_breakpoint_at_line(path, line)`.
2. **Toggle Bookmark** — `toggle_bookmark_at_line(path, line)`, including
   its existing "Bookmark added/removed at line N" notification.
3. **Show Bookmarks** — `toggle_bookmarks_popup()`, the existing `Ctrl
   +F3` target; opens the full bookmarks list (not scoped to the clicked
   line — showing *all* bookmarks is the entire point of that popup).
4. **Toggle Blame Annotations** — `toggle_blame_annotations()`, the
   existing palette-only command; toggles the *active tab's* blame lane
   on/off (not scoped to the clicked line, since blame is a whole-buffer
   toggle, not a per-line one — `docs/features/tui-blame.md` §3.1). This
   gives it a genuine, discoverable second entry point beyond the
   command palette, for the first time.

All four close the menu immediately on selection (§2.6's
`confirm_gutter_context_menu` unconditionally `.take()`s the state
first).

### 3.4 Popup-priority and overlay-reset placement

`handle_key`'s dispatch checks `gutter_context_menu.is_some()`
immediately after `git_gutter_popup_line.is_some()` (`app.rs:6202-6204`)
— while open, every key routes there first, exactly the same "owns all
input" effect `git_gutter_popup_line` already has. `any_popup_open`
gains a matching `|| self.gutter_context_menu.is_some()` arm right next
to `git_gutter_popup_line`'s own (`app.rs:6393`), which is what makes
`handle_mouse_click`'s top-of-function `any_popup_open()` guard (`app.rs:
6439`) correctly block a left-click from reaching the editor while the
menu is up.

**Deliberately not added to `close_all_overlays`** — mirrors
`git_gutter_popup_line`'s own identical omission (verified: it isn't
there either, `app.rs:2333-2374`) for the same reason that omission is
safe rather than a bug: the popup-priority check in `handle_key` runs
*before* any command-dispatch path that could open a competing overlay,
so as long as the menu is open, no other overlay-opening keybinding is
even reachable — the priority-chain check itself is the exclusivity
guard, not `close_all_overlays`. Since `handle_mouse_click` also bails
out via `any_popup_open()` before reaching any lane-click code, no mouse
path can open a second overlay either. `close_all_overlays` therefore has
nothing to do here that the existing guards don't already cover.

### 3.5 Cursor-position and existing-lane interaction

`render_editor`'s native-terminal-cursor placement (`ui.rs:702-704`)
already adds `app.editor_lane_width()` as a flat offset — since that
method is widened to include the new lane (§2.2) rather than replaced,
the caret continues landing on the correct screen column with zero
changes to that call site. Blame and git-gutter rendering/click-handling
are untouched; they simply end up two/four columns further left on
screen than before whenever line numbers are also showing, which the
lane-width composition already accounts for.

## 4. Constraints & invariants

- Right-clicking anywhere **other** than the line-number lane's column
  range (blame lane, git-gutter lane, the text itself, or outside the
  editor entirely) is a no-op in this run — no general editor-body
  context menu, no gutter-lane-specific menus for blame/git-gutter
  (those already have their own click-to-open popups, unchanged).
- `resolve_gutter_line`'s bounds check (`clicked_row >= visual.row_count
  ()`) matches `click_blame_lane`/`click_git_gutter_lane`'s existing
  "click past the buffer's last line does nothing" behavior exactly — no
  new failure mode introduced.
- The menu's 4-item list is fixed and unconditional — it does not vary
  based on whether the target line already has a breakpoint/bookmark, or
  whether the active tab has blame already on. Simpler than a
  state-reflecting menu, matches this crate's existing "small fixed
  popups don't reflect target state in their content" convention
  (`render_git_gutter_popup`'s two actions are likewise unconditional).
- `line_number_lane_width` reads the *active* buffer's line count only —
  a background tab's own gutter width, if it were ever rendered, is not
  this run's concern (only one tab's editor body is ever on screen at a
  time in this crate).

## 5. Examples

Left-click toggles a breakpoint directly:

```rust
// Column 3 of a 40-line file (2-digit lane) lands in the line-number
// lane; row maps to buffer line 7.
app.click_line_number_lane(7);
assert!(app.debug.breakpoints_for(&path).contains(&8)); // 1-based
```

Right-click opens the menu, `Down` then `Enter` toggles a bookmark:

```rust
app.handle_mouse_right_click(right_click_at(line_number_lane_x, row), &hits);
assert!(app.gutter_context_menu.is_some());
app.handle_key(down_key());   // selected: 0 -> 1 (Toggle Bookmark)
app.handle_key(enter_key());
assert!(app.gutter_context_menu.is_none());
assert!(app.nav_state.bookmarks.iter().any(|b| b.line == clicked_line));
```

## 6. Dependencies & integration points

- `blame_lane_width`/`git_gutter_lane_width`/`editor_lane_width`
  (`app.rs:4025/4071/4088`) — widened, not replaced.
- `VisualLines::build`/`buffer_line`/`row_count` (already used by
  `click_blame_lane`/`click_git_gutter_lane`) — read-only reuse in
  `resolve_gutter_line`.
- `App::debug.toggle_breakpoint(PathBuf, u32)` (`debug_panel.rs:143`),
  `App::nav_state.toggle_bookmark(PathBuf, usize)`, `toggle_bookmarks_
  popup`, `toggle_blame_annotations` — all pre-existing, unchanged;
  this run only adds call sites.
- `render_scrollable_list` (`ui.rs:320`) — reused as-is for the new menu.
- `theme.gutter_fg` (`theme.rs:87`) — pre-existing token, currently only
  used for dim/secondary text in the AI panel (`ui.rs:1839/1871`); this
  run is its first use for an actual gutter, i.e. its originally-intended
  purpose.
