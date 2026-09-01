# Multiple Cursors (`ide-tui`)

Roadmap item **T20** (`docs/roadmap.md` §10), porting
`docs/features/multiple-cursors.md` (A3) to the terminal frontend. Single
role, **`rust-tui-dev`**, `crates/tui/**` only. No security-sensitive path
is touched, so `hacker` is skipped.

## 1. Purpose

A3's `ide-core` half (`Selections::{push_primary, remove_primary,
remove_at, index_at}`, `ide_core::{next_occurrence, all_occurrences,
MAX_OCCURRENCES}`) is already merged and used by nothing outside tests —
`ide-ui`'s own A3 phase consumed it, `ide-tui` never has. This phase wires
it into `ide-tui`'s own keyboard-driven editor, and — unlike every prior
`T`-item in this crate — that wiring is not free. `ide-ui`'s A3 doc could
say "the editing machinery underneath is already built and does not
change" because A2 built every edit path (`apply_intent`) generically over
`Selections` from the start. `ide-tui`'s own smart-editing functions
(`T18a`/`T18b`) were **not** built that way: `open_delimiter`,
`insert_newline_with_indent`, `delete_backward`, forward `Delete`, and
arrow-key movement in `handle_editor_key` all read `selections().primary()`
alone and end by calling `set_selections(Selections::single(..))` —
silently discarding every other selection. So this phase has two parts,
not one: the commands A3 actually asks for, and making the keystrokes a
user reaches for immediately after creating a second cursor — typing,
arrows, Enter, Backspace, Delete, Tab — not erase it out from under them
on the very next keystroke. Shipping the commands without the second part
would make the feature actively worse than not having it: cursors that
vanish the instant anything happens to them.

### 1.1 Scope

In, keyboard-only (no gutter/mouse/click handling exists anywhere in this
crate, confirmed the same way `T19` confirmed it):

| Action | Binding | ide-ui's mac binding | Why this binding |
|---|---|---|---|
| `AddNextOccurrence` | `Ctrl+G` | `⌃G` | Already `Ctrl`-based in JetBrains' own macOS keymap (same precedent as `JoinLines`'s `⌃⇧J`) — used literally, not translated. |
| `UnselectOccurrence` | `Ctrl+Shift+G` | `⌃⇧G` | Same reasoning. |
| `SelectAllOccurrences` | `Ctrl+Alt+Shift+J` | `⌃⌘G` | The mac binding needs `⌘`, which a terminal cannot deliver (`tui-shell-and-editor.md` §2.4's standing rule: Cmd-chords are frequently intercepted by the terminal emulator itself). Unlike every other `⌘`-chord this crate translates by substituting `Ctrl` for `⌘`, this one *combines* literal `Ctrl` with `⌘` — substituting would collide with `AddNextOccurrence`'s own `Ctrl+G`. `multiple-cursors.md` §1.2 already records a genuine, non-invented Windows/Linux JetBrains binding for this exact action (`Ctrl+Alt+Shift+J`), so this phase uses that other half verbatim instead of inventing a chord — the same interpretation of `{mac, other}` `tui-shell-and-editor.md` §2.4 already established for this crate as a whole ("`T1` therefore always uses the binding `CLAUDE.md` calls `other`, even on macOS"), applied here to one binding instead of the whole table because only this one binding needs it. |
| `CollapseSelections` | `Esc` | `Esc` | Global command; the action itself no-ops outside `Focus::Editor` or with one selection, so registering it unconditionally is safe (see §3.4). |

`Ctrl+G`/`Ctrl+Shift+G` were previously proven *not* globally registered
(`commands.rs`'s `ctrl_g_and_ctrl_shift_g_are_not_globally_registered`,
`tui-find.md` §4.2) specifically so the find bar could claim them as
bar-local bindings. That reasoning still holds and this phase does not
touch it: `handle_key`'s `self.find.is_some()` check runs before
`binding_for` is ever consulted (`app.rs`, `handle_key`), so while the find
bar is open these chords still mean "jump to next/previous match" and
never reach the new global commands at all. Only while `self.find` is
`None` do they now resolve to `AddNextOccurrence`/`UnselectOccurrence`.
The old test is renamed and inverted (see Revision notes) rather than
deleted, since it still documents a real invariant (find-bar-local when
open) — it just isn't "not registered at all" anymore.

Out, with reasons (mirroring `ide-ui`'s own doc, which cuts nothing —
every cut below is specific to this crate's terminal/keyboard-only
nature):

- **`⌥Click` (Add Caret).** No mouse anywhere in this crate.
- **Clone Caret (`⌥⌥`+`↑`/`↓` double-tap).** Two independent problems, either
  one sufficient to cut it: (1) the gesture needs to detect a bare
  modifier *press edge* with no other key, which needs
  `KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES` beyond what
  this crate currently negotiates (`lib.rs` only ever requests
  `DISAMBIGUATE_ESCAPE_CODES`) — unreliable across terminals even where it
  could work at all; (2) `Alt+Up`/`Alt+Down` are already
  `ExtendSelection`/`ShrinkSelection` (`T18b`), so a double-tap detector
  would have to arbitrate between "plain `Alt+Down`" and "second tap of
  `Alt`, then `Down`" with no way to distinguish them if the enhancement
  flag isn't honoured. Not attempted this phase.
- **Column Selection Mode (`⌘⇧8` / other `Alt+Shift+Insert`).** Inherently a
  mouse-drag rectangle gesture (`multiple-cursors.md` §3.5's own "a drag in
  the text area...") — no mouse to drag with.

### 1.2 Zero new `ide-core` API

Every piece this phase needs already exists and is unchanged:
`Selections::{push_primary, remove_primary, collapse_to_primary, all,
primary, primary_index, len, is_multiple, new, single, map}`,
`Selection::{caret, new, start, end, head, anchor, range, is_empty}`,
`ide_core::{next_occurrence, all_occurrences, word_at}`,
`TextBuffer::{surround_selections, indent_selection_lines,
outdent_selection_lines, type_text, apply}`. All already `pub use`d and
already imported in `app.rs` (`word_at`, `Change`, `Transaction`,
`Selection`, `Selections`, `TextBuffer` are pre-existing imports; only
`all_occurrences`/`next_occurrence` are new names added to the same `use
ide_core::{...}` block).

`TextBuffer::surround_selections` in particular (`crates/core/src/text/
ops.rs`, added by the smart-editing phase this crate's own `T18a` already
partially ported) turns out to be exactly what `open_delimiter`'s
non-empty-selection wrap branch needs and was **not** using — `T18a`
reimplemented the wrap inline instead of calling it, a pre-existing doc/
code drift `tui-smart-editing.md` claims (`surround_selections`, ...") but
the code never followed. This phase fixes that as a side effect of making
the same function multi-selection-safe (§3.6).

## 2. Interface

### 2.1 `commands.rs`

Four new `Action` variants (`AddNextOccurrence`, `UnselectOccurrence`,
`SelectAllOccurrences`, `CollapseSelections`) and four new `Command`
entries with the bindings in §1.1's table.

### 2.2 `app.rs`

Four new `App` methods, each called from a new `run_action` arm:

```rust
fn trigger_add_next_occurrence(&mut self);
fn trigger_unselect_occurrence(&mut self);
fn trigger_select_all_occurrences(&mut self);
fn trigger_collapse_selections(&mut self);
```

One new scroll helper, alongside `sync_editor_scroll`/`scroll_to_and_reveal`:

```rust
/// Reveals whatever fold hides `buf`'s new primary caret, then scrolls
/// only as far as needed to keep it visible (`sync_editor_scroll`'s
/// minimal adjustment, not `scroll_to_and_reveal`'s top-align).
/// `AddNextOccurrence` resolves a match against raw buffer text, so
/// unlike ordinary caret motion (already fold-aware via
/// `move_caret_with_folds`) the match can land inside a currently
/// collapsed fold -- the same problem `tui-code-folding.md` §3.6's three
/// jump sites solved, with different scroll semantics (this one is an
/// incremental, exploratory action, not a big jump to a distant
/// location).
fn reveal_and_sync_scroll(buf: &mut OpenBuffer, viewport_rows: u16);
```

One new generic multi-selection edit driver:

```rust
/// Builds one `Transaction` from `per_selection`'s answer for every
/// selection in `buf`'s current `Selections` (in existing order, which
/// `Selections`' own invariant guarantees is sorted by `start()`),
/// applies it once, and re-derives every selection's post-edit position
/// directly from the sorted, non-overlapping change list this function
/// already built -- never from `Transaction`'s own default offset
/// mapping, which answers "where does an existing offset move to", not
/// "where inside newly-inserted text should this caret land" (a
/// different question each caller already knows the answer to).
///
/// `per_selection(text_buffer, selection)` returns `(range_to_replace,
/// replacement_text, anchor_offset_into_replacement,
/// head_offset_into_replacement)`. A bare-caret result sets the two
/// offsets equal. A true no-op for one selection (nothing to do there --
/// e.g. Backspace at the start of the buffer) returns its own empty
/// range at its own head, an empty replacement, and `(0, 0)` -- so the
/// entries list always has exactly one entry per original selection and
/// `primary_index` never needs adjusting for a skipped entry.
///
/// Returns `false` (no `Transaction` built or applied, no
/// `set_selections` call at all) when every entry is a true identity.
/// Also returns `false`, leaving the buffer and every selection
/// completely untouched, if any two selections' own derived ranges
/// overlap or the resulting change set is otherwise invalid -- a
/// documented, deliberate all-or-nothing fallback (§4.6), not a silent
/// partial edit.
fn apply_per_selection(
    buf: &mut OpenBuffer,
    per_selection: impl FnMut(&TextBuffer, Selection) -> (Range<usize>, String, usize, usize),
) -> bool;
```

`move_caret_with_folds` gains a parameter:

```rust
// was: fn move_caret_with_folds(buf: &OpenBuffer, direction: Direction) -> (usize, Option<usize>)
fn move_caret_with_folds(buf: &OpenBuffer, offset: usize, direction: Direction) -> (usize, Option<usize>);
```

`offset` replaces the internal `text_buffer.selections().primary().start()`
read; `buf.desired_column` is still read directly inside (unchanged,
shared across every selection in a keystroke — see §3.2's sticky-column
note). Every existing call site is updated to pass an explicit offset;
none change behaviour when there is exactly one selection, since that
offset is still that selection's own `start()`.

### 2.3 `highlight.rs` / `ui.rs`: rendering

`LineOverlays` gains one field:

```rust
pub struct LineOverlays<'a> {
    // ... unchanged fields ...
    /// Every selection's range with `start() < end()` -- **including**
    /// the primary's, for visual consistency (a selection that happens
    /// to be the primary looks exactly as selected as any other). Bare
    /// carets contribute nothing (an empty range clamps to nothing in
    /// `styled_line`'s existing `start < end` guard, same as `highlights`/
    /// `bracket_pair` already handle). Distinct background
    /// (`Color::Yellow`) from `highlights` (`Color::DarkGray`) and
    /// `bracket_pair` (`Color::Blue`).
    pub selections: &'a [Range<usize>],
}
```

`ui.rs`'s `render_editor` computes it once per frame from
`text_buffer.selections().all()` (cheap: empty in the overwhelmingly
common single-bare-caret case, since `.filter(|r| !r.is_empty())` drops
it before it ever reaches the boundary walk). The real terminal cursor
(`frame.set_cursor_position`) switches from `.primary().start()` to
`.primary().head` — a correctness fix, dormant until this phase: every
selection in this crate was previously a bare caret by construction
(`start() == head` always), so the two were always the same value; once
`AddNextOccurrence`'s first press can leave the primary as a genuine
non-empty forward-or-reversed range, the visible caret has to track
whichever end is the *active* one, matching every other editor's
convention (and `ide-ui`'s own `EditorOutput::cursor_offset`, which is
`head`-based already).

## 3. Behaviour

### 3.1 `Ctrl+G` — add selection for next occurrence

Same two-step JetBrains behaviour `multiple-cursors.md` §3.2 specifies:

1. Primary empty: replace it **in place** (same index, not a new
   selection) with `ide_core::word_at(text, primary.head)`'s range, via
   `Selections::new` rebuilding the whole selection list with that one
   entry swapped — `Selections::new` already re-sorts/re-merges/re-points
   `primary` for free if the new word range happens to touch a neighbour.
   `None` (caret isn't on a word) is a no-op. Stops here — does not chain
   into step 2 on the same press, matching `ide-ui`'s own staged
   behaviour.

   Note: this crate's `word_at` (`crates/core/src/text/
   selection_hierarchy.rs`, already used by `T18b`'s `ExtendSelection`)
   differs from `ide-ui`'s own `word_range_at`: it does **not** reject a
   run starting with a digit, so on a number literal the first `Ctrl+G`
   *does* select something (unlike `ide-ui`'s documented "a number literal
   yields no-op" behaviour). Not worth reconciling for this phase — this
   crate already made that call for `ExtendSelection` and staying
   consistent with it beats matching `ide-ui` exactly here.
2. Primary non-empty: needle = the primary's own text.
   `ide_core::next_occurrence(text, &needle, primary.end())`, wrapping.
   `None` (nothing found) is a no-op. `Selections::push_primary` with the
   result; `false` (absorbed into an existing selection — the natural
   "every occurrence is already selected" stopping point) is a no-op.

On success, `reveal_and_sync_scroll` (§2.2).

### 3.2 `Ctrl+Shift+G` / `Ctrl+Alt+Shift+J`

- **`UnselectOccurrence`**: `Selections::remove_primary()`; `false` (one
  selection left) is a no-op. No scroll sync — the predecessor selection
  it falls back to was already on screen (it was added by an earlier
  `Ctrl+G`, which already revealed it).
- **`SelectAllOccurrences`**: needle resolved exactly as step 1/2 above
  (word under an empty primary, else the primary's own text; empty needle
  is a no-op) — but unlike `Ctrl+G`, a single press does the word-select
  *and* the select-all in one step, since there is no staged
  "select-word-then-stop" behaviour to preserve for this command.
  `ide_core::all_occurrences` (already capped at `MAX_OCCURRENCES` inside
  `ide-core` — nothing new to enforce here); empty result is a no-op
  (shouldn't happen once a needle resolved, since the needle's own
  occurrence is always one of the matches, but checked instead of assumed).
  The match whose range contains the *old* primary's `start()` becomes the
  new primary (`Selections::new(ranges, that_index)`), so — per
  `multiple-cursors.md` §3.3 — the view does not jump; no scroll sync
  call is made, matching that invariant literally.

### 3.3 `Esc` — collapse

```rust
fn trigger_collapse_selections(&mut self) {
    if self.focus != Focus::Editor {
        return;
    }
    let Some(buf) = self.active_buffer_mut() else { return };
    let mut selections = buf.buffer.text_buffer().selections().clone();
    if selections.is_multiple() {
        selections.collapse_to_primary();
        buf.buffer.text_buffer_mut().set_selections(selections);
    }
}
```

No egui-style focus-lock/event-consumption dance is needed here —
`ide-ui`'s `multiple-cursors.md` §3.6 spends most of its length on that
because `egui`'s focus system drops `Esc` before a widget's own frame runs
unless a filter says otherwise. `ide-tui` has no such layer: `handle_key`
is a plain, ordered `if` chain over raw `crossterm` events (§ "Every other
id... reused... translated" convention already established), so
registering `Esc` as a global `Command` and letting the action itself
no-op is the entire story. `Esc` had no prior global binding
(`commands.rs` had no `KeyCode::Esc` entry) and `handle_editor_key` had no
`KeyCode::Esc` arm either (fell into its `_ => {}` catch-all) — so this
is a clean, non-colliding addition, not a reassignment.

### 3.4 Typing, Enter, Backspace, Delete, Tab: making them multi-selection-safe

This is the part `ide-ui`'s own doc didn't need. Each function below
follows the same shape: read every selection *before* editing (the
existing `Selections` invariant already guarantees they're sorted and
non-overlapping), compute each one's own `(range, replacement,
anchor_offset, head_offset)` independently, hand the whole batch to
`apply_per_selection`.

- **Plain character typing** (`insert_char`'s no-syntax-rules path and its
  final fallback) already went through `TextBuffer::type_text`, which
  builds one `Transaction` over `self.selections.all()` internally
  (`crates/core/src/text/mod.rs`) — already correct, untouched.
- **Auto-close / bracket typing** (`open_delimiter`): the non-empty-
  selection branch now delegates to `TextBuffer::surround_selections`
  (§1.2) instead of reimplementing the wrap inline — already
  multi-selection-safe at the core level, already tested there. The
  empty-selection branch (every selection bare — `surround_selections`
  returned `false`) goes through `apply_per_selection`: each selection
  independently re-evaluates `may_open_pair`/`is_quoted_or_commented`
  against its own position, inserting `open+close` or bare `open`
  accordingly. `OpenBuffer::auto_closed` stays a single `Option<usize>`
  slot, deliberately **not** widened to one per selection — a scope cut,
  not an oversight: `auto_closed` only enables the very next keystroke's
  type-over (`types_over`, gated on `buffer.selections().primary()`
  specifically), so it records only the *primary*'s own resulting
  position. Typing a matching closer immediately after a multi-cursor
  auto-close types over the primary's pair and inserts a literal
  duplicate closer at every other cursor rather than typing over there
  too. Narrow, honestly cut, and no worse than "nothing happens" — the
  character the user typed still appears, just as a literal rather than a
  skip-over.
- **Type-over** (`move_past`): not a buffer edit at all (no `Transaction`),
  so it doesn't go through `apply_per_selection` — it directly replaces
  the primary's own entry in the selection list with a caret one
  character further along, leaving every other selection untouched.
- **Enter** (`insert_newline_with_indent`): per selection, exactly the
  existing single-selection logic (`newline_indent`, `splits_a_pair`'s
  `{|}`-splitting case), each evaluated against *that* selection's own
  `start()` and its own surrounding text — a selection on one line and
  another on a completely different line each get their own, independently
  correct indent. `anchor_offset == head_offset == first.len()` (the caret
  lands right after the first line's own indent, before any closer-line
  text the `{|}` case appended — unchanged from the single-selection
  behaviour, now just computed per selection).
- **Backspace** (`delete_backward`): per selection, exactly the existing
  three-way logic (non-empty selection deletes its range; empty selection
  checks the immediately-surrounding characters for a bracket pair;
  otherwise a fold-aware one-step-left deletion via the now-parameterized
  `move_caret_with_folds`). A selection with truly nothing before it (start
  of buffer) contributes the identity no-op entry.
- **Forward Delete** (`KeyCode::Delete` in `handle_editor_key`): per
  selection, the fold-aware one-step-right range via
  `move_caret_with_folds`, replaced with nothing; a selection at the very
  end of the buffer contributes the identity no-op entry.
- **Tab** (`indent_or_insert_tab`): the non-empty-selection branch already
  delegated to `TextBuffer::indent_selection_lines`, already
  multi-selection-safe at the core level (`crates/core/src/text/ops.rs`),
  untouched. The empty-selection branch (`buf.buffer.insert(head, "\t")` —
  previously a single, primary-only `Buffer::insert` call) now goes
  through `apply_per_selection`: each empty selection gets its own indent
  unit inserted at its own head.
- **Arrow-key movement** (`handle_editor_key`'s direction block): every
  selection moves, not just the primary. `Left`/`Right` are fully
  independent per selection (each is `move_caret_with_folds(buf,
  selection.start(), direction)` against that selection's own position,
  same post-step correction as before). `Up`/`Down` share **one**
  `buf.desired_column` across every selection in the buffer, deliberately
  — not per-selection sticky columns. `ide-ui`'s A2 (irrelevant here,
  never audited) aside, tracking N independent sticky columns would need
  `desired_column: Vec<Option<usize>>` remapped alongside `Selections`
  through every edit and selection-count change; a single shared value is
  a real, bounded simplification (documented here, not silently assumed):
  every cursor snaps toward the *same* target column when moving
  vertically, which only visibly differs from independent columns when
  cursors sit on lines of very different lengths and only for the
  duration of a vertical run. `shrink_stack` is cleared the same way
  every other action that changes selections already clears it (matching
  `run_line_op`'s own explicit-clear precedent, since these run through
  `run_action`, not through `handle_editor_key`'s own unconditional
  top-of-function clear).

### 3.5 `apply_per_selection`'s all-or-nothing fallback

`Transaction::new` rejects overlapping changes. Two selections' own
*derived* ranges (not their original ranges, which can never overlap) can
theoretically collide — e.g. two bare carets two characters apart, both
independently computing a bracket-pair-delete range that reaches into the
same character. `apply_per_selection` treats this the only safe way
available to it: build nothing, apply nothing, leave the buffer and every
selection completely untouched, rather than partially applying half the
batch or panicking. This is the same conservative shape
`open_delimiter`/`insert_newline_with_indent`/`delete_backward` already
used individually (`let Ok(transaction) = Transaction::new(...) else {
return false }`) before this phase, just now shared by one function
instead of duplicated four times.

## 4. Constraints & invariants

1. **Non-empty by construction, sorted, non-overlapping.** Inherited
   entirely from `ide-core`'s `Selections` type — nothing in this phase
   constructs one by hand outside `apply_per_selection`'s own
   `Selections::new` call, which re-derives these guarantees the same way
   every other caller of `Selections::new` already does.
2. **One edit, one undo step.** `apply_per_selection` builds exactly one
   `Transaction` and calls `Buffer::apply` exactly once per invocation,
   the same invariant `type_text`/`insert_at_selections` already give
   `ide-core` callers for free.
3. **`MAX_OCCURRENCES` cap.** Enforced entirely inside
   `ide_core::all_occurrences` — nothing new to check in this crate.
4. **Selection-creating commands never touch the undo history beyond the
   group break `set_selections` already performs** (`crates/core/src/
   text/mod.rs`'s `set_selections` calls `history.break_group()`) — moving
   or adding cursors is not itself an edit.
5. **Rendering cost.** The new `selections` overlay is `O(selections)` to
   build per frame and contributes at most two boundaries per non-empty
   selection to `styled_line`'s existing boundary walk — bounded by
   `MAX_OCCURRENCES`, same as every other per-frame overlay in this file.
6. **`apply_per_selection`'s all-or-nothing fallback** (§3.5) is a
   deliberate safety property, not an oversight — verified by a dedicated
   test (§5).
7. **No new keyboard reading outside `commands.rs`/`handle_editor_key`'s
   existing dispatch shape** — `binding_for` stays the single lookup
   table; nothing new reads `crossterm` events directly.

## 5. Examples

**Batch-rename via occurrence search:**

```rust
// Ctrl+G, Ctrl+G, Ctrl+G -- select "count" and its next two occurrences
app.run_action(Action::AddNextOccurrence); // primary empty -> selects "count"
app.run_action(Action::AddNextOccurrence); // adds the next "count"
app.run_action(Action::AddNextOccurrence); // adds the one after that
// typing now replaces all three in one transaction, one undo step
```

**`apply_per_selection`'s all-or-nothing guarantee:**

```rust
// Two carets whose derived backspace ranges would overlap: nothing
// changes anywhere, for either selection.
let before = active_text(&app);
app.handle_key(plain_key(KeyCode::Backspace));
assert_eq!(active_text(&app), before);
```

## 6. Dependencies & integration points

**Depends on**: `T18a`/`T18b`'s smart-editing functions (rewritten to be
multi-selection-safe, not otherwise changed), `T19`'s `folding.rs`
(`move_caret_with_folds`'s new `offset` parameter), `ide-core`'s A3 API
(already merged, previously unused by this crate).

**Tests.** `#[cfg(test)] mod tests` alongside the code, ≥80% line coverage
on every non-rendering file touched. `ui.rs`'s per-frame wiring is
rendering-only and exempt, same as every prior `T`-item.

