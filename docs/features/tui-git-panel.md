# TUI Git Panel (T11)

## 1. Purpose

Ports `docs/features/git-support.md` (commit graph, side-by-side diff,
three-way conflict resolution) and `docs/features/diff-viewer-enhancements
.md`'s intraline (`DiffSpan`) highlighting to `ide-tui`, reusing
`ide_core::git`'s already-merged, already-`hacker`-reviewed public API
verbatim: `GitRepo`, `GitError`, `CommitNode`, `FileDiff`, `DiffHunk`,
`DiffLine`, `DiffSpan`, `ConflictSides`, `MAX_DIFF_LINES`,
`MAX_DIFF_FILES`. **Zero new `ide-core` API** — this is a
`crates/tui/**`-only diff, same shape as every prior `T`-item.

v1's scope matches `git-support.md` §1 exactly: local, read-mostly git
operations plus the one write path needed to make conflict resolution
useful (viewing a commit graph, side-by-side diffs, resolving conflicts).
Staging/unstaging, creating commits, and branch operations are still
`⏸`/`❌` in `docs/roadmap.md` even for `ide-ui` (tracked as `E1`) — there
is nothing to port for them yet.

### Scope cuts specific to porting

- **No toolbar view-mode toggle.** `ide-ui` replaces its center panel with
  `GitPanel`'s rendering via an "Editor"/"Source Control" toolbar button
  (`git-support.md` §2.2) — `ide-tui` has no toolbar at all. The Git Panel
  is instead a full-screen overlay, the same shape as the Problems panel
  and the Cargo panel (`tui-problems.md`, `tui-cargo-panel.md`), toggled by
  a new `ToggleGitPanel` command.
- **`ToggleGitPanel` has no default keybinding.** Grepping `ide-ui`'s own
  `crates/ui/src/command.rs` finds no entry for the view-mode toggle at
  all — it is toolbar-click-only in `ide-ui` today, never bound to a key
  in any JetBrains keymap reference this project tracks either (`docs/
  roadmap.md`'s keymap table binds the **VCS tool window** to `⌘9`, a
  different action — opening/focusing a tool window, not this v1 feature's
  narrower graph+diff+conflict view). Per `CLAUDE.md`'s "never invent a
  binding" rule, `ToggleGitPanel` is registered with `binding: None` —
  palette-only, joining `ToggleNotifications`/`ToggleCargoPanel` in that
  category, not translated from `⌘9` the way `ToggleProjectToolWindow` was
  translated from `⌘1` (there is no real per-project-JetBrains-IDE
  precedent to translate here, unlike that case).
- **No manual "Refresh."** `ide-ui`'s `GitPanel::refresh` reruns on its
  existing tree-refresh action so a `git init` run outside the app is
  picked up without restarting (`git-support.md` §3). `ide-tui` has **no**
  refresh/reload-project action of any kind (grepped `app.rs` — no such
  command exists at all, for the tree or anything else). `git.refresh()`
  therefore runs exactly once, in `App::new`, and never again for the
  lifetime of the process — a real, narrower behavior than `ide-ui`'s,
  not an oversight. A `git init` (or a merge started/finished) outside the
  running `ide-tui` process is not picked up without restarting it.
- **No free-hand editing of the conflict Result text.** `ide-ui`'s Result
  area is a plain editable text field the user can type into directly, in
  addition to the Accept Ours/Accept Theirs buttons (`git-support.md`
  §3). `ide-tui` has no general-purpose multi-line text-input widget
  outside the main editor `Buffer` type, and wiring the full editor
  keymap onto an ad hoc scratch string is out of scope for this batch.
  v1 of this port supports **Accept Ours** / **Accept Theirs** only, plus
  **Mark Resolved** on whichever of those two `result` currently holds
  (pre-seeded from "ours" by `select_conflict`, unchanged from `ide-ui`).
  There is no way in this port to feed a genuinely custom, hand-composed
  resolution into `resolve_conflict` — a user who needs one must resolve
  the conflict outside the app (edit the working-tree file's conflict
  markers directly with an external tool, then run `git add`) exactly as
  `git-support.md` §3 already prescribes for a *binary* conflict, just
  applied here to the "I need something other than ours/theirs verbatim"
  case too. This is a real, deliberate v1 gap versus `ide-ui`'s free-hand
  field, left for a future batch rather than solved by a workaround here.
- **No graph line-drawing.** `git-support.md` §3's lane assignment
  (`assign_lanes`, ported verbatim below) is a data-layer concern, not a
  rendering one — but `ide-ui` renders actual connector graphics between
  lanes (egui line-drawing). A terminal cell grid makes real branch-line
  ASCII art (`git log --graph`-style `│`/`╭`/`╮` connectors) a
  meaningfully bigger rendering feature on its own. v1 of this port
  indents each commit row by `2 * lane` spaces (`assign_lanes`'s existing
  output, unmodified) as a lightweight visual approximation, without
  drawing connector lines between rows.
