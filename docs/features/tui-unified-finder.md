# T46: TUI Unified Fuzzy-Finder Overlay

## 1. Purpose

`docs/roadmap.md` §11's `T44`-`T48` series adopts the `Orbit TUI.dc.html`
mockup's full-screen navigation model for `ide-tui`. `T46` is the third
run in that series: a single overlay — "Search Everywhere" in JetBrains
terms — that merges three already-correct, already-shipped result sources
into one ranked list:

- **Files**, via the existing `files_search: FilesSearchPanel` background
  fuzzy file search (`docs/features/tui-go-to-file-and-symbol.md` §2.1,
  the same machinery `GoToFileState` already drives).
- **Symbols**, via the existing `lsp.workspace_symbols` (`docs/features/
  tui-go-to-file-and-symbol.md` §2.2, the same machinery `GoToSymbolState`
  already drives for its own non-empty-query branch).
- **Commands**, via the existing `commands()` registry (`commands.rs`),
  the same static list the command palette (`PaletteState`) and
  colon-command mode (`docs/features/tui-colon-command.md`) both already
  search.

Per the roadmap's own scope note: **no new search logic**. `T46` adds a
merge/ranking layer over these three sources — it re-scores each
candidate with `ide_core::fuzzy_score` (already used elsewhere in this
crate for `recent_files_rows`/`file_structure`/the branches popup) so all
three sources land on one comparable numeric scale, then sorts the
combined pool by that score. It does not add a new matcher, a new
background-search mechanism, or a fourth result category.

This is additive, not a replacement. `GoToFile` (`Ctrl+Shift+N`),
`GoToSymbol` (`Ctrl+Alt+Shift+N`), `FileStructure` (`F12`) and the command
palette (`Ctrl+Shift+A`, "Find Action") all keep their existing, unchanged
single-category behavior — exactly the same relationship `ide-ui`'s own
`search-everywhere.md` (roadmap `C2`, already shipped) already
established between its Search Everywhere popup and its own separate Go
to File/Go to Class/Go to Symbol/Go to Line shortcuts. `ide-tui` gaining
this overlay is TUI parity with that already-approved GUI design, not a
new product decision.

## 2. Interface

### 2.1 `FinderRow` (`app.rs`)

```rust
#[derive(Clone)]
pub(crate) enum FinderRow {
    File(PathBuf),
    Symbol(Symbol),
    Command(&'static Command),
}
```

One merged row. `Symbol` clones `ide_lsp::Symbol` (already `Clone`, same
as `go_to_symbol_rows`'/`file_structure_rows`' own return types already
require). `Command` is the same `&'static Command` reference
`PaletteState::filtered`/`ColonCommandState::filtered` already hold.

### 2.2 `UnifiedFinderState` (`app.rs`)

```rust
#[derive(Default)]
pub(crate) struct UnifiedFinderState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    ran_query: Option<String>,
    last_workspace_query: Option<String>,
}
```

Same "UI-local state only, results live elsewhere" convention
`GoToFileState`/`GoToSymbolState` already establish. `ran_query` gates the
`files_search` trigger exactly like `GoToFileState::ran_query`.
`last_workspace_query` gates the `lsp.query_workspace_symbols` trigger
exactly like `GoToSymbolState::last_workspace_query`. Unlike
`GoToSymbolState`, there is no `requested_for`/empty-query
`document_symbols` branch — see §3.1 for why the empty-query case is
simply "no rows" here.

New `App` field: `pub(crate) unified_finder: Option<UnifiedFinderState>`.
Presence is visibility, same convention every other overlay in this file
uses.

### 2.3 `DoubleTap` (new file, `double_tap.rs`)

```rust
pub const DOUBLE_TAP_WINDOW: f64 = 0.35;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct DoubleTap {
    last_press: Option<f64>,
}

impl DoubleTap {
    pub(crate) fn press(&mut self, now: f64) -> bool { .. }
    pub(crate) fn disarm(&mut self) { .. }
}
```

