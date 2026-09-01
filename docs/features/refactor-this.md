# Refactor This (D2)

## 1. Purpose

`code-actions.md` (A8) already fetches every `CodeAction` available at the
caret and can apply one — but the menu it opens (`⌥↩`, "Intention
Actions") shows *everything* a server offers, ungrouped, applied
immediately with no preview. This phase adds JetBrains' "Refactor This"
(`⌃T`) on top of that same already-fetched data: a menu **filtered to
refactoring-kind actions only**, plus five direct-invoke commands (Extract
Variable/Method/Constant/Field, Inline) that skip the menu when there's
exactly one obvious match. Both paths funnel through a **new shared
Preview dialog that renders an actual line-level diff** of the
`WorkspaceEdit` before it's applied — a step up from `rename-refactoring
.md` (D1)'s own preview, which only shows a file-and-occurrence-count
list. Nothing here adds a new LSP request kind or changes how a
`CodeAction` is fetched/resolved/converted — this phase is entirely about
*which* cached actions a command reaches for, and *what the user sees*
before applying one.

**Scope, stated up front:**

- No new wire traffic. `⌃T` and the five Extract/Inline commands all read
  from `lsp.code_actions`, the exact same ambiently-kept-fresh cache
  `⌥↩` already reads from (`code-actions.md` §2.3) — same "up to one
  round trip" latency if the caret only just moved.
- **Matching a specific refactor kind to a cached `CodeAction` is a
  title/kind heuristic, not a new protocol concept** — LSP has no
  `CodeActionKind` finer than `"refactor.extract"`/`"refactor.inline"`;
  distinguishing "extract into variable" from "extract into function"
  within that has to read the server's own `title` string (§3.2). This is
  inherent to the protocol, not a shortcut this phase is taking.
- **Extract Field has no rust-analyzer equivalent today** (Rust doesn't
  have Java/C#-style field extraction the way JetBrains' generic keymap
  terminology assumes). The command is still registered and bound — per
  `CLAUDE.md`'s "never invent a binding, use the JetBrains binding
  verbatim" rule, the binding exists regardless of whether *this*
  server ever offers a matching action — it will commonly report "not
  available here" for Rust code, which is correct behavior, not a bug.
- **No disambiguation submenu.** If more than one cached action matches a
  direct command's heuristic (rare in practice — rust-analyzer doesn't
  typically offer two "extract into variable"-shaped actions at the same
  caret position), the first match (server's own response order) wins.
  Documented v1 simplification, same shape as every other "don't build
  UI for an edge case that's not the common path" cut this project has
  made before (e.g. `code-actions.md` §1's own deferred `CodeActionKind`
  grouping).
- **No mnemonic/letter-key selection inside the `⌃T` popup.** `docs/
  roadmap.md` §5.1 names "`⌃T` → буква" as the eventual chord shape once
  `G2`'s prefix/accord mechanism is generalized to cover it, but that
  generalization is `G2`'s own scope, already merged for double-tap
  (`⇧⇧`, `⌥⌥`+arrow) and not yet extended to letter-mnemonic popups. This
  phase's `⌃T` opens an ordinary list popup — arrow-key nav + Enter, the
  same interaction every other popup in this app already uses
  (`render_usages_popup`, `render_code_actions_popup`, `render_goto_
  popup`) — not literal single-letter selection. Revisit if that specific
  interaction is ever requested; nothing here blocks it.

Does not touch `crates/lsp/**` or `crates/dap/**` at all — every change
is in `ide-core` (one new pure function, one visibility change) and
`ide-ui`.

## 2. Interface

### 2.1 `ide-core`

```rust
// crates/core/src/git/mod.rs -- new free function, no Repository needed
/// Line-level diff between two arbitrary in-memory texts -- not backed by
/// any git object or working tree. Built on `git2::Patch::from_buffers`
/// (no `Repository` required at all), reusing this module's existing
/// line/hunk extraction and post-processing (`truncate_file_diff`,
/// `pair_intraline_spans`) so the result renders through the exact same
/// `Self::render_diff` the Source Control view already uses -- one
/// diff-rendering code path for both git diffs and this phase's
/// refactor-preview diffs.
pub fn diff_text(path: &Path, old: &str, new: &str) -> Option<FileDiff>;
```

Returns `None` when `old == new` (no visible change — same convention
`diff_file` already establishes for "nothing to show"). `path` is used
only for the returned `FileDiff`'s `old_path`/`new_path` (both set to the
same value — this is never a rename), display purposes only.

```rust
// crates/core/src/workspace_edit.rs -- visibility change, no signature
// change and no behavioural change
pub fn apply_transaction(content: &str, transaction: &Transaction) -> Option<String>;
```

Was a private helper `apply_workspace_edit_to_disk` already used
internally; this phase is its second caller (§3.3) — made `pub` (and
added to `crates/core/src/lib.rs`'s re-export list) rather than
duplicated, since it already does exactly "apply a `Transaction` to a
`String`, `None` if any change's range doesn't fit," identically to what
the refactor preview needs to compute a file's post-edit text. No change
to its existing strict (non-clamping) bounds-check behavior — see its own
doc comment, unchanged.

### 2.2 `ide-ui`

```rust
// crates/ui/src/app.rs -- new state on IdeApp
struct RefactorPreview {
    what: String,
    edit: ide_lsp::WorkspaceEdit,
    /// One entry per `FileEdit` in `edit.edits`, same order. `None`
    /// means the diff itself couldn't be computed (the file couldn't be
    /// read, or `apply_transaction` rejected an out-of-range edit) -- the
    /// row still renders (path + "(diff unavailable)"), it is not
    /// dropped from the list, matching `code-actions.md` §4's "a
    /// `WorkspaceEdit` is validated and applied as one unit" spirit: this
    /// preview must show every file the edit touches, not a filtered
    /// subset, even when one file's diff preview specifically fails.
    diffs: Vec<Option<ide_core::FileDiff>>,
}
```

- `pending_refactor_preview: Option<RefactorPreview>` — presence is
  visibility, same convention `pending_rename_preview` already uses.
- `show_refactor_menu_popup: bool` — mirrors `show_code_actions_popup`.
- `via_refactor_preview: bool` — set immediately before this phase's
  code sends `LspRequest::ApplyCodeAction`, cleared unconditionally
  inside `handle_workspace_edit_ready` the next time it observes
  `lsp.workspace_edit_ready` (§3.4). Routes that one, specific
  `WorkspaceEditReady` into `show_refactor_preview` instead of
  `handle_workspace_edit_ready`'s existing immediate-apply body — the
  *only* new piece of control flow this phase adds to that shared event
  path; `⌥↩`'s own direct `select_code_action` calls leave this `false`
  and keep applying immediately, unchanged.

New methods:

```rust
impl IdeApp {
    /// `⌃T`'s entry point.
    fn trigger_refactor_this(&mut self);
    /// A `⌃T` popup row click: closes the popup, routes the click through
    /// `apply_code_action_via_preview` (below).
    fn select_refactor_action(&mut self, index: usize);
    /// The five direct commands' shared entry point.
    fn trigger_direct_refactor(&mut self, kind: DirectRefactorKind);
    /// Shared by `select_refactor_action` and `trigger_direct_refactor`:
    /// sets `via_refactor_preview = true`, calls
    /// `self.lsp.apply_code_action(index)`.
    fn apply_code_action_via_preview(&mut self, index: usize);
    /// Builds `pending_refactor_preview` from a ready `WorkspaceEdit`:
    /// for each `FileEdit`, reads old text (open tab's buffer if any,
    /// else a fresh disk read -- same source `apply_workspace_edit`
    /// already reads from), computes new text via
    /// `workspace_text_edits_to_transaction` +
    /// `ide_core::apply_transaction`, and diffs old/new via
    /// `ide_core::diff_text`.
    fn show_refactor_preview(&mut self, what: String, edit: ide_lsp::WorkspaceEdit);
    /// The preview's Apply button: calls the existing shared
    /// `apply_workspace_edit` primitive (`rename-refactoring.md` §2.3),
    /// same success/failure message shape every other apply path uses.
    fn confirm_refactor_preview(&mut self);
    /// The preview's Cancel button / window close: clears
    /// `pending_refactor_preview`, no I/O (matches `pending_rename_
    /// preview`'s Cancel semantics exactly -- the request already
    /// completed and was answered; cancelling only declines to apply it).
    fn cancel_refactor_preview(&mut self);
}

/// The five direct commands, one heuristic each (§3.2) -- a closed enum
/// rather than five near-identical methods, since `trigger_direct_
/// refactor`'s body is otherwise byte-for-byte the same for all five.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectRefactorKind {
    ExtractVariable,
    ExtractMethod,
    ExtractConstant,
    ExtractField,
    Inline,
}
```

`command.rs` gains six `CommandAction` variants — `RefactorThis`,
`ExtractVariable`, `ExtractMethod`, `ExtractConstant`, `ExtractField`,
`Inline` — all in the existing `"Refactor"` category (`rename-
refactoring.md` §2.3 created it for `Rename`, this phase is its second
member). Bindings (`docs/roadmap.md` §5.2, all `Binding::same` — every
one is a pure Cmd/Ctrl-abstracted or literal-Ctrl chord, no `{mac,
other}` divergence):

| Command | Chord |
|---|---|
| `RefactorThis` | `KeyChord::new(Key::T).ctrl()` — literal Control, the same `ctrl()` (not `command()`) helper `GoToTypeDeclaration` already uses for its own genuinely-Ctrl-on-mac binding |
| `ExtractVariable` | `KeyChord::new(Key::V).command().alt()` |
| `ExtractMethod` | `KeyChord::new(Key::M).command().alt()` |
| `ExtractConstant` | `KeyChord::new(Key::C).command().alt()` |
| `ExtractField` | `KeyChord::new(Key::F).command().alt()` |
| `Inline` | `KeyChord::new(Key::N).command().alt()` |

`is_command_enabled` gates all six identically to `Rename`: an active tab
with a path **and** `self.lsp.is_running()`.

```rust
// crates/ui/src/app/render.rs -- two new popups
fn render_refactor_menu_popup(&mut self, ctx: &egui::Context);   // "Refactor This"
fn render_refactor_preview(&mut self, ctx: &egui::Context);      // "Refactor Preview"
```

No new `CodeEditor` builder method, no new gutter marker — the existing
lightbulb from A8 is what `⌃T`/the five direct commands key off of; this
phase adds no second ambient indicator.

## 3. Behaviour

### 3.1 `trigger_refactor_this` / the `⌃T` menu

No-op with `self.error = Some("Refactor This: no refactoring available
here".into())` if `lsp.code_actions.iter().filter(|a|
is_refactor_kind(a)).next()` is `None` (§3.2's `is_refactor_kind`).
Otherwise `self.show_refactor_menu_popup = true` — no new request, same
"open on whatever's cached" shape `trigger_show_intention_actions`
already established.

`render_refactor_menu_popup` renders exactly the same row shape
`render_code_actions_popup` already does (title, `★` for `is_preferred`,
`(kind)` subtitle, disabled+greyed with `disabled_reason` as a tooltip)
but iterates `lsp.code_actions.iter().filter(|a| is_refactor_kind(a))`
instead of the full list — title `"Refactor This"`, not `"Intention
Actions"`. A row click calls `select_refactor_action(action.index)` (the
action's own `index` field, same "index is the token, not list
position" discipline `select_code_action` already follows).

### 3.2 Kind/title matching

```rust
fn is_refactor_kind(action: &ide_lsp::CodeAction) -> bool {
    action.kind.as_deref().is_some_and(|k| k.starts_with("refactor"))
}
```

`trigger_direct_refactor(kind)` finds the first entry in `lsp.code_actions`
**with `disabled_reason: None`** (a disabled action is never a valid
auto-selection target — matches `render_code_actions_popup`'s existing
"disabled entries are shown but never selectable" rule, applied here to
an automatic match instead of a click) matching `kind`'s heuristic
(case-insensitive substring match against `title`, plus a `kind`-prefix
check first so an unrelated `quickfix` action never matches by title
alone):

| `DirectRefactorKind` | `kind` prefix | `title` must contain (any) |
|---|---|---|
| `ExtractVariable` | `"refactor.extract"` | `"variable"` |
| `ExtractMethod` | `"refactor.extract"` | `"function"`, `"method"` |
| `ExtractConstant` | `"refactor.extract"` | `"constant"` |
| `ExtractField` | `"refactor.extract"` | `"field"` |
| `Inline` | `"refactor.inline"` | *(any title)* |

No match → `self.error = Some(format!("{name}: not available here"))`
where `name` is the command's own title (e.g. `"Extract Variable"`). A
match → `apply_code_action_via_preview(action.index)`.

### 3.3 `show_refactor_preview`

For each `FileEdit` in `edit.edits`, in order:

1. Old text: the open tab's `buffer.text()` if one exists for that path,
   else `std::fs::read_to_string` — identical source selection
   `apply_workspace_edit` already uses (§2.3's doc comment cross-
   references it deliberately: same two-branch lookup, not a second
   implementation of it).
2. `workspace_text_edits_to_transaction(&old_text, &file_edit.text_edits)`
   → `Option<Transaction>`; `None` (shouldn't happen for an edit that
   already round-tripped through the same conversion once to get here,
   but not assumed impossible) → this file's diff entry is `None`.
3. `ide_core::apply_transaction(&old_text, &transaction)` → `Option<
   String>` (new text); `None` (out-of-range) → this file's diff entry
   is `None`.
4. `ide_core::diff_text(&file_edit.path, &old_text, &new_text)` → this
   file's diff entry (`None` if unchanged, which is possible but unusual
   for a server-computed edit).

Sets `pending_refactor_preview = Some(RefactorPreview { what, edit,
diffs })`. Never touches disk or any buffer — building the preview is
read-only, symmetric with `handle_rename_ready`'s own preview-escalation
path never applying anything either.

### 3.4 `handle_workspace_edit_ready`'s new branch

Immediately after the method's existing `if !self.lsp.workspace_edit_ready
{ return; }` guard, before `what` is even computed: `let via_preview =
std::mem::take(&mut self.via_refactor_preview);` — unconditional
take-and-reset on *every* real event this method processes, not only
ones that turn out to have a usable edit, so a stray `true` can never
leak into a later, unrelated apply (mirrors `format_ready`'s own "always
reset at the top" fix from `formatting.md`'s post-review round, applied
here to a boolean gate rather than a self-resolving one, same reasoning:
never trust a flag to still mean what it meant when it was set unless
it's read and cleared atomically, on every path through the method, not
just the one the flag was originally meant for).

- The `edit: None` branch (`"{what}: nothing to apply"`) is completely
  unchanged — `via_preview` is never consulted there, since there is
  nothing to preview.
- Inside the `Some(edit)` branch only: `via_preview == true` →
  `self.show_refactor_preview(what, edit)` instead of the method's
  existing apply-immediately body. This is the **entire** behavioural
  change to this method — the error-message wording and every other
  branch are untouched.
- `via_preview == false` → existing behavior, completely unchanged.

### 3.5 The preview dialog

`render_refactor_preview`, an `egui::Window` titled `"Refactor Preview"`
(joining `"Rename Preview"`/`"Intention Actions"`/`"Git Change"` as this
app's established popup-naming convention), shown whenever
`pending_refactor_preview.is_some()`:

- A summary line: `"{what}: {N} file{s}"` where `N =
  preview.edit.edits.len()`.
- One block per file, in `edit.edits`' order: the path as a sub-heading,
  then either `Self::render_diff(ui, tokens, std::slice::from_ref(diff))`
  (reusing the Source Control view's own diff renderer verbatim — same
  `egui::Grid`/`ScrollArea` gutter-numbered rendering, so a refactor's
  preview looks exactly like every other diff already in this app) when
  `diffs[i]` is `Some`, or the label `"(diff unavailable — see file list
  above)"` when it's `None` (§2.2's "still shown, not dropped").
- **Apply**: `self.confirm_refactor_preview()`.
- **Cancel**, or the window's own close button: `self.cancel_refactor_
  preview()`.

### 3.6 Escape

`show_refactor_menu_popup` and `pending_refactor_preview` join the
existing `Esc`-closes-the-topmost-popup priority chain in `handle_
shortcuts` (`rename-refactoring.md` §3.6 lists the current members) —
same rule, `Esc` closes whichever of these is open without applying
anything, before falling through to the editor's own handling.

## 4. Constraints & invariants

- **`via_refactor_preview` must be read-and-cleared atomically at the top
  of `handle_workspace_edit_ready`, every single call, whether or not it
  was set.** This is the one piece of new shared-state control flow this
  phase adds to an existing method; getting the reset ordering wrong
  (checking it without clearing, or clearing it somewhere other than the
  very top) would either leak a preview-detour into an unrelated `⌥↩`
  apply, or silently apply a refactor that was supposed to show a preview
  first — the exact class of bug `formatting.md`'s own `format_ready`
  post-review fix already exists as precedent for.
- **`ide_core::diff_text` is pure and has no filesystem/network access of
  its own** — it operates only on the two `&str` arguments it's given
  (`git2::Patch::from_buffers` needs no `Repository`, confirmed by
  reading the vendored `git2` source before choosing this API over
  hand-rolling a diff algorithm). The caller (`show_refactor_preview`)
  is the only place doing I/O (the fresh-disk-read fallback), same
  division of responsibility every other `ide-core` pure function in this
  codebase already keeps.
- **The preview never mutates anything.** Building it (§3.3) only reads;
  confirming it (§3.5's Apply) is the only path that writes, and reuses
  the already-hardened shared `apply_workspace_edit` primitive
  (disk-then-buffer ordering, all-or-nothing rollback,
  `code-actions.md` §3.4) rather than a new write path.
- **No new security-sensitive surface.** `diff_text` reads no path
  itself; `apply_transaction`'s visibility change exposes an already-
  reviewed, already-tested pure string function, with no behavioural
  change. `crates/core/src/workspace_edit.rs` is already on `CLAUDE.md`'s
  security-sensitive list (added when `code-actions.md` merged) — this
  phase doesn't change what that module writes or how, only adds a
  second, read-only caller of one of its existing helpers. `hacker` is
  not automatically required for this phase; still independently
  re-checked against the actual diff before merge per this project's own
  standing practice.

## 5. Examples

**Direct command, single unambiguous match:**

```rust
// Caret sits inside `let y = x + 1;`, selection covers `x + 1`.
// lsp.code_actions already has (ambiently, via sync_code_actions):
// [CodeAction { index: 0, title: "Extract into variable", kind:
//   Some("refactor.extract".into()), .. }, ...]

app.run_command(CommandAction::ExtractVariable, &ctx);
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

**`⌃T` menu with multiple refactor-kind actions available:**

```rust
app.trigger_refactor_this();
// show_refactor_menu_popup = true; the popup lists only entries whose
// kind starts with "refactor" -- e.g. "Extract into function",
// "Inline variable" -- omitting any quickfix/import-style entries
// lsp.code_actions might also currently hold.

app.select_refactor_action(2); // "Inline variable"
// apply_code_action_via_preview(2) -- same via_refactor_preview flow as
// the direct-command example above.
```

**Preview confirm/cancel:**

```rust
app.confirm_refactor_preview();
// self.apply_workspace_edit(preview.edit, &preview.what) runs; on
// success, self.error names the file count, same wording every other
// apply path already produces. pending_refactor_preview = None.

// -- or --

app.cancel_refactor_preview();
// pending_refactor_preview = None. Nothing was read or written beyond
// what show_refactor_preview already did to build the (now-discarded) diff.
```

## 6. Dependencies & integration points

- No new external dependency in any crate — `diff_text` reuses `git2`
  (already a dependency), specifically `Patch::from_buffers`, which needs
  no `Repository`/no filesystem access of its own.
- `ide-core`: `crates/core/src/git/mod.rs` gains `diff_text`;
  `crates/core/src/workspace_edit.rs`'s `apply_transaction` goes from
  private to `pub`; `crates/core/src/lib.rs`'s re-export list gains both.
  No other `ide-core` file changes.
- `ide-ui`: extends `app.rs` (state + the methods in §2.2),
  `app/render.rs` (two new popups), `command.rs` (six new commands, one
  new enum). Does not touch `crates/ui/src/cargo_panel.rs` or
  `crates/ui/src/claude_panel.rs`.
- Builds entirely on `code-actions.md` (A8, the `CodeAction`/
  `WorkspaceEdit` machinery and the shared `apply_workspace_edit`
  primitive `rename-refactoring.md` (D1) already extracted) — no new LSP
  request/event, no change to `ide-lsp`'s public API at all.

## Revision notes

Implemented and verified 2026-08-28, both halves in one pass (no
cross-crate dependency to stage separately beyond the two `ide-core`
commits already needed for `diff_text`/`apply_transaction`).

- Self-review before implementing caught two real gaps in the original
  draft, both fixed before writing any code: (1) `trigger_direct_
  refactor`'s heuristic match didn't exclude actions with a
  `disabled_reason` set, which would have auto-applied a disabled action;
  (2) the `via_refactor_preview` reset was originally specified as
  happening only inside the `Some(edit)` branch, which would have left a
  stale `true` set across a `None`-edit response and leaked into the
  *next* unrelated apply. Both are reflected in §3.2/§3.4 as they stand
  now, not as originally drafted.
- `git2::Patch::from_buffers`/`hunk`/`line_in_hunk` (used by `ide-core`'s
  `diff_text`, committed as part of this phase's `ide-core` half) needed
  no API surprises beyond what `code-actions.md`'s prior `git2` API
  discoveries already established — confirmed by reading the vendored
  source before using it, same discipline as every prior phase's git2
  usage in this codebase.
- Coverage: `app.rs` 96.46%, `command.rs` 99.33% (both `cargo llvm-cov`,
  cache cleaned before measuring). `app/menu.rs` measures 44.40% overall,
  but that is a pre-existing condition of its `#[cfg(target_os =
  "macos")]`-gated `muda` calls (untestable without a live window
  system), not something this phase's diff worsened — this phase only
  appended six entries to the already-tested `MENU_GROUPS` static array,
  exercised by the existing `menu_groups_reference_only_real_commands`/
  `every_non_build_command_appears_in_the_native_menu_exactly_once`
  tests (both still passing).
- No security-sensitive path touched (`git diff --name-only` against
  `main`: `crates/ui/src/{app.rs,app/menu.rs,app/render.rs,command.rs}`
  only) — no `hacker` pass required, confirmed against `CLAUDE.md`'s list
  rather than assumed from the doc's own §4 prediction.
- Full workspace `cargo fmt --all -- --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo build --workspace --all-targets`,
  and `cargo test --workspace` all green (501 `ide-core` + 784 `ide-ui`
  relevant totals).
