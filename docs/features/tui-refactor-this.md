# TUI Refactor This (T43)

## 1. Purpose

Parity port of `refactor-this.md` (D2, `ide-ui`) into `ide-tui`. `ide-ui`'s
`⌃T` ("Refactor This") narrows the already-fetched `lsp.code_actions` cache
(`tui-code-actions-and-rename.md`, T13) down to refactoring-kind actions
only, plus five direct-invoke commands (Extract Variable/Method/Constant/
Field, Inline) that skip the menu when exactly one action matches a
heuristic. Both paths funnel through a shared **Refactor Preview** popup
that renders an actual line-level diff of the pending `WorkspaceEdit`
before applying it — a step up from `ide-tui`'s own existing Rename
Preview (`pending_rename_preview`, T13), which only shows a file-and-
occurrence-count list, exactly the same relationship D2 has to D1 in
`ide-ui`.

**Zero new `ide-core`/`ide-lsp` API.** `ide_core::diff_text` and
`ide_core::apply_transaction` are already `pub` (added for D2, confirmed
by reading `crates/core/src/git/mod.rs:2005` and `crates/core/src/
workspace_edit.rs:155`, both already re-exported from `crates/core/src/
lib.rs`) — this phase is `diff_text`'s and `apply_transaction`'s second
call site, first port to a second frontend. `ide_lsp::CodeAction` already
carries `index`/`title`/`kind`/`is_preferred`/`disabled_reason`
(`crates/lsp/src/types.rs:137-148`) — the same struct `ide-ui`'s D2 reads,
shared by both frontends without duplication.

Everything in this phase lands in `crates/tui/src/{app.rs,commands.rs,
ui.rs}` — no new file, mirroring T39/T38's "zero new `ide-core`" shape.

## 2. Interface

### 2.1 `app.rs` — new state

```rust
/// One entry per `FileEdit` in `edit.edits`, same order. `None` means the
/// diff itself couldn't be computed (unreadable file, or `apply_
/// transaction` rejected an out-of-range edit) -- the row still renders
/// (path + "(diff unavailable)"), never dropped from the list, mirroring
/// `ide-ui`'s `RefactorPreview` (`crates/ui/src/app.rs:426`) exactly.
struct RefactorPreview {
    what: String,
    edit: ide_lsp::WorkspaceEdit,
    diffs: Vec<Option<ide_core::FileDiff>>,
    /// Plain scroll offset into the flattened diff lines, the same
    /// convention `GitPanelState::diff_scroll` already establishes for
    /// `render_git_diff` -- `ide-ui`'s own preview needs no equivalent
    /// field, since `egui::ScrollArea` scrolls itself.
    scroll: u16,
}

/// The five direct commands, one heuristic each (§3.2) -- a closed enum
/// rather than five near-identical methods, ported verbatim from
/// `ide-ui`'s own `DirectRefactorKind` (`crates/ui/src/app.rs:454`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectRefactorKind {
    ExtractVariable,
    ExtractMethod,
    ExtractConstant,
    ExtractField,
    Inline,
}