A direct, trimmed port of `ide-ui`'s own `editor::double_tap::DoubleTap`
(`crates/ui/src/editor/double_tap.rs`, backing its already-shipped `⇧⇧`
Search Everywhere gesture). Trimmed: `ide-ui`'s version also tracks
`armed_until`/`is_armed` for `A3`'s `⌥⌥`+arrow gesture, where the second
tap only *arms* a window for a *third*, different key (the arrow) to
follow. `T46`'s gesture completes on the second tap alone — nothing has to
follow it — so `is_armed`/`armed_until` have no caller here and are cut
rather than carried as dead code. If a future `ide-tui` run ports `A3`
itself, restore them then (mirroring `ide-ui`'s full struct at that
point), rather than speculatively keeping them now.

New `App` fields: `shift_double_tap: DoubleTap` and
`created_at: std::time::Instant` (the monotonic clock `press`'s `now: f64`
is measured against — `self.created_at.elapsed().as_secs_f64()` at each
call site, since `ide-tui` has no existing per-frame time source the way
`ide-ui`'s `egui::Context::input(|i| i.time)` already is for the GUI
port).

### 2.4 New/changed methods (`app.rs`)

- `fn toggle_unified_finder(&mut self)` — same open/close-via-`close_all_
  overlays` shape as every other `toggle_*` in this file.
- `fn handle_unified_finder_key(&mut self, key: KeyEvent) -> LoopSignal` —
  `Esc` closes; `Up`/`Down` move `selected` clamped to `unified_finder_
  rows().len()`; `Backspace` pops `query` (and resets `selected`, `ran_
  query` unaffected — the sync pass below re-detects the change);
  `Enter` calls `confirm_unified_finder`; any other unmodified `Char`
  appends to `query`. Same shape as `handle_go_to_file_key`/`handle_
  colon_command_key`.
