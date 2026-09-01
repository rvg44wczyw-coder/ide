# TUI Smart Editing (T18a)

## 1. Purpose

Ports `docs/features/smart-editing.md` (roadmap phase A4a) to `ide-tui`:
auto-indent on `Enter`, auto-close/type-over for brackets and quotes,
surround-by-typing, matching-bracket highlight + jump, and `Tab`/`Shift+Tab`
indent/outdent. **Zero new `ide-core` API** — every type and method this
doc names (`IndentUnit`, `newline_indent`, `splits_a_pair`,
`TextBuffer::{syntax, matching_bracket, indent_selection_lines,
outdent_selection_lines, surround_selections}`, `SyntaxRules::{brackets,
indent_line_suffixes}`, `BracketPair`, `MAX_BRACKET_SCAN_BYTES`) is
already merged and already used by `ide-ui`. This is a
`crates/tui/**`-only diff.

**Scope split, mirroring `smart-editing.md`'s own A4a/A4b split.** The
source phase was itself split into A4a (this doc's source) and
`line-commands-and-editorconfig.md` (A4b — Duplicate/Delete/Join/Move
Line, Move Statement, Toggle Comment, Extend/Shrink Selection, Toggle
Case, and `.editorconfig`), specifically because "nothing in A4a depends
on A4b, so A4a can merge and ship on its own." That boundary holds
exactly as well for a TUI port: this run covers only A4a's features.
`docs/roadmap.md`'s T18 row is split into **T18a** (this doc) and
**T18b** (line commands + EditorConfig, a separate follow-up batch) to
match. `IndentUnit::default()` (four spaces) is used everywhere below —
there is no `.editorconfig` reader in `ide-tui` yet; T18b is where one
would change what `IndentUnit` resolves to, exactly as A4b does for
`ide-ui`.

### Scope cuts specific to porting

- **Single selection only.** `ide-tui` has no extend-selection
  (`Shift`+arrow) of any kind yet — every arrow keystroke collapses to a
  bare caret (confirmed by reading `handle_editor_key`: it always
  constructs `Selections::single(Selection::caret(new_offset))`). A
  non-empty selection can still exist here, though — a Goto/Find-Usages
  jump, an in-buffer Find match, or a future feature can already leave one
  — so "surround a selection" and "`Tab`/`Shift+Tab` over a selection"
  are real, reachable behaviors, just reached less often than in `ide-ui`
  today. Every operation below still goes through `ide-core`'s
  selection-generic methods (`indent_selection_lines`,
  `surround_selections`, ...), so none of this needs revisiting once a
  future batch adds extend-selection.
- **No pair-highlight recomputation cache.** `smart-editing.md` §3.4 notes
  the pair is "recomputed once per frame after input, from the primary
  selection." `ide-tui`'s `render_editor` already recomputes
  viewport-scoped overlays (semantic tokens, document highlights, inlay
  hints) fresh every frame from scratch — `matching_bracket` is cheap
  enough (bounded by `MAX_BRACKET_SCAN_BYTES`, and only runs once per
  frame against the *primary* selection) to join that same fresh-every-
  frame group rather than introduce a new cached, invalidated-on-input
  field the way `EditorState::bracket_pair` is in `ide-ui`. Behaviorally
  identical; simpler for this crate's existing render loop shape.
- **`JumpToMatchingBracket` has no default binding**, exactly as in
  `ide-ui` — `smart-editing.md` §2.6 is explicit that no JetBrains
  binding exists for this action, so per `CLAUDE.md`'s "never invent a
  binding" rule it's palette-only here too, joining `ToggleNotifications`/
  `ToggleCargoPanel`/`ToggleGitPanel` in that category.
- **`Tab`/`BackTab` are literal `KeyCode`s here, not an `Intent`/`rewrite`
  split.** `ide-ui`'s `Frame::rewrite` decides between `Intent::Insert`
  and `Intent::Indent` for a bare `Tab` because `intent_for` is a pure
  function with no access to selection state. `ide-tui`'s
  `handle_editor_key` already has full `&mut OpenBuffer` access at the
  point it matches `key.code`, so the equivalent decision (insert one
  indent unit vs. indent every touched line) is a plain `if` inside one
  match arm — no analogous split is needed. `Shift+Tab` is
  `crossterm::event::KeyCode::BackTab` (confirmed by reading
  `crossterm`'s own unix and Windows key parsers: `CSI Z` and
  `VK_TAB`+`SHIFT` both produce `BackTab` with `KeyModifiers::SHIFT` set,
  never `Tab` with a shift bit) — this crate's existing Kitty-protocol
  opt-in changes nothing about that mapping, so no Ctrl-masking-style
  disambiguation is needed here the way it is for `Ctrl+<letter>` chords.
- **Not security-sensitive.** Zero new `ide-core` API and no path on
  `CLAUDE.md`'s security-sensitive list is touched — `hacker` is skipped,
  matching every prior `T`-item's own reasoning.

## 2. Interface

### 2.1 `crates/tui/src/app.rs`

`OpenBuffer` gains one field:

```rust
pub(crate) struct OpenBuffer {
    // ... existing fields ...
    /// The offset of a closing delimiter *this crate's own* auto-close
    /// inserted on the immediately preceding keystroke, if any -- ported
    /// from `ide-ui`'s `EditorState::auto_closed` (`smart-editing.md`
    /// §2.7/§3.2), collapsed from `Vec<usize>` to `Option<usize>` since
    /// this crate has only ever one caret. Consumed (taken) at the top of
    /// every `handle_editor_key` call; only the `Char(c)` arm may set it
    /// again afterward.
    auto_closed: Option<usize>,
}
```

`handle_editor_key` is restructured (§3 below); no new `pub(crate)`
methods beyond the existing ones. `run_action` gains one arm:
`Action::JumpToMatchingBracket => self.trigger_jump_to_matching_bracket()`.

### 2.2 `crates/tui/src/commands.rs`

One new `Action`/`Command` entry:

```rust
Command {
    id: "JumpToMatchingBracket",
    title: "Jump to Matching Bracket",
    binding: None, // no JetBrains binding exists -- see §1
    action: Action::JumpToMatchingBracket,
}
```

### 2.3 `crates/tui/src/highlight.rs`

`LineOverlays` gains one field, and `styled_line` one more background
check, both following the exact pattern `highlights` (document-highlight
wash) already established:

```rust
pub struct LineOverlays<'a> {
    pub semantic_tokens: &'a [Token],
    pub highlights: &'a [Range<usize>],
    pub inlay_hints: &'a [(usize, String)],
    /// The matched bracket pair's two ranges (open, close), or empty when
    /// the caret isn't on a bracket or it's unmatched. A distinct
    /// background from `highlights`' `Color::DarkGray` wash (§3.3).
    pub bracket_pair: &'a [Range<usize>],
}
```

### 2.4 `crates/tui/src/ui.rs`

`render_editor` computes the primary caret's `matching_bracket` fresh each
frame (§1's "no cache" scope cut) and passes its two ranges (or an empty
slice) as `LineOverlays::bracket_pair`. Pure rendering — no test
obligation beyond what already covers `highlight.rs`.

## 3. Behaviour

### 3.1 `Enter` — auto-indent

`handle_editor_key`'s `KeyCode::Enter` arm replaces its previous bare
`type_text("\n")` with `ide_core::indent::newline_indent` exactly per
`smart-editing.md` §3.1: the new line's indentation is the current line's
own indent, plus one level after an unclosed bracket or a trailing
`indent_line_suffixes` match (never both at once), minus one level before
a dangling closer. `splits_a_pair` at an empty-selection caret triggers
the three-line `{|}` expansion (indented line, then the closer's own line
at the original indent, caret on the first), exactly as `ide-ui` does —
one `Transaction`, one undo step, `IndentUnit::default()` throughout (§1).

### 3.2 Auto-close, type-over, and surround

Typing a character (`KeyCode::Char(c)`) is restructured into, in order:

1. **Type-over**: if the *previous* keystroke recorded `auto_closed =
   Some(offset)`, the caret is still a bare caret at exactly that offset,
   and the character right after it is `c` itself, the caret moves past
   it instead of inserting — no buffer mutation, so `changed` stays
   `false` and `sync_lsp_did_change` is not called.
2. **No syntax rules**: falls through to plain `type_text`, unchanged
   from before this feature.
3. **Non-empty selection, `c` opens a bracket/quote pair**: wraps the
   selection via `surround_selections(open, close)` — the selection
   afterward covers the original text, not the delimiters (that
   post-condition is `surround_selections`' own contract, reused
   verbatim).
4. **Non-empty selection, `c` doesn't open a pair** (a closing bracket, or
   a plain character): falls through to plain `type_text`, which replaces
   the selection — unchanged behavior for that case.
