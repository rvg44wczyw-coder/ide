# T24 — TODO Panel (`ide-tui`)

## 1. Purpose

Ports **G5** (`todo-panel.md`) to `ide-tui`. `ide-ui`'s own G5 is still
`❌` (`docs/roadmap.md` §3, line ~183/539) — same situation `T17` already
documented: this is a fresh v1 design from the roadmap's one-line
description and `ide_core::search_tree`'s existing shape, not a port of an
existing screen.

### 1.1 Scope cut

- **Configurable patterns** — cut. G5's own description promises
  "настраиваемые паттерны" (user-configurable patterns); `ide-tui` has no
  settings UI to expose that yet (the same gap `T17` already noted for
  numbered bookmarks). v1 ships a **fixed, literal** three-pattern set —
  `TODO`, `FIXME`, `HACK` — matching the exact set `ide_core::search_tree`
  is already exercised against and the set most JetBrains IDEs ship as
  defaults out of the box.
- **Grouped-by-file tree view** — cut. This crate's every existing list
  panel (`render_problems_panel`'s `flattened_diagnostics`, both `T17`
  popups) is a flat, single-line-per-row list sorted by `(path, line,
  column)` — never a nested tree. The TODO panel follows the same
  convention rather than introducing a new one.
- **Live refresh on file change** — cut. That's exactly **T25**'s job
  (file watcher, `docs/roadmap.md`'s own next item after this one) — the
  panel re-scans fresh every time it's opened, not continuously.

### 1.2 Binding

No default binding — palette-only, joining `ToggleGitPanel`/
`ToggleCargoPanel`/`JumpToMatchingBracket` in that category. No JetBrains
macOS keymap entry exists for a TODO tool-window shortcut (`docs/roadmap.md`
§5.2's tool-window row lists Project/Find/Run/Debug/Problems/VCS only —
TODO isn't among them), so per `CLAUDE.md`'s "never invent a binding" rule
this is reachable from the command palette only.

## 2. Interface

### 2.1 `crates/tui/src/todo_panel.rs` (new)

```rust
pub(crate) const TODO_PATTERNS: [&str; 3] = ["TODO", "FIXME", "HACK"];

pub(crate) struct TodoMatch {
    pub(crate) pattern: &'static str,
    pub(crate) inner: ide_core::SearchMatch,
}

pub(crate) struct TodoResults {
    pub(crate) matches: Vec<TodoMatch>,
    /// `true` if *any* of the three per-pattern `search_tree` calls hit
    /// `MAX_SEARCH_RESULTS` and stopped early.
    pub(crate) truncated: bool,
}

#[derive(Default)]
pub(crate) struct TodoPanel {
    pub(crate) results: Option<TodoResults>,
    pub(crate) searching: bool,
    generation: u64,
    rx: Option<Receiver<(u64, TodoResults)>>,
}

impl TodoPanel {
    pub(crate) fn run(&mut self, tree: ide_core::DirEntry);
    pub(crate) fn poll(&mut self) -> bool;
}
```

Structurally identical to `search_panel.rs`'s `SearchPanel` (background
thread, generation counter, `rx.try_recv()` polled once per frame) — the
only difference is `run`'s background closure calls `ide_core::
search_tree(&tree, pattern)` **three times** (once per literal in
`TODO_PATTERNS`), tags each resulting match with which pattern it came
from, concatenates, and sorts the combined list by `(path, line, column)`
— the same three-key sort `App::flattened_diagnostics` already uses for
Problems. No new `ide-core` API: this is built entirely on the existing
`search_tree`/`SearchMatch`/`SearchResults` surface, exactly as
`docs/roadmap.md`'s own T24 row already anticipated ("поверх уже
существующего `ide_core::search_tree`, дёшево").

### 2.2 `app.rs` additions

```rust
pub(crate) struct TodoPanelState {
    pub(crate) selected: usize,
}
```

`App` gains `pub(crate) todo: TodoPanel` and `pub(crate) todo_panel:
Option<TodoPanelState>` (presence-is-visibility, same convention
`ProblemsState`/`CodeActionsState` already establish — the results
themselves live in `todo.results`, this only tracks the selected row).

- `fn toggle_todo_panel(&mut self)` — opens (spawning `todo.run(self.tree.
  clone())` unconditionally on open, so every open is a fresh scan per
  §1.1's "no live refresh" cut) or closes, same `close_all_overlays`-first
  shape every other toggle in this crate uses.
- `fn handle_todo_panel_key(&mut self, key) -> LoopSignal` — `Esc` closes;
  `Up`/`Down` move `selected`, clamped to the current result count (`0`
  while still searching or with no results yet); `Enter` defers to
  `confirm_todo_jump`.
- `fn confirm_todo_jump(&mut self)` — resolves the selected row's
  `inner.path`/`inner.byte_offset` and calls the **existing** `open_search_
  result(path, byte_offset)` (`tui-find-in-path.md` §2.2) verbatim — a
  `TodoMatch`'s `inner: SearchMatch` carries exactly the same
  `path`/`byte_offset` shape Find in Path's own jump already consumes, so
  this reuses that helper rather than duplicating its clamp-on-stale-
  offset/scroll-and-reveal logic.
- `pub(crate) fn poll_todo(&mut self)` — called once per frame from
  `main.rs`'s loop (`self.todo.poll()`), the same unconditional-every-
  frame shape `poll_search`/`poll_cargo` already use, so a scan keeps
  running in the background even while the panel is closed.

`close_all_overlays` gains `self.todo_panel = None;`. `handle_key`'s
interception chain gains a `self.todo_panel.is_some()` check. `commands.rs`
gains `Action::ToggleTodoPanel`, bound to `None` (§1.2).

### 2.3 Rendering

`render_todo_panel`, structurally identical to `render_problems_panel`
(centered popup, `Clear`+bordered `List`, `REVERSED` on the selected row).
Each row: `"{pattern}: {path}:{line+1}: {line_text}"` (1-based line,
trimmed `line_text` — the same per-row shape `SearchMatch` already carries
for Find in Path's own popup). `"Scanning..."` while `todo.searching`;
`"No TODOs/FIXMEs/HACKs found."` on an empty completed result; a trailing
truncation-notice row when `results.truncated`.

## 3. Behaviour

Opening the panel always re-scans (§1.1) — `toggle_todo_panel` calls
`run` unconditionally, and `run` itself is the no-op-while-already-
running guard (matching `SearchPanel::run`'s own contract) so rapidly
toggling the panel open/closed/open never spawns a second concurrent scan.
`Enter` opens the file and places the caret at the exact match
(`byte_offset`, not just the line — more precise than `T17`'s bookmarks,
which only ever store a line number since they're user-placed markers, not
derived from a text search).

## 4. Constraints / invariants

- `TODO_PATTERNS` is matched via `search_tree`'s existing case-insensitive
  substring semantics — `search_tree` lowercases its query internally, so
  `"todo"` in a comment matches the literal `"TODO"` pattern too. Accepted
  as consistent with every other use of `search_tree` in this crate (Find
  in Path is case-insensitive too); a future customizable-patterns phase
  can add case-sensitivity as an option without changing this shape.
- Running three whole-tree walks per scan (one per pattern) costs roughly
  3x one `search_tree` call — accepted for v1 given the roadmap's own
  "дёшево" framing and that this only runs once per panel-open, never per
  keystroke.
- Combined `truncated` is `true` if *any* of the three per-pattern calls
  independently hit `MAX_SEARCH_RESULTS` (1000) — each pattern's own cap
  is unaffected by the other two's match counts.

## 5. Examples

A project with `// TODO: fix this` in `a.rs` and `# FIXME later` in
`b.py` produces two rows, `a.rs` sorting before `b.py` (path order),
regardless of which pattern matched which file.

## 6. Dependencies / integration / tests

No new dependency. Diff scope: `crates/tui/src/todo_panel.rs` (new),
`crates/tui/src/app.rs`, `crates/tui/src/commands.rs`, `crates/tui/src/
lib.rs` (`mod todo_panel;` + `app.poll_todo()` in the main loop),
`crates/tui/src/ui.rs`, this doc, `docs/roadmap.md`. No `ide-core`/
`ide-lsp` change, no security-sensitive path touched.

Tests: `todo_panel.rs` mirrors `search_panel.rs`'s own six-test suite
(no-op-while-running, poll-with-nothing-running, run-and-poll-yields-
merged-results-from-multiple-patterns, generation-match/stale-drop,
disconnected-channel) at ≥80% coverage. `app.rs`: open/close, overlay
mutual exclusivity, `Up`/`Down` clamping, `Enter` jumping via
`open_search_result`, `Esc` closing without acting, re-opening triggers a
fresh scan.

## 7. Revision notes

(Filled in during implementation if anything is found worth recording.)
