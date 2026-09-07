# G5 — TODO Panel (GUI)

## §1 Overview

A bottom-tool-window tab that searches the project tree for `TODO`, `FIXME`,
and `HACK` comments and displays them in a scrollable list. Clicking an entry
opens the file at the matching line.

### §1.1 Scope (v1)

- Literal patterns only: `TODO`, `FIXME`, `HACK` — same set as the TUI version
  (`tui-todo-panel.md` §1.1).
- No user-configurable patterns (needs a settings UI that doesn't exist yet).
- Off-thread scan via `ide_core::search_tree`, same pattern as
  `search_panel.rs` / `files_search.rs`.
- Results sorted by `(path, line, column)`.
- Truncation indicator when `MAX_SEARCH_RESULTS` is hit.

## §2 Data Model

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
version (`tui-todo-panel.md` §2.2/§2.3).

## §3 UI Integration

### §3.1 Bottom tool window

`BottomView` gets a new variant `Todo`. The bottom tool-window stripe gains a
"TODO" tab, rendered next to Problems / Cargo / Usages / Search.

The tab shows the count badge when matches exist (same pattern as the Problems
badge).

### §3.2 Panel rendering

`render_todo_panel` renders a scrollable list of matches. Each row shows:
- Pattern label (`TODO` / `FIXME` / `HACK`) in a category color
- File path (relative to project root)
- Line content (truncated to fit)

Clicking a row calls `app.open_file_at(path, position)` — same as Problems
panel click handling.

### §3.3 Scan trigger

The scan runs once when the TODO tab is first selected (lazy). Subsequent
tab switches show cached results without re-scanning. A manual refresh
button re-triggers the scan.

## §4 Command

`ShowTodoPanel` — toggles the bottom tool window open and selects the TODO tab.
Default binding: none (reachable from command palette).

## §5 Tests

- `TodoPanel::run_and_poll_merges_matches` — port from TUI tests
- `TodoPanel::run_while_searching_is_noop`
- `TodoPanel::poll_drops_stale_generation`
- `render_todo_panel_shows_matches` — egui snapshot or state assertion
