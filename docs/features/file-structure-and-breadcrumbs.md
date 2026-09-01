# File Structure & Breadcrumbs (C3)

## 1. Purpose

Two small navigation features built entirely on the `textDocument/
documentSymbol` machinery `search-everywhere.md` (C2) already wired up —
neither needs a new LSP request kind:

- **File Structure popup** (`⌘F12`): a standalone dialog listing the
  active file's own outline (every symbol `documentSymbol` reported for
  it), indented by nesting depth, filterable by typing, Enter/click jumps
  the caret to the selected symbol. This is Search Everywhere's own
  Symbols tab narrowed to "this file only" and given a persistent tree
  shape instead of a flat query-ranked list — a different enough
  presentation (always-visible hierarchy, no workspace-wide fan-out) to
  earn its own dialog rather than a third "scope" toggle bolted onto
  Search Everywhere.
- **Breadcrumbs**: a thin bar under the tab strip, above the editor,
  showing the chain of symbols the caret currently sits inside — outermost
  first (e.g. `impl Foo › fn bar`). Clicking a segment jumps the caret to
  that symbol's own start. Empty (renders nothing) when the caret isn't
  inside any symbol, or no `documentSymbol` answer exists yet for the
  active file.

`docs/roadmap.md` §2.7 lists this as "lsp + ui" — in practice `ide-lsp`
already has everything both features need (`Symbol`, `SymbolKind`,
`LspRequest::DocumentSymbol`/`LspEvent::DocumentSymbol`, all landed for
C2). The only `ide-lsp` addition this phase makes is one small pure
function, `symbols_containing` (§2.2) — the rest is `ide-ui`-only,
consuming what C2 already exposed.

## 2. Interface

### 2.1 `ide-core`

No change.

### 2.2 `ide-lsp` (`crates/lsp/src/types.rs`)

```rust
pub fn symbols_containing(symbols: &[Symbol], position: Position) -> Vec<&Symbol>
```

Every symbol in `symbols` whose `location.range` contains `position`,
**in the same relative order they appear in `symbols`**. Reuses the exact
range-containment test `position_is_within_interface` already uses
(`goto-declaration-interface-redirect.md` §2.1), generalized from "does an
`Interface` exist in the chain" to "give me the whole chain".

The order guarantee is what makes this useful for breadcrumbs without any
extra sorting: `flatten_document_symbols` (`crates/lsp/src/client.rs`)
always pushes a parent `Symbol` before recursing into its `children`, so a
whole file's `documentSymbol` result is already a pre-order, depth-first
listing — a parent always precedes every one of its descendants, and
siblings appear in declaration order. Filtering that list down to "ranges
containing one point" preserves relative order (a stable filter), and
because a `documentSymbol` tree's ranges nest strictly (a parent's range
always spans every descendant's — the same invariant
`position_is_within_interface`'s own doc comment already states), any two
symbols that both contain the same `position` cannot be unrelated siblings
(siblings' ranges don't overlap, so two of them can't both contain one
point) — they must be in an ancestor/descendant relationship. Put
together: the filtered output is automatically outermost-first, with no
separate sort step and no dependency on `container_name` string-matching.

Exported from `crates/lsp/src/lib.rs` alongside `position_is_within_interface`.

### 2.3 `ide-ui`

**New module `crates/ui/src/file_structure.rs`** (pure, no `IdeApp`
dependency — same shape as `editor::git_gutter`):

```rust
pub struct FileStructureRow {
    pub symbol_index: usize,
    pub depth: usize,
}

pub fn symbol_depths(symbols: &[Symbol]) -> Vec<usize>

pub fn visible_rows(symbols: &[Symbol], query: &str) -> Vec<FileStructureRow>
```

- `symbol_depths` — one nesting depth per entry in `symbols` (0 at the
  top level), computed from the same pre-order/range-nesting invariant
  §2.2 documents, via a single stack-based left-to-right pass (§3.1) —
  O(n), not the O(n²) an all-pairs containment check would cost.
- `visible_rows` — what the popup actually lists this frame (§3.1):
  every row when `query` is empty (natural declaration order, indented by
  `symbol_depths`), or a flat `ide_core::fuzzy_score`-ranked subset when
  it isn't (depth always `0` — filtering breaks the tree shape, so v1
  doesn't try to preserve indentation for a filtered result, same
  "search results are flat" precedent `search_everywhere_rows`'s Actions
  tab already set).