- `fn confirm_unified_finder(&mut self) -> LoopSignal` — resolves the
  selected row (`unified_finder_rows().get(state.selected)`), closes the
  overlay (`self.unified_finder = None`) unconditionally first (matching
  `handle_palette_key`'s own `Enter` arm shape, which clears `self.palette`
  before dispatching, not `confirm_go_to_file`'s early-return-without-
  closing shape — a user pressing `Enter` on a currently-empty list has no
  more reason to stay in it than an empty command palette does), then
  dispatches by variant: `File(path)` → `open_or_focus_tab` + caret reset
  to offset 0 + `push_nav_location(0)`, verbatim `confirm_go_to_file`'s own
  body. `Symbol(s)` → `open_location(s.location.clone())`, verbatim
  `confirm_go_to_symbol`'s own body. `Command(cmd)` → `return self.run_
  action(cmd.action)`, verbatim `handle_palette_key`'s own `Enter` arm —
  the `-> LoopSignal` return type (unlike `confirm_go_to_file`/`confirm_go_
  to_symbol`, both `()`) exists solely so a `Command` row bound to
  `Action::Exit` actually propagates `LoopSignal::Exit` instead of being
  silently swallowed, the same reason `handle_palette_key`'s own `Enter`
  arm already returns it instead of falling through to its trailing
  `LoopSignal::Continue`. Every other branch returns `LoopSignal::Continue`.
- `pub(crate) fn unified_finder_rows(&self) -> Vec<FinderRow>` — see §3.2
  for the merge/ranking algorithm.
- `pub(crate) fn sync_unified_finder(&mut self)` — called once per frame
  from `lib.rs`'s main loop, right alongside `sync_go_to_file`/`sync_go_
  to_symbol`. See §3.1.

`close_all_overlays` and `any_popup_open` both gain a `unified_finder`
line. Both, not just `any_popup_open` — diverging from `PaletteState`'s
own precedent (only in `any_popup_open`) the same way `T45`'s
`ColonCommandState` already deliberately diverged, and for the identical
reason recorded in that doc: `PaletteState`'s omission from `close_all_
overlays` looks like an unexamined gap in the code that predates this
convention being thought through, not a deliberate design worth
propagating a third time.

No new `Command`/`Action` registry entry. `ide-ui`'s own Search Everywhere
has none either (confirmed by grepping `crates/ui/src/command.rs` — no
`SearchEverywhere` entry exists there), consistent with `T45`'s own
colon-command trigger also having no registry entry: a gesture-triggered
overlay whose *opening* keystroke isn't a normal orthogonal chord doesn't
need a palette-visible "open me" command any more than colon-command's `:`
trigger does.

## 3. Behaviour

### 3.1 Query lifecycle (`sync_unified_finder`)

No-op unless `unified_finder.is_some()`. An empty (trimmed) query shows no
rows at all and triggers no background work — same as `GoToFileState`'s
own "an empty query never runs a search" rule, and, unlike `GoToSymbolState`,
also covers the symbol source: `T46` never falls back to `lsp.document_
symbols` for an empty query the way `go_to_symbol_rows` does, because a
merged list has no single active file to key that fallback off, and
mixing "the current file's outline, unranked" with "nothing, because
there's no file/command query yet" would be a visibly inconsistent
row set. An empty query overlay is simply an empty list — the same UX an
empty-query `GoToFile` popup already shows ("Type to fuzzy-match a
file.").

For a non-empty (trimmed) query, on each frame where it differs from
`ran_query` and `files_search` isn't already mid-search: calls
`self.files_search.run(self.tree.clone(), query.clone())` and records
`ran_query`, resetting `selected` to 0 — verbatim `sync_go_to_file`'s own
body, reusing `self.files_search` (the *same* panel instance `GoToFile`
itself drives — safe because only one overlay is ever open at a time, per
`close_all_overlays`). Separately, on each frame where the query differs
from `last_workspace_query`: calls `self.lsp.query_workspace_symbols
(&query)` and records `last_workspace_query`, resetting `selected` to 0 —
verbatim `sync_go_to_symbol`'s own non-empty-query branch, reusing
`self.lsp.workspace_symbols` (again, the same field `GoToSymbol` itself
populates).

### 3.2 Row merge/ranking (`unified_finder_rows`)

Empty (trimmed) query → `vec![]` (§3.1). Otherwise, builds one
`Vec<(i64, FinderRow)>` from three sources, then sorts by score
descending and drops the score:

1. **Files**: every `FuzzyFileMatch` currently in `self.files_search.
   results` (already capped at `ide_core::MAX_FUZZY_FILE_RESULTS` and
   already carrying its own `.score` from `ide_core::fuzzy_match_files` —
   used as-is, not re-scored).
2. **Symbols**: every entry in `self.lsp.workspace_symbols`, re-scored via
   `ide_core::fuzzy_score(query, &symbol.name)` — the LSP response is
   already the *candidate pool* (an actual search, already correct, left
   untouched), but its members carry no client-visible numeric score
   comparable to a `FuzzyFileMatch`'s, so this doc's merge layer applies
   the same scorer everything else here already uses to get one. A symbol
   the server matched on some field other than its bare `name` (e.g. its
   container) and that doesn't independently satisfy `fuzzy_score` against
   `name` alone is dropped from this merged list (`None` → filtered out) —
   an accepted narrowing versus `GoToSymbol`'s own dedicated view (§4).
3. **Commands**: every entry in `commands()`, scored via `ide_core::
   fuzzy_score(query, cmd.title)`, `None` filtered out — same treatment as
   symbols, applied to the identical static list `filter_commands_by_
   title` (`T45`) already searches by plain substring; this is a stricter,
   differently-shaped filter than that one (subsequence-with-score, not
   substring), used here only because the merged list needs a score to
   rank by, not because `filter_commands_by_title` was wrong for its own,
   score-free use (that function is untouched by this doc).

Ties are broken by the three sources' own relative insertion order
(`Vec::sort_by_key` is stable) — no explicit tie-break is specified beyond
that.

### 3.3 The `⇧⇧` gesture (`handle_key`)

Checked once per key event, near the top of `handle_key` — before the
popup-priority dispatch chain reads it, but structured as a pure side
effect that never itself returns/consumes the event (see rationale
below), mirroring `ide-ui`'s own placement (`app/render.rs`'s per-frame
input pass) rather than `T45`'s consuming `:`-trigger placement:

```rust
if !self.is_text_editing_focused()
    && key.kind == KeyEventKind::Press
    && key.modifiers == KeyModifiers::SHIFT
{
    let now = self.created_at.elapsed().as_secs_f64();
    if self.shift_double_tap.press(now) && !self.any_popup_open() {
        self.toggle_unified_finder();
    }
}
```

Gated on `!self.is_text_editing_focused()` (`T45`'s own helper) for the
identical reason `ide-ui`'s version gates on `!ctx.text_edit_focused()`:
without it, two quick `Shift+Down` (selection-extend) keystrokes while
actually editing code — an extremely common sequence — would spuriously
pop this overlay open mid-edit. Gated additionally on `!self.any_popup_
open()` so the gesture can't fire out from under an already-open, unrelated
overlay.

**This mirrors `ide-ui`'s own approximation, not JetBrains' literal
bare-modifier-only gesture, and that gap is deliberate — see §4.** Every
`KeyEvent` whose `modifiers` is *exactly* `SHIFT` counts as a tap, not only
a bare, otherwise-empty Shift press — a deliberately narrower rule than
`ide-ui`'s own `if shift && !self.search_everywhere_shift_down` check
(`app/render.rs`), which reacts to `ctx.input(|i| i.modifiers.shift)` and
so also counts a `Ctrl+Shift+X` chord as a tap. `ide-tui` can't get away
with that looser version: this crate binds many real chords as
`CONTROL.union(SHIFT)` (`NextTab`, `Redo`, ...), and reusing `ide-ui`'s
"any modifier state with Shift held" rule verbatim meant two quick presses
of a single such chord — e.g. repeatedly cycling tabs with `Ctrl+Shift+]`
— spuriously popped this overlay open. Caught by an existing regression
test failing outright (`ctrl_shift_close_bracket_cycles_to_the_next_tab_
and_wraps`) during implementation, not by inspection — recorded here so a
future port of this same gesture elsewhere in this crate doesn't
reintroduce the looser check by copying `ide-ui`'s version too literally.
Requiring exact-`SHIFT` still keeps the gesture reachable on every
terminal without depending on the kitty keyboard protocol's
`REPORT_ALL_KEYS_AS_ESCAPE_CODES`/`REPORT_EVENT_TYPES` flags (`crossterm`
only reports a bare `KeyCode::Modifier(LeftShift)` press/release pair when
both are pushed, and most terminal emulators outside kitty/wezterm/recent
iTerm2 don't support them at all — `setup_terminal` currently pushes
neither).

The check itself never returns early — the same key event keeps flowing
through the rest of `handle_key`'s dispatch chain afterward. In practice
that chain immediately includes the `unified_finder.is_some()`
popup-priority check (placed right below, §2.4), so a keystroke that
*completes* the double-tap (and therefore just opened the overlay) is, on
that same call, handed straight to `handle_unified_finder_key` instead of
whatever it would otherwise have done (e.g. a tree-selection-extending
`Shift+Down`) — unlike `ide-ui`'s version, which has no equivalent
same-frame popup-priority re-read to worry about, since its check and the
underlying widget input live on entirely separate polling paths.
Concretely: with an empty query this is harmless (`handle_unified_finder_
key`'s `Up`/`Down` arms no-op against an empty row list, `Char`/`Backspace`
just seed the query), and is arguably the more intuitive outcome anyway —
the tap that opens Search Everywhere reads as "spent" opening it, not as
double-duty with whatever else that chord does elsewhere. This is a
deliberate acceptance, not an overlooked side effect: replicating `ide-ui`'s
literal "opens and also still acts normally" behavior would require
specifically suppressing the just-opened popup-priority check for the one
event that opened it, adding a second piece of one-shot state for a
cosmetic difference nobody would notice in practice.

### 3.4 Rendering (`ui.rs`)

`render_unified_finder`, wired into `render()` alongside `render_palette`/
`render_colon_command`. Reuses `render_scrollable_list` verbatim (the same
helper `render_go_to_file_popup` already uses) — no new list-rendering
logic. Title: `format!("Search Everywhere: {}", state.query)`. Each row's
display line, by variant:

- `File(path)` → the path relative to `project_root` (`display()`), same
  string `render_go_to_file_popup` shows via `m.relative`.
- `Symbol(s)` → `format!("{}  ({:?})", s.name, s.kind)`, same shape
  `go_to_symbol`'s own row rendering already uses (verified against the
  current `render_go_to_symbol_popup` body).
- `Command(cmd)` → `format!("{}  ({})", cmd.title, cmd.id)`, identical to
  `render_palette`'s own row format.

Popup geometry: near-fullscreen-minus-margin, the same `area.width/height.
saturating_sub(4)` centered shape `render_go_to_file_popup` already uses
(not the small fixed-height box `render_palette`/`render_colon_command`
use) — a merged three-source list needs the room a single-category one
doesn't.

## 4. Constraints & invariants

- The `⇧⇧` gesture is an accepted approximation, not the literal bare-
  modifier gesture (§3.3). Two concrete, accepted false-positive/negative
  gaps: (a) any two keystrokes whose modifiers are *exactly* `Shift`
  (no other modifier) within `DOUBLE_TAP_WINDOW` (350ms) outside
  text-editing focus can open the finder — e.g. rapidly repeating
  `Shift+Down` to extend a tree selection twice quickly, or `Shift+Tab`
  twice; the cost of hitting this is one `Esc` keystroke to close an
  unwanted overlay, never data loss or a stuck state. `Ctrl+Shift+X`-style
  chords are excluded by construction (exact-match, not `contains`), so
  this crate's own `NextTab`/`PrevTab`/`Redo`/etc. bindings never trigger
  it. (b) on a terminal that
  auto-repeats a physically-held key as a stream of otherwise-
  indistinguishable `KeyEventKind::Press` events (no `REPORT_EVENT_TYPES`
  enabled, `ide-tui`'s current default), holding a single `Shift+X`
  combo can itself register as repeated taps for the same reason. Both
  are judged acceptable for the same reason `ide-sanitizer` documents its
  own masking as "best-effort, heuristic" rather than a hard guarantee:
  the alternative (kitty-protocol-only bare-modifier detection) would make
  the *entire* gesture silently unreachable on any terminal without that
  protocol, which is strictly worse than an occasional harmless spurious
  popup on the terminals where it IS reachable via chorded Shift use.
- Symbol rows only ever come from `lsp.workspace_symbols` — never
  `lsp.document_symbols` (§3.1). A user who wants "jump to a symbol in the
  file I already have open" without typing anything still has `FileStructure`
  (`F12`) unchanged for exactly that case.
- No text-search ("Text" tab in `ide-ui`'s own Search Everywhere) — out of
  scope per the roadmap's own T46 line ("no new search logic... over three
  already-correct sources"); `ide-tui`'s equivalent (Search in Path,
  `docs/features/tui-search-and-replace-in-path.md`) remains a separate,
  unmerged entry point, same as `ide-ui`'s own Search Everywhere still
  keeps Search in Path as its own thing outside the popup.
- `unified_finder_rows()` is recomputed on every call (every render frame,
  every key-handling call that needs `.len()`) rather than cached on
  `UnifiedFinderState` — same as `go_to_symbol_rows`/`file_structure_rows`
  already are; the combined pool is bounded (≤200 files + workspace-symbol
  response size + the static command count) and re-scoring it is cheap
  relative to a full terminal redraw.
- `shift_double_tap`/`created_at` are process-lifetime `App` fields, not
  per-overlay state — they must keep ticking (and be checked) regardless
  of whether `unified_finder` is currently `Some`, otherwise the second of
  two taps made just as the first tap's window is about to expire would
  never be compared against anything.

## 5. Examples

Opening: user is browsing the Project tree (not text-editing-focused),
taps `Shift` (via any Shift-chorded key, §3.3) twice within 350ms →
`unified_finder` opens empty. Typing `mai` → `sync_unified_finder` starts
a `files_search` run and a `workspace_symbols` query for `"mai"`;
`unified_finder_rows()` returns, say, `main.rs` (a file match, high
score), a `fn main` symbol (a symbol match), and no commands (nothing in
`commands()` fuzzy-matches `"mai"`) — sorted by score, not by source.
Pressing `Enter` on the `fn main` row calls `open_location` and closes the
overlay, identical to what pressing `Enter` on that same row would do
inside a plain `GoToSymbol` popup.

## 6. Dependencies & integration points

- `ide_core::fuzzy_score`, `ide_core::fuzzy_match_files`,
  `ide_core::FuzzyFileMatch`/`FuzzyFileResults`/`MAX_FUZZY_FILE_RESULTS` —
  unchanged, reused as-is.
- `self.files_search: FilesSearchPanel`, `self.lsp.workspace_symbols`,
  `commands()` — unchanged, shared with `GoToFile`/`GoToSymbol`/the
  palette respectively.
- `T45`'s `is_text_editing_focused()` and `any_popup_open()` — reused
  verbatim as the gesture's guard conditions.
- New file `crates/tui/src/double_tap.rs`, a trimmed port of `ide-ui`'s
  `crates/ui/src/editor/double_tap.rs` (§2.3) — the only new production
  code that isn't itself the merge/ranking layer or the overlay
  plumbing.
