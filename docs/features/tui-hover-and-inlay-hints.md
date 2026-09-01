# `ide-tui`: Hover, Inlay Hints, Document Highlight (T12)

## 1. Purpose

Fourth item of the TUI-parity backlog (`docs/roadmap.md` §10). Ports
`inlay-hints-and-hover.md` (A7) to `ide-tui`: **Quick Documentation**
(`F1`, shows hover text in a popup), **inlay hint chips** (inferred
types/parameter names rendered inline), and **symbol highlighting**
(every occurrence of the symbol at the caret gets a background wash,
ambient, no keybinding). No new `ide-core`/`ide-lsp` surface -- A7 already
carries the full protocol machinery (`InlayHint`, `LspRequest::{Hover,
DocumentHighlight, InlayHint}`, `LspEvent::{Hover, DocumentHighlight,
InlayHint}`, bounded decode), already merged into `main`. This doc covers
only `ide-tui`'s own consumption, the same split every prior `T`-item in
this backlog has used.

## 2. Interface / API

### 2.1 `src/lsp_bridge.rs`

```rust
pub(crate) hover: Option<String>,
pub(crate) finding_hover: bool,
pub(crate) document_highlights: Vec<ide_lsp::Range>,
pub(crate) inlay_hints: HashMap<PathBuf, Vec<ide_lsp::InlayHint>>,

impl LspBridge {
    pub(crate) fn request_hover(&mut self, path: &Path, position: Position);
    pub(crate) fn request_document_highlight(&mut self, path: &Path, position: Position);
    pub(crate) fn clear_document_highlights(&mut self);
    pub(crate) fn request_inlay_hints(&mut self, path: &Path, range: ide_lsp::Range);
}
```

Direct ports of `ide-ui`'s `LspBridge` fields/methods of the same name
(A7 §2.2) -- same clear-at-send-time convention for `hover`/
`document_highlights`, same stale-but-plausible-until-replaced convention
for `inlay_hints` (matches `semantic_tokens`/`diagnostics`). `poll()`
gains three arms: `Hover` replaces `hover` wholesale and clears
`finding_hover`; `DocumentHighlight` replaces `document_highlights`
wholesale; `InlayHint` replaces `inlay_hints[path]` wholesale.
`ServerExited` clears `finding_hover`, `document_highlights`, and
`inlay_hints` -- but **not** `hover` itself, exactly matching `ide-ui`'s
own reasoning (its field doc comment, ported verbatim below): a popup
already showing hover text isn't made misleading by the server dying, the
way stale highlights/hints silently rendered as current would be.

### 2.2 `src/app.rs`

```rust
pub(crate) struct App {
    // ... existing ...
    pub(crate) hover_open: bool,
    last_highlighted_target: Option<(PathBuf, Position)>,
}

impl App {
    pub(crate) fn active_inlay_hints(&self) -> &[ide_lsp::InlayHint];
    pub fn sync_document_highlights(&mut self); // called once per frame by lib.rs
}
```

`hover_open` joins the four existing overlay booleans/`Option`s in
`close_all_overlays` (five-way mutual exclusion now). `trigger_quick_
documentation` (`F1`) reuses `lsp_query_target()` verbatim (the same
target-resolution `trigger_go_to_declaration`/`trigger_find_usages`
already share) -- opens the popup immediately (unlike Goto, which only
opens once zero-or-many is known) and shows a "loading…" state while
`lsp.finding_hover` is true, same shape A7 §2.2 describes.

`sync_document_highlights` fires a fresh query whenever
`lsp_query_target()` names a different target than
`last_highlighted_target`, and clears (once, not every idle frame) when
there's no valid target. Called once per frame from `lib.rs`'s run loop,
immediately after `poll_lsp`/`poll_cargo` -- ambient, ties to no key.

`request_inlay_hints` is folded into `sync_lsp_did_change` and
`open_or_focus_tab`, alongside the `request_semantic_tokens` calls
`tui-semantic-highlighting.md` §2.2 already added there -- same
"`ide-tui` has no `sync_inlay_hints`-sibling call site to duplicate at"
reasoning, now covering two per-file query kinds from the same two spots
instead of one.

