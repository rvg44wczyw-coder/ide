# `ide-tui`: Find in Path / Global Search (T15)

## 1. Purpose

Fifth item of the TUI-parity backlog (`docs/roadmap.md` §10). Ports the
`global-search-and-languages.md` global-search half to `ide-tui`:
whole-project, case-insensitive substring search across every file in the
tree, with per-line results the user can jump straight to. No new
`ide-core` surface -- `ide_core::search_tree`/`SearchResults`/`SearchMatch`
already exist and are already used by `ide-ui`; this doc covers only
`ide-tui`'s own consumption, same split every prior `T`-item in this
backlog has used. The additional-languages half of that doc has no TUI
analog to port (language configuration is a settings-window concept this
crate doesn't have yet) and is out of scope here.

## 2. Interface / API

### 2.1 `src/search_panel.rs` (new file)

```rust
pub(crate) struct SearchPanel {
    pub(crate) results: Option<ide_core::SearchResults>,
    pub(crate) searching: bool,
    generation: u64,
    rx: Option<Receiver<(u64, ide_core::SearchResults)>>,
}

impl SearchPanel {
    pub(crate) fn run(&mut self, tree: ide_core::DirEntry, query: String);
    pub(crate) fn poll(&mut self) -> bool;
}
```

A near-verbatim port of `crates/ui/src/search_panel.rs`'s
generation-counter state machine (that file's own doc comment already
describes it precisely -- ported here unchanged): `run` no-ops while
already searching, otherwise spawns a background thread running
`ide_core::search_tree(&tree, &query)` and bumps `generation`; `poll`
drains the result channel, clearing `searching` unconditionally and
writing `results` only if the arriving result's generation still matches
current. `discard_in_flight` is **not** ported -- `ide-ui`'s only caller is
project-switch cleanup (`crates/ui/src/app.rs:858`), and `ide-tui` has no
project-switch feature (one project per process invocation, per
`docs/features/tui-shell-and-editor.md`), so there is nothing that would
ever need to discard an in-flight search out from under itself.

### 2.2 `src/app.rs`

```rust
pub(crate) struct SearchOverlayState {
    query: String,
    selected: usize,
    ran_query: Option<String>,
}

pub struct App {
    // ... existing ...
    pub(crate) search: SearchPanel,
    pub(crate) search_state: SearchOverlayState,
    pub(crate) search_open: bool,
}
```

`search_open` joins the five existing overlay booleans/`Option`s in
`close_all_overlays` (six-way mutual exclusion now). `Ctrl+Shift+F`
(`Action::FindInPath`) calls `toggle_search_panel`, the same
open/close-without-resetting-background-state shape
`toggle_cargo_panel`/`toggle_problems`/`toggle_notifications` already
use -- closing hides the overlay; it never stops a running search or
clears `search`/`search_state`, so reopening shows the same query and
results exactly where they were left (`docs/features/
tui-cargo-panel.md` §3's precedent, applied here).

`handle_search_key` intercepts all input while `search_open`:
- `Esc` closes the overlay (`search_open = false`), nothing else.
- `Up`/`Down` move `search_state.selected`, clamped to
  `self.search.results`' current match count (`0` if no results yet).
- `Backspace` pops a char off `search_state.query`.
- Any `Char(c)` not holding `Ctrl` appends to `search_state.query` (same
  guard `handle_find_key`'s own query-typing arm uses).
- `Enter` calls `submit_or_open_search_result` (below).
- Every other key is ignored.

```rust
impl App {
    fn submit_or_open_search_result(&mut self);
}
```

The one piece of behaviour this doc adds beyond a direct `ide-ui` port,
because `ide-ui`'s own model doesn't need it: `ide-ui`'s "Enter runs the
search" and "click a row to open it" are two different input channels (a
text field's Enter vs. a mouse click on a row), so they never conflict.
`ide-tui` has only the keyboard, and `Enter` is the natural key for
*both* "submit this query" and "open the selected row" -- so this
function disambiguates by comparing `search_state.query` against
`search_state.ran_query` (the query string `self.search.results`
currently reflects, or the one a not-yet-finished search is answering):

- If `self.search.searching`, do nothing -- a submission is already in
  flight for whatever query it was sent with; pressing `Enter` again
  while waiting neither resubmits nor opens a (possibly stale) row.
- Else if `search_state.query.trim()` is empty, do nothing -- same guard
  `ide-ui`'s own `run_search` uses before ever calling `SearchPanel::run`.
- Else if `Some(query.trim().to_string()) != search_state.ran_query` (the
  query changed since the results currently shown were produced, or
  nothing has ever been run) -- call `self.search.run(tree.clone(),
  query.trim().to_string())`, record that trimmed string into
  `ran_query`, and reset `selected` to `0`.
- Else (the query is unchanged and nothing is in flight, i.e. `results`
  already reflects exactly this query) -- open the match at `selected`
  (`open_search_result`, below) and close the overlay.

This means: type a query, press Enter once to search; once results land,
`Up`/`Down` to pick a row, press Enter again (same key, same query, now a
no-op edit) to open it. Editing the query at any point (even after
results arrived) immediately desyncs it from `ran_query`, so the very
next Enter searches again rather than opening a stale row against new
text.

```rust
impl App {
    fn open_search_result(&mut self, path: PathBuf, byte_offset: usize);
}
```

Same shape as `open_location` (`docs/features/tui-goto-and-usages.md`
§2.2) but starting from an already-absolute byte offset -- a
`SearchMatch` carries one directly (`crates/core/src/search.rs`), unlike
a `Location`'s LSP `Position`, so there's no `position_to_byte_offset`
conversion step. Opens/focuses the tab, top-aligns the match's line,
places the caret at `byte_offset` (`Selection::caret` clamps internally,
so a byte offset stale against a file that changed on disk since the
search ran can't panic -- it just lands somewhere sane).

### 2.3 `src/commands.rs`

One new entry, direct `Ctrl`-translation of `ide-ui`'s own binding
(`crates/ui/src/command.rs`'s `FindInPath`: `Binding::same(KeyChord::new(
Key::F).command().shift())`, i.e. `{ mac: ⌘⇧F, other: Ctrl+Shift+F }`,
already a same-on-every-platform binding, nothing to special-case):

```rust
Command {
    id: "FindInPath",
    title: "Find in Path",
    binding: Some((KeyModifiers::CONTROL.union(KeyModifiers::SHIFT), KeyCode::Char('f'))),
    action: Action::FindInPath,
},
```

No digit-collision, no letter-reuse conflict: `Ctrl+F` (`Find`) and
`Ctrl+Shift+F` (`FindInPath`) are disambiguated by the `SHIFT` bit the
same way `Ctrl+Z`/`Ctrl+Shift+Z` (`Undo`/`Redo`) already are, requiring
the same lowercase-`'f'`-plus-`SHIFT`-modifier convention `commands.rs`'s
module doc already documents for `Redo`.

### 2.4 `src/ui.rs`

```rust
fn render_search_panel(frame: &mut Frame, app: &App, area: Rect);
```

Same popup shape as `render_problems_panel`/`render_cargo_panel`: a
centered `Rect`, `Clear` first, then a `List` (query line as the first,
non-selectable row, then one row per match, `{path}:{line+1}:{column+1}
{line_text}` -- no per-file heading grouping the way `ide-ui`'s row-per-
match-with-heading rendering does, since a flat list is what this
crate's own `List` widget already renders for every other overlay and
`search_tree`'s output is already grouped-by-file via its depth-first
walk order, so a flat scan still reads file-by-file). "Searching…"
in place of the list while `app.search.searching`; "No results." for an
empty completed result with a non-empty query; a trailing truncation
note when `results.truncated`, matching `ide-ui`'s own wording.

## 3. Behaviour

### 3.1 Opening / closing

`Ctrl+Shift+F` toggles the panel open/closed, same five-existing-overlay
mutual-exclusion rule extended to six. Reopening after a close shows
whatever `query`/`results` were last left in place -- nothing is reset by
closing.

### 3.2 Query lifecycle

Typing edits `search_state.query` freely at any time, including while
results from a previous query are still showing (they simply go stale
relative to `ran_query` the instant the text changes, per §2.2). `Enter`
disambiguates submit-vs-open exactly as §2.2 describes. A completed
search with `results.matches.is_empty()` renders "No results." rather
than an empty list, so the difference between "hasn't searched yet" and
"searched, found nothing" stays visible.

### 3.3 Opening a result

Enter (on an unchanged, already-answered query) opens the file at
`selected`'s line, closes the overlay, and leaves `search`/`search_state`
otherwise untouched -- pressing `Ctrl+Shift+F` again immediately after
reopens on the exact same results, so jumping to one match and then
wanting to check another doesn't require re-running the search.

## 4. Constraints & invariants

- **No new `ide-core`/`ide-lsp` surface.** Entirely inside `crates/tui/**`,
  consuming `ide_core::search_tree`/`SearchResults`/`SearchMatch`, already
  merged and already used by `ide-ui`.
- **`discard_in_flight` is deliberately not ported** -- see §2.1; nothing
  in `ide-tui` can switch projects mid-process, the only condition that
  would ever need it.
- **Enter's submit-vs-open disambiguation (§2.2) is the one designed
  deviation from a literal `ide-ui` port**, forced by having only a
  keyboard where `ide-ui` has both a text field and mouse clicks. Tracked
  via `ran_query`, not a separate focus-mode enum, to keep the state
  machine to one comparison rather than an explicit mode field.
- **Not on `CLAUDE.md`'s security-sensitive path list.** This diff spawns
  no subprocess and reads no path the tree scan hasn't already validated
  -- `search_tree` itself already only walks an already-`Project::
  scan_tree`-validated `DirEntry` tree (its own module doc says so) and
  is exercised identically by `ide-ui` today. `open_search_result`'s
  byte offset comes from a `SearchMatch` produced by that same walk, not
  from unvalidated input.

## 5. Examples

```
$ ide-tui ~/code/my-rust-project
```

`Ctrl+Shift+F` opens the panel; typing `TODO` then `Enter` shows every
line containing `TODO` (case-insensitively) across the project, grouped
file-by-file in `search_tree`'s own walk order; `Down` a few times then
`Enter` opens that file with the caret on the matching line; `Ctrl+Shift+F`
again reopens on the same result set.

## 6. Dependencies & integration points

No new dependencies. Touches `crates/tui/src/{search_panel (new),app,
commands,ui,lib}.rs`.

## 7. Diagrams

None -- a direct, already-diagrammed-elsewhere state machine
(`ide-ui`'s own `search_panel.rs`) plus this crate's already-established
overlay/toggle pattern; nothing new enough to warrant one.

## Revision notes

Ported `ide-ui`'s `SearchPanel` state machine essentially unchanged,
adapting only the input model (§2.2's Enter disambiguation) to the
keyboard-only medium and dropping `discard_in_flight` as genuinely
unneeded here rather than carrying dead code across for parity's own
sake. Self-reviewed inline (`rev`-style pass; no `hacker` pass, per the
security-sensitive-paths reasoning in §4): no controversial findings --
the flat, ungrouped-by-heading result list was considered against
porting `ide-ui`'s per-file heading rows, but `search_tree`'s output
already reads file-by-file in its walk order and this crate's `List`
widget has no established convention for a non-selectable heading row
the way `ide-ui`'s `ui.heading()` does, so a flat list was the
straightforwardly-consistent choice, not a coin flip.

Implementation note (found while building, matching this crate's own
established precedent rather than anything new): a `Ctrl+Shift+F`
keypress reaching `App` while `search_open` is already `true` never
resolves to `Action::FindInPath` a second time -- `handle_key`'s overlay
interception runs before the palette/`binding_for` dispatch, exactly the
same order every other `Ctrl`-bound overlay toggle (`Ctrl+P`/Problems,
the Cargo panel) is already subject to, so this isn't a gap specific to
Find in Path. `Esc` is the only way to close the panel once it's open;
verified with `run_action_find_in_path_toggles_the_panel_open_and_closed`
(direct `run_action` calls, the same pattern the equivalent Cargo-panel
test already used) rather than asserting a real double-keypress closes
it, since that would assert something this crate's dispatch order
doesn't actually do.

Implemented, tested (`cargo test -p ide-tui`: 264 passed), and verified
(`fmt`/`clippy -D warnings`/build, workspace-wide, all green). Coverage
on every touched non-rendering file is well above the 80% floor
(`app.rs` 96.6%, `commands.rs` 100%, `search_panel.rs` 100%);
`ui.rs`/`lib.rs` stay at this crate's established rendering-only/entry-
point exemption.
