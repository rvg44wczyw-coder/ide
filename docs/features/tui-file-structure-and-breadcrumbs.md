# TUI: File Structure & Breadcrumbs (T36)

## 1. Purpose

Ports `file-structure-and-breadcrumbs.md` (C3) — the File Structure popup
(`⌘F12`, a standalone outline of the active file's own symbols) and
Breadcrumbs (a thin bar showing the chain of symbols the caret currently
sits inside) — into `ide-tui`. Both features are built entirely on
`textDocument/documentSymbol` machinery that is **already merged in this
crate** from `tui-go-to-file-and-symbol.md` (T16): `LspBridge::
document_symbols`/`document_symbols_path`/`request_document_symbols`
already exist, and `ide_lsp::symbols_containing` (the one small pure
function C3 added to `ide-lsp` for `ide-ui`) is already exported from
`crates/lsp/src/lib.rs` and untouched by this phase — **zero new `ide-
core`/`ide-lsp` API**, entirely `rust-tui-dev` scope, the same position
T16 itself was in for its own dependencies.

### 1.1 What's reused vs. new

| Piece | Status before this phase |
|---|---|
| `ide_lsp::symbols_containing` | Already merged (C3, `ide-ui`'s own phase) — this crate already depends on `ide-lsp` and can call it directly |
| `LspBridge::document_symbols: Vec<Symbol>` / `document_symbols_path: Option<PathBuf>` | Already merged (T16) |
| `LspBridge::request_document_symbols(&mut self, path: &Path)` | Already merged (T16) — currently only called from `sync_go_to_symbol`'s empty-query branch (on-demand, only while the Go to Symbol popup is open) |
| Continuous per-edit refresh of `document_symbols` for the active file | **New this phase** — needed so Breadcrumbs stay live without the Go to Symbol popup ever being opened |
| File Structure popup (list, filter, jump) | **New this phase** |
| Breadcrumbs bar | **New this phase** |

### 1.2 Scope cuts (explicit)

- **No click-to-jump on breadcrumb segments.** `ide-ui`'s version makes
  each segment a clickable label; this is a real, accepted capability
  gap relative to it, not a free cut — breadcrumbs exist precisely so a
  user can answer "what am I inside of right now" and act on it without
  opening anything, and a read-only bar that only answers the first half
  of that is a smaller feature. Cut anyway because closing the gap needs
  a new per-segment `HitMap` entry (`docs/features/tui-mouse-support.md`
  §2.2) purely to reach jump targets the File Structure popup's own
  fully keyboard-navigable list (Up/Down/Enter) already reaches — the
  implementation cost buys back a convenience shortcut, not new
  capability, and isn't worth it for this phase. The bar is read-only
  context in `ide-tui` as a result, matching the "no gutter click"
  precedent other panels in this crate have already set for
  keyboard-first parity ports (e.g. `git_gutter.rs`/`blame_gutter.rs`
  popups open from the keyboard, not a gutter click, since there's no
  gutter column at all).
- **No real collapsible tree widget** in the File Structure popup — same
  cut `file-structure-and-breadcrumbs.md` §3.5 already made for `ide-ui`
  itself (a flat, indentation-only list). `visible_rows`/`symbol_depths`
  are ported verbatim, so this crate inherits the same shape for free.
- **No settings toggle for hiding breadcrumbs** — `ide-ui` doesn't have
  one either (§3.5: "no toggle command… JetBrains' own default keymap has
  none"), and `ide-tui` has no settings screen to host a checkbox in even
  if it wanted one.

## 2. Interface

### 2.1 New module: `crates/tui/src/file_structure.rs`

A direct, near-verbatim port of `crates/ui/src/file_structure.rs` — pure,
no `App` dependency, same shape `blame_gutter.rs`/`git_gutter.rs` already
establish for this crate's "pure logic, tested without a terminal harness"
modules:

```rust
pub struct FileStructureRow {
    pub symbol_index: usize,
    pub depth: usize,
}

pub fn symbol_depths(symbols: &[Symbol]) -> Vec<usize>
pub fn visible_rows(symbols: &[Symbol], query: &str) -> Vec<FileStructureRow>
```

Identical algorithm and behaviour to `file-structure-and-breadcrumbs.md`
§3.1 (stack-based left-to-right depth pass; `visible_rows("")` returns
every row in declaration order with real depths, `visible_rows(query)`
for non-empty `query` fuzzy-scores each symbol's `name` via `ide_core::
fuzzy_score`, keeps matches, sorts by score descending with a stable tie
-break on declaration order, and emits every surviving row at `depth: 0`).
No changes needed to port this — it has zero `egui`/`IdeApp` surface to
strip, unlike e.g. `clone_panel.rs`'s port of `ide-ui`'s `CloneState`.

### 2.2 `crates/tui/src/lsp_bridge.rs`

No new fields or methods — `document_symbols`/`document_symbols_path`/
`request_document_symbols` already exist (T16). This phase only adds two
new **call sites** for the existing `request_document_symbols` (§3.3).

### 2.3 `crates/tui/src/app.rs`

```rust
#[derive(Default)]
pub(crate) struct FileStructureState {
    pub(crate) query: String,
    pub(crate) selected: usize,
}
```

`App` gains `file_structure: Option<FileStructureState>`, joining
`close_all_overlays`'s reset list and `handle_key`'s interception chain
(alongside `go_to_file`/`go_to_symbol` — position among mutually-exclusive
overlay siblings is arbitrary, same as every existing entry there) and
`any_popup_open`.

New methods, mirroring `toggle_go_to_symbol`/`handle_go_to_symbol_key`/
`confirm_go_to_symbol`'s existing three-part shape:

```rust
fn toggle_file_structure(&mut self);
fn handle_file_structure_key(&mut self, key: KeyEvent) -> LoopSignal;
fn confirm_file_structure(&mut self);

fn active_document_symbols(&self) -> &[Symbol];
fn active_breadcrumbs(&self) -> Vec<&Symbol>;
```

- `active_document_symbols` — same shape as `active_semantic_tokens`/
  `active_inlay_hints`: `&self.lsp.document_symbols` when `self.lsp.
  document_symbols_path` equals the active tab's path, else `&[]`. Unlike
  those two (which key a `HashMap<PathBuf, _>` by path), `document_
  symbols`/`document_symbols_path` is a single-slot pair (T16's own
  shape, matching `ide-ui`'s identical single-slot design) — the
  comparison is a direct `Some(path) == Some(&buf.path)` check, not a map
  lookup.
- `active_breadcrumbs` — `ide_lsp::symbols_containing(self.
  active_document_symbols(), position)`, where `position` comes from
  `active_caret_offset()` converted via `ide_lsp::byte_offset_to_position`
  against the active buffer's current text (the same conversion `app.rs`
  already performs at three other call sites, e.g. line 2835); `Vec::
  new()` with no active tab, no path, an offset that doesn't convert, or
  an empty `active_document_symbols()`. Same one-frame-staleness-on-edit
  acceptance `file-structure-and-breadcrumbs.md` §2.3 already documents
  for `ide-ui`'s identical accessor — self-corrects next frame, not worth
  new synchronization machinery.
- `toggle_file_structure` — no-op (doesn't open) with no active tab or an
  untitled/path-less buffer, mirroring `trigger_fetch`'s early-return
  shape (`docs/features/git-fetch-pull-push.md` §2.3) rather than opening
  an empty popup with nothing to show. Otherwise opens with `query`/
  `selected` reset to their defaults.
- `confirm_file_structure` — resolves the selected row against
  `visible_rows(active_document_symbols(), &query)`, calls `self.
  open_location(symbol.location.clone())` (the same existing Goto/Find
  Usages/Go to Symbol jump helper — no new jump path needed), then closes.
  No-op (dialog stays open) if the selection is out of range, same
  "row list changed under an in-flight action" guard `confirm_go_to_symbol`
  already has.

### 2.4 New command (`crates/tui/src/commands.rs`)

```rust
Command {
    id: "FileStructure",
    title: "File Structure",
    binding: Some((KeyModifiers::NONE, KeyCode::F(12))),
    action: Action::FileStructure,
}
```

Literal `F12`, not a `Ctrl`-translated letter — same reasoning
`QuickDocumentation`'s own doc comment already gives for `F1`: `ide-ui`'s
binding (`⌘F12`/`Ctrl+F12` via `Binding::same`'s `.command()`
substitution) is the *same physical key* on both platforms, a function
key needs no `Ctrl`-masking or Kitty-protocol disambiguation on any
terminal, so it's used exactly as written rather than picking an
unrelated free letter. Confirmed no existing `KeyCode::F(12)` binding in
`commands.rs` before adding this (`grep` returned nothing) — covered
going forward by `no_two_bound_commands_share_the_same_chord`.

### 2.5 `crates/tui/src/ui.rs`

```rust
fn render_file_structure_popup(frame: &mut Frame, app: &App, area: Rect);
fn render_breadcrumbs(frame: &mut Frame, app: &App, area: Rect);
```

- `render_file_structure_popup` — same centered-`Rect`-plus-bordered-`List`
  shape `render_go_to_symbol_popup` already establishes; each row shows
  `name` indented by `row.depth * 2` spaces (no real tree lines, per
  §1.2/§3.1) with `kind` as trailing context in parens (e.g. `"bar
  (Method)"`), the selected row reverse-styled the same way every other
  list popup in this crate already highlights its selection.
- `render_breadcrumbs` — always allocates its 1-row `area` (see §3.4 for
  why "always," not conditional); renders a single `Paragraph` joining
  `active_breadcrumbs()`'s symbol names with `" › "`, or nothing (blank
  row, no placeholder text) when the chain is empty.

Both wired into `render_editor` (§3.4), not the top-level popup dispatch
list `render_file_structure_popup` otherwise would have joined — a
breadcrumb bar is inline chrome, not an overlay.

## 3. Behaviour

### 3.1 File Structure popup lifecycle

`F12` (no-op with no active path, §2.3) calls `toggle_file_structure`.
While open:

- Typing/Backspace edit `query` freely (same `Ctrl`-guard on the char arm
  `handle_go_to_symbol_key`/`handle_search_key` already use), resetting
  `selected` to `0` on every edit (same "stale index past a shorter new
  list" fix `tui-go-to-file-and-symbol.md` §7 already made for its own
  two overlays — applied here from the start).
- `Up`/`Down` move `selected`, clamping at `0` and `visible_rows(...).len()
  - 1` rather than wrapping — the actual shape `handle_go_to_file_key`/
  `handle_go_to_symbol_key` use (checked directly: neither wraps, despite
  an earlier draft of this section claiming otherwise).
- `Enter` calls `confirm_file_structure` — no row-click path, consistent
  with `render_go_to_symbol_popup`'s own popup (this crate's `HitMap` has
  no per-row click target for any list popup, only `tree_area`/
  `editor_text_area`/`tab_strip`) and with §1.2's own framing of this
  popup as reached via "Up/Down/Enter".
- `Esc` closes without jumping.

No background thread, no polling — `visible_rows` is called fresh every
render from whatever `active_document_symbols()` currently holds (already
computed for breadcrumbs' sake regardless, §3.3); a big file's outline is
at most a few hundred symbols, cheap enough to fuzzy-score synchronously
on every keystroke, matching Go to Symbol's own non-background empty
-query branch.

### 3.2 Breadcrumbs

Rendered every frame from `active_breadcrumbs()` — no lifecycle, no
open/close state, no key handling (§1.2 cuts click; there is nothing else
for a key to do to a read-only bar). Empty renders as a blank row, not a
placeholder — matches `render_git_gutter`'s existing "nothing to show
today" convention of drawing nothing rather than an explicit "no
info"-style label.

### 3.3 Keeping `document_symbols` fresh for the active file

Both features need `LspBridge::document_symbols` to already be the
*active* tab's own outline continuously, not just while the Go to Symbol
popup happens to be open. This phase adds exactly one new call to the
already-existing `request_document_symbols` at each of the two places
`tui-semantic-highlighting.md`/`tui-hover-and-inlay-hints.md` already
fire their own per-file LSP refresh from:

- `open_or_focus_tab`, immediately after the existing `self.lsp.
  request_semantic_tokens(&path)` / `request_inlay_hints` calls, before
  `self.lsp.send(LspRequest::DidOpen { .. })`.
- `sync_lsp_did_change`, immediately after its existing `request_
  semantic_tokens` call.

Unlike `sync_go_to_symbol`'s `requested_for`-gated empty-query branch
(needed there because that method runs **every frame** while the popup is
open, and re-sending an identical request every frame for the whole
duration a slow server takes to answer would be wasteful), neither of
these two call sites is a per-frame poll — `open_or_focus_tab` fires
exactly once per actual file open, `sync_lsp_did_change` exactly once per
actual edit. Both already fire their sibling `request_semantic_tokens`/
`request_inlay_hints` calls unconditionally at the same cadence with no
extra gating field, so `request_document_symbols` joins them the same
way — **no new tracking field needed**, a simpler shape than T16's own
`requested_for`/`document_symbols_requested_for`, because this call site
genuinely has no "called every frame" problem to gate against.

### 3.4 Rendering placement and `EDITOR_CHROME_ROWS`

`render_editor`'s existing `sections` `Layout` (`Constraint::Length(1)`
tab strip + `Constraint::Min(0)` text) gains a middle row:
`[Length(1) tab strip, Length(1) breadcrumbs, Min(0) text]`.
`ui::EDITOR_CHROME_ROWS` (currently `4`: 1 status bar + 2 `render_editor`
block borders + 1 tab strip) becomes `5`.

This row is **always reserved, whether or not `active_breadcrumbs()` is
non-empty** — a deliberate divergence from `ide-ui`'s conditional height
(egui negotiates layout at render time, so an empty breadcrumb bar there
costs zero height). `EDITOR_CHROME_ROWS`'s own doc comment already states
why that shortcut isn't available here: `lib.rs`'s main loop calls
`app.set_editor_viewport_rows(term_size.height.saturating_sub(
EDITOR_CHROME_ROWS))` **before** any `Layout` pass runs, to know how many
text rows are visible for scroll/caret math in `handle_editor_key`. If the
breadcrumb row's presence varied frame-to-frame with cursor position, that
pre-computed row count would drift from what `render_editor` actually
draws on any frame where the caret enters or leaves a symbol — exactly
the kind of desync this constant's own comment warns a `Layout` change
must not introduce. Reserving the row unconditionally (rendering it blank
rather than omitting it) keeps the single source of truth intact at the
cost of one always-present, sometimes-blank line — the same trade-off
this crate already made for `render_tab_strip`'s row (never conditionally
hidden either, even with a single tab open).

### 3.5 What v1 doesn't cover

Same three items `file-structure-and-breadcrumbs.md` §3.5 already
excludes for `ide-ui` (no real tree widget, no breadcrumb toggle, no
kind-based breadcrumb filtering) apply unchanged here, plus §1.2's
`ide-tui`-specific click-to-jump cut.

## 4. Constraints & invariants

- Not security-sensitive per `CLAUDE.md`'s list: no new subprocess, no
  new network/file-path input, no new disk write. `symbols_containing` is
  a pure read over already-validated `Symbol`/`Position` data (`Location`s
  inside `Symbol` are already validated against `project_root` inside
  `ide-lsp`, unchanged by this phase). `hacker` is skipped for this run,
  matching `file-structure-and-breadcrumbs.md` §4's identical conclusion
  for `ide-ui`.
- `visible_rows`/`symbol_depths` are pure functions with no per-frame
  background thread — never called from anywhere but `render_file_
  structure_popup`/`confirm_file_structure`, both driven by the current
  frame's already-in-memory `document_symbols`.
- `EDITOR_CHROME_ROWS`'s change (`4` → `5`) is the one layout-affecting
  edit in this phase — `lib.rs`'s single consumer (`set_editor_viewport_
  rows`) needs no other change, since it already just subtracts whatever
  the constant currently is.
- File Structure / Breadcrumbs's data source (`active_document_symbols`)
  is independent of `go_to_symbol`'s workspace-query branch — opening File
  Structure never touches `workspace_symbols`, and vice versa.

## 5. Examples

```rust
// caret inside `fn bar` inside `impl Foo`, file already open
let breadcrumbs = app.active_breadcrumbs(); // [&Foo, &bar], outermost first
// rendered as "Foo › bar" on the always-reserved row under the tab strip

app.toggle_file_structure();                // file_structure = Some(..)
// render_file_structure_popup lists every symbol in the file, `Foo` at
// depth 0 and `bar` indented one level under it, no query typed yet
app.confirm_file_structure();               // jumps to the selected symbol, closes
```

## 6. Dependencies & integration points

- Depends on `ide_lsp::symbols_containing`/`Symbol`/`SymbolKind` (C3/T16,
  already merged) and `LspBridge::document_symbols`/`document_symbols_
  path`/`request_document_symbols` (T16, already merged) — zero new
  `ide-core`/`ide-lsp` API.
- Reuses `open_location` (jump), `active_buffer`/`active_caret_offset`
  (already-existing accessors), `ide_lsp::byte_offset_to_position`
  (already used at three other call sites in `app.rs`).
- New file: `crates/tui/src/file_structure.rs`. Modified:
  `crates/tui/src/{app,commands,ui,lib}.rs` (`lib.rs` only if the
  `EDITOR_CHROME_ROWS` bump needs a comment update there — no logic
  change, since it already just reads the constant).
- Not security-sensitive per `CLAUDE.md`'s list — `hacker` pass not
  expected (§4).

Tests required:
1. `file_structure.rs`: same test shape `crates/ui/src/file_structure.rs`
   already has — `symbol_depths` over a nested fixture, `visible_rows("")`
   preserves declaration order and real depths, `visible_rows(query)`
   fuzzy-filters and flattens to `depth: 0`, empty-query vs. non-empty
   branches, an empty `symbols` slice.
2. `app.rs`: `active_document_symbols`/`active_breadcrumbs` (no active
   tab, path mismatch, real match, empty document\_symbols, offset that
   doesn't convert); `toggle_file_structure`'s no-op-without-a-path guard
   and open-with-reset; typing/Backspace/Up/Down/Esc; `confirm_file_
   structure` jumps via `open_location` and closes, no-ops out of range;
   `open_or_focus_tab` and `sync_lsp_did_change` both now call `request_
   document_symbols` (assert via a fake/no-op LSP client the same way
   existing `sync_lsp_did_change_requests_inlay_hints_for_the_whole_
   document`-style tests already verify sibling calls); `file_structure`
   joins `close_all_overlays`'s reset list, `handle_key`'s interception
   chain, and `any_popup_open`.
3. `commands.rs`: the new `F12` binding resolves to `Action::
   FileStructure` and collides with nothing.
4. `ui.rs` is this crate's exempt pure-rendering file (per its own module
   doc comment) — not required to hit the coverage target; `EDITOR_CHROME_
   ROWS`'s new value is exercised indirectly by any existing scroll/caret
   test that already depends on the constant being consistent with
   `render_editor`'s actual layout.

## Revision notes

- §3.1: corrected the Up/Down description from "wraps via `rem_euclid`"
  to "clamps" after `rev`'s code-review pass checked
  `handle_go_to_file_key`/`handle_go_to_symbol_key` directly and found
  neither actually wraps — the original wording was simply wrong about
  this crate's established convention. The shipped implementation
  clamps (matching every other list popup here) and needed no code
  change.
- §3.1: dropped "or a row click" from `confirm_file_structure`'s trigger
  list — no list popup in this crate has a `HitMap` entry for its rows
  (confirmed: `HitMap` only tracks `tree_area`/`editor_text_area`/
  `tab_strip`), and §1.2 already correctly describes this popup as
  keyboard-only. The shipped implementation has no click handling and
  needed no code change.
