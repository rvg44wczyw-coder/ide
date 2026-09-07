# E3 — Git Log Viewer (GUI)

## §1 Purpose

A bottom-tool-window tab showing the project's git commit history with
filters, search, commit details, and per-file diff view. Builds on existing
core methods (`commit_graph`, `commit_detail`, `diff_commit`, `file_history`).

### §1.1 Scope (v1)

- Commit list with hash, summary, author, date.
- Filter bar: author, path, message query inputs.
- Click a commit → show detail (full message) + file diff list.
- Click a file in diff list → open diff view (reuse git panel's side-by-side).
- Manual refresh button.
- No `hacker` required — read-only git operations, core validates paths.

## §2 Interface / API

### §2.1 `LogViewerPanel` (crates/ui/src/log_viewer.rs)

```rust
pub struct LogViewerPanel {
    pub commits: Vec<CommitNode>,
    pub selected: Option<usize>,
    pub detail: Option<CommitDetail>,
    pub diff: Option<Vec<FileDiff>>,
    pub filter: CommitLogFilter,
    pub loading: bool,
    generation: u64,
    rx: Option<Receiver<(u64, Vec<CommitNode>)>>,
    repo_root: Option<PathBuf>,
}
```

### §2.2 Methods

- `run(root: PathBuf, filter: CommitLogFilter)` — spawn background
  `GitRepo::open(&root)?.commit_graph(limit, &filter)` call.
- `poll() -> bool` — drain channel, update commits. Returns true if changed.
- `select_commit(index: usize)` — load detail + diff via
  `repo.commit_detail(&id)` and `repo.diff_commit(&id)`.
- `refresh()` — re-run with current filter.
- `has_repo() -> bool` — whether a project root is known.

### §2.3 Command

`ShowLogPanel` — toggles bottom tool window + selects Log tab. No default
binding. Registered in `command.rs` and `app/menu.rs`.

## §3 Behaviour

### §3.1 Bottom tool window

`BottomView` gains `Log` variant. Tab shows "Log" in the bottom stripe.

### §3.2 Panel layout

```
┌─────────────────────────────────────────────┐
│ Author: [____] Path: [____] Query: [____]  │
│ [Refresh]  ● loading...  123 commits       │
├─────────────────────────────────────────────┤
│ a1b2c3d Fix parser bug     Alice  2h ago   │
│ d4e5f6g Add tests           Bob    1d ago  │
│ h7i8j9k Initial commit      Alice  3d ago  │
├─────────────────────────────────────────────┤
│ Commit: a1b2c3d                            │
│ Author: Alice <alice@example.com>          │
│ Date: 2026-09-07                           │
│                                             │
│ Fix parser bug                              │
│                                             │
│ Files changed:                              │
│   M src/parser.rs                           │
│   A tests/parser_test.rs                    │
└─────────────────────────────────────────────┘
```

### §3.3 Interactions

- Type in filter inputs → press Enter or click Refresh → re-run with filter.
- Click a commit row → load detail + diff below.
- Click a file in diff list → open via `open_diagnostic` at the file path.

## §4 Constraints and invariants

- Off-thread: `run` spawns a thread, `poll` drains once per frame.
- Generation counter: stale results dropped.
- `LOG_LIMIT = 500` max commits (matches `COMMIT_GRAPH_LIMIT` in git_panel).
- Filter inputs are `String` fields, converted to `CommitLogFilter` at
  run time.

## §5 Tests

- `LogViewerPanel::run_while_loading_is_noop`
- `LogViewerPanel::poll_with_nothing_returns_false`
- `LogViewerPanel::run_and_poll_on_real_repo`

## §6 Dependencies & integration points

- No new crate dependencies.
- `mod log_viewer` added to `lib.rs`.
- `BottomView::Log` added to `app.rs`.
- `ShowLogPanel` added to `command.rs` + `app/menu.rs`.
- `log_viewer: LogViewerPanel` field added to `IdeApp`.
- `render_log_viewer` method added to `app/render.rs`.
- Role: `rust-ui-dev` only (pure UI, core methods already exist).

## Revision notes

- Round 1: Initial doc.