### 2.3 `src/highlight.rs`

```rust
pub fn document_highlight_marks(text: &str, ranges: &[ide_lsp::Range]) -> Vec<Range<usize>>;
pub fn inlay_hint_chips(text: &str, hints: &[ide_lsp::InlayHint]) -> Vec<(usize, String)>;

pub struct LineOverlays<'a> {
    pub semantic_tokens: &'a [Token],
    pub highlights: &'a [Range<usize>],
    pub inlay_hints: &'a [(usize, String)],
}

pub fn styled_line(text_buffer: &TextBuffer, line: usize, overlays: &LineOverlays<'_>) -> Line<'static>;
// signature change: bundles the third-through-fifth parameters that
// tui-semantic-highlighting.md's single `semantic_tokens: &[Token]`
// param would otherwise have grown into, into one struct -- the same
// `LineContext` shape ide-ui's paint.rs already uses for the identical
// reason (multiple independent per-line overlay sources).
```

`document_highlight_marks` converts LSP ranges to absolute byte
`Range<usize>`, same drop-on-invalid-conversion tolerance
`semantic_token_marks` already established, sorted by start. No `kind`
mapping needed (unlike semantic tokens) -- a highlight is a plain
background wash, not a colored token.

`inlay_hint_chips` converts each hint's `position` to a byte offset via
`ide_lsp::position_to_byte_offset`, drops entries that don't convert, and
bakes `padding_left`/`padding_right` into the returned label string as a
literal leading/trailing space (`ide-ui`'s own §3.5 does the same padding
at paint time; here it happens once at conversion time instead, since
there's no separate paint-time padding decision to make in a plain-text
renderer). Sorted by offset.

`styled_line` is rewritten from its previous sequential-cursor walk (one
token boundary at a time) to a full boundary-list walk, the same
technique `ide-ui`'s `line_layout_job` already uses for the equivalent
reason: three independent overlay sources (merged fg tokens, highlight
bg ranges, point-insertion chips) no longer decompose into one ordered
non-overlapping sequence the old algorithm assumed. See §3.3 for the
exact algorithm and §4 for its no-overlap/no-gap invariants.

## 3. Behaviour

### 3.1 Quick Documentation

`F1` calls `trigger_quick_documentation`, gated by the same `lsp_query_
target()` no-op conditions `Ctrl+B`/`Ctrl+U` already share (no active
tab, no cursor offset, no running server). Opens `hover_open` immediately
and shows "Loading…" while `finding_hover` is true; `Esc` is the only key
`handle_hover_key` recognizes (closes, no navigation -- this is a
read-only popup, not a picker).

**Binding**: `F1` literally, not a `Ctrl`-translated letter. `ide-ui`'s
own binding is `{ mac: F1, other: Ctrl+Q }` -- the one case `CLAUDE.md`'s
keyboard-shortcuts section already names as a genuine mac/other split, not
`Binding::same`. `Ctrl+Q` is unreliable in a bare terminal (many terminals
still treat it as software flow-control XON/XOFF even with raw mode's
`IXON` typically disabled, and this crate has never needed to depend on
that being reliable before); `F1` needs no `Ctrl` masking or Kitty-
protocol disambiguation at all -- every terminal sends a distinct escape
sequence for a function key -- so it's used literally, matching the mac
binding exactly rather than choosing an unrelated free letter the way
`ToggleProblems`/`FindUsages` had to.

### 3.2 Query lifecycle

Identical shape to Goto/Find Usages/diagnostics/semantic tokens --
`request_hover`/`request_document_highlight`/`request_inlay_hints` are
no-ops with no client running; responses are permissive (`None`/`vec![]`
on error or `null`, never leaving a caller waiting forever).

### 3.3 Rendering: the boundary-list `styled_line` rewrite

For line `[line_start, line_end)`:

1. Compute `merged` fg-coloring tokens exactly as `tui-semantic-
   highlighting.md` §3 already does (regex tokens for the line merged
   with semantic tokens overlapping the line).
2. Clamp `overlays.highlights` to the line's bounds, dropping any that
   become empty.
3. Collect every boundary: `line_start`, `line_end`, every `merged`
   token's (clamped) start/end, every clamped highlight's start/end.
   Sort and dedup.
4. Walk consecutive boundary pairs `[b0, b1)`. For each non-empty
   subrange: **before** appending its text span, append a chip span
   (muted style, `Color::DarkGray` -- the same token this crate already
   uses for comments, reused here as "de-emphasized annotation" the same
   way `ide-ui`'s `Colors::fg_muted` is reused rather than adding a new
   token) for every entry in `overlays.inlay_hints` whose byte offset
   equals `b0`. Then append the text span itself, styled with the fg
   color of whichever `merged` token (if any) covers `[b0, b1)` --
   `merge_semantic_tokens`' no-overlap postcondition guarantees at most
   one does -- and, additionally, a background color
   (`Color::DarkGray`) if `[b0, b1)` falls inside any clamped highlight
   range. fg and bg are independent style channels, so a span can be
   both a colored token *and* highlighted at once (e.g. a highlighted
   occurrence of a type name keeps its type color, gains a background).
5. After the loop, append any remaining chips whose offset equals
   `line_end` (a hint positioned at the very end of the line, e.g. after
   the last token before a trailing `;`).

### 3.4 A terminal has no floating overlay -- chips are literal inserted characters

`ide-ui`'s inlay-hint chips are painted as a separate `painter.text()`
call at a precise pixel x-coordinate, never touching the row's own
`Galley` -- so on-screen glyphs after the chip's position are not
shifted, and the caret's own screen position (computed from the
*unmodified* galley) stays exactly aligned with the buffer's real text.
A terminal has no sub-cell/pixel compositing: `ratatui::text::Line` is a
flat concatenation of `Span`s, so inserting a chip's characters
necessarily shifts every subsequent character on that row right by the
chip's width, the same way any other terminal-based editor that renders
inline annotations as literal text does.

