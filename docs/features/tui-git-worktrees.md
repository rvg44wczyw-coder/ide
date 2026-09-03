# TUI Git Worktrees (T32)

## 1. Purpose

Ports the list/add/remove half of `git-worktrees.md` (E8) to `ide-tui`'s
Git Panel: view every linked worktree of the open repository, create a new
one (from an existing branch or a fresh one), and remove one, all from
inside the running terminal session, using the `GitRepo::worktrees`/
`add_worktree`/`remove_worktree` API that phase already added to
`ide-core` (zero new `ide-core` work needed here — this is a pure
`ide-tui` port, the same shape T28/T29/T30/T31 already are).

**Scope is narrower than `ide-ui`'s own feature**, and deliberately so —
not a v1 trim to revisit, but the exact subset `git-worktrees.md` §1
itself names as the one worth a future TUI port ("list/add/remove
worktrees only, almost certainly, dropping the window-management
two-thirds of that doc entirely"):

- **"Switch here"** — `ide-ui`'s button calls its existing `open_project`
  to reload `self.project` against the worktree's path *while the same
  process keeps running*. `ide-tui` has no such capability at all: `App`
  reads `project_root` once in `App::new` and nothing in this crate ever
  reassigns it afterward (confirmed by reading `App::new` and grepping for
  writes to `project_root` — there are none outside construction). Adding
  "reload everything against a new root" is a real architectural feature
  in its own right, not a one-line addition this phase can absorb as a
  side effect of listing worktrees.
- **"Open in New Window"** — `ide-ui`'s version spawns a second `ide`
  process pointed at the worktree path, which inherits a new OS window for
  free from the GUI toolkit. A TUI has no window to inherit: `ide-tui`
  occupies the *current* terminal via raw mode / the alternate screen
  (`lib.rs`'s setup), and spawning "a new terminal window" from inside one
  has no portable, GUI-toolkit-provided equivalent — it would mean
  shelling out to a platform-specific terminal emulator (`Terminal.app`,
  `gnome-terminal`, `wt.exe`, ...), a different and much larger feature
  than this one, with no single obvious choice and no JetBrains-terminal
  precedent to copy.

What's left is exactly a worktree *lifecycle-management* tool bolted onto
the Git Panel: see what worktrees exist on disk, clean up ones that are
done, add a new one for parallel work — never a way to move between them
from inside `ide-tui` itself (the existing "open a different directory"
answer for that remains: quit and re-launch `ide --tui <path>`, unchanged
by this phase).

## 2. Interface

### 2.1 `crates/tui/src/git_panel.rs` — extended, not replaced

Same precedent branches/log-filter/blame already established: a new
popup-state struct plus methods on the existing `GitPanel`, not a sibling
module.

```rust
use ide_core::WorktreeInfo; // already re-exported alongside BranchInfo etc.

/// The worktrees popup's own transient UI state (`docs/features/
/// git-worktrees.md` §2.2.1's `WorktreesPopupState`, adapted for
/// keyboard-only interaction).
#[derive(Default)]
pub struct WorktreesPopupState {
    pub open: bool,
    pub worktrees: Vec<WorktreeInfo>,
    /// Row navigation. `ide-ui`'s own `WorktreesPopupState` has no such
    /// field -- each row carries its own click targets there. `ide-tui`
    /// is keyboard-only, so it needs an index the same way
    /// `branches_popup.selected` already does.
    pub selected: usize,
    /// `true` while the "Add worktree" form has focus (§2.2's `n` key) --
    /// `ide-tui`-specific: `ide-ui`'s three fields are always-visible
    /// inline text boxes with no modal "now typing" state to track.
    pub adding: bool,
    pub add_field: WorktreeAddField,
    pub new_name: String,
    pub new_path: String,
    /// Empty means "create a new branch named `new_name`" (`GitRepo::
    /// add_worktree`'s own `branch: None` case) -- surfaced as placeholder
    /// text while `add_field == Branch` and the field is still empty
    /// (§2.4), not a separate checkbox, exactly like `ide-ui`'s own form.
    pub new_branch: String,
    pub error: Option<String>,
    /// Set when a plain (`force: false`) `remove_worktree` call fails with
    /// `WorktreeHasUncommittedChanges` or `WorktreeLocked`, so the popup
    /// can offer a force-confirm instead of just showing the error --
    /// same two-step pattern `branches_popup.pending_delete` already uses.
    pub pending_force_remove: Option<String>,
}

/// Which of the "Add worktree" form's three fields is currently being
/// typed into. `Tab`/`Shift+Tab` cycle through all three in this order
/// (wrapping); this is a 3-way generalization of `DebugConfigField`'s
/// existing 2-way toggle (`tui-debugger.md` §2.5) -- the same shape, one
/// more variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorktreeAddField {
    #[default]
    Name,
    Path,
    Branch,
}

impl WorktreeAddField {
    fn next(self) -> Self {
        match self {
            Self::Name => Self::Path,
            Self::Path => Self::Branch,
            Self::Branch => Self::Name,
        }
    }
    fn prev(self) -> Self {
        match self {
            Self::Name => Self::Branch,
            Self::Path => Self::Name,
            Self::Branch => Self::Path,
        }
    }
}
```

`GitPanel` gains a `worktrees_popup: WorktreesPopupState` field and four
methods, near-verbatim ports of `ide-ui`'s own (`crates/ui/src/
git_panel.rs:630-718`) with no behavioural change beyond what §1 already
cut:

```rust
impl GitPanel {
    /// Opens the popup: resets `worktrees_popup` to a fresh (but `open:
    /// true`) default *before* loading, then calls `refresh_worktrees` --
    /// this order matters here specifically because (unlike
    /// `open_branches_popup`, where the loaded list lives in a
    /// `GitPanel`-level field the popup-state reset never touches)
    /// `worktrees_popup.worktrees` lives *inside* the struct being reset,
    /// so resetting after loading would silently discard what was just
    /// loaded.
    pub fn open_worktrees_popup(&mut self, project_root: &Path);

    pub fn close_worktrees_popup(&mut self);

    /// Calls `GitRepo::worktrees`, populating `worktrees_popup.worktrees`
    /// on success or `worktrees_popup.error` on failure. Not eagerly
    /// called by `refresh()` itself -- same lazy-load reasoning
    /// `reload_branches` already documents.
    pub fn refresh_worktrees(&mut self);

    /// Creates a worktree from the popup's own `new_name`/`new_path`/
    /// `new_branch` fields (empty `new_branch` becomes `None`). On
    /// success, clears the form, exits `adding` mode, and refreshes the
    /// list; on failure, sets `error` and leaves the form fields as-is so
    /// the user can fix and retry rather than retype. Doesn't close the
    /// whole popup either way -- unlike `create_branch`, adding a worktree
    /// isn't a context switch.
    pub fn create_worktree(&mut self);

    /// Removes `name`. On a `WorktreeHasUncommittedChanges` or
    /// `WorktreeLocked` failure with `force: false`, sets
    /// `pending_force_remove` *instead of* `error` -- the popup's confirm
    /// step uses a fixed message, not the raw error text, same two-step
    /// pattern `confirm_delete_branch` already uses for `BranchNotMerged`.
    /// Any other failure (including a retry with `force: true` that still
    /// fails) surfaces as `error`.
    pub fn remove_worktree(&mut self, name: &str, force: bool);
}
```

Both `create_worktree`/`remove_worktree` write directly into
`worktrees_popup.error` rather than returning `Result<(), String>` the way
`checkout_branch`/`create_branch`/`confirm_delete_branch` do (bubbling up
to `App::status` at the call site) — a deliberate exception to *this
file's own* branches-popup convention, in favor of matching `ide-ui`'s
shape exactly (§1's "near-verbatim port" framing) and `LogFilterState`'s
own precedent of an inline, popup-local `error` field for a failure that's
specific to that popup's own form rather than a general operation like a
checkout.

### 2.2 `crates/tui/src/app.rs`

New `Action::GitWorktrees` arm and a `trigger_git_worktrees` method,
same shape as `trigger_git_branches`:

```rust
fn trigger_git_worktrees(&mut self) {
    if self.git_panel.is_none() {
        self.toggle_git_panel();
    }
    self.git.open_worktrees_popup(&self.project_root);
}
```

`handle_git_panel_key`'s existing precedence chain (`tui-git-staging-
branches-and-log-filters.md` §3.2) gains three more checks, inserted
right after the existing branch-popup checks (items 2-4 of that doc's
list) and before the conflict-resolution check — worktrees and branches
popups are both modal sub-popups of the Git Panel and can never be open
at the same time, so their relative order doesn't change behaviour, but
keeping them adjacent keeps the chain's own doc comment readable:

1. `worktrees_popup.pending_force_remove` — confirm/cancel a force
   remove.
2. `worktrees_popup.adding` — typing into the Add-worktree form.
3. `worktrees_popup.open` — the worktrees popup's normal navigation.

```rust
fn handle_git_worktrees_key(&mut self, key: KeyEvent) -> LoopSignal {
    match key.code {
        KeyCode::Esc => self.git.close_worktrees_popup(),
        KeyCode::Up => {
            self.git.worktrees_popup.selected =
                self.git.worktrees_popup.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            let count = self.git.worktrees_popup.worktrees.len();
            if self.git.worktrees_popup.selected + 1 < count {
                self.git.worktrees_popup.selected += 1;
            }
        }
        KeyCode::Char('r') => {
            // Remove the selected row (`false` -- the safe attempt).
        }
        KeyCode::Char('n') => {
            // worktrees_popup.adding = true; add_field = Name.
        }
        _ => {}
    }
    LoopSignal::Continue
}

fn handle_git_worktree_add_key(&mut self, key: KeyEvent) -> LoopSignal {
    match key.code {
        KeyCode::Esc => { /* cancel: adding = false, clear all three fields */ }
        KeyCode::Tab => { /* add_field = add_field.next() */ }
        KeyCode::BackTab => { /* add_field = add_field.prev() */ }
        KeyCode::Enter => self.git.create_worktree(),
        KeyCode::Backspace => { /* pop from whichever field is focused */ }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            // push c onto whichever field is focused
        }
        _ => {}
    }
    LoopSignal::Continue
}