**`IdeApp` (`crates/ui/src/app.rs`)** — new fields:

```rust
file_structure_open: bool,
file_structure_query: String,
file_structure_selected: usize,
pending_file_structure_focus: bool,
```

New methods, modeled directly on `open_search_everywhere`/
`close_search_everywhere`/`search_everywhere_owns_escape`/
`search_everywhere_move_selection`/`search_everywhere_confirm`:

```rust
fn trigger_file_structure(&mut self)
fn close_file_structure(&mut self)
fn file_structure_owns_escape(&self) -> bool
fn file_structure_move_selection(&mut self, delta: isize)
fn file_structure_confirm(&mut self)
fn active_document_symbols(&self) -> &[Symbol]
fn active_breadcrumbs(&self) -> Vec<&Symbol>
```

- `active_document_symbols` — the active tab's own outline: `&self.lsp.
  document_symbols` if `self.lsp.document_symbols_path` equals the active
  tab's path, else `&[]`. Same shape as `active_inlay_hints`/
  `active_semantic_tokens`.
- `active_breadcrumbs` — `ide_lsp::symbols_containing(self.
  active_document_symbols(), position)`, where `position` comes from
  `self.active_cursor_offset` converted via `ide_lsp::
  byte_offset_to_position` against the active tab's current text; `Vec::
  new()` if there's no active tab, no path, no cursor offset yet, the
  offset doesn't convert, or `active_document_symbols()` is empty. Like
  every other `active_cursor_offset`-driven read in this file (see
  `find_usages_target`'s own doc comment), this can trail one frame behind
  a just-happened tab switch or cursor move — cosmetic only, self-corrects
  next frame, not worth a new synchronization mechanism for.
- `trigger_file_structure`/`close_file_structure` — same open/reset/close
  shape as their Search Everywhere counterparts (query cleared, selection
  reset to `0`, focus requested on open).
- `file_structure_confirm` — `Enter` or a row click (§3.2): resolves the
  selected row against `visible_rows(...)`/`active_document_symbols()`,
  calls `self.open_definition(&symbol.location.path, symbol.location.
  range.start)`, then `close_file_structure()`. No-op (dialog stays open)
  if the selection is out of range (e.g. the row list just changed under
  an in-flight click).

**New command** (`crates/ui/src/command.rs`):

```rust
Command {
    id: "FileStructure",
    title: "File Structure",
    category: "Navigate",
    binding: Some(Binding::same(KeyChord::new(Key::F12).command())),
    action: CommandAction::FileStructure,
}
```

`⌘F12` on mac, `Ctrl+F12` elsewhere via `.command()`'s existing symbolic
mac→ctrl substitution — matches JetBrains' own cross-platform default
verbatim, no `{mac, other}` split needed (`docs/roadmap.md` §5.2's own
table already lists `⌘F12` for this exact action). `is_command_enabled`
gains a `FileStructure` arm: enabled only when the active tab has a real
path (an untitled buffer has no `documentSymbol` answer to show). Neither
`fleet_binding` nor `vscode_binding` (`crates/ui/src/keymap.rs`) gets an
entry for `"FileStructure"` — this project has no confirmed reference for
either app's own File Structure binding, and `CLAUDE.md`'s "never invent a
binding" rule means an unconfirmed id is left to the existing `_ => None`
catch-all (reachable from the command palette, user-bindable), the same
precedent `ToggleFormatOnSave` already set.

**`crates/ui/src/app/render.rs`**:

- `render_file_structure_popup(&mut self, ctx: &egui::Context)` — an
  `egui::Window` "File Structure", modeled on `render_search_everywhere_
  popup`: a single-line query field (focus-requested on open), a scroll
  area of rows built from `visible_rows`, each row's label indented by
  `row.depth` (`ui.horizontal` + `ui.add_space(row.depth as f32 *
  INDENT_PX)` ahead of the symbol's name — no attempt at a real collapsible
  tree widget in v1, a flat indented list reads the same way and is far
  less code), Escape/click-outside closes via the same `open: &mut bool`
  pattern every other popup in this file uses.
- `render_breadcrumbs(&mut self, idx: usize, ui: &mut egui::Ui)` — one
  `ui.horizontal` row, rendered from `render_tabs_and_editor` right after
  the tab strip and before `render_external_change_banner`/
  `render_find_bar` (i.e. "under the tab, above the editor" per
  `docs/roadmap.md` §2.7's own phrasing). Renders nothing (no reserved
  height) when `active_breadcrumbs()` is empty. Each segment is an
  `ui.link`/`ui.selectable_label`-style clickable label; a `›` glyph
  separator between segments; clicking segment `i` calls `self.
  open_definition(&symbol.location.path, symbol.location.range.start)`
  (jumping within the same already-open file, same call `goto-definition.
  md`'s own same-file jumps already use).

## 3. Behaviour

### 3.1 `symbol_depths` / `visible_rows`

`symbol_depths` walks `symbols` left to right maintaining a stack of
`(end_position)` for every range currently "open". For each symbol: pop
the stack while its top does not contain the current symbol's own
`location.range.start` (a closed ancestor); the resulting stack length is
this symbol's depth; push its own `location.range.end`. This relies on
exactly the ordering/nesting invariant §2.2 already establishes — it does
not work (and isn't used) on an arbitrary, non-pre-order `Vec<Symbol>`.

`visible_rows("")` returns one row per input symbol, `depth` from
`symbol_depths`, in input order — the file's natural outline.
`visible_rows(query)` for non-empty `query` instead scores every symbol's
`name` with `ide_core::fuzzy_score(query, &symbol.name)`, keeps only
matches, sorts by score descending (ties broken by original/declaration
order, a stable sort), and emits each surviving row at `depth: 0`.

### 3.2 File Structure popup lifecycle

`⌘F12` (enabled only with a real active file, §2.3) calls
`trigger_file_structure`. While open: `↑`/`↓` call
`file_structure_move_selection(±1)` (wrapping, same `rem_euclid` shape as
`search_everywhere_move_selection`/`command_palette_move_selection`),
typing updates `file_structure_query` and resets `file_structure_selected`
to `0`, `Enter` or a row click calls `file_structure_confirm`, `Escape`
closes without jumping. `file_structure_open` is added to
`handle_shortcuts`'s existing `suppress_dispatch` set (so a background
`⌘F12`-unrelated global shortcut can't fire while the query field owns
keyboard focus, same reasoning already covers `command_palette_open`/
`search_everywhere_open`) and to the Escape-arbitration chain, checked
alongside `show_go_to_line` (same tier — a small standalone modal, not
list-navigable the way the palette/Search Everywhere are collectively
mutually exclusive with each other).

### 3.3 Keeping `document_symbols` fresh for the active file

Both features need `self.lsp.document_symbols` to already be the *active*
tab's own outline, continuously, not just the moment Search Everywhere's
Symbols tab happens to be opened (C2's own lazy-fire, §2.3's
`SearchEverywhereTab::Symbols` arm, `document_symbols_requested_for`).
This phase adds a `sync_document_symbols(&mut self, idx: usize)` method,
fired from the exact same two call sites `sync_inlay_hints`/
`sync_semantic_tokens` already fire from (`open_file`'s `DidOpen` block,
and `render_tabs_and_editor`'s per-actual-`DidChange` block) — always
requests the whole document, same as those two siblings:

```rust
fn sync_document_symbols(&mut self, idx: usize) {
    let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) else {
        return;
    };
    self.lsp.request_document_symbols(&path);
    self.document_symbols_requested_for = Some(path);
}
```

Reuses the existing `document_symbols_requested_for` tracking field rather
than adding a second one — C2's own lazy-fire in the Symbols-tab arm is
left untouched as a harmless fallback (it already no-ops once this
method's own request for the same path has already landed) rather than
removed, since removing it isn't in this phase's scope and it costs
nothing to leave in place.

### 3.4 Breadcrumb click / File Structure row click

Both jump the same way `goto-definition.md`/C2's Symbols-tab confirm
already do: `open_definition(path, position)` → `open_at` → focuses the
already-open tab (no duplicate tab created) and sets the caret. Neither
feature pushes a navigation-history entry (`push_nav_location`) —
breadcrumbs/File Structure moves are treated as "local reorientation
within the file you're already looking at", the same category
`confirm_go_to_line` already falls into, not a "go somewhere else" jump
worth a Back/Forward stop.

### 3.5 What v1 doesn't cover

- **No real collapsible tree widget** in the File Structure popup —
  a flat, indentation-only list (§2.3). Collapsing/expanding subtrees is
  future work if it turns out to matter for files with hundreds of
  symbols.
- **No toggle command for breadcrumbs.** JetBrains IDEs' own default
  keymap has no shortcut for this (only a settings/context-menu toggle) —
  per `CLAUDE.md`'s "never invent a binding" rule, v1 always shows
  breadcrumbs when a chain exists; a user-facing on/off toggle (settings
  checkbox, no keybinding) is future work, not required by this phase.
- **No filtering/sorting of the breadcrumb chain by kind** — every symbol
  `symbols_containing` returns renders as its own segment, including e.g.
  a `Method` nested in an `impl` nested in a `Module`; JetBrains-style
  "collapse uninteresting ancestors" is not attempted.
- **One-frame staleness on tab switch**, as already noted in §2.3 for
  `active_breadcrumbs` — accepted, matches this codebase's existing
  `active_cursor_offset` precedent.

## 4. Constraints

Not security-sensitive per `CLAUDE.md`'s list: no new subprocess, no new
network/file-path input, no new disk write. `symbols_containing` is a pure
read over already-validated `Symbol`/`Position` data (`Location`s inside
`Symbol` are already validated against `project_root` by `ide-lsp`, same
as every other symbol-bearing event). `hacker` is skipped for this run.

## 5. Examples

Given a Rust file:

```rust
impl Foo {
    fn bar(&self) {
        // caret here
    }
}
```

`documentSymbol` reports `Foo` (`Class`/`Struct` depending on the server,
range covering the whole `impl` block) containing `bar` (`Method`, range
covering just the fn). With the caret on the marked line:
`symbols_containing` returns `[Foo, bar]` (outermost first) —
breadcrumbs render `Foo › bar`; clicking `Foo` moves the caret to the
`impl` line, clicking `bar` moves it to the `fn` line (a no-op if it's
already there). `symbol_depths` over the full-file symbol list gives
`[0, 1]` — the File Structure popup (empty query) lists `Foo` at the left
margin and `bar` indented one level under it.

## 6. Dependencies / integration

No new external dependency. Touches `crates/lsp/src/{types,lib}.rs` and
`crates/ui/src/{app,app/render,app/menu,command,file_structure}.rs` (new
file; `app/menu.rs` needed one registry-consistency line, see Revision
notes) — two roles, `rust-lsp-dev` then `rust-ui-dev` (merge order per
`CLAUDE.md`'s dev-chain).

## Revision notes

- **`crates/ui/src/app/menu.rs` needed one line too**, not anticipated in
  §2.3/§6's original file list: the native macOS menu bar (`native-menu-
  bar.md`, Batch D) has its own test,
  `every_non_build_command_appears_in_the_native_menu_exactly_once`, that
  fails closed on any registry command missing from `MENU_GROUPS` — so
  `FileStructure` needed a `Some("FileStructure")` entry in the "Go" menu
  group (right after `GoToLine`, matching real JetBrains Navigate-menu
  ordering) before the crate's own test suite went green. A reminder that
  `command.rs` additions in this codebase have a second consumer beyond
  the palette/keymap.
- `Binding::same`'s `mac`/`other` `KeyChord`s are *literally identical*
  (same `modifiers.command = true`, `modifiers.ctrl = false` on both) —
  the Cmd→Ctrl substitution happens at `KeyChord::pressed`/`label` time via
  egui's own `command`-flag semantics, not by the registry pre-computing a
  separate `.ctrl()` chord for `other`. The first version of this phase's
  own binding test asserted `binding.other.modifiers.ctrl`, which is
  false — fixed to assert equality against
  `Binding::same(KeyChord::new(Key::F12).command())` directly, matching
  every sibling `Binding::same` command's own test style in this file.
- `symbol_depths`' stack-based algorithm and `ide_lsp::symbols_containing`'s
  ordering guarantee were both designed and tested before touching
  `ide-ui` at all (`crates/lsp/src/types.rs`'s own new tests) — the
  `ide-ui` side (`file_structure.rs`, `active_breadcrumbs`) then had zero
  surprises against real nested-symbol fixtures, matching the pattern
  every prior LSP-consuming phase in this project has followed (get the
  pure algorithm right and tested first, wire it up second).
- Final coverage on touched/new files: `crates/lsp/src/types.rs` 95.43%;
  `crates/ui/src/file_structure.rs` 100%, `app.rs` 96.53%, `command.rs`
  99.31% (`app/render.rs`/`app/menu.rs` are rendering/native-menu-
  construction, exempt per this project's existing convention). Not
  security-sensitive per `CLAUDE.md`'s list — `hacker` skipped.