5. **Empty selection, `c` opens a pair, and the auto-close guard admits
   it** (character right after the caret is end-of-line, whitespace, or a
   closing bracket; for a quote, the caret is additionally not already
   inside a string or a comment per `tokens()`): inserts `{open}{close}`,
   places the caret between them, and records `auto_closed = Some(caret)`.
6. **Empty selection, everything else**: plain `type_text` (a bare
   opener, or any ordinary character).

**Backspace symmetry**: `KeyCode::Backspace` with an empty selection whose
immediately-surrounding characters are a matching `(open, close)` pair
from `rules.brackets` deletes both in one `Buffer::delete` call, instead
of the plain one-character-left delete every other case still uses.

**Found during implementation**: rewriting this arm also changes what
`Backspace` does with a *non-empty* selection. The pre-T18a code computed
its delete range from `selection.start()` unconditionally, which for a
non-empty selection deleted one character *before* the selection rather
than the selection itself — a latent quirk, reachable even before this
batch via a Find-jump or Goto-jump selection followed by `Backspace`.
Restructuring the arm to check `selection.is_empty()` first (needed
regardless, to gate the new pair-symmetry check) fixes this for free:
a non-empty selection is now deleted whole, matching every other editor's
convention and `ide-ui`'s own `delete_backward_character`.
`KeyCode::Delete`'s equivalent quirk is untouched — out of this doc's
scope, since §1/§2.1 never named it as part of this port.