fn handle_git_worktree_remove_confirm_key(&mut self, key: KeyEvent) -> LoopSignal {
    match key.code {
        KeyCode::Esc => self.git.worktrees_popup.pending_force_remove = None,
        KeyCode::Char('r') => {
            // remove_worktree(&pending_name, true)
        }
        _ => {}
    }
    LoopSignal::Continue
}
```

(Full match-arm bodies are the implementing role's to fill in exactly —
the shape above, mirroring `handle_git_branches_key`/
`handle_debug_adapter_config_key`'s existing patterns byte-for-byte, is
what's being specified; there is no remaining design decision left for
the implementation to make.)

No new field on `App` itself — every bit of new state lives in
`self.git.worktrees_popup`, same as `branches_popup`. `any_popup_open`
needs no new arm either: `worktrees_popup.open` can only ever be `true`
while `git_panel.is_some()` (set by `trigger_git_worktrees` in that
order), exactly the same transitive coverage `branches_popup.open`
already relies on.

### 2.3 `crates/tui/src/commands.rs`

One new `Action` variant and command entry, directly beside
`GitBranches`:

```rust
Command {
    id: "GitWorktrees",
    title: "Git Worktrees...",
    // Palette-only -- this action has no JetBrains-IDE precedent to copy
    // a binding from (`git-worktrees.md` §2.2.2 already establishes this
    // for the GUI side; nothing about the TUI changes that reasoning).
    binding: None,
    action: Action::GitWorktrees,
},
```

### 2.4 `crates/tui/src/ui.rs`

`render_git_panel`'s existing popup-dispatch (which already draws
`branches_popup`'s centered popup over whichever view is active
underneath) gains a matching `worktrees_popup.open` branch, same
"small fixed-size popup" shape `render_git_branches_popup`/
`render_debug_adapter_config_popup` already use:

- **Normal mode** (`!adding`): one row per `WorktreeInfo` — name, branch
  (or `"(detached / unavailable)"` for `None`), path, and a `"[locked]"`
  suffix when `is_locked`; the selected row reverse-styled, same as every
  other list-popup in this file. A row whose name matches
  `pending_force_remove` appends `"  (has uncommitted changes or is
  locked -- press r again to force remove)"`, mirroring the branches
  popup's own "press d again to force delete" inline text. Title:
  `"Worktrees  (r: remove, n: add, Esc: close)"`. Empty list renders a
  single `"No worktrees."` row, matching the branches popup's own
  `"No branches match."` empty-state convention.
