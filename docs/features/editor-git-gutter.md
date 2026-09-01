# Editor git gutter (E7)

## 1. Purpose

JetBrains-style change bars in the code editor's own gutter: a colored
strip next to the line numbers marking lines added, modified, or deleted
(relative to `HEAD`) in the currently open file, plus a small popup on
click offering "Revert Hunk" and "Show Diff". `crates/ui/src/editor/
geometry.rs` already reserves gutter space for this ("git bars (E7),
breakpoints (F5), fold arrows (A6)") — this phase is the first to actually
paint into it. Depends on **A2** (the code editor widget), done.

`ui`-only: `ide-core`'s `GitRepo::diff_file` (`docs/features/
git-support.md`) already returns exactly the `Vec<DiffHunk>` this feature
needs against `HEAD`; no new `ide-core`/`ide-lsp` surface.

## 2. Interface

### 2.1 `crates/ui/src/editor/git_gutter.rs` (new, pure logic)

```rust
pub enum GutterMarkKind { Added, Modified, Deleted }

pub struct GutterMark {
    /// 0-based buffer line this mark decorates. For `Deleted`, the line
    /// immediately *after* the removed run (where GoLand/IntelliJ draw
    /// the small notch) -- one past the buffer's last line if the
    /// deletion is the file's last lines (a real line index would only
    /// exist if there's a following line; this is a known v1 gap, §3.4).
    pub line: usize,
    pub kind: GutterMarkKind,
}

/// Converts a file's `DiffHunk`s (already ordered by `GitRepo::diff_file`)
/// into gutter marks. Pure, no I/O.
pub fn marks_from_hunks(hunks: &[DiffHunk]) -> Vec<GutterMark>;

/// The `Change` that undoes exactly the hunk containing `clicked_line` (a
/// buffer line, 0-based) against `buffer`'s *current* text -- `None` if no
/// hunk covers it. Callers apply it via `TextBuffer::apply` (one undo
/// step), never write to disk directly (§3.3).
pub fn revert_hunk_change(
    hunks: &[DiffHunk],
    clicked_line: usize,
    buffer: &TextBuffer,
) -> Option<Change>;
```

**`marks_from_hunks` algorithm**: for each hunk, walk `lines` with a
running `new_line` cursor starting at `hunk.new_start - 1` (0-based).
`Context` lines just advance the cursor. A maximal contiguous run of
non-`Context` lines (a "segment", possibly mixing `Removed`/`Added` in any
order git2 happens to emit) is resolved as: `removed_count` = the
segment's total `Removed` lines; walking the segment in order, the *k*-th
`Added` line (0-based) is marked `Modified` if `k < removed_count`, else
`Added`, at the cursor's current value (only `Added`/`Context` advance the
cursor, matching new-file line numbering); if `removed_count` exceeds how
many `Added` lines got paired against it, emit exactly one `Deleted` mark
at the cursor's value *after* the segment (one marker for "N lines
removed here", not one per removed line — matches every real IDE's own
gutter convention).

**`revert_hunk_change`**: finds the hunk whose new-side line range
contains `clicked_line` — `[hunk.new_start - 1, hunk.new_start - 1 +
affected)` where `affected` = count of `Context`+`Added` lines in the
hunk, except a pure-deletion hunk (`affected == 0`, nothing on the new
side at all) matches only `clicked_line == hunk.new_start - 1` (the exact
line its `Deleted` marker sits on). Reconstructs the pre-image text from
the hunk's `Context`+`Removed` lines (each with `\n` re-appended — `git2`'s
own line content already strips it, see `crates/core/src/git/mod.rs`'s
`line_cb`) and returns a `Change` replacing
`buffer.lines().line_start(start_line)..buffer.lines().line_start(end_line)
.unwrap_or(buffer.text().len())` with it.

### 2.2 Theme (`crates/ui/src/theme/{mod,palette}.rs`)

New token `diff_modified_fg: Color32` alongside the existing
`diff_added_fg`/`diff_removed_fg` — a change that has both a removed and
an added side needs its own color (blue, JetBrains' convention) rather
than reading as a plain addition. Both `DARCULA`/`INTELLIJ_LIGHT` get a
value; `no_color_literals_outside_this_module`'s existing test enforces
the literal only ever appears in `palette.rs`.

### 2.3 `crates/ui/src/git_panel.rs`

```rust
impl GitPanel {
    /// Same repo-relative conversion `show_working_tree_diff` already
    /// does, but returns gutter marks instead of populating `self.diff` --
    /// independent of whatever the Source Control view is currently
    /// showing (a commit's diff, or nothing). Empty with no repo, an
    /// untracked file, or no diff.
    pub fn gutter_marks_for(&self, absolute_path: &Path) -> Vec<GutterMark>;

    /// The `DiffHunk`s backing `gutter_marks_for`'s answer for the same
    /// path -- `IdeApp::trigger_revert_hunk` needs the hunks themselves
    /// (for `revert_hunk_change`), not just the derived marks.
    pub fn hunks_for(&self, absolute_path: &Path) -> Vec<DiffHunk>;
}
```

### 2.4 `crates/ui/src/app.rs`

`IdeApp` gains:

```rust
/// The active tab's gutter marks -- recomputed every frame
/// `sync_git_gutter` runs (§3.1), same per-frame-recompute cost `git.
/// show_working_tree_diff` already accepts for the Source Control view.
git_gutter: Vec<GutterMark>,
/// The path `git_gutter` answers -- lets a render call tell "these marks
/// are for the tab currently on screen" apart from one frame's staleness
/// at a tab switch.
git_gutter_path: Option<PathBuf>,
/// The buffer line a gutter-mark click landed on, while its popup
/// ("Revert Hunk" / "Show Diff") is open. `None` when closed.
git_gutter_popup_line: Option<usize>,
```

`fn sync_git_gutter(&mut self)`, called once per frame alongside
`sync_document_highlights`/`sync_code_actions`: clears both fields (no
active tab, an untitled tab, or a **dirty** buffer — §3.1 on why), else
sets them from `self.git.gutter_marks_for(path)`.

`fn handle_git_gutter_click(&mut self, output: &EditorOutput)`, called
once per frame right after the editor widget's `show()` returns: if
`output.git_gutter_clicked_line` is `Some(line)`, sets
`git_gutter_popup_line = Some(line)` (closing the popup on a second click
on a different mark just moves it, matching every other popup's "open on
gesture" convention in this codebase).

`fn trigger_revert_hunk(&mut self)`: no-op unless `git_gutter_popup_line`
and the active tab's path both resolve; builds `revert_hunk_change` from
`self.git.hunks_for(path)` and, if `Some`, applies it via
`Transaction::new(vec![change]).expect(...)` on the active buffer (one
undo step) and closes the popup. **Never writes to disk directly** — the
user's next `⌘S` persists it, same as every other in-editor edit (§3.3).

`fn trigger_show_diff_for_gutter(&mut self)`: closes the popup, switches
`view_mode` to `ViewMode::SourceControl`, and calls
`self.git.show_working_tree_diff(path)` for the active tab's path — reuses
the existing Source Control diff view wholesale rather than a
hunk-scrolled sub-view (v1 simplification, §3.4).

A small popup window (mirroring `render_code_actions_popup`'s exact
`egui::Window` shape) renders while `git_gutter_popup_line.is_some()`,
with the two buttons above.

### 2.5 `crates/ui/src/editor/mod.rs`, `paint.rs`

`CodeEditor` gains `.git_gutter_marks(&'a [GutterMark])` (default `&[]`),
threaded into `Frame` the same way `code_action_line` already is.
`EditorOutput` gains `git_gutter_clicked_line: Option<usize>`.

`paint_gutter` paints a 2px vertical strip in the gutter's *leading*
padding (`rect.min.x` .. `marker_left`, the `space.sm` gap before the
existing fold-arrow/code-action marker lane starts) for `Added`/`Modified`
lines, and a 2px horizontal strip at the row's top edge for `Deleted` —
distinct visual shape, and a distinct lane from fold arrows/the code-action
lightbulb, so nothing ever collides on the same row.

`handle_mouse` gains a hit-test (`git_gutter_click_target`, same shape as
`fold_click_target`, checked alongside it) against that same leading strip
and sets `clicked_line` — checked before the general click-to-select
handling, same reason `fold_click_target` is.

## 3. Behaviour

### 3.1 Dirty buffers show no marks

`GitRepo::diff_file` diffs the on-disk working-tree file against `HEAD` —
it has no notion of an editor's live, unsaved buffer content. Computing
marks from it while the buffer has unsaved edits would attach them to
line numbers that no longer match what's actually on screen (an insertion
above a hunk shifts every mark below it), which is worse than showing
nothing. `sync_git_gutter` therefore clears `git_gutter` whenever
`buffer.is_dirty()` is true, and marks reappear the frame after the next
save. This is a deliberate v1 scope cut, not an oversight — the
alternative (diffing the live buffer against a git blob) needs a new
`ide-core` API this "ui"-only phase doesn't have.

### 3.2 Click-and-popup

Clicking the leading-strip bar on a marked line opens the same-shaped
popup `render_code_actions_popup` already uses, offering "Revert Hunk" and
"Show Diff". Clicking elsewhere (including a different mark) doesn't
require an explicit close first — same convention every other popup here
follows.

### 3.3 Revert Hunk never touches disk directly

`trigger_revert_hunk` applies through `TextBuffer::apply` — the same
mutation entry point every keystroke, paste, and code action already
goes through — so it's one ordinary undo step, `⌘Z` undoes it, and nothing
is written to disk until the user's own next save. This keeps the feature
off `CLAUDE.md`'s security-sensitive list: there's no new write-to-disk
path, no new subprocess, no new path input from outside the project (the
hunk data comes from `GitRepo::diff_file`, already-audited in
`git-support.md`'s own review).

### 3.4 What v1 doesn't cover

- A deletion at the true end of the file has no following line to attach
  its marker to (§2.1) — it simply doesn't render. Rare in practice (most
  trailing deletions are followed by at least the file's own final
  newline, which is still a line for `LineIndex`'s purposes).
- "Show Diff" opens the whole file's Source Control diff, not scrolled to
  the clicked hunk specifically.
- No gutter tooltip/hover preview of the hunk's content before clicking —
  IntelliJ shows a small inline diff preview on hover; this phase only
  wires up the click.
- Only two change colors plus a deletion marker (`Added`/`Modified`
  green/blue, `Deleted` a small red notch) — no separate "staged vs
  unstaged" distinction (this project's git integration has no staging
  concept surfaced anywhere else in `ide-ui` either).

## 4. Constraints

- Not on `CLAUDE.md`'s security-sensitive path list, and doesn't newly
  qualify: no subprocess, no network, no new disk-write path (§3.3), no
  new user-controlled path input (`diff_file`'s path comes from the
  already-open, already-validated active tab). `hacker` skipped.
- `marks_from_hunks`/`revert_hunk_change` are pure and fully unit-testable
  against hand-built `DiffHunk` fixtures — no live repo needed for the
  algorithm's own correctness; `GitPanel::gutter_marks_for`/`hunks_for`
  get a couple of real-repo integration tests mirroring
  `show_working_tree_diff`'s own test style.

## 5. Examples

- A file with one line changed: that line's hunk has one `Removed` +
  one `Added` line → the `Added` line (the only one, `k=0 <
  removed_count=1`) is marked `Modified` — a single blue bar.
- A file with 3 new lines appended at the end: one hunk, `Context`s then
  3 `Added` with `removed_count == 0` → all 3 marked `Added` — green bars,
  no `Deleted`.
- A file with 2 lines deleted and nothing added in their place: one hunk
  with 2 `Removed`, 0 `Added` → `removed_count(2) > added_seen(0)` → one
  `Deleted` mark at the cursor position right after the (empty) run —
  attached to the line that now sits where the deletion happened.
- User clicks that `Deleted` mark's popup → "Revert Hunk": `affected ==
  0` for that hunk, so the match rule is `clicked_line ==
  hunk.new_start - 1` exactly; the change re-inserts the 2 removed lines
  at that position as one undo step.

## 6. Dependencies / integration

No new external dependency. Touches `crates/ui/src/editor/{mod,paint,
git_gutter}.rs` (new), `crates/ui/src/{app,app/render,git_panel}.rs`,
`crates/ui/src/theme/{mod,palette}.rs` — single role, `rust-ui-dev`.

## Revision notes

- **`trigger_revert_hunk` ordering bug (found via its own test).** The
  first implementation cleared `self.git_gutter_popup_line = None` *before*
  calling `git_gutter_popup_target()` — but that method's gating logic
  reads `self.git_gutter_popup_line?` to get the clicked line, so it always
  saw `None` and the whole method silently no-op'd (the popup closed but
  nothing ever reverted). Fixed by capturing
  `git_gutter_popup_target()`'s result into a local *before* clearing the
  field, mirroring the "snapshot what you need, then mutate" shape already
  used elsewhere in this file (e.g. `code_action_gutter_line`). Caught by
  `trigger_revert_hunk_applies_the_change_and_closes_the_popup`, which
  built a real one-line-modification working-tree diff via `git2`/a real
  `git` binary rather than a hand-built fixture — worth noting since the
  pure-logic `revert_hunk_change` tests in `git_gutter.rs` (hand-built
  `DiffHunk` fixtures) all passed throughout; the bug was entirely in the
  `IdeApp`-level wiring, not the algorithm.
- `diff_modified_fg`'s RGB values (`#6CA0E0` dark / `#1A5FB4` light) were
  computed with a throwaway WCAG relative-luminance/contrast script before
  being hardcoded, to guarantee both clear the same 4.5:1 floor
  `diff_added_fg`/`diff_removed_fg` are already held to against `bg_base` —
  confirmed by `diff_text_is_legible_on_the_panel`'s extended assertion
  rather than by trial and error against the test.
- Final coverage on touched/new files (`cargo llvm-cov -p ide-ui`):
  `editor/git_gutter.rs` 99.74%, `git_panel.rs` 97.84%, `app.rs` 96.45%,
  `theme/mod.rs` 98.43%, `theme/palette.rs` 97.86%, `editor/mod.rs` 82.94%
  (includes non-rendering click-routing logic alongside the exempt paint
  code, still clears the 80% floor). Not security-sensitive per
  `CLAUDE.md`'s list — `hacker` skipped, consistent with the plan made
  before implementation started.
