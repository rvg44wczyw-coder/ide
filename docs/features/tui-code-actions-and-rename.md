# Code Actions + Rename in `ide-tui` (T13)

## 1. Purpose

Ports `code-actions.md` (A8) and `rename-refactoring.md` (D1) to `ide-tui`:
`⌥Enter`-equivalent "Show Intention Actions" and `⇧F6`-equivalent Rename,
both built entirely on `ide-lsp`'s already-merged `CodeAction`/
`WorkspaceEdit`/`PrepareRename`/`Rename` wire types and `ide-core`'s
already-merged `workspace_edit::apply_workspace_edit_to_disk` — no new
`ide-lsp` or `ide-core` surface, exactly like every prior `ide-ui`-parity
phase this backlog has ported (`tui-hover-and-inlay-hints.md`,
`tui-find-in-path.md`). This is the only remaining consumer of
`ide_lsp::{CodeAction, WorkspaceEdit, PrepareRename, Rename}` outside
`ide-ui`.

- **Show Intention Actions** (`Alt+Enter`) — opens a popup listing every
  code action available at the caret, ambiently kept fresh the same way
  `tui-hover-and-inlay-hints.md`'s document-highlight sync already is.
  `Enter` applies the selected action; `Esc` closes without applying.
- **Rename** (`Shift+F6`) — opens a small editable popup pre-filled with
  the identifier under the caret. `Enter` confirms and sends the real
  rename request; `Esc` cancels. A same-file-only result applies
  immediately; a multi-file result opens a read-only preview listing every
  affected file, gated on `Enter` (apply) / `Esc` (cancel).

**Deliberately not re-litigated here** — every scope cut
`code-actions.md` §1 and `rename-refactoring.md` §1 already made
(`workspace/executeCommand`, resource operations, `CodeActionKind`
filtering, `context.diagnostics`, selection-range-aware requests,
document-version checking, live per-keystroke rename, `PrepareRename`'s
`range`/`placeholder` fields, an explicit preview toggle, renaming a file,
renaming from any entry point but the caret) carries over unchanged — this
phase reuses `ide-lsp`'s wire behavior verbatim, it doesn't re-decide any
of it.

**New scope cuts specific to porting into this crate** (`ide-ui` has no
equivalent gap for these — ported deliberately narrower, not by oversight):

- **No gutter lightbulb.** `render_editor` (`ui.rs`) has no line-number/
  gutter column at all — confirmed by reading it before writing this doc,
  it renders plain `styled_line` text with no per-line marker column of
  any kind. `sync_code_actions` still ambiently refreshes `lsp.
  code_actions` every frame the caret sits on a new target (so `Alt+Enter`
  opens on already-cached data, same latency `ide-ui`'s own lightbulb-free
  trigger already has), but there is no ambient visual surface to paint a
  marker on, so none is invented. A gutter is a separate, larger feature
  this backlog doesn't otherwise need yet.
- **`workspace_text_edits_to_transaction` uses `ide_lsp::
  position_to_byte_offset` directly, not `ide-ui`'s locally-improved
  `position_to_byte_offset_indexed`.** `code-actions.md`'s `ide-ui` half
  (`crates/ui/src/app.rs`) hand-rolls a `LineIndex`-based position
  conversion with one deliberate fix over `ide_lsp::position_to_byte_
  offset`'s own known quirk (an end-of-file insert position resolving to
  `text.len() + 1` instead of `text.len()`) — accepted there as an
  `ide-ui`-local improvement out of scope for `crates/lsp/**`. Every
  existing position-to-offset conversion already merged into `ide-tui`
  (`hover`, `document_highlight`, `inlay_hints`, `goto`) calls `ide_lsp::
  position_to_byte_offset` directly with no local patch — re-deriving
  `ide-ui`'s one-off fix here would make this crate internally
  inconsistent for a one-byte edge case neither `TextBuffer::apply`'s
  clamping nor `apply_workspace_edit_to_disk`'s strict bounds check turns
  into a corruption (worst case: that one file's edit is rejected with
  `OffsetOutOfRange` instead of applied, the same outcome any genuinely
  out-of-range edit already produces). Consistency with this crate's own
  established convention wins over bug-for-bug parity with `ide-ui` here.