/// `⌃T`'s list-selection state -- see the field doc below for why this
/// isn't a reuse of `CodeActionsState`.
pub(crate) struct RefactorMenuState {
    pub(crate) selected: usize,
}
```

On `App`, alongside the existing `code_actions: Option<CodeActionsState>`
(`app.rs:311-312`) and `pending_rename_preview: Option<(WorkspaceEdit,
String)>` (`app.rs:905`):

```rust
/// `⌃T`'s list-selection state -- its own type
/// (`RefactorMenuState { selected: usize }`), not a reuse of `CodeActionsState`.
/// This mirrors `GenerateMenuState`'s own documented reasoning exactly
/// (`app.rs:315-318`): it indexes into `refactor_menu_actions()`'s
/// filtered view, not `lsp.code_actions` wholesale, and the two lists can
/// have different lengths/orderings, so an index meaningful in one is not
/// meaningful in the other -- the same reason `GenerateMenuState` isn't a
/// `CodeActionsState` either, despite an identical `{selected: usize}`
/// shape.
pub(crate) refactor_menu: Option<RefactorMenuState>,
pub(crate) pending_refactor_preview: Option<RefactorPreview>,
/// Set immediately before this phase's code sends `LspRequest::
/// ApplyCodeAction`, taken (read-and-cleared) unconditionally at the top
/// of `handle_workspace_edit_ready`, every call -- see §3.4/§4. Routes
/// that one `WorkspaceEditReady` into `show_refactor_preview` instead of
/// `handle_workspace_edit_ready`'s existing immediate-apply body;
/// `Alt+Enter`'s/`Shift+F6`'s own paths never set it and keep applying
/// immediately, unchanged.
via_refactor_preview: bool,
```

`close_all_overlays` (`app.rs:1985`) and `any_popup_open` (`app.rs:5529`)
each gain two lines: `self.refactor_menu = None;` / `self.refactor_menu.
is_some()` and `self.pending_refactor_preview = None;` / `self.
pending_refactor_preview.is_some()` — placed next to the existing `code_
actions`/`pending_rename_preview` lines they mirror. `via_refactor_
preview` is **not** added to either — it is not itself a visible overlay,
and per §4 it must only ever be cleared by `handle_workspace_edit_ready`'s
own unconditional top-of-method take, never by `close_all_overlays` (which
runs synchronously on a keypress, never in response to an LSP event, so
there is no path where it needs to intervene).

New methods on `App` (placed alongside `trigger_show_intention_actions`,
`app.rs:4812`):

```rust
impl App {
    fn is_refactor_kind(action: &ide_lsp::CodeAction) -> bool;
    /// `⌃T`'s filtered view of `lsp.code_actions` -- mirrors `generate_
    /// menu_actions()` (`app.rs:4847-4853`) exactly, same reason: the
    /// popup's list position is **not** the same thing as `CodeAction::
    /// index` once the list is filtered, so every reader of this filtered
    /// view (render, Up/Down clamping, Enter's lookup) must go through
    /// this one method rather than three independently-written filters
    /// that could drift apart.
    pub(crate) fn refactor_menu_actions(&self) -> Vec<&ide_lsp::CodeAction>;
    /// `⌃T`'s entry point.
    fn trigger_refactor_this(&mut self);
    fn handle_refactor_menu_key(&mut self, key: KeyEvent) -> LoopSignal;
    /// A `⌃T` popup row's Enter: closes the popup, routes through
    /// `apply_code_action_via_preview`.
    fn select_refactor_action(&mut self, index: usize);
    /// The five direct commands' shared entry point.
    fn trigger_direct_refactor(&mut self, kind: DirectRefactorKind);
    /// Shared by `select_refactor_action` and `trigger_direct_refactor`:
    /// sets `via_refactor_preview = true`, calls `self.lsp.apply_code_
    /// action(index)` (`crates/tui/src/lsp_bridge.rs:356`, unchanged).
    fn apply_code_action_via_preview(&mut self, index: usize);
    /// Builds `pending_refactor_preview` from a ready `WorkspaceEdit`
    /// (§3.3).
    fn show_refactor_preview(&mut self, what: String, edit: ide_lsp::WorkspaceEdit);
    fn handle_refactor_preview_key(&mut self, key: KeyEvent) -> LoopSignal;
    /// Applies via the existing shared `apply_workspace_edit`
    /// (`app.rs:4957`), same success/failure `self.status` shape every
    /// other apply path already uses.
    fn confirm_refactor_preview(&mut self);
    /// Clears `pending_refactor_preview`, no I/O.
    fn cancel_refactor_preview(&mut self);
}
```

### 2.2 `commands.rs` — six new `Action` variants

```rust
enum Action {
    // ...
    RefactorThis,
    ExtractVariable,
    ExtractMethod,
    ExtractConstant,
    ExtractField,
    Inline,
}
```

Bindings — all direct `Ctrl`-translations of `ide-ui`'s own `⌘⌥<letter>`
chords (`refactor-this.md` §2.2's table), the exact same translation
`ReformatCode`'s `⌘⌥L` → `Ctrl+Alt+L` already established
(`tui-formatting.md`, T38, `commands.rs:1108-1115`):

| `id` | `title` | binding | translated from |
|---|---|---|---|
| `RefactorThis` | "Refactor This" | **none — palette-only** | `⌃T` (see below) |
| `ExtractVariable` | "Extract Variable" | `Ctrl+Alt+V` | `⌘⌥V` |
| `ExtractMethod` | "Extract Method" | `Ctrl+Alt+M` | `⌘⌥M` |
| `ExtractConstant` | "Extract Constant" | `Ctrl+Alt+C` | `⌘⌥C` |
| `ExtractField` | "Extract Field" | `Ctrl+Alt+F` | `⌘⌥F` |
| `Inline` | "Inline" | `Ctrl+Alt+N` | `⌘⌥N` |

Checked against every existing `Ctrl+Alt+<key>` binding in `commands.rs`
(`ToggleBlockComment` `/`, `NavigateBack`/`NavigateForward` Left/Right,
`ReformatCode` `l`) — `v`/`m`/`c`/`f`/`n` are all free, no collision.

**`RefactorThis` gets no default binding — a genuine, checked collision,
not an invented gap.** `ide-ui`'s own binding is a literal `Ctrl+T`
(`KeyChord::new(Key::T).ctrl()`, not a `Cmd`-based chord to translate), and
`ide-tui`'s `Ctrl+T` is already `ToggleProjectToolWindow`'s binding
(`commands.rs:294-298`, `Action::ToggleLeftDockFocus`, occupied since
before T33). Unlike `Cmd+<digit>` (`ToggleProjectToolWindow`'s own
mac-side chord, masked to the unrelated `Ctrl+Q` byte and worked around via
the Kitty/CSI-u protocol per `commands.rs`'s own module doc comment),
`Ctrl+T` produces one unambiguous byte with no alternate encoding to fall
back on — there is no second binding to mask against, only an outright
occupied slot. This project's own established precedent for exactly this
situation (`ToggleCargoPanel`, `ToggleGitPanel`, `ToggleClonePanel`: "no
safe translation exists") is no default binding, palette-only — followed
here rather than displacing the pre-existing, long-shipped `Ctrl+T`
binding or inventing a substitute chord no reference keymap actually uses.

`run_action`'s match statement gains six arms:

```rust
Action::RefactorThis => self.trigger_refactor_this(),
Action::ExtractVariable => self.trigger_direct_refactor(DirectRefactorKind::ExtractVariable),
Action::ExtractMethod => self.trigger_direct_refactor(DirectRefactorKind::ExtractMethod),
Action::ExtractConstant => self.trigger_direct_refactor(DirectRefactorKind::ExtractConstant),
Action::ExtractField => self.trigger_direct_refactor(DirectRefactorKind::ExtractField),
Action::Inline => self.trigger_direct_refactor(DirectRefactorKind::Inline),
```

### 2.3 `ui.rs` — two new render functions

```rust
fn render_refactor_menu_popup(frame: &mut Frame, app: &App, area: Rect);
fn render_refactor_preview(frame: &mut Frame, app: &App, area: Rect);
```

Wired into the same per-frame popup dispatch `render_code_actions_popup`/
`render_rename_preview` already are (grep `render_code_actions_popup(`/
`render_rename_preview(` in `ui.rs` for the exact call site — both are
called unconditionally every frame and early-return internally when their
backing `Option` is `None`; the two new functions follow the identical
shape).

## 3. Behaviour

### 3.1 `trigger_refactor_this` / the `⌃T` menu

```rust
fn is_refactor_kind(action: &ide_lsp::CodeAction) -> bool {
    action.kind.as_deref().is_some_and(|k| k.starts_with("refactor"))
}

/// Mirrors `generate_menu_actions()` (`app.rs:4847`) field-for-field: same
/// borrow-and-collect shape, same "position in this Vec is not `.index`"
/// caveat.
pub(crate) fn refactor_menu_actions(&self) -> Vec<&ide_lsp::CodeAction> {
    self.lsp
        .code_actions
        .iter()
        .filter(|a| Self::is_refactor_kind(a))
        .collect()
}

fn trigger_refactor_this(&mut self) {
    if self.refactor_menu_actions().is_empty() {
        self.status = Some("Refactor This: no refactoring available here".to_string());
        return;
    }
    self.close_all_overlays();
    self.refactor_menu = Some(RefactorMenuState { selected: 0 });
}
```

No new request — reads `lsp.code_actions`, the same ambiently-kept-fresh
cache `Alt+Enter` already reads (`sync_code_actions`, unchanged).

`render_refactor_menu_popup` renders exactly the same row shape `render_
code_actions_popup` does (`ui.rs:2164-2201`: reversed-video highlight on
the selected row, `"{title} (disabled)"` for a `disabled_reason`-carrying
action) but iterates `app.refactor_menu_actions()` instead of the full
list, title `"Refactor This  (Enter: apply, Esc: close)"` instead of
`"Show Intention Actions  ..."`. `handle_refactor_menu_key` mirrors
`handle_generate_menu_key` (`app.rs:4874-4906`) exactly, **not** `handle_
code_actions_key`'s simpler direct-index shape — `handle_code_actions_key`
can use `state.selected` as a raw index into `lsp.code_actions` only
because its popup shows the *unfiltered* list, which does not hold here:
`Esc` clears `refactor_menu`; `Up`/`Down` move `selected` clamped to
`refactor_menu_actions().len()` (not `lsp.code_actions.len()` — an
out-of-range `selected` against the unfiltered length would let `selected`
land past the filtered list's actual end); `Enter` reads `self.refactor_
menu.as_ref().map(|s| s.selected)`, clears `refactor_menu`, then looks up
`self.refactor_menu_actions().get(selected).map(|a| a.index)` (the
action's own token, not filtered-list position) and calls `select_
refactor_action(index)` only if that lookup succeeded.

```rust
fn select_refactor_action(&mut self, index: usize) {
    self.refactor_menu = None;
    self.apply_code_action_via_preview(index);
}
```

### 3.2 Kind/title matching

`trigger_direct_refactor(kind)` finds the first entry in `lsp.code_actions`
**with `disabled_reason: None`** (a disabled action is never a valid
auto-selection target, same rule `render_code_actions_popup` already
applies to clicks, applied here to an automatic match instead) matching
`kind`'s heuristic — `kind`-prefix check first so an unrelated `quickfix`
action never matches by title alone, then a case-insensitive substring
check against `title`:

| `DirectRefactorKind` | `kind` prefix | `title` must contain (any) |
|---|---|---|
| `ExtractVariable` | `"refactor.extract"` | `"variable"` |
| `ExtractMethod` | `"refactor.extract"` | `"function"`, `"method"` |
| `ExtractConstant` | `"refactor.extract"` | `"constant"` |
| `ExtractField` | `"refactor.extract"` | `"field"` |
| `Inline` | `"refactor.inline"` | *(any title)* |

No match → `self.status = Some(format!("{name}: not available here"))`
where `name` is the command's own title (e.g. `"Extract Variable"`). A
match → `apply_code_action_via_preview(action.index)`.

### 3.3 `show_refactor_preview`

For each `FileEdit` in `edit.edits`, in order — identical algorithm to
`apply_workspace_edit`'s own old-text source selection
(`app.rs:4957-4972`, read-only variant of the same two-branch lookup, not
a second implementation):

1. Old text: the open tab's `buffer.text()` if a tab for that path exists,
   else `std::fs::read_to_string`; an unreadable file with no open tab
   yields a `None` diff entry for that file (not an error — the row still
   renders, §2.1).
2. `workspace_text_edits_to_transaction(&old_text, &file_edit.text_edits)`
   (`app.rs:8108`, already used by `apply_workspace_edit`) → `Option<
   Transaction>`; `None` → `None` diff entry.
3. `ide_core::apply_transaction(&old_text, &transaction)` → `Option<
   String>` (new text); `None` (out-of-range) → `None` diff entry.
4. `ide_core::diff_text(&file_edit.path, &old_text, &new_text)` → this
   file's diff entry (`None` if unchanged, possible but unusual for a
   server-computed edit).

Sets `pending_refactor_preview = Some(RefactorPreview { what, edit, diffs,
scroll: 0 })`. Never touches disk or any buffer — read-only, symmetric
with `handle_rename_ready`'s own preview-escalation path never applying
anything either.

### 3.4 `handle_workspace_edit_ready`'s new branch

Current body (`app.rs:5040-5065`):

```rust
fn handle_workspace_edit_ready(&mut self) {
    if !self.lsp.workspace_edit_ready { return; }
    let what = /* ... */;
    let Some(edit) = self.lsp.workspace_edit.take() else {
        self.status = Some(format!("{what}: nothing to apply"));
        return;
    };
    let file_count = match self.apply_workspace_edit(edit, &what) { /* ... */ };
    self.status = Some(/* ... */);
}
```

New body: immediately after the existing `if !self.lsp.workspace_edit_
ready { return; }` guard, before `what` is even computed:

```rust
let via_preview = std::mem::take(&mut self.via_refactor_preview);
```

— unconditional take-and-reset on *every* real event this method
processes, not only ones with a usable edit, so a stray `true` can never
leak into a later, unrelated apply (`formatting.md`'s `format_ready`
post-review fix is the precedent this mirrors, already applied once in
this crate for a self-resolving flag; this is the same discipline applied
to a boolean gate instead).

- The `edit: None` branch (`"{what}: nothing to apply"`) is unchanged —
  `via_preview` is never consulted there.
- Inside the `Some(edit)` branch only: `via_preview == true` →
  `self.show_refactor_preview(what, edit); return;` instead of the
  existing apply-immediately body. This is the **entire** behavioural
  change to this method.
- `via_preview == false` → existing behavior, completely unchanged.

### 3.5 The preview popup

`render_refactor_preview` — near-fullscreen, same sizing convention
`render_git_panel`/`render_cargo_panel` use (not the small centered-box
convention `render_rename_preview` uses, since this one needs to show
actual multi-line diff content, potentially several files' worth):

- A summary line: `"{what}: {N} file{s}"` where `N =
  preview.edit.edits.len()`.
- One block per file, in `edit.edits`' order, flattened into `Line`s the
  same way `render_git_diff` already flattens a `Vec<FileDiff>`
  (`ui.rs:2985-3021`, reusing `diff_line_to_line` verbatim, unchanged): the
  path as a bold heading line, then either every hunk's lines (when
  `diffs[i]` is `Some`) or a single line `"(diff unavailable)"` (when
  `None` — §2.1's "still shown, not dropped").
- `preview.scroll` (plain `u16` offset into the flattened lines, `Up`/
  `Down` ±1, `PageUp`/`PageDown` ±10 clamped to `0..=lines.len()`) — the
  same scroll-not-list convention `GitPanelState::diff_scroll` already
  uses for the structurally identical Git Diff view.
- Title: `"Refactor Preview  (Enter: apply, Esc: cancel, ↑↓/PgUp/PgDn:
  scroll)"`.

`handle_refactor_preview_key`:

```rust
fn handle_refactor_preview_key(&mut self, key: KeyEvent) -> LoopSignal {
    match key.code {
        KeyCode::Esc => self.cancel_refactor_preview(),
        KeyCode::Enter => self.confirm_refactor_preview(),
        KeyCode::Up => { /* preview.scroll = preview.scroll.saturating_sub(1) */ }
        KeyCode::Down => { /* preview.scroll = preview.scroll.saturating_add(1), clamped */ }
        KeyCode::PageUp => { /* -10, clamped */ }
        KeyCode::PageDown => { /* +10, clamped */ }
        _ => {}
    }
    LoopSignal::Continue
}

fn confirm_refactor_preview(&mut self) {
    let Some(preview) = self.pending_refactor_preview.take() else { return };
    match self.apply_workspace_edit(preview.edit, &preview.what) {
        Ok(file_count) => self.status = Some(format!(
            "{}: applied to {file_count} file{}",
            preview.what, if file_count == 1 { "" } else { "s" },
        )),
        Err(e) => self.status = Some(e),
    }
}

fn cancel_refactor_preview(&mut self) {
    self.pending_refactor_preview = None;
}
```

### 3.6 Key routing

`handle_key`'s popup-priority chain (`app.rs:5440-5461`) gains two entries,
placed next to the `code_actions`/`pending_rename_preview` checks they
mirror:

```rust
if self.refactor_menu.is_some() {
    return self.handle_refactor_menu_key(key);
}
// ... (existing code_actions/generate_menu/rename_popup checks here) ...
if self.pending_refactor_preview.is_some() {
    return self.handle_refactor_preview_key(key);
}
```

`any_popup_open` (`app.rs:5529`) gains the matching two `||` clauses.

## 4. Constraints & invariants

- **`via_refactor_preview` must be read-and-cleared atomically at the top
  of `handle_workspace_edit_ready`, every single call, whether or not it
  was set.** Getting the reset ordering wrong (checking without clearing,
  or clearing anywhere other than the very top) would either leak a
  preview-detour into an unrelated `Alt+Enter`/`Shift+F6` apply, or
  silently apply a refactor meant to show a preview first.
- **The preview never mutates anything.** Building it (§3.3) only reads;
  confirming it is the only path that writes, and reuses the already-
  hardened shared `apply_workspace_edit` (disk-then-buffer ordering,
  all-or-nothing rollback) rather than a new write path.
- **No new `ide-core` I/O surface.** `ide_core::diff_text` is pure — it
  operates only on the two `&str` arguments it is given
  (`git2::Patch::from_buffers` needs no `Repository`, per `refactor-
  this.md` §4's own verification, unchanged here). `show_refactor_preview`
  is the only place doing I/O (the fresh-disk-read fallback), same
  division of responsibility `ide-core`'s pure functions already keep.
- **No new security-sensitive surface.** `crates/tui/src/app.rs` is not
  itself on `CLAUDE.md`'s declared security-sensitive list, and this
  phase touches no path that is (`crates/tui/src/git_panel.rs` and its
  gutter/blame siblings are the only `crates/tui/**` entries on that
  list; this phase never touches them). `diff_text`/`apply_transaction`
  are already-reviewed `ide-core` pure functions gaining a second,
  read-only-until-Apply caller. `hacker` is not required for this phase
  — independently re-checked against the actual diff before merge
  regardless, per this project's standing practice, exactly mirroring
  `refactor-this.md` §4's own conclusion for the `ide-ui` half.

## 5. Examples

**Direct command, single unambiguous match:**

```rust
// Caret sits inside `let y = x + 1;`, selection covers `x + 1`.
// lsp.code_actions already has (ambiently, via sync_code_actions):
// [CodeAction { index: 0, title: "Extract into variable", kind:
//   Some("refactor.extract".into()), .. }, ...]

app.run_action(Action::ExtractVariable);
// -> trigger_direct_refactor(ExtractVariable) finds index 0 (kind
// prefix + "variable" in title), apply_code_action_via_preview(0):
// via_refactor_preview = true, LspRequest::ApplyCodeAction { index: 0 }.

// ... LspEvent::WorkspaceEditReady arrives, edit touches 1 file ...
app.handle_workspace_edit_ready();
// via_refactor_preview was true -> show_refactor_preview(...) instead of
// applying immediately. pending_refactor_preview is now Some, with one
// diff entry showing the extracted variable's new line plus the
// call-site's changed line.
```

**`⌃T` menu (palette-only — no default binding, §2.2) with multiple
refactor-kind actions available:**

```rust
app.trigger_refactor_this();
// refactor_menu = Some(RefactorMenuState { selected: 0 }); the popup
// lists only entries whose kind starts with "refactor" -- e.g. "Extract
// into function", "Inline variable" -- omitting any quickfix/import-style
// entries lsp.code_actions might also currently hold.

app.handle_refactor_menu_key(down_arrow); // selected = 1, "Inline variable"
app.handle_refactor_menu_key(enter);
// select_refactor_action(<that action's .index>) -- same via_refactor_
// preview flow as the direct-command example above.
```

**Preview confirm/cancel:**

```rust
app.confirm_refactor_preview();
// apply_workspace_edit(preview.edit, &preview.what) runs; on success,
// self.status names the file count, same wording every other apply path
// already produces. pending_refactor_preview = None.

// -- or --

app.cancel_refactor_preview();
// pending_refactor_preview = None. Nothing was read or written beyond
// what show_refactor_preview already did to build the (now-discarded)
// diff.
```

## 6. Dependencies & integration points

- No new dependency in any crate.
- `ide-core`/`ide-lsp`: zero changes — this phase's `diff_text`/`apply_
  transaction` calls are its second and third-plus call sites respectively
  (first port to `ide-tui`), and `ide_lsp::CodeAction` is already shared,
  unchanged.
- `ide-tui`: extends `app.rs` (state + methods, §2.1), `commands.rs` (six
  new `Action` variants + bindings, §2.2), `ui.rs` (two new render
  functions, §2.3). Does not touch `crates/tui/src/{cargo_panel.rs,
  claude_panel.rs,git_panel.rs}` or any other module.
- Builds entirely on `tui-code-actions-and-rename.md` (T13, the `CodeAction
  `/`WorkspaceEdit` machinery, `lsp.code_actions`, `apply_workspace_edit`,
  `workspace_text_edits_to_transaction`) — no new LSP request/event.

## Revision notes

`rev` (round 1) caught one real design gap in the original draft: §3.1 had
`trigger_refactor_this`/`render_refactor_menu_popup`/`handle_refactor_
menu_key` each independently re-filtering `lsp.code_actions` by `is_
refactor_kind` inline, with `handle_refactor_menu_key`'s Enter arm
specified as `handle_code_actions_key`'s direct `state.selected` →
`lsp.code_actions[selected]` lookup — correct for `handle_code_actions_key
` only because that popup shows the *unfiltered* list, but wrong here,
where list position and `CodeAction::index` diverge as soon as the filter
drops any earlier entry. Fixed by adding a single `refactor_menu_actions`
method (mirroring the already-shipped `generate_menu_actions`, `app.rs:
4847`, which has exactly this same filtered-list-vs-index shape) that
every reader — trigger, render, Up/Down clamping, Enter's lookup — goes
through, and rewriting `handle_refactor_menu_key` to mirror `handle_
generate_menu_key`'s take-selected/clear-popup/look-up-by-position-then-
resolve-`.index` shape instead. All other line-number/API citations
(`diff_text`/`apply_transaction` already `pub`, `CodeAction`'s fields,
`Ctrl+T`'s existing occupant, the full absence of any `Ctrl+Alt+{v,m,c,f,
n}` binding) were independently re-checked against current source and
confirmed accurate as originally drafted.