1. `commands.rs`: each of the four new bindings resolves to its `Action`;
   `Ctrl+G`/`Ctrl+Shift+G` resolve to the new actions (inverting the old
   "not globally registered" test, §"Revision notes"); no collision with
   any of the 33 pre-existing bindings (same collision-check script every
   prior `T`-item has run).
2. `AddNextOccurrence`: empty primary selects the word under the caret and
   stops; a second press adds the next occurrence; wraps once; is a no-op
   once every occurrence is already selected; is a no-op with an empty
   needle; a number-literal caret *does* select (this crate's `word_at`
   difference from `ide-ui`'s, stated explicitly, not just implied by the
   passing test).
3. `UnselectOccurrence`: removes the most-recently-added selection; no-op
   at one selection.
4. `SelectAllOccurrences`: selects every occurrence in one press from an
   empty primary; the match containing the old primary stays primary;
   no-op with an empty needle.
5. `CollapseSelections`: collapses to primary; no-op at one selection; no-op
   outside `Focus::Editor`.
6. `apply_per_selection`: one entry per selection regardless of skips (a
   no-op entry keeps the list aligned); one `Transaction`/one `apply` call
   for N real edits; the all-or-nothing fallback on an overlapping derived
   range (§3.5's example); `primary_index` survives unchanged.
7. Every rewritten function (`open_delimiter`, `move_past`,
   `insert_newline_with_indent`, `delete_backward`, forward `Delete`,
   `indent_or_insert_tab`, arrow-key movement) gets a regression test with
   **two or more** selections proving every selection survives and is
   correctly edited/moved, plus confirmation that every pre-existing
   single-selection test for these functions still passes unchanged (no
   behavioural regression at N=1).
8. `styled_line`/`LineOverlays`: a non-empty selection washes its range; a
   bare caret contributes nothing; the primary's own non-empty selection
   washes exactly like any other; two adjacent non-empty selections don't
   create a gap or double-wash at their shared boundary.
9. `render_editor`'s native-cursor fix: primary's `.head` (not `.start()`)
   is what's asserted after a reversed non-empty primary selection is
   created via `AddNextOccurrence`'s first press against a caret sitting
   at a word's right edge (`word_at` returns the same range regardless of
   approach direction, but the *caret* placed on it should read as
   "forward" per this crate's own selection convention — verified against
   `word_at`'s actual output shape rather than assumed).

## 7. Revision notes

Self-review round (inline, no `hacker` pass — no security-sensitive path
touched):

1. Discovered mid-implementation, not anticipated at doc-drafting time:
   `open_delimiter`, `insert_newline_with_indent`, `move_past`, and
   `delete_backward` all discarded every non-primary selection via
   `set_selections(Selections::single(..))`, and arrow-key movement in
   `handle_editor_key` did the same. None of this is mentioned in
   `ide-ui`'s own `multiple-cursors.md`, because `ide-ui`'s A2 never had
   the bug in the first place. §1/§3.4 above are written to state this
   plainly rather than silently presenting the fix as if it were always
   the plan.
2. `TextBuffer::surround_selections` already existed
   (`crates/core/src/text/ops.rs`, from this crate's own `T18a`/A4a
   lineage) and `tui-smart-editing.md`'s own prose already claimed
   `open_delimiter` used it — the actual code never did. Fixed as a
   byproduct of making the wrap branch multi-selection-safe (§1.2).
3. `ctrl_g_and_ctrl_shift_g_are_not_globally_registered` (`commands.rs`)
   is renamed/inverted rather than deleted, since the invariant it checked
   (find-bar-local while the bar is open) is still true — only the
   "and therefore never resolves through `binding_for` at all" half of its
   old name is now false.
4. `reveal_caret_if_hidden` had the same primary-only-collapse bug as
   §1's other four functions, just missed on the first pass through that
   list — it read/wrote only `selections().primary()` and called
   `set_selections(Selections::single(..))` on scroll-reveal, silently
   dropping every non-primary selection the instant the view needed to
   scroll to keep the caret visible. Fixed to loop over every selection,
   only calling `set_selections` if at least one of them actually moved
   (avoiding a spurious edit-adjacent notification on every frame where
   nothing needed revealing).
5. Forward `Delete` had a pre-existing bug unrelated to multi-selection
   per se: it always computed one-character-right-of-`start()` regardless
   of whether the selection was empty, so deleting a non-empty selection
   only ever nibbled its first character instead of removing the whole
   range — inconsistent with `delete_backward`'s existing convention.
   Fixed as part of routing `Delete` through `apply_per_selection` (§3.4),
   and covered by
   `delete_forward_on_a_non_empty_selection_deletes_its_whole_range`.
6. Rendering (`highlight.rs`/`ui.rs`, §2.3): `LineOverlays` gained the
   `selections` field and `styled_line`'s boundary walk applies
   `Color::Yellow`, checked after `highlights` and before `bracket_pair`
   per §2.3's ordering. `ui.rs`'s `render_editor` computes the list once
   per frame from `text_buffer.selections().all()`, filtering to non-empty
   ranges; the native terminal cursor switched from `.primary().start()`
   to `.primary().head` (dormant correctness fix — every selection was a
   bare caret by construction before this phase, so `start() == head`
   always held and the bug was unobservable until now). New tests:
   `styled_line_applies_a_yellow_background_to_a_non_empty_selection`,
   `styled_line_a_bare_caret_selection_contributes_no_wash`,
   `styled_line_two_adjacent_selections_wash_with_no_gap_or_double_wash`.