### 3.3 Matching brackets — highlight and jump

`render_editor` calls `text_buffer.matching_bracket(primary_caret_offset)`
once per frame; `Some(pair)` feeds `pair.open`/`pair.close` into
`LineOverlays::bracket_pair`, which `styled_line` paints with a
`Color::Blue` background (distinct from document-highlight's
`Color::DarkGray`) — checked, and applied, after the existing
`highlights` check, so a bracket match's background wins if a span
happens to be covered by both (rare, and harmless either way; `bracket_pair`
is simply checked second).

`JumpToMatchingBracket` (palette-only, §1/§2.2) moves the caret to just
past the matched bracket -- mirroring `matching_bracket`'s own
after-the-caret-first rule: if the caret touches the *close* side, it
jumps to the open side's end; otherwise to the close side's end. No-op if
`matching_bracket` returns `None`.

### 3.4 `Tab` and `Shift+Tab` (`BackTab`)

`KeyCode::Tab` (no modifiers): if the primary selection is empty and
sits on one line, inserts one indent unit (`IndentUnit::default().one()`)
— a real change from this crate's previous bare `"\t"`. If the selection
is non-empty or spans more than one line, calls
`indent_selection_lines(IndentUnit::default())` instead.

`KeyCode::BackTab` (§1 — this is what `crossterm` reports for
`Shift+Tab`, never `Tab` plus a shift modifier) calls
`outdent_selection_lines(IndentUnit::default())` unconditionally
(outdent's own no-op-on-unindented-lines behavior is `ide-core`'s, not
re-implemented here).

## 4. Constraints & invariants

- Zero new `ide-core`/`ide-lsp` public API (§1).
- `auto_closed` is taken (reset to `None`) at the top of every
  `handle_editor_key` call, mirroring `apply_intent`'s unconditional
  `mem::take` in `ide-ui` — only the `Char(c)` arm may set it again
  afterward. Getting this order backwards (e.g. only clearing it inside
  the `Char(c)` arm) would let a stale `auto_closed` survive an
  intervening `Enter`/arrow keystroke and wrongly type-over a closer the
  user typed themselves later.
- `IndentUnit::default()` throughout — no per-buffer indent-unit field is
  added in this batch (§1); T18b is where one would be threaded from
  `.editorconfig`.
- `matching_bracket` already returns `None` above
  `MAX_HIGHLIGHTED_FILE_BYTES` and is bounded by `MAX_BRACKET_SCAN_BYTES`
  — both `ide-core` invariants, unchanged and un-re-validated here.
- Every byte offset used here comes from `LineIndex`, `tokens()`, or a
  `char_indices` walk (matching `smart-editing.md` §4.3) — no raw
  arithmetic indexing of a `str`.

## 5. Examples

**Auto-indent inside an open block:**

```text
fn main() {|      <- Enter here
```
produces
```text
fn main() {
    |
```

**Auto-close then type-over:**

```text
Type `(` -> `(|)`  (auto_closed = Some(caret))
Type `)` -> `()|`  (type-over, no insertion)
```

**Surrounding a Find match:**

```text
Ctrl+F, type a query, Enter (lands on the match, selection = the match text)
Esc (closes the find bar, selection survives)
Type `"` -> the match is now wrapped in quotes, selection still covers the original text
```

## 6. Dependencies & integration points

- No new dependency.
- `ui.rs`'s `render_editor` gains one more per-frame call
  (`matching_bracket`) and one more `LineOverlays` field to populate —
  no new `lib.rs` run-loop call, since this isn't an ambient LSP-style
  refresh, just a computation `render_editor` already had the data for.
- Does not touch `ide-lsp` — purely a `TextBuffer`/`SyntaxRules` feature.

## Revision notes

- Implementation self-review found one real gap between this doc and the
  code as first written: rewriting `Backspace` for pair-symmetry also
  changed non-empty-selection behavior (see §3.2's own "Found during
  implementation" note) — documented after the fact rather than left
  silent. Everything else in §2/§3 matched the implementation exactly on
  first pass.