**Consequence, stated plainly rather than hidden**: `App::handle_key`'s
cursor-screen-column math (`editor::cursor_line_column`, buffer-offset-
based, unaware of chips) does not account for this shift. If a chip
renders earlier on the same row than the caret's column, the caret's
visual on-screen column will be off by the chip's width for that one
row, until the caret moves to a row without an earlier chip. This is a
real, documented v1 rough edge -- accepted rather than solved here,
because solving it needs either suppressing chips on the caret's own row
(a visible flicker exactly where the user is looking) or a second,
chip-aware column-mapping function threaded through every cursor-position
call site, and neither is justified by this batch's scope. Chips are
short (a type name, a parameter name) and the common case (a chip after
the caret's own column, or on a different row entirely) is unaffected.

### 3.5 Symbol highlighting

Ambient, ties to no key -- ranges follow the caret. Rendered as a
background wash (`Color::DarkGray`) via §3.3's boundary walk, the same
color reused for inlay-hint chip text -- distinct *channel* (background
vs. foreground), so there's no visual collision between "this span is a
chip" and "this span is a highlighted occurrence" even though both use
the same named color.

## 4. Constraints & invariants

- **No new `ide-core`/`ide-lsp` surface.** Entirely inside `crates/tui/**`.
- **`styled_line`'s boundary walk must produce a sequence of contiguous,
  non-overlapping spans covering exactly `[line_start, line_end)`** (chip
  spans are additional insertions, not part of this covering sequence --
  they consume no buffer-range width). Same postcondition
  `tui-semantic-highlighting.md` §4 already requires of the token-merge
  step, extended to cover the highlight-boundary splitting this batch
  adds; a gap or overlap here would either lose text or duplicate it on
  screen.
- **`hover` survives `ServerExited`, `document_highlights`/`inlay_hints`
  don't.** See §2.1 -- a deliberate asymmetry, not an oversight, ported
  directly from `ide-ui`'s own reasoning.
- **Caret-column drift under an earlier same-row chip is a known, accepted
  v1 limitation.** See §3.4. Not treated as a bug to silently work around.
- **Not on `CLAUDE.md`'s security-sensitive path list.** Same reasoning
  `tui-semantic-highlighting.md` §4 already gives: this diff only
  consumes already-validated, already-bounded-decoded `ide_lsp` data.
  Hover text is rendered as a plain `ratatui::widgets::Paragraph` --
  literal glyphs, no markdown/HTML interpretation anywhere in this path,
  the same "no parser to exploit" property A7 §3.3 documents for `ide-ui`.

## 5. Examples

```
$ ide-tui ~/code/my-rust-project
```

Caret on a function call: `F1` shows the callee's doc comment in a
popup; `Esc` closes it. Caret on a local variable `foo`: every other
`foo` in the file gets a dim grey background, following the caret as it
moves. A `let x = some_call();` line grows a muted `: i32` chip right
after `x` once `rust-analyzer` answers.

## 6. Dependencies & integration points

No new dependencies. Touches
`crates/tui/src/{lsp_bridge,app,commands,highlight,ui,lib}.rs`.

## 7. Diagrams

None -- established request/response/render shape throughout this
backlog; the one genuinely new algorithm (`styled_line`'s boundary walk)
is fully specified in prose in §3.3/§4 and is more precisely described in
code than a diagram would add.

## Revision notes

Implemented as the fourth item of the TUI-parity backlog, porting A7's
already-reviewed `ide-ui`-side design (query lifecycle, the `hover`-
survives-`ServerExited` asymmetry, the muted-color chip styling) rather
than redesigning it, adapted only where the terminal rendering medium
forces a real difference (§3.4's chip-shifts-text consequence, §3.3's
boundary-list rewrite of `styled_line` in place of `ide-ui`'s pixel-space
overlay). Self-reviewed inline (`rev`-style pass; not on the
security-sensitive path list, so no `hacker` pass, same reasoning
`tui-semantic-highlighting.md` gives): one controversial note --
`Color::DarkGray` for both inlay-hint-chip text and symbol-highlight
background was considered against giving each its own named color, and
kept as one reused token deliberately (fewer named tokens to keep
distinct across future theme work, and the two never appear on the exact
same character so there's no literal collision) -- flagged here as a
judgment call, not obviously the only right answer.

Implementation note (found during the build, not anticipated when this
doc was first written): §3.3's boundary list, as literally specified,
only collects line bounds, merged-token bounds, and clamped-highlight
bounds -- it doesn't mention inlay-hint-chip offsets. Taken completely
literally, a chip positioned in the middle of an otherwise-unbroken plain-
text span (no token or highlight edge nearby) would never get a boundary
to attach to and would silently fail to render. The implementation folds
chip offsets into the same boundary set as a correctness fix over the
doc's literal wording -- every consecutive pair a chip offset creates is
still a valid, correctly-styled split, so this doesn't weaken the no-gap/
no-overlap invariant §4 requires, it just makes the invariant's covering
finer-grained. Verified directly: `styled_line_inserts_a_chip_before_its_
target_offset` in `highlight.rs` places a chip at an offset with no other
nearby token/highlight boundary and confirms it still renders as its own
span ahead of the text that follows.

Self-reviewed inline (`rev`-style pass; no `hacker` pass, per the
security-sensitive-paths reasoning above) after implementation: no other
controversial findings beyond the two already noted (chip/highlight color
reuse, and this boundary-list correctness fix) -- the query lifecycle,
`hover`-survives-`ServerExited` asymmetry, and mutual-exclusion wiring all
match this doc's design and `ide-ui`'s own precedent exactly. Coverage on
every touched non-rendering file is well above the 80% floor (`app.rs`
96%, `highlight.rs` 99%, `lsp_bridge.rs` 91%, `commands.rs` 100%);
`ui.rs`/`lib.rs` stay at this crate's established rendering-only/entry-
point exemption.
