# Richer Highlighting & Usages Popup

## 1. Purpose

Three changes driven by direct user feedback on the shipped editor:

1. **Syntax highlighting covered too little and was hard to read.** Two
   separate causes, both addressed here: the token vocabulary was six kinds
   (`Keyword`/`String`/`Number`/`Comment`/`Punctuation`/`Key`), so in a
   programming-language file *every identifier* — function names, types,
   constants, macros — rendered as plain text; and the color table was a
   single fixed dark-tuned palette, so the light theme showed pastels on
   white.
2. **No affordance for code navigation.** `Cmd+Click` already searched
   references, but nothing on screen said so: the pointer stayed an I-beam
   and no symbol was marked.
3. **Find-usages results were buried in the bottom panel.** The user wants
   the *navigation* gesture to raise a compact, dismissable list.

Scope, by crate:

- `crates/core/**` — five new `TokenKind` variants, four new
  identifier-classification rules, two new `SyntaxRules` list fields, and
  six new built-in languages (13 → 19).
- `crates/ui/**` — theme-aware token palette, the `Cmd`-hover link
  treatment, and the floating Usages window plus its `Cmd+B` binding.

**Explicitly not in scope:** semantic highlighting (the tokenizer still has
no symbol table and cannot tell a call from a definition, or a type from a
same-shaped variable); go-to-definition (`Cmd+Click` still means "find
references", per `find-usages.md` §1); multi-line/cross-line tokenizer
state; user-configurable colors.

## 2. Interface / API

### 2.1 `ide-core` (`crates/core/src/syntax.rs`)

```rust
pub enum TokenKind {
    // unchanged
    Keyword, String, Number, Comment, Punctuation, Key,
    /// Identifier immediately followed by `(`.
    Function,
    /// `type_keywords` match, `capitalized_is_type` identifier, or a
    /// `sigil_words` entry naming one (`<div`, `'a`).
    Type,
    /// `name!(…)`, `#[…]`, `@decorator`, `#include` — "this is meta".
    Macro,
    /// `SCREAMING_SNAKE_CASE`, or a shell/Makefile `$VAR`.
    Constant,
    /// A single operator character. `Punctuation` (brackets, separators)
    /// deliberately keeps the plain text color.
    Operator,
}

pub struct SyntaxRules {
    // ... existing fields ...
    /// Exact-word matches colored `Type` regardless of shape (`u32`,
    /// `int`, `string`) — what `capitalized_is_type` cannot reach.
    pub type_keywords: &'static [&'static str],
    /// Single chars tokenized as `Operator`. Checked before
    /// `punctuation`; the two sets must be disjoint (asserted by a test
    /// over every builtin).
    pub operators: &'static [char],
    /// `(prefix, kind)` pairs where prefix + the identifier right after it
    /// form one token: `<div`, `</div`, `'a`, `$PATH`, `@media`, `.card`.
    /// Tried in order, so `"</"` must precede `"<"` (asserted by a test).
    pub sigil_words: &'static [(&'static str, TokenKind)],
    pub capitalized_is_type: bool,
    pub upper_case_is_constant: bool,
    pub macro_bang: bool,
    /// Identifier immediately followed by `=` is a `Key` — XML/HTML
    /// attribute names. Off everywhere else, where `=` is assignment.
    pub attribute_names: bool,
}

// New builtins, all re-exported from `ide_core`:
pub const INI: SyntaxRules;         // ini, cfg, conf, properties, .editorconfig, …
pub const JAVASCRIPT: SyntaxRules;  // js, jsx, mjs, cjs, ts, tsx
pub const C: SyntaxRules;           // c, h, cc, cpp, cxx, hpp, hh, hxx
pub const JAVA: SyntaxRules;        // java
pub const SQL: SyntaxRules;         // sql
pub const CSS: SyntaxRules;         // css, scss, sass, less
```

`tokenize`, `syntax_for_path`, `syntax_for_extension`,
`MAX_HIGHLIGHTED_FILE_BYTES` keep their signatures.

### 2.2 `ide-ui`

```rust
// crates/ui/src/app/render.rs
fn token_color(kind: TokenKind, default: Color32, dark_mode: bool) -> Color32;