- **No `is_command_enabled`-style greying-out.** `commands.rs`'s `Command`
  table has no enabled/disabled concept anywhere in this crate (unlike
  `ide-ui`'s palette) — `Rename` and `ShowIntentionActions` are reachable
  unconditionally like every other binding here; `trigger_rename`'s own
  no-op-with-a-status-message gating (§3.2) is what a real invocation with
  nothing to rename falls back to, exactly like `trigger_go_to_
  declaration`'s existing "No language server running." status message.

Does not touch `crates/dap/**`, `crates/core/**`, or `crates/lsp/**` — no
new API in any of the three; `crates/tui/**` only.

## 2. Interface / API

### 2.1 `crates/tui/src/editor.rs` (new pure function)

```rust
/// Ported verbatim (behavior, not code layout) from `ide-ui`'s
/// `editor::geometry::word_range_at` -- the identifier `offset` falls
/// inside, as a byte range. `None` off an identifier entirely, or when the
/// run it touches starts with a digit (a number literal, not a symbol). A
/// caret between two characters resolves to the identifier on its left
/// when there's none to its right.
pub(crate) fn word_range_at(text: &str, offset: usize) -> Option<Range<usize>>;
```

### 2.2 `crates/tui/src/lsp_bridge.rs` (additions to the existing `LspBridge`)

```rust
pub(crate) struct LspBridge {
    // ... existing fields (docs/features/tui-hover-and-inlay-hints.md
    //     §2.1, tui-goto-and-usages.md, tui-problems.md,
    //     tui-semantic-highlighting.md) ...

    /// Code actions for whatever `(path, position)` was last queried --
    /// replaced wholesale on each `LspEvent::CodeAction`, cleared at
    /// send-time (same convention `document_highlights` already follows).
    pub(crate) code_actions: Vec<CodeAction>,
    /// The `(path, position)` the current `code_actions` answers.
    pub(crate) code_actions_target: Option<(PathBuf, Position)>,
    /// The outcome of the most recently applied `WorkspaceEdit` -- set by
    /// `LspEvent::WorkspaceEditReady`, from `apply_code_action`. This
    /// bridge never receives a server-initiated `workspace/applyEdit`
    /// distinctly from this event (`ide-lsp` folds both origins into the
    /// same `LspEvent`, `code-actions.md` §3.5) -- unlike `ide-ui`, there
    /// is no separate origin to distinguish, since this crate never wires
    /// up anything that would make that distinction visible.
    pub(crate) workspace_edit: Option<WorkspaceEdit>,
    pub(crate) workspace_edit_label: Option<String>,
    /// One-frame-true, reset unconditionally at the top of `poll()` --
    /// safe here for the same reason `goto_ready`/`references_ready`
    /// already are: never set synchronously outside `poll()`'s own drain
    /// loop.
    pub(crate) workspace_edit_ready: bool,
    /// The `(path, position)` the current `prepare_renameable` answers.
    pub(crate) prepare_rename_target: Option<(PathBuf, Position)>,
    pub(crate) prepare_renameable: Option<bool>,
    pub(crate) prepare_rename_ready: bool,
    pub(crate) rename_edit: Option<WorkspaceEdit>,
    pub(crate) rename_new_name: Option<String>,
    pub(crate) rename_ready: bool,
}

impl LspBridge {
    /// No-op with no client running. Clears `code_actions` and records
    /// `code_actions_target` before sending.
    pub(crate) fn request_code_actions(&mut self, path: &Path, position: Position);
    /// Clears `code_actions`/`code_actions_target` without sending
    /// anything -- mirrors `clear_document_highlights`.
    pub(crate) fn clear_code_actions(&mut self);
    /// Sends `LspRequest::ApplyCodeAction { index }` -- no-op with no
    /// client running.
    pub(crate) fn apply_code_action(&self, index: usize);
    /// No-op with no client running. Records the target, clears any
    /// previous answer, sends `LspRequest::PrepareRename`.
    pub(crate) fn request_prepare_rename(&mut self, path: &Path, position: Position);
    /// No-op with no client running. Clears any previous `rename_edit`/
    /// `rename_new_name`, sends `LspRequest::Rename`.
    pub(crate) fn request_rename(&mut self, path: &Path, position: Position, new_name: String);
}
```

`poll()` grows three more top-of-call resets
(`workspace_edit_ready`/`prepare_rename_ready`/`rename_ready` all `false`),
four more matched `LspEvent` arms (`CodeAction`, `WorkspaceEditReady`,
`PrepareRenameReady`, `RenameReady` — replacing four of the six cases the
crate's existing "every other event kind... has no state in this crate to
update" catch-all comment lists; `DocumentSymbol`/`WorkspaceSymbol`/
`FormatReady` still fall through it unchanged, since T13 doesn't touch
Search Everywhere or Formatting), and two more fields cleared in
`ServerExited` (`code_actions`/`code_actions_target` — ambient state,
same as `document_highlights`/`inlay_hints`; `workspace_edit*`/
`prepare_rename*`/`rename_edit`/`rename_new_name` are deliberately **not**
cleared there, matching `ide-ui`'s own `LspBridge::poll`'s `ServerExited`
arm exactly — `workspace_edit_ready`/`prepare_rename_ready`/`rename_ready`
are already `false` by the time `ServerExited` is reached within the same
`poll()` call, per the unconditional top-of-call reset, so there is
nothing stale left to observe even without an explicit clear). `start_
with_command` clears every new field (there is no separate `stop()` in
this crate's bridge — single project per process, `docs/features/
tui-find-in-path.md` §... already notes this).

### 2.3 `crates/tui/src/app.rs`

```rust
pub(crate) struct CodeActionsState {
    pub(crate) selected: usize,
}

pub(crate) struct RenamePopup {
    pub(crate) path: PathBuf,
    pub(crate) position: Position,
    pub(crate) original_name: String,
    /// The popup's editable text, pre-filled with `original_name` --
    /// mutated directly by `handle_rename_popup_key`, the same "render
    /// code reads a field the key handler writes" convention `find`'s
    /// query and `search_state.query` already use.
    pub(crate) input: String,
}
```

`App` gains:

- `code_actions: Option<CodeActionsState>` — presence is visibility, same
  convention `problems: Option<ProblemsState>` already establishes (the
  actions themselves live in `lsp.code_actions`, only the list-selection
  index lives here).
- `last_code_actions_target: Option<(PathBuf, Position)>` — drives
  `sync_code_actions`'s "did the target change" check, mirroring `last_
  highlighted_target`.
- `rename_popup: Option<RenamePopup>` — presence is visibility.
- `pending_rename_preview: Option<(ide_lsp::WorkspaceEdit, String)>` —
  `(edit, new_name)` awaiting Apply/Cancel; presence is visibility.

`close_all_overlays` grows from six-way to nine-way: adds `code_actions`,
`rename_popup`, `pending_rename_preview` to its existing clear list (Goto
picker, Notifications, Problems, Cargo, Hover, Find in Path). `handle_key`
grows three more `is_some()` interception checks, alongside the existing
six, each routing to its own `handle_*_key` — order among the nine doesn't
matter, since `close_all_overlays` guarantees at most one is ever `Some`/
`true` at a time.

New methods (signatures only — behavior in §3):

```rust
impl App {
    pub(crate) fn sync_code_actions(&mut self);
    fn trigger_show_intention_actions(&mut self);
    fn handle_code_actions_key(&mut self, key: KeyEvent) -> LoopSignal;
    fn apply_workspace_edit(&mut self, edit: ide_lsp::WorkspaceEdit, what: &str) -> Result<usize, String>;
    fn handle_workspace_edit_ready(&mut self);
    fn trigger_rename(&mut self);
    fn handle_rename_popup_key(&mut self, key: KeyEvent) -> LoopSignal;
    fn confirm_rename(&mut self);
    fn handle_prepare_rename_ready(&mut self);
    fn handle_rename_ready(&mut self);
    fn handle_rename_preview_key(&mut self, key: KeyEvent) -> LoopSignal;
}
```

`handle_workspace_edit_ready`/`handle_prepare_rename_ready`/`handle_rename_
ready` are **not** separate per-frame calls from `lib.rs` — `poll_lsp`
already folds `goto_ready`/`references_ready` handling inline rather than
exposing one `pub` per-ready-flag method the run loop calls individually
(`handle_goto_results`, called straight from `poll_lsp`'s own body); these
three join that same body for consistency, rather than introducing a
second convention for "something to do once a `poll()`-set flag comes
back true" in the same file. Only `sync_code_actions` is a genuinely new
`lib.rs` run-loop call, alongside the existing `sync_document_highlights`
— it isn't response-driven, it fires ambiently every frame the caret
target changes, the same shape `sync_document_highlights` already has.

Plus a free function, `workspace_text_edits_to_transaction(text: &str,
text_edits: &[ide_lsp::TextEdit]) -> Option<ide_core::Transaction>` (§1's
"no `LineIndex`-indexed variant" note).

`crates/tui/src/commands.rs` gains two `Action` variants,
`ShowIntentionActions` and `Rename`:

```rust
Command {
    id: "ShowIntentionActions",
    title: "Show Intention Actions",
    binding: Some((KeyModifiers::ALT, KeyCode::Enter)),
    action: Action::ShowIntentionActions,
},
Command {
    id: "Rename",
    title: "Rename",
    binding: Some((KeyModifiers::SHIFT, KeyCode::F(6))),
    action: Action::Rename,
},
```

Both are literal ports of `ide-ui`'s own `Binding::same` bindings (`⌥↩`,
`⇧F6` — both identical across every JetBrains keymap variant, `docs/
roadmap.md` §5.2), not `Ctrl`-translations: neither chord starts from a
`Cmd`/`Ctrl` combo in `ide-ui`, so this crate's usual `Ctrl`-masking/
Kitty-protocol disambiguation concern (`commands.rs`'s own module doc)
doesn't apply to either, the same reasoning `QuickDocumentation`'s literal
`F1` already established for a different kind of exception.

### 2.4 `crates/tui/src/ui.rs`

Three new render functions, following the existing `List`/`Paragraph`
popup conventions exactly (`render_goto_popup`/`render_problems_panel`/
`render_hover_popup`):

- `render_code_actions_popup` — a `List`, one row per `lsp.code_actions`
  entry (or "No actions available." if empty), `REVERSED` on the selected
  row, a `(disabled)` suffix on any entry with `disabled_reason: Some`
  (not selectable — same `Enter` still no-ops on it below, §3.1).
- `render_rename_popup` — a single-line `Paragraph` showing `rename_
  popup.input` live, titled `"Rename"`.
- `render_rename_preview` — a `List`: a summary row (`"Rename to
  `{new_name}`: {N} occurrence{s} across {M} file{s}"`) plus one row per
  `FileEdit` (path and its own occurrence count) — the same content
  `rename-refactoring.md` §3.5 specifies for `ide-ui`'s own preview
  window, ported to a list row shape instead of an `egui::Window`.

## 3. Behaviour

### 3.1 `sync_code_actions` / `trigger_show_intention_actions` / applying

Called once per frame (`lib.rs`'s run loop, alongside `sync_document_
highlights`): `lsp_query_target()` differing from `last_code_actions_
target` fires `request_code_actions` and updates the target; unchanged is
a no-op; `None` clears `lsp.code_actions`/`last_code_actions_target` via
`clear_code_actions`, but only when there was a previous target to clear
(`sync_document_highlights`'s own already-established guard-before-clear
convention in this crate, not `ide-ui`'s unconditional-clear-every-frame
version — `T12`'s doc already chose this optimization for the sibling
sync function, and this one follows it for consistency within the crate
rather than re-deriving `ide-ui`'s version).

`Alt+Enter` → `trigger_show_intention_actions`: `close_all_overlays()`,
then `code_actions = Some(CodeActionsState { selected: 0 })` — no new
request, opens on whatever `sync_code_actions` already cached (§1's
"no gutter, but still ambient" note).

`handle_code_actions_key`: `Esc` closes. `Up`/`Down` move `selected`,
clamped to `0..lsp.code_actions.len()` (a no-op on an empty list either
way). `Enter` closes the popup and, only if `selected` is still a valid
index into `lsp.code_actions` at that moment (empty list ⇒ never valid),
calls `lsp.apply_code_action(selected)` — this crate does not
special-case an entry with `disabled_reason: Some(..)`: selecting one
still sends `ApplyCodeAction`, which `ide-lsp`'s own `send_request` arm
already reports as `edit: None` for an unsupported/unresolvable action
(`code-actions.md` §3.3's "found, unsupported" case) — the same
permissive "let the wire round trip say no" behavior every other query in
this bridge already relies on rather than duplicating the server's own
disabled/resolvable bookkeeping client-side.

`handle_workspace_edit_ready` (called from `poll_lsp`'s own body, right
after it drains `self.lsp.poll()`): no-op unless `lsp.workspace_edit_ready`. `edit: None` → `self.
status = Some(format!("{what}: nothing to apply"))` where `what` is `lsp.
workspace_edit_label` or `"Code action"`. `Some(edit)` → `apply_workspace_
edit(edit, &what)`; success sets `self.status` to `"{what}: applied to N
file(s)"`, failure sets it to the error string.

`apply_workspace_edit` (§2.3) is a direct, unmodified-logic port of
`ide-ui`'s own method of the same name (`code-actions.md` §2.3/§3.4):
partitions `edit.edits` by whether an open tab's `path` field matches;
reads each disk-subset file fresh from disk (open-tab subset reads the
tab's own current text instead); converts every file's `text_edits` via
`workspace_text_edits_to_transaction`; if *any* file's read or conversion
fails, returns immediately with no write at all (matches `code-
actions.md` §4's whole-edit-atomicity invariant — nothing partial is ever
attempted); applies the disk subset via `ide_core::apply_workspace_edit_
to_disk` (itself all-or-nothing with rollback, unmodified); only once
that fully succeeds does the buffer subset apply via `Buffer::apply`, one
undo step per open tab touched. Returns the total file count on success.

### 3.2 `trigger_rename` / the popup / confirming

`Shift+F6` → `trigger_rename`: no-op with no active tab. `self.status =
Some("Rename: no language server is running")` if `!lsp.is_running()`.
Otherwise reads the caret offset from the active buffer's primary
selection, resolves `word_range_at(text, offset)`; `None` → `self.status =
Some("Rename: no symbol under the caret")`. On a resolved range: `close_
all_overlays()`, opens `rename_popup` pre-filled with the identifier's
text, and fires `request_prepare_rename` ambiently (not gating the popup
— it's already open by the time the request goes out).

`handle_rename_popup_key`: `Esc` clears `rename_popup` (cancel). `Backspace`
pops the last char off `input`. A plain (non-`Ctrl`) `Char(c)` pushes `c`.
`Enter` → `confirm_rename`.

`confirm_rename`: takes `rename_popup` (closes it regardless of outcome).
`input.trim()` empty or equal to `original_name` → sends nothing (silent
cancel, matching JetBrains' own "confirming unchanged is a no-op"
behavior). Otherwise `lsp.request_rename(&path, position, trimmed)`.

### 3.3 `handle_prepare_rename_ready` / `handle_rename_ready` / the preview

Both called from `poll_lsp`'s own body, alongside `handle_workspace_edit_
ready`.

`handle_prepare_rename_ready`: no-op unless `lsp.prepare_rename_ready`.
`prepare_renameable == Some(false)` **and** `rename_popup`'s own
`(path, position)` still matches `lsp.prepare_rename_target` → closes the
popup, `self.status = Some("Rename: this element cannot be renamed")`.
Every other case (renameable, or the popup already closed/superseded) is
a no-op — `PrepareRenameReady` is never a hard gate on opening, only on
an explicit negative answer for what's still open (`rename-refactoring
.md` §4).

`handle_rename_ready`: no-op unless `lsp.rename_ready`. `edit: None` →
status message "nothing to apply". Otherwise, re-reads the *currently*
active tab's path fresh (not the popup's stale target — the active tab
may have changed while the request was in flight): a single-file edit
whose one file matches applies immediately via `apply_workspace_edit`,
same status-message shape as §3.1. Anything else (more than one file, or
the one file isn't the current tab's, or there's no active tab at all)
calls `close_all_overlays()` (closing anything the user opened during the
async wait — §4) then sets `pending_rename_preview = Some((edit,
new_name))`.

`handle_rename_preview_key`: `Esc` clears `pending_rename_preview`
(cancel — nothing read from or written to disk/buffer, the `rename`
request already completed, cancelling only declines to apply its
answer). `Enter` takes `pending_rename_preview` and calls `apply_
workspace_edit`, same status-message shape.

## 4. Constraints & invariants

- **Every `WorkspaceEdit` this crate ever sees came from `ide-lsp`'s own
  already-validated `convert_workspace_edit`** (`code-actions.md` §3.3/§4)
  — a path outside the project root, or any resource operation, already
  failed the *entire* conversion inside `ide-lsp` before this crate's
  `LspEvent::{CodeAction as an apply outcome, WorkspaceEditReady,
  RenameReady}` ever fires. `apply_workspace_edit` here does no
  additional path validation of its own, matching `ide-ui`'s own
  `apply_workspace_edit` exactly — this is intentional reuse of an
  already-enforced invariant, not a gap introduced by this phase.
- **Async responses are matched against fresh state, never trusted
  stale** — `handle_rename_ready`'s active-tab-at-response-time check
  (§3.3) and `handle_prepare_rename_ready`'s target-still-matches check
  (§3.3) both re-derive from current `App` state rather than anything
  captured at request time, mirroring `rename-refactoring.md` §4's own
  stated invariant.
- **At most one of the nine overlays this crate now tracks is ever open
  at once** (§2.3) — `close_all_overlays` is the single enforcement
  point; every trigger function calls it before setting its own state,
  and `handle_rename_ready`'s preview-escalation path calls it again
  before opening the preview specifically because an arbitrary number of
  frames (and therefore arbitrary user input) can elapse between `confirm_
  rename` closing the popup and the real `Rename` response landing.
- **`workspace_text_edits_to_transaction`'s direct `ide_lsp::position_to_
  byte_offset` use is a deliberate, documented divergence from `ide-ui`'s
  own conversion helper** (§1) — not an oversight; re-derive this
  reasoning before "fixing" it to match `ide-ui` bug-for-bug in some
  future revision.
- **No document-version checking, no live per-keystroke rename echo** —
  both unchanged carryovers from `rename-refactoring.md` §1/§4, not
  re-decided here.

## 5. Examples

**Same-file rename, applies immediately:**

```rust
// Caret on a local variable used twice, only in this file.
app.trigger_rename();
// rename_popup = Some(RenamePopup { original_name: "x", input: "x", .. })
// request_prepare_rename fired ambiently; a frame later:
// lsp.prepare_renameable = Some(true) -- popup stays open, unchanged.

// User types "count", presses Enter:
// (handle_rename_popup_key routes Enter -> confirm_rename)
// rename_popup = None; LspRequest::Rename { new_name: "count", .. } sent.

// Response arrives: LspEvent::RenameReady { new_name: "count",
//   edit: Some(WorkspaceEdit { edits: [FileEdit { path, text_edits: [2 edits] }] }) }
// handle_rename_ready: edit.edits.len() == 1, path == active tab's path
// -> apply_workspace_edit runs immediately.
// self.status = Some("Rename to `count`: applied to 1 file")
```

**Cross-file rename, escalates to preview:**

```rust
app.trigger_rename();
// ... user confirms "process_batch" ...
// Response touches 3 files:
// handle_rename_ready: edit.edits.len() == 3
// -> close_all_overlays(); pending_rename_preview = Some((edit, "process_batch"))
// render_rename_preview shows:
//   "Rename to `process_batch`: 4 occurrences across 3 files"
//   src/pipeline.rs -- 1 occurrence
//   src/batch/mod.rs -- 2 occurrences
//   src/batch/worker.rs -- 1 occurrence
// User presses Enter: apply_workspace_edit runs (disk subset first, then
// buffer subset), pending_rename_preview = None.
```

**Show Intention Actions, immediate apply:**

```rust
// Ambient, once per frame: sync_code_actions fires request_code_actions
// as the caret sits on a function call with a suggested import.
// A frame later: lsp.code_actions = [CodeAction { index: 0,
//   title: "Import `Foo`", .. }]

// User presses Alt+Enter:
app.trigger_show_intention_actions();
// code_actions = Some(CodeActionsState { selected: 0 }) -- no new request.

// User presses Enter:
// (handle_code_actions_key) code_actions = None; lsp.apply_code_action(0);
// -> LspEvent::WorkspaceEditReady { edit: Some(edit), label: Some("Import `Foo`") }

// Next frame: handle_workspace_edit_ready applies it, one undo step on
// the active tab's buffer.
// self.status = Some("Import `Foo`: applied to 1 file")
```

## 6. Dependencies & integration points

- No new external dependencies. No new `ide-lsp` or `ide-core` public API —
  both already carry everything this phase needs
  (`code-actions.md`/`rename-refactoring.md`'s own `ide-lsp` halves;
  `ide-core`'s `workspace_edit.rs` module, both merged well before this
  backlog started).
- `crates/tui/**` only: `editor.rs` (new `word_range_at`), `lsp_bridge.rs`,
  `app.rs` (`poll_lsp` gains three more drained-ready branches; `lib.rs`'s
  run loop gains exactly one new call, `sync_code_actions`, alongside the
  existing `sync_document_highlights`), `commands.rs`, `ui.rs`, `lib.rs`.
- **Not a `CLAUDE.md`-declared security-sensitive path** — `crates/tui/**`
  isn't unconditionally listed the way `crates/lsp/**`/`crates/core/src/
  git/**`/`crates/core/src/workspace_edit.rs` are, and this phase touches
  neither of the latter two files' own code (only calls their already-
  reviewed public API). Same reasoning `tui-hover-and-inlay-hints.md` §6
  and `tui-find-in-path.md` §6 already gave for their own phases: no
  `hacker` pass is automatically required, but `apply_workspace_edit`
  writing to disk based on LSP-response-derived data is exactly the kind
  of new consumer of a sensitive operation `code-actions.md` §6 already
  flagged for `ide-ui`'s own equivalent method — reviewed with that same
  scrutiny inline (self-review, §"Revision notes" below) rather than
  skipped for being off the declared list.
- Reuses, unmodified: `ide_core::{apply_workspace_edit_to_disk, FileEdit,
  WorkspaceEdit, Transaction, Change}`, `ide_lsp::{CodeAction,
  WorkspaceEdit, TextEdit, position_to_byte_offset, byte_offset_to_
  position}`, this crate's own `lsp_query_target`/`close_all_overlays`/
  `open_or_focus_tab`.

## Revision notes

Self-reviewed inline (`rev`-style checklist + devil's-advocate pass), no
`hacker` pass — reasoning in §6. Verified against the actual current
source before writing this doc, not assumed from `code-actions.md`/
`rename-refactoring.md`'s prose alone: `ide_lsp`'s public API already
exports every type/variant this phase needs (`CodeAction`,
`ApplyCodeAction`, `WorkspaceEdit`, `PrepareRename`, `Rename`,
`PrepareRenameReady`, `RenameReady`, `WorkspaceEditReady`); `ide_core::
workspace_edit`'s `apply_workspace_edit_to_disk`/`FileEdit`/`WorkspaceEdit`
are unmodified and reusable as-is; `render_editor` (`ui.rs`) genuinely has
no gutter column to paint a lightbulb on; `ide-tui`'s `OpenBuffer.path` is
a bare `PathBuf` (no untitled-buffer concept in this crate, unlike
`ide-ui`'s `Option<PathBuf>`), simplifying `trigger_rename`'s gating versus
`ide-ui`'s own version. Implementation, tests (≥80% coverage on every
touched file), `cargo fmt`/`clippy -D warnings`/`build --all-targets`/
`test` (crate-scoped and workspace-wide) all green before commit — see
`docs/roadmap.md`'s T13 row for the final coverage numbers.
