# G5 — TODO Panel (GUI)

## §1 Purpose

A bottom-tool-window tab that searches the project tree for `TODO`, `FIXME`,
and `HACK` comments and displays them in a scrollable list. Clicking an entry
opens the file at the matching line.

### §1.1 Scope (v1)

- Literal patterns only: `TODO`, `FIXME`, `HACK` — same set as the TUI version
  (`crates/tui/src/todo_panel.rs`).
- No user-configurable patterns (needs a settings UI that doesn't exist yet).
- Off-thread scan via `ide_core::search_tree`, same pattern as
  `search_panel.rs` / `files_search.rs`.
- Results sorted by `(path, line, column)`.
- Truncation indicator when `MAX_SEARCH_RESULTS` is hit.
- Security posture: read-only project tree scan. No `hacker` required.

## §2 Interface / API

### §2.1 `TodoMatch` (crates/ui/src/todo_panel.rs)

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoMatch {
    pub pattern: &'static str,
    pub inner: SearchMatch,
}
```

### §2.2 `TodoResults`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TodoResults {
    pub matches: Vec<TodoMatch>,
    pub truncated: bool,
}
```

### §2.3 `TodoPanel`

```rust
#[derive(Default)]
pub struct TodoPanel {
    pub results: Option<TodoResults>,
    pub searching: bool,
    generation: u64,
    rx: Option<Receiver<(u64, TodoResults)>>,
}
```

Methods: `run(DirEntry)`, `poll() -> bool` — identical semantics to the TUI
version (`crates/tui/src/todo_panel.rs`).

### §2.4 Command

`ToggleTodoToolWindow` — toggles the bottom tool window open and selects the
TODO tab. No default binding (reachable from command palette). Registered in
`command.rs` and `app/menu.rs`.

## §3 Behaviour

### §3.1 Bottom tool window

`BottomView` gets a new variant `Todo`. The bottom tool-window stripe gains a
"TODO" tab, rendered next to Problems / Cargo / Usages / Search / Debug /
Custom Actions.

### §3.2 Panel rendering

`render_todo_panel` renders:
- Top bar: Refresh button + spinner (while searching) + match count
- Scrollable list of matches, each row showing:
  - Pattern label (`TODO` / `FIXME` / `HACK`) in a category color
  - File path (relative to project root)
  - Line content (truncated to 80 chars)
- Clicking a row calls `self.open_diagnostic(path, Position)` — same as
  Problems panel click handling.

### §3.3 Scan trigger

The scan runs when the user clicks the Refresh button. Subsequent tab
switches show cached results without re-scanning. Manual refresh
re-triggers the scan.

## §4 Constraints and invariants

- Off-thread: `run` spawns a thread, `poll` drains the channel once per
  frame. No blocking in the UI thread.
- Generation counter: stale results from a previous `run` are dropped.
- Truncation: if `MAX_SEARCH_RESULTS` is hit, `truncated` is set.

## §5 Tests

- `TodoPanel::run_and_poll_merges_matches` — port from TUI tests
- `TodoPanel::run_while_searching_is_noop`
- `TodoPanel::poll_drops_stale_generation`
- `TodoPanel::poll_accepts_matching_generation`
- `TodoPanel::poll_on_disconnected_channel`

## §6 Dependencies & integration points

- No new crate dependencies.
- `mod todo_panel` added to `lib.rs`.
- `BottomView::Todo` added to `app.rs`.
- `ToggleTodoToolWindow` added to `command.rs` + `app/menu.rs`.
- `todo: TodoPanel` field added to `IdeApp`.
- `render_todo_panel` method added to `app/render.rs`.
- Role: `rust-ui-dev` only (pure UI).

## Revision notes

- Round 1: Fixed phantom references (`open_file_at` → `open_diagnostic`,
  removed nonexistent `tui-todo-panel.md` reference), corrected scan trigger
  (button click, not lazy), corrected command name (`ToggleTodoToolWindow`),
  removed unimplemented count badge claim, added security posture.
