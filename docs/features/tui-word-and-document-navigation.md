# `ide-tui`: word, line-boundary and document-boundary caret motion

## 1. Purpose

`ide-tui`'s editor (`crates/tui/src/editor.rs`, `App::handle_editor_key`)
only ever moved the caret one character or one row at a time
(`Direction::{Left,Right,Up,Down}` via `move_cursor`/
`move_caret_with_folds`) -- there was no word-by-word motion, no jump to
the start/end of the current line, and no jump to the start/end of the
whole document. `ide-ui`'s editor already has all three (`Granularity::
{Word,Line,Document}` in `crates/ui/src/editor/input.rs`, part of **A2**);
this brings `ide-tui` to parity, live-tested by the user against a MacBook
keyboard that has no dedicated Home/End/Page Up/Page Down keys at all --
only `Fn+Left`/`Fn+Right` (which macOS itself translates to real `Home`/
`End` key events before either frontend ever sees them) reach those
semantics, and only the physical `Cmd`/`Ctrl` key plus a plain arrow reaches
document-start/end.

Scope is caret motion only, matching this crate's own current capability
level: **no Shift-extend-selection variant of any of these** (`Shift+
Left`/`Right`/`Up`/`Down` don't extend a selection in `ide-tui` today
either -- confirmed by grep before writing this doc -- so adding
Shift-extend only for the four new motions and not the four existing ones
would be a new, inconsistent asymmetry, not a fix). Multi-cursor-aware
(every selection moves, mirroring `tui-multiple-cursors.md` §3.4's
established shape for `Direction`-based motion), and fold-aware (a target
that lands inside a collapsed fold's hidden interior redirects to the
nearest visible boundary, the same correction `move_caret_with_folds`
already applies to a plain character step -- `code-folding.md` §2.6/§3.4).

Does not touch `crates/core/**` or `crates/ui/**`.

## 2. Interface

### 2.1 `crates/tui/src/editor.rs` (additions)

```rust
/// Start of the buffer line `offset` is on (excludes the line terminator,
/// same convention `ide_core::text::Lines::line_range` already follows).
pub fn line_start_offset(buffer: &TextBuffer, offset: usize) -> usize;
/// End of the buffer line `offset` is on (excludes the line terminator).
pub fn line_end_offset(buffer: &TextBuffer, offset: usize) -> usize;
/// One word left from `offset`: skips a run of non-identifier characters
/// (including newlines -- word motion crosses blank lines, matching
/// `ide-ui`'s own `word_start_before`), then the run of identifier
/// characters before that.
pub fn word_start_before(text: &str, offset: usize) -> usize;
/// Symmetric, one word right from `offset`.
pub fn word_end_after(text: &str, offset: usize) -> usize;
```

Ported verbatim (behavior, not code layout -- this crate already has its
own `is_identifier_char`/`prev_char_boundary`/`next_char_boundary` to
build on, from `code-actions-and-rename.md`'s `word_range_at`) from
`ide-ui`'s own `word_start_before`/`word_end_after` in `crates/ui/src/
editor/input.rs`. Document start/end need no function of their own --
they're the constants `0` and `buffer.text().len()`.

### 2.2 `crates/tui/src/app.rs` (additions)

```rust
/// Which extended (non-single-step) caret motion a key press requested --
/// shares one fold-aware, multi-cursor-aware dispatch method rather than
/// four near-identical ones (§3.1/§3.2).
enum ExtendedMotion { LineStart, LineEnd, WordLeft, WordRight, DocumentStart, DocumentEnd }

impl App {
    /// `redirect_hidden`'s free-function shape, extracted out of
    /// `move_caret_with_folds`'s `Left`/`Right` arm so both it and
    /// `move_caret_extended` share the exact same correction rather than
    /// each re-implementing it (§3.2).
    fn redirect_hidden(text_buffer: &TextBuffer, visual: &VisualLines, offset: usize, backward: bool) -> usize;

    /// Computes the raw target via `ExtendedMotion`'s matching pure
    /// function, then applies `redirect_hidden` exactly like
    /// `move_caret_with_folds` already does for a plain step (§3.2).
    fn move_caret_extended(buf: &OpenBuffer, offset: usize, motion: ExtendedMotion) -> usize;
}
```

`handle_editor_key` gains a `KeyCode` match against `Home`, `End`, and
`Left`/`Right` qualified by `KeyModifiers::CONTROL`, dispatching to
`ExtendedMotion::{LineStart,LineEnd,WordLeft,WordRight}` respectively, plus
`Ctrl+Home`/`Ctrl+End` for `DocumentStart`/`DocumentEnd` (§3.1). Every
selection moves to a fresh caret at its own `move_caret_extended` result,
`desired_column` is cleared (these are horizontal motions, same as plain
`Left`/`Right` already clear it), mirroring the existing `Direction` block's
per-selection shape exactly.

No `commands.rs` changes -- these are raw editor-local key matches, the
same as plain arrow keys and Backspace/Delete, never routed through the
command registry (`Action`/`Command`).

## 3. Behaviour

### 3.1 Bindings

| Key (this crate) | Motion | JetBrains equivalent this mirrors |
|---|---|---|
| `Home` | Start of current line | `⌘←` / `Home` (`ide-ui` already has this) |
| `End` | End of current line | `⌘→` / `End` (`ide-ui` already has this) |
| `Ctrl+Left` | One word left | `⌥←` on macOS -- JetBrains' own Windows/Linux keymap already binds plain `Ctrl+Left` to this, so no Cmd-to-Ctrl translation judgment call is needed here, unlike most of this crate's other bindings |
| `Ctrl+Right` | One word right | Symmetric, `Ctrl+Right` |
| `Ctrl+Home` | Start of document | JetBrains Windows/Linux keymap's own binding for "Move Caret to Text Start" (macOS uses `⌘↑` instead, which doesn't translate to a terminal chord -- `Ctrl+Home` is the genuine non-mac JetBrains binding, not an invented one) |
| `Ctrl+End` | End of document | Symmetric, "Move Caret to Text End" |

`Home`/`End` reach the terminal as real key events via `Fn+Left`/`Fn+Right`
on a MacBook keyboard with no dedicated Home/End keys -- standard macOS
firmware behavior, confirmed live by the user, nothing this crate needs to
special-case. `Ctrl+Home`/`Ctrl+End` need a terminal that forwards a
modified Home/End sequence (most modern terminal emulators do -- xterm's
`modifyOtherKeys`-style `CSI 1;5H`/`CSI 1;5F` or equivalent); a terminal
that doesn't is a pre-existing limitation of this crate's whole keybinding
approach (`Ctrl+Shift+G`/etc. already depend on the same kind of terminal
support), not something new this doc introduces.

`PageUp`/`PageDown` are deliberately left unchanged (still whatever they
already do -- nothing before this doc bound them in the editor either): the
user confirmed page-scroll semantics should stay separate from
document-start/end, reachable via `Ctrl+Home`/`Ctrl+End` instead.

### 3.2 Fold-awareness

`move_caret_extended` computes the raw target from `ExtendedMotion`'s
pure function, then calls `redirect_hidden`: if the raw offset's line is
hidden by a collapsed fold, forward motions (`LineEnd`, `WordRight`,
`DocumentEnd`) land on the start of the first visible row after the fold,
backward motions (`LineStart`, `WordLeft`, `DocumentStart`) land on the end
of that fold's own visible `start_line` text -- identical correction
`move_caret_with_folds`'s `Left`/`Right` arm already applies to a plain
character step. `DocumentStart` (offset `0`) never actually needs the
redirect in practice (line `0` is always either unfolded or is some fold's
own always-visible `start_line`), but running it through the same function
uniformly is simpler than special-casing it out, and costs nothing.

### 3.3 Multi-cursor

Every selection collapses to a caret at its own `move_caret_extended`
result, the same per-selection map `Direction`-based motion already uses
(`tui-multiple-cursors.md` §3.4) -- `buf.desired_column` is cleared
unconditionally (these are horizontal motions; only `Up`/`Down` ever set
it).

## 4. Constraints

- Pure functions in `editor.rs` are `char`-boundary-safe by construction
  (built on the crate's existing `prev_char_boundary`/`next_char_boundary`/
  `is_identifier_char`), matching this module's own stated invariant.
- Not security-sensitive: no subprocess, no path, no network surface.
  `hacker` skipped.

## 5. Examples

- Cursor mid-word in `"hello_world foo"` at the underscore, `Ctrl+Right`
  twice lands after `"world"`, then after `"foo"`.
- Cursor anywhere on an indented line, `Home` lands right before the first
  character of that line's actual text content -- **not** a
  smart-first-non-whitespace toggle (JetBrains' plain `Home` is a literal
  line-start jump; the smart/toggle behavior is a separate, unimplemented
  JetBrains feature this doc doesn't add).
- `Ctrl+Home` from anywhere in the file lands at offset `0`; `Ctrl+End`
  lands at `buffer.text().len()`, both with an empty selection.
- A folded region whose `start_line` is line 5 and hides lines 6-9: `End`
  pressed while the caret sits on line 5's hidden continuation (impossible
  through the UI directly, but `move_caret_extended` is still correct
  against it) or `Ctrl+End` when the fold's `end_line` is the buffer's last
  line both land on the first visible row after the fold, never inside it.

## 6. Dependencies / integration

No new external dependency. Touches only `crates/tui/src/editor.rs` and
`crates/tui/src/app.rs` -- single role, `rust-tui-dev`.

## Revision notes

- `handle_editor_key`'s pre-existing `direction` match (`KeyCode::Left =>
  Direction::Left`, etc.) is keyed only on `key.code`, blind to modifiers
  -- it caught `Ctrl+Left`/`Ctrl+Right` first and returned before the new
  extended-motion dispatch below it ever ran, so word motion silently
  degraded to a plain one-character step. Fixed by adding `if
  !key.modifiers.contains(KeyModifiers::CONTROL)` guards to the `Left`/
  `Right` arms of that pre-existing match, letting the Ctrl-qualified case
  fall through to the new dispatch instead.
- `ExtendedMotion::DocumentEnd`'s fold redirect initially reused
  `backward: false` like every other forward motion (`LineEnd`,
  `WordRight`) -- wrong specifically for `DocumentEnd`: `buffer.len()`
  sits on the buffer's true last line, which can itself be hidden by a
  collapsed fold whose `end_line` is that last line, and unlike every
  other forward motion there is no line *after* the buffer's end to
  redirect into. Fixed by passing `backward: true` for this one case only,
  mirroring `ide-ui`'s own `vertical_step`'s identical special-case for
  `Granularity::Document`. Caught by a dedicated test using a fixture with
  no trailing newline (so the fold's `end_line` really is the buffer's
  last line) -- the original fixture with a trailing newline doesn't
  exercise this at all, since the trailing empty virtual line after the
  final `\n` is never part of any fold range and stays visible regardless.
- Coverage: `editor.rs`'s four new pure functions and `app.rs`'s six new/
  changed methods are all covered by the 11 new tests above (4 pure-logic,
  7 `App`-level, including both fold-redirect edge cases and the
  multi-cursor case) -- no separate coverage tool run needed beyond the
  existing full-suite pass (724 tests, all green).