- **Unified diff, not side-by-side columns.** `git-support.md` §3
  specifies two scrollable columns (old | new) kept in sync;
  `diff-viewer-enhancements.md` §3.2/§3.3 add per-side line-number
  gutters and a solid 3px "change bar" stripe on top of that — all three
  are pixel/column-budget decisions that don't carry over cleanly to an
  80-column terminal pane that's already split three ways (tree, editor,
  and now this overlay). v1 of this port renders a single unified column
  instead — `Context` lines plain, `Removed`/`Added` lines prefixed
  `- `/`+ ` and colored (`Color::Red`/`Color::Green`), the same
  convention plain `git diff` output itself uses — with no separate
  gutter column and no change-bar stripe; the colored prefix and text
  are the entire "this line changed" signal. Intraline spans (`DiffSpan`)
  still render distinctly within a colored line (`Modifier::REVERSED`),
  matching §3.4 below.
- **Not security-sensitive for this role.** `crates/core/src/git/**` is
  declared security-sensitive in `CLAUDE.md`, but this run's diff never
  touches it (zero new `ide-core` API, confirmed by `git diff --name-only`
  once implemented) — per `CLAUDE.md`'s own rule, `hacker` is skipped.
  `resolve_conflict`'s disk-write path is being driven by a genuinely new
  consumer for the first time, though (`ide-tui`'s `mark_resolved` call),
  so `rev`'s code-quality pass gives it the same extra scrutiny `T13`'s
  `apply_workspace_edit` got for the same reason.

## 2. Interface

### 2.1 `crates/tui/src/git_panel.rs` (new file)

Ported near-verbatim from `crates/ui/src/git_panel.rs` — that file has
**zero** `egui`/`eframe` dependency already (`use ide_core::{CommitNode,
ConflictSides, FileDiff, GitRepo};` is its only non-std import), so its
entire state-machine (not just the shape, the actual logic) carries over
unchanged. Only doc comments referencing `crates/ui`/`IdeApp` are updated
to `crates/tui`/`App`, and one small addition (`cancel_conflict`, needed
for the TUI's modal Esc handling — `ide-ui` never needed an explicit
"stop resolving without picking a conflict" action since its UI just lets
the user click a different list row).

```rust
pub struct ConflictResolutionState {
    pub path: PathBuf,
    pub sides: ConflictSides,
    pub result: String,
}

pub const COMMIT_GRAPH_LIMIT: usize = 500; // matches git-support.md §3

#[derive(Default)]
pub struct GitPanel {
    pub graph: Vec<CommitNode>,
    pub selected_commit: Option<String>,
    pub diff: Option<Vec<FileDiff>>,
    pub conflicts: Vec<PathBuf>,
    pub active_conflict: Option<ConflictResolutionState>,
    pub binary_conflict: Option<PathBuf>,
    pub current_branch: Option<String>,
    // repo: Option<GitRepo> stays private, as in ide-ui's version.
}

impl GitPanel {
    pub fn is_repo(&self) -> bool;
    pub fn refresh(&mut self, project_root: &Path);
    pub fn select_commit(&mut self, commit_id: &str);
    pub fn show_working_tree_diff(&mut self, absolute_path: &Path);
    pub fn select_conflict(&mut self, path: &Path);
    pub fn accept_ours(&mut self);
    pub fn accept_theirs(&mut self);
    /// New (not in `ide-ui`): clears `active_conflict`/`binary_conflict`
    /// without touching anything else -- `handle_git_panel_key`'s Esc
    /// while resolving.
    pub fn cancel_conflict(&mut self);
    pub fn mark_resolved(&mut self) -> Result<(), String>;
}

pub fn assign_lanes(graph: &[CommitNode]) -> HashMap<String, usize>;
```

Every method's behavior, error handling, and path-provenance discipline
(§3/§4 of `git-support.md`) is exactly as documented there — this doc
does not repeat it; see that doc for `refresh`/`select_commit`/
`show_working_tree_diff`/`select_conflict`/`accept_ours`/`accept_theirs`/
`mark_resolved`/`assign_lanes`'s full behavior.

### 2.2 `crates/tui/src/app.rs`

```rust
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum GitPanelFocus {
    #[default]
    Graph,
    Conflicts,
    Diff,
}

impl GitPanelFocus {
    /// `Tab`'s cycle order. Skips `Conflicts` when there are none to
    /// browse -- mirrors `handle_code_actions_key`'s existing "nothing to
    /// select" guards elsewhere in this file, applied to focus cycling
    /// instead of list movement.
    fn next(self, conflicts_empty: bool) -> Self;
}

#[derive(Default)]
pub(crate) struct GitPanelState {
    focus: GitPanelFocus,
    graph_selected: usize,
    conflicts_selected: usize,
    diff_scroll: u16,
}
```

`App` gains:

```rust
pub(crate) git: GitPanel,               // always alive, refreshed once in `App::new`
pub(crate) git_panel: Option<GitPanelState>, // presence = overlay open
last_git_diff_target: Option<PathBuf>,  // sync_git_working_tree_diff's guard, mirrors last_code_actions_target
```

`App::new` calls `git.refresh(project.root())` once, after `scan_tree`,
before constructing `Self` (needs `project.root()`, already in scope
there).

New methods (placed after `sync_code_actions`, before `handle_goto_key`,
same neighborhood `T13`'s additions used):

```rust
pub(crate) fn sync_git_working_tree_diff(&mut self); // §3.1
fn toggle_git_panel(&mut self);                       // `ToggleGitPanel`'s entry point
fn handle_git_panel_key(&mut self, key: KeyEvent) -> LoopSignal; // §3.2
```

`close_all_overlays` gains `self.git_panel = None;` (a tenth arm — it
does **not** reset `self.git`'s own fields; that state persists across
toggle, matching `ide-ui`'s toolbar-toggle persistence). `handle_key`
gains a `self.git_panel.is_some()` check, appended after the
`pending_rename_preview` check (same position every `T13` addition used
relative to the modal-priority chain). `run_action` gains `Action::
ToggleGitPanel => self.toggle_git_panel(),`.

### 2.3 `crates/tui/src/commands.rs`

One new `Action`/`Command` entry:

```rust
Command {
    id: "ToggleGitPanel",
    title: "Git",
    binding: None, // palette-only -- see §1's "no default keybinding"
    action: Action::ToggleGitPanel,
}
```

### 2.4 `crates/tui/src/ui.rs`

One new render function, `render_git_panel(frame, app, area)`, dispatched
from `render()` alongside the other `Option`/`bool`-gated overlays:

```rust
if app.git_panel.is_some() {
    render_git_panel(frame, app, size);
}
```

Pure rendering — exempt from the coverage floor, per this crate's
established convention.

## 3. Behaviour

### 3.1 Repository detection and working-tree diff sync

- `App::new` opens the repository (or not) exactly once via `git.refresh`.
  `git.is_repo()` false for the rest of the process's life if the project
  root wasn't (inside) a repository at startup — see §1's "no manual
  refresh" scope cut.
- `sync_git_working_tree_diff` is called once per frame from `lib.rs`'s
  run loop, unconditionally (same ambient shape as `sync_code_actions`/
  `sync_document_highlights`), but is cheap to no-op: it returns
  immediately if `git_panel` is closed or if `git.selected_commit` is
  `Some` (an explicit commit selection wins over the ambient working-tree
  diff, per `git-support.md` §3). Otherwise it compares the active tab's
  path against `last_git_diff_target`; on a change, it calls `git.
  show_working_tree_diff(path)` (or clears `git.diff` directly if there's
  no active tab) and updates `last_git_diff_target` — the same
  changed-since-last-frame guard `sync_code_actions` already established,
  applied here so an open Git Panel doesn't re-run a `git2` diff every
  single frame while the caret sits still.

### 3.2 Git Panel overlay: focus, navigation, conflict resolution

- `ToggleGitPanel` (palette-only) opens/closes the overlay via
  `toggle_git_panel`, which — like `toggle_problems`/`toggle_cargo_panel`
  — calls `close_all_overlays()` first, then sets `git_panel = Some
  (GitPanelState::default())` only when it was opening (closing just
  leaves it `None`).
- While open, `Tab` cycles focus `Graph → Conflicts → Diff → Graph`
  (skipping `Conflicts` when `git.conflicts` is empty). `Up`/`Down` move
  the selected row in whichever list has focus (`Graph`/`Conflicts`,
  clamped to length) or scroll the diff pane by one line (`Diff` focus);
  `PageUp`/`PageDown` scroll the diff pane by ten lines, only meaningful
  while `Diff` is focused.
- `Enter` on `Graph` focus calls `git.select_commit` for the highlighted
  row, resets `diff_scroll` to `0`, and switches focus to `Diff` — so the
  freshly-loaded diff is immediately what `Up`/`Down` scrolls, without a
  second keypress.
- `Enter` on `Conflicts` focus calls `git.select_conflict` for the
  highlighted row. This does **not** change `GitPanelFocus` — conflict
  resolution is a distinct mode layered on top, detected by `git.
  active_conflict.is_some() || git.binary_conflict.is_some()` (see next
  bullet), not a fourth `GitPanelFocus` variant, since the underlying
  focus (which list/pane was active before resolving) is what Esc-while-
  resolving needs to return to.
- **While resolving** (`active_conflict` or `binary_conflict` is `Some`),
  every key is intercepted before the `Tab`/`Up`/`Down`/`Enter` handling
  above: `o` calls `accept_ours`, `t` calls `accept_theirs`, `Enter`
  calls `mark_resolved` (surfacing its `Err` to `self.status`, same
  pattern `confirm_rename` uses for its own fallible call), `Esc` calls
  the new `cancel_conflict` (clears both fields, returns to ordinary
  `Conflicts`-focus browsing — the overlay itself stays open). A
  `binary_conflict` offers only `Esc` in practice (`o`/`t`/`Enter` are
  still dispatched but are no-ops against `None`/absent state, matching
  `git-support.md` §3's "no Result/Mark-Resolved UI is offered for it").
- `Esc` while **not** resolving closes the whole overlay
  (`git_panel = None`) — the outer `handle_key` dispatch never reaches
  this far while resolving, since the inner branch above returns first.

## 4. Constraints & invariants

- Zero new `ide-core`/`ide-lsp` public API (§1) — every type this doc
  names is already merged and already covered by `git-support.md`'s own
  `hacker` passes.
- `git.refresh` runs exactly once (§1/§3.1) — this is a real, narrower
  guarantee than `ide-ui`'s re-attempted-on-refresh behavior, not a bug.
- Free-hand Result editing is out of scope for this port (§1) — `Mark
  Resolved` only ever writes `ConflictResolutionState::result`, which only
  `accept_ours`/`accept_theirs` (and the pre-seed in `select_conflict`)
  ever set. This is a real capability gap versus `ide-ui`, not a full
  substitute — noted for a future batch, not resolved here.
- `assign_lanes` is unmodified from `ide-ui`'s version — same inputs,
  same outputs, same tests. The TUI's renderer only reads the `usize`
  lane index for indentation; it draws no connector lines (§1).
- `sync_git_working_tree_diff`'s guard (§3.1) means the diff pane can lag
  by up to one frame behind a caret-triggered tab switch — same latency
  class every other `sync_*` ambient refresh in this crate already
  accepts (e.g. `sync_code_actions`, `sync_document_highlights`).
- `handle_git_panel_key`'s resolving-mode interception (§3.2) must be
  checked first, before the ordinary `Tab`/`Up`/`Down`/`Enter` match —
  getting this order backwards would let `o`/`t`/an unrelated `Enter`
  leak into list navigation while a conflict is being resolved, or vice
  versa let arrow keys move the background list selection while the user
  thinks they're resolving a conflict.
- Not security-sensitive for this role (§1) — no `hacker` pass expected;
  confirm via `git diff --name-only` against `main` once implemented
  that the diff really does stay `crates/tui/**`-only.

## 5. Examples

**Opening the panel and browsing a commit's diff:**

```text
Ctrl+Shift+A → type "Git" → Enter   -- opens the overlay (palette-only, §1)
Down, Down                           -- move the Graph selection
Enter                                -- loads that commit's diff, focus -> Diff
Down, Down, Down                     -- scroll the diff pane
Esc                                  -- closes the overlay
```

**Resolving a conflict, accepting "theirs":**

```text
Tab                    -- Graph -> Conflicts (skipped if none)
Down                    -- pick a conflicted path
Enter                   -- loads its three sides, enters resolving mode
t                       -- Result := sides.theirs
Enter                   -- mark_resolved(); path drops out of `conflicts`
```

## 6. Dependencies & integration points

- No new dependency — `git2` is already `ide-core`'s (not `ide-tui`'s)
  dependency; `ide-tui` only ever calls `ide_core::git`'s public API.
- `lib.rs`'s run loop gains exactly one new call, `sync_git_working_tree_
  diff`, alongside `sync_document_highlights`/`sync_code_actions`/
  `poll_cargo`/`poll_search`.
- Does not touch `ide-lsp` — no language-server interaction in this
  feature, same as `git-support.md` itself.

## Revision notes

- Found during implementation: the first draft's §1 scope-cut bullet
  described keeping `diff-viewer-enhancements.md`'s per-side line-number
  gutter columns (just dropping the pixel-level change-bar stripe).
  Building the actual diff pane made clear that a `Table`-style two-
  column-plus-gutters layout doesn't fit an 80-column terminal already
  split three ways (tree, editor, this overlay) nearly as well as it fits
  a resizable `egui` window. Switched to a single unified column (`- `/
  `+ `-prefixed, colored lines, matching plain `git diff` convention) —
  simpler than originally planned, not a missed requirement. §1 and §2.4
  updated to describe the unified view actually built.