- **Add mode** (`adding`): the list is replaced by the three-field form
  (same full-replace shape `render_debug_adapter_config_popup` already
  uses for its two fields), each row `"{marker} {label}: {value}"` with a
  `>` marker on the focused field; the `Branch` row's value is
  `"(new branch named <new_name>)"` as placeholder-style text when
  `new_branch` is empty and `new_name` isn't (making the *effect* of
  leaving the field blank visible, not just documented in a tooltip
  nothing here has room for). Title: `"Add Worktree  (Tab: next field,
  Enter: create, Esc: cancel)"`.
- `error`, if set, renders as an extra row below either mode's content
  (same "inline red text" treatment `render_git_branch_line_row`'s filter
  bar already gives `log_filter.error`).

Pure rendering — exempt from the coverage floor, per this crate's
established convention.

## 3. Behaviour & edge cases

- `worktrees()` never fails just because one entry is broken (`ide-core`'s
  own guarantee, unchanged by this doc) — a broken entry still needs to
  render so `r` can remove it.
- `add_worktree` with an empty `new_branch` field creates a **new** branch
  named `new_name` (`GitRepo::add_worktree`'s own `branch: None` default)
  — the Add form's placeholder text on the `Branch` row (§2.4) is the
  mechanism for surfacing that, matching `ide-ui`'s own placeholder-text
  approach rather than a separate explanation block neither frontend has
  room for.
- `create_worktree`/`remove_worktree` never touch `App::status` — every
  failure renders inline via `worktrees_popup.error`, so a worktree
  operation failing never displaces whatever unrelated status message the
  status bar was already showing (a real, if narrow, improvement over the
  branches popup's own `self.status = Some(e)` side effect, which *does*
  clobber it — not fixed here since that's out of this phase's scope, but
  worth noting for anyone comparing the two popups).
- Pressing `r` on an empty worktrees list, or with `selected` pointing
  past the end after the list shrank (a remove just succeeded), is a
  silent no-op — same `.get(selected)` guard shape every other indexed
  popup list in this file already uses.
- `Esc` while `adding` cancels the form (clearing all three fields)
  without creating anything, and returns to the worktrees list — it does
  **not** close the whole popup, matching `show_new_branch_input`'s own
  `Esc` behaviour in the branches popup.
- `Esc` while `pending_force_remove.is_some()` cancels the confirm without
  removing anything and returns to the worktrees list (not the whole
  popup closing either) — same as `branches_popup.pending_delete`'s own
  `Esc`.

## 4. Constraints & invariants

- No new `ide-core` API — `WorktreeInfo`/`GitRepo::worktrees`/
  `add_worktree`/`remove_worktree`/the `WorktreeNameTaken`/
  `InvalidWorktreeName`/`WorktreeInsideRepo`/`WorktreeHasUncommittedChanges`/
  `WorktreeLocked` `GitError` variants are all already merged
  (`git-worktrees.md`, GUI-only phase) and used here exactly as `ide-ui`
  already uses them.
- `open_worktrees_popup` must reset `worktrees_popup` **before** calling
  `refresh_worktrees`, never after (§2.1's own note) — this is the one
  place in this doc where getting the order backwards silently breaks the
  feature (an always-empty list) rather than producing a visibly wrong
  but obviously-broken result.
- Neither "Switch here" nor "Open in New Window" gets a keybinding, a
  menu entry, or any code path in this phase — see §1. If a future phase
  ever adds either (e.g. once `ide-tui` grows a project-reload capability
  for unrelated reasons), it is new scope, not something this doc left a
  hook for.
- This is not a security-sensitive path per `CLAUDE.md`'s declared list:
  `crates/tui/src/git_panel.rs` is already on it (covered by the existing
  "parses a repository's on-disk data" entry for `crates/core/src/git/
  **`, which this phase's TUI-side code doesn't touch), and the
  `add_worktree`/`remove_worktree` path-validation logic itself lives
  entirely in already-`hacker`-reviewed `ide-core` code
  (`git-worktrees.md` §6). This phase adds no new call into that
  validation beyond what `ide-ui` already exercises identically — a
  `hacker` pass is not expected for this run.

## 5. Examples

```
1. User runs "Git Worktrees..." from the command palette.
   -> trigger_git_worktrees opens the Git Panel (if not already open) and
      calls open_worktrees_popup, which loads the current worktree list.
2. User presses `n`.
   -> The popup switches to the Add form, focused on Name.
3. User types "feature-x", presses Tab, types "/path/to/feature-x", then
   presses Enter (leaving Branch empty).
   -> create_worktree calls add_worktree("feature-x", "/path/to/feature-x",
      None), which creates a brand-new branch named "feature-x" checked
      out in the new worktree. On success, the form clears, `adding`
      becomes false, and the list refreshes to include the new row.
4. User presses `r` on a worktree with uncommitted changes.
   -> remove_worktree(name, false) fails with
      WorktreeHasUncommittedChanges; pending_force_remove is set. The row
      now shows "press r again to force remove."
5. User presses `r` again.
   -> remove_worktree(name, true) succeeds; the row disappears from the
      refreshed list.
```

## 6. Dependencies & integration points

- `crates/tui/src/git_panel.rs`: new `WorktreesPopupState`/
  `WorktreeAddField`, new `worktrees_popup` field on `GitPanel`, and the
  four new methods in §2.1.
- `crates/tui/src/app.rs`: new `trigger_git_worktrees`/
  `handle_git_worktrees_key`/`handle_git_worktree_add_key`/
  `handle_git_worktree_remove_confirm_key` methods, three new arms in
  `handle_git_panel_key`'s precedence chain, one new `run_action` arm.
- `crates/tui/src/commands.rs`: one new `Action` variant and command
  entry (§2.3).
- `crates/tui/src/ui.rs`: `render_git_panel`'s popup dispatch gains a
  `worktrees_popup` branch (§2.4).
- No `ide-core`/`ide-lsp`/`ide-dap` changes (§4).
- Not security-sensitive; `hacker` is skipped for this run per §4's own
  reasoning and the `dev-chain` skill's rule.