fn tab_layout_job(
    ui: &Ui, text: &str, wrap_width: f32,
    tokens: &[Token], diagnostics: &[Diagnostic],
    link: Option<&Range<usize>>,   // new: the Cmd-hover underline
) -> LayoutJob;

// crates/ui/src/app.rs — free functions, unit-tested
fn byte_offset_at_char(text: &str, char_index: usize) -> usize;
fn word_range_at(text: &str, offset: usize) -> Option<Range<usize>>;
```

`IdeApp` gains:

- `hover_link: Option<Range<usize>>` — the identifier under the pointer
  while the command modifier is held, as of the *previous* frame (§4).
- `show_usages_popup: bool`.
- `trigger_find_usages_popup(&mut self)` — `find_usages()` plus raising the
  window; deliberately does **not** touch `bottom_view`.
- `display_path(&self, &Path) -> String` — path shortened against the
  project root for display.
- `sorted_references(&self) -> Vec<Location>` — the single ordering both
  the bottom panel and the popup render, so they cannot drift.

## 3. Behaviour

### Tokenizer rule order

`tokenize` keeps its single left-to-right pass, never revisiting a byte
position (`syntax-highlighting.md` §4). The per-position cascade, with the
two new steps marked:

1. (line start) `key_separator` → `Key`
2. (line start) `line_prefix_tokens` → the rule's kind
3. `line_comment_prefixes` → `Comment`
4. `block_comment` → `Comment`
5. `string_quotes` → `String`
6. **`sigil_words` → the rule's kind** (prefix + following identifier, one
   token; declines when no identifier follows, which is what keeps `$(cmd)`
   and `.5em` out)
7. number → `Number`
8. identifier → `classify_word` (below), or plain text
9. **`operators` → `Operator`**
10. `punctuation` → `Punctuation`
11. skip one char

`classify_word` decides an identifier's kind in this order — first match
wins: `keywords` → `type_keywords` → `macro_bang` (`name!` followed by
`(`/`[`/`{`; the delimiter check is what stops `a != b` reading as a macro)
→ followed by `(` → `Function` → `attribute_names` (followed by `=`) →
`upper_case_is_constant` → `capitalized_is_type` → plain text.

Constant is checked before type on purpose: `SCREAMING_SNAKE_CASE` also
starts with an uppercase letter, and "constant" is the more specific
reading. A *single* uppercase letter is excluded from the constant rule —
`T`/`K`/`V` are generic parameters far more often than one-letter
constants, so they fall through to `Type`.

One pre-existing limitation is fixed as a side effect: the `key_separator`
forward scan now stops at `{`/`}`. Without it a CSS rule head
(`.card { color: red; }`) hands its whole span to the Key rule — and the
same bug had been documented for YAML flow mappings (`{a: 1}`).

### Colors

Two palettes, selected by `ui.visuals().dark_mode`, same hues at different
luminance. `Punctuation` maps to the caller's plain text color in both, so
brackets and separators recede and `Operator` — the characters that carry
the logic — is what stands out.

### Cmd-hover

While the command modifier is held (`egui::Modifiers::command`, so `Cmd` on
macOS and `Ctrl` elsewhere) and the pointer is over the editor:

- The byte offset under the pointer comes from the just-rendered galley
  (`galley.cursor_from_pos(pointer - galley_pos)`, converted from egui's
  char index).
- `word_range_at` expands that offset to the surrounding identifier
  (`is_alphanumeric() || '_'`). A caret sitting *just past* a name — where
  a click on its right half lands — still resolves to that name. A run
  starting with a digit is rejected: that's a number literal, not a symbol.
- Non-`None` result → `CursorIcon::PointingHand` (set *after*
  `TextEdit::show`, so it wins over the I-beam the widget itself asked for)
  and the range is stored in `hover_link` for the next frame's underline.

### Cmd+B / Cmd+Click → the Usages window

- `Cmd+B` is checked in `handle_shortcuts` alongside the existing
  save/undo/redo/`Alt+F7` bindings. `Escape` closes the window.
- `Cmd+Click` in the editor triggers the same thing, so the link the hover
  treatment advertises actually leads somewhere.
- Both call `trigger_find_usages_popup`: the same `find_usages()` query
  (same no-op conditions as `find-usages.md` §3 — no active tab, no path,
  no cursor offset, not the Editor view) plus `show_usages_popup = true`.
  The window opens even when the query itself no-ops, rendering "No usages
  found." rather than swallowing the gesture silently.
- The window lists one row per usage, `file:line` (path relative to the
  project root, 1-based line), ordered by `sorted_references`. Clicking a
  row calls `open_usage` and closes the window.
- The toolbar button and `Alt+F7` are unchanged: they still fill the bottom
  panel's Usages view, which stays available for browsing the same results.

## 4. Constraints & invariants

- **Single-pass tokenizing survives.** Both new steps are forward-only and
  bounded by the token they consume (`try_sigil_word` by the identifier's
  own length), so `tokenize` stays O(n) — the invariant
  `syntax-highlighting.md` §4 calls load-bearing.
- **`operators` and `punctuation` must be disjoint** per language:
  operators are checked first, so an overlap would silently make the
  punctuation entry unreachable. A test asserts this across every builtin.
- **`sigil_words` order matters**: entries are tried in sequence, so a
  prefix that another entry starts with must come first. A test asserts no
  entry is shadowed by an earlier one.
- **The link underline lags one frame, by construction.** The `TextEdit`
  layouter runs before the frame's pointer position is known, so
  `hover_link` is written at the end of frame *n* and painted in frame
  *n+1*. A change to it forces `request_repaint()` — pointer motion alone
  wouldn't cover pressing the modifier with the pointer already parked on a
  symbol.
- **A diagnostic underline wins over the link underline** where they
  overlap: an error on the hovered symbol is the more urgent signal, and
  the pointer shape already says "link".
- **Path provenance is unchanged** (`find-usages.md` §4): the popup's rows
  come only from `LspBridge::references`, already validated against the
  project root inside `ide-lsp`; `display_path` only shortens for display
  and falls back to the full path when `strip_prefix` fails.
- **`word_range_at` never panics on a non-boundary offset** — it rejects
  one, which is what makes it safe to feed a galley-derived offset in a
  file with multibyte identifiers.

## 5. Examples

`fn main() { let x = 42; } // done` with `RUST` now yields `fn` Keyword,
**`main` Function**, `(`/`)`/`{` Punctuation, `let` Keyword, **`=`
Operator** (was Punctuation), `42` Number, `;`/`}` Punctuation, `// done`
Comment.

```rust
// Cmd-hover, in render_tabs_and_editor after TextEdit::show:
let hovered = command_held
    .then(|| output.response.hover_pos())
    .flatten()
    .and_then(|p| {
        let ccursor = output.galley.cursor_from_pos(p - output.galley_pos);
        word_range_at(scratch, byte_offset_at_char(scratch, ccursor.index.0))
    });
```

## 6. Dependencies & integration points

- No new external dependencies in any crate. `ide-lsp` is untouched: the
  popup renders results the existing `LspRequest::References` /
  `LspEvent::References` round trip already produces.
- Touches `crates/core/src/syntax.rs`, `crates/core/src/lib.rs` (re-exports
  for the six new languages), `crates/ui/src/app.rs`, and
  `crates/ui/src/app/render.rs`. None of `CLAUDE.md`'s security-sensitive
  paths are in the diff — no subprocess spawning, no command construction,
  no path input beyond what `ide-lsp` already validated — so a `hacker`
  pass is not required for this feature.
