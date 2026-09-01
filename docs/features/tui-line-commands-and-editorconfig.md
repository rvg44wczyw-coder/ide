# TUI Line Commands and EditorConfig (T18b)

## 1. Purpose

Ports `docs/features/line-commands-and-editorconfig.md` (roadmap phase
A4b) to `ide-tui`: Duplicate/Delete/Join/Move Line, Move Statement,
Toggle Line/Block Comment, Extend/Shrink Selection, Toggle Case, and
`.editorconfig` resolution (applied on input and on save). **Zero new
`ide-core` API** — every type and method this doc names
(`TextBuffer::{duplicate_selection_lines, delete_selection_lines,
join_selection_lines, move_selection_lines, move_selection_statements,
toggle_line_comment, toggle_block_comment, toggle_selection_case,
extended_selection}`, `LineDirection`, `word_at`, `editorconfig::{resolve,
save_edit, save_charset}`, `EditorConfig`, `Charset`,
`Buffer::save_with`) is already merged and already used by `ide-ui`
(A4b's own implementation). This is a `crates/tui/**`-only diff, and
completes T18 (`smart-editing.md` + this doc), split from T18a
(auto-indent/auto-close/matching-bracket/Tab) the same way the source A4
phase was split into A4a/A4b.

### Scope cuts specific to porting

- **No Save As in `ide-tui`.** `ide-ui`'s own port re-resolves
  `.editorconfig` after Save As, since the new path may sit under
  different rules. `ide-tui` has no Save As action at all (`SaveActive`
  is the only save path, always writing back to the tab's existing
  path) — so this port's EditorConfig wiring has exactly one call site
  (`open_or_focus_tab`) that ever resolves a config, not two.
- **No mouse, so "an edit or an arrow key" is the whole clearing
  trigger for the shrink stack.** `smart-editing.md`'s source doc says
  the stack clears "by any edit and by any selection change from
  another source (a click, an arrow key)". `ide-tui` has no click, so
  the predicate collapses to: cleared unconditionally at the top of
  `handle_editor_key` (covers every arrow move and every T18a/T18b edit
  reachable through that function), and cleared explicitly by
  `run_line_op` for the dozen actions in this doc that reach the buffer
  through `run_action` instead (Duplicate/Delete/Join/Move/*, comments,
  case) — `run_action` never routes through `handle_editor_key`, so
  those need their own clear.
- **Extend/Shrink Selection need no `Frame::rewrite`-style
  double-tap-vs-plain-arrow disambiguation.** `ide-ui`'s `⌥↑`/`⌥↓` is
  ambiguous with A3's Clone Caret (`⌥⌥`+arrow) at the pure-predicate
  level, so `ide-ui` resolves it in a second pass. `ide-tui` has no
  Clone Caret / multiple-cursors feature at all (roadmap `T20`, not yet
  started) — so `Alt+Up`/`Alt+Down` map to `ExtendSelection`/
  `ShrinkSelection` unconditionally, as plain global `Command`s exactly
  like every other action in this doc, no `rewrite`-equivalent hook
  needed.
- **Bindings are one flat table, not `{ mac, other }`.** Every binding
  below is `commands.rs`'s usual "`Ctrl`-translated form of the
  JetBrains macOS binding" — except the four rows that were never a
  `Cmd`/`Ctrl` chord to begin with (`⌥⇧↑`/`⌥⇧↓` for Move Line, `⌥↑`/`⌥↓`
  for Extend/Shrink, and `⌃⇧J` for Join Lines, already `Ctrl`-based in
  JetBrains' own macOS keymap), which are used literally, the same
  precedent `Alt+Enter`/`Shift+F6` already established in
  `tui-code-actions-and-rename.md`. `docs/roadmap.md` §5.2's source
  table's Windows/Linux half is not reproduced here for the same reason
  it wasn't in T18a: this crate has one binding table, not two.
- **`EditorConfig`'s charset notice is a `Notification`, not a status
  message.** `ide-ui` shows a one-line status the first time a tab's
  save can't honor its configured charset. `ide-tui` already has an
  in-app notification log (`App::notifications`, `docs/features/
  tui-goto-and-usages.md` §2.4) that survives past the next keystroke
  (`self.status` is typically transient, overwritten by whatever
  happens next) — `self.notify(...)` is the better fit and is reused
  verbatim rather than adding a second transient-message channel.
- **Not security-sensitive from this crate's side.** `crates/core/src/
  editorconfig.rs` is on `CLAUDE.md`'s security-sensitive list (it walks
  directories upwards from a file, reading `.editorconfig` files the
  user never opened) — but that module is unmodified by this run; this
  diff only *calls* its already-hardened, already-`hacker`-reviewed
  public API from `crates/tui/**`. Zero new `ide-core` API, so `hacker`
  is skipped here, matching every prior `T`-item's own reasoning.

## 2. Interface

### 2.1 `crates/tui/src/app.rs`

`OpenBuffer` gains four fields:

```rust
pub(crate) struct OpenBuffer {
    // ... existing fields (path, buffer, scroll, desired_column, auto_closed) ...
    /// Resolved once, at tab-open time, from `editorconfig::resolve`
    /// (`None` config for an unresolvable path -- `EditorConfig::default()`
    /// then, same as `ide-ui`). Kept separately from `indent` because
    /// `editorconfig::save_edit`/`save_charset` need the untranslated
    /// config, not the `IndentUnit` derived from it.
    config: ide_core::EditorConfig,
    /// `config.indent_style`/`config.indent_size` mapped onto
    /// `IndentUnit::default()`, replacing every `IndentUnit::default()`
    /// call T18a's editing functions made -- computed once at tab-open
    /// time (`resolve_editor_config` + `indent_unit_for`), not per
    /// keystroke.
    indent: ide_core::IndentUnit,
    /// Whether the "saved as UTF-8" notice has already fired for this
    /// tab's `config.charset` -- mirrors `ide-ui`'s
    /// `charset_notice_shown`, reset whenever `config` is (re-)applied.
    charset_notice_shown: bool,
    /// `Extend`/`Shrink Selection`'s undo-like stack of prior
    /// `Selections`, newest last. Cleared by any edit and by any arrow
    /// move (`handle_editor_key`'s own top, and `run_line_op` -- see §1).
    shrink_stack: Vec<ide_core::Selections>,
}
```

`run_action` gains twelve arms (§2.2's `Action` variants), each either a
one-line call into a new shared helper `run_line_op` or one of two new
`trigger_extend_selection`/`trigger_shrink_selection` methods (§3.4).
`open_or_focus_tab` gains two new steps, `resolve_editor_config` (resolves
the config for the tab's path) and `indent_unit_for` (pure mapping onto
`IndentUnit`), run right after `set_syntax` and before the tab is pushed.
`run_action`'s
`Action::SaveActive` arm is rewritten to run the `.editorconfig` save
sequence (§3.6) instead of a bare `buf.buffer.save()`.

### 2.2 `crates/tui/src/commands.rs`

Twelve new `Action`/`Command` entries:

| `Action` | Binding | Note |
|---|---|---|
| `DuplicateLines` | `Ctrl+D` | `⌘D` translated |
| `DeleteLines` | `Ctrl+Backspace` | `⌘⌫` translated |
| `JoinLines` | `Ctrl+Shift+J` | `⌃⇧J` -- already `Ctrl`-based, literal |
| `MoveLinesUp` / `MoveLinesDown` | `Alt+Shift+Up` / `Down` | `⌥⇧↑`/`⌥⇧↓` -- not a `Cmd`/`Ctrl` chord, literal |
| `MoveStatementsUp` / `MoveStatementsDown` | `Ctrl+Shift+Up` / `Down` | `⌘⇧↑`/`⌘⇧↓` translated |
| `ToggleLineComment` | `Ctrl+/` | `⌘/` translated |
| `ToggleBlockComment` | `Ctrl+Alt+/` | `⌘⌥/` translated |
| `ExtendSelection` / `ShrinkSelection` | `Alt+Up` / `Alt+Down` | `⌥↑`/`⌥↓` -- literal, see §1 |
| `ToggleCase` | `Ctrl+Shift+U` | `⌘⇧U` translated |

`LineDirection` is not threaded through the `Action` enum (which stays
unit-variants-only, this file's existing convention -- see `NextTab`/
`PreviousTab`) — Move Line/Move Statement get one `Action` variant per
direction instead of one variant carrying a `LineDirection`.

### 2.3 `crates/tui/src/lib.rs`

No change. `.editorconfig` resolution is synchronous, at tab-open time
only (§3.6), not an ambient per-frame refresh — nothing to add to the
run loop's poll sequence.

## 3. Behaviour

### 3.1 Line operations and Move Statement

Each of `DuplicateLines`/`DeleteLines`/`JoinLines`/`MoveLinesUp`/`Down`/
`MoveStatementsUp`/`Down` runs through the new `run_line_op` helper
(§3.4), which calls straight through to the `ide_core::TextBuffer`
method of the same name and lets that method's own contract (one
`Transaction`, selections carried or set explicitly) stand unmodified —
this port adds no line-operation logic of its own, exactly as T18a
added none for auto-indent/auto-close beyond wiring `ide_core`'s
functions into `handle_editor_key`.

### 3.2 Comments and Toggle Case

`ToggleLineComment` reads `buf.indent` (§2.1) and passes it to
`toggle_line_comment` — the only one of the twelve actions that needs a
value beyond the `TextBuffer` itself. `ToggleBlockComment`/`ToggleCase`
call their `TextBuffer` methods directly.

### 3.3 Extend and Shrink Selection

`trigger_extend_selection`: reads the primary selection, calls
`extended_selection`; `None` (already the whole buffer) is a no-op.
`Some(extended)` pushes the *pre-extension* `Selections` onto
`buf.shrink_stack` and installs the extended range as the sole
selection. `trigger_shrink_selection`: pops `shrink_stack` and restores
it verbatim if non-empty; an empty stack falls back to `ide_core::word_at`
under the primary caret, then to a bare caret if that returns `None` —
exactly `line-commands-and-editorconfig.md` §3.4's fallback chain, using
`ide_core::word_at` rather than this crate's own `editor::word_range_at`
(`smart-editing.md`'s port already established that these two exist for
different reasons — a hover/rename target vs. a selection-hierarchy
step — and are not interchangeable).

### 3.4 `run_line_op`, the shared wiring

```rust
fn run_line_op(&mut self, op: impl FnOnce(&mut TextBuffer, IndentUnit) -> bool) {
    // fetch active buffer, run `op` against its TextBuffer with its
    // resolved `indent`, mark dirty + clear shrink_stack + re-sync
    // scroll/LSP on a real change, no-op otherwise.
}
```

Every line/comment/case action goes through this one function so the
dirty-marking, shrink-stack-clearing and LSP-resync steps are written
once. `ExtendSelection`/`ShrinkSelection` do **not** go through it: they
never mark the buffer dirty (moving a selection is not an edit) and must
not clear the very stack they exist to maintain.

### 3.5 `.editorconfig` on open

`open_or_focus_tab` calls `self.resolve_editor_config(&path)` (which wraps
`editorconfig::resolve(self.project_root(), &path).unwrap_or_default()`)
right after `syntax_for_path`, then `indent_unit_for(&config)`, which maps
`indent_style`/`indent_size` onto an `IndentUnit` seeded from
`IndentUnit::default()` (only the fields the config actually set override
the default, exactly as `ide-ui`'s own `Tab::apply_editor_config` does)
and stores both the raw `config` and the derived `indent` on the new tab.

### 3.6 `.editorconfig` on save

`Action::SaveActive`'s arm becomes:

```rust
if let Some(edit) = editorconfig::save_edit(buf.buffer.text(), &buf.config) {
    buf.buffer.apply(edit); // marks dirty, one undo step, carries every caret
}
if let Err(err) = buf
    .buffer
    .save_with(editorconfig::save_charset(&buf.config))
{
    self.status = Some(err.to_string());
} else if !buf.charset_notice_shown
    && matches!(buf.config.charset, Some(Charset::Latin1 | Charset::Utf16Le | Charset::Utf16Be))
{
    buf.charset_notice_shown = true;
    self.notify(format!(
        "{} was saved as UTF-8: {:?} from .editorconfig isn't supported",
        buf.path.display(),
        buf.config.charset.expect("just matched Some above"),
    ));
}
```

`save_with(None)` behaves exactly like the pre-T18b `save()` for a tab
whose config names no charset (or one the buffer can already represent),
so every existing `SaveActive` test's expectations hold unless the
config specifically sets a charset `save_with` cannot honor.

## 4. Constraints & invariants

- Zero new `ide-core`/`ide-lsp` public API (§1).
- `shrink_stack` is cleared by every path in and out of
  `handle_editor_key` and by `run_line_op` — never by
  `trigger_extend_selection`/`trigger_shrink_selection` themselves,
  which only push/pop it.
- `buf.indent` is computed once per tab-open (or reopen), never
  recomputed per keystroke — every T18a call site that used
  `IndentUnit::default()` directly is updated to read `buf.indent`
  instead, so `.editorconfig`'s `indent_style`/`indent_size` now affect
  auto-indent, Tab, and comment alignment exactly as
  `line-commands-and-editorconfig.md` §3.6 requires.
- Every byte offset comes from `LineIndex`, `tokens()`, or a
  `char_indices` walk, inherited unmodified from the `ide_core` methods
  this port calls — no raw arithmetic indexing added here.
- `MAX_EDITORCONFIG_DEPTH`/`MAX_EDITORCONFIG_BYTES`/
  `MAX_EDITORCONFIG_SECTIONS` are `ide-core` invariants, unchanged and
  un-re-validated by this port.

## 5. Examples

**Duplicate then comment, one undo step each:**

```text
foo
```
`Ctrl+D` → `foo\nfoo` (caret on the copy) → `Ctrl+/` → `foo\n// foo`

**Extend then shrink:**

```text
f(x + 1)   <- caret inside `x`
```
`Alt+Up` → selection = `x` → `Alt+Up` → selection = `x + 1` → `Alt+Down`
→ back to `x`.

## 6. Dependencies & integration points

- No new dependency.
- Does not touch `ide-lsp`.
- Depends on T18a (`tui-smart-editing.md`) for `OpenBuffer`'s existing
  shape and for `IndentUnit`/`SyntaxRules::brackets` already being in
  scope; must merge after it (already true — T18a is `main` by the time
  this doc's implementation starts).

## Revision notes

Self-review round (inline, no `hacker` pass per §1's reasoning): the
implementation split tab-open-time config resolution into two functions,
`resolve_editor_config` (wraps `editorconfig::resolve`) and
`indent_unit_for` (the pure `EditorConfig` → `IndentUnit` mapping),
rather than the single `apply_editor_config` step this doc originally
named — §2.1/§3.5 and `OpenBuffer`'s own doc comments updated to match.
Binding table cross-checked against `commands.rs` line by line: no
collisions with any of the 33 registered commands. Diff confirmed
`crates/tui/**`-only (plus this doc) via `git diff --name-only main`.
