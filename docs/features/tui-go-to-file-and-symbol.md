# TUI: Go to File / Go to Symbol (T16)

## 1. Purpose

`ide-ui`'s Search Everywhere (`search-everywhere.md`, C2) is a single
tabbed popup — Files / Symbols / Actions / Text — reachable via `⇧⇧` plus
four standalone per-tab shortcuts. `ide-tui` already has independent,
separately-bound answers for two of those four tabs: the command palette
(`T1`) is Actions, Find in Path (`T15`) is Text. This phase adds the
remaining two as their own overlays — **Go to File** and **Go to
Symbol** — rather than porting the tabbed-popup shape itself; two small,
separately-triggered overlays fit this crate's existing pattern (Find vs.
Find in Path are two separate mechanisms too, not one dialog with a mode
switch) better than introducing this codebase's first tab-strip widget
for a two-tab merge that doesn't otherwise need one.

The roadmap's own framing for this item ("command palette already gives
fuzzy-filter infra — extend to files/symbols") is the scope line: this
phase reuses `ide_core::fuzzy_match_files`/`fuzzy_score` (already merged,
`crates/core/src/fuzzy.rs`, C2's `ide-core` half) and `ide_lsp::Symbol`/
`DocumentSymbol`/`WorkspaceSymbol` (already merged, `crates/lsp/src/
types.rs` + `client.rs`, C2's `ide-lsp` half) — **zero new `ide-core`/
`ide-lsp` API**, exactly the same "already-done groundwork" position every
prior `T`-item has been in.

### 1.1 Scope cuts (explicit)

- **Go to Class** (`ide-ui`'s `⌘O`/`Ctrl+N`, Go to Symbol with a
  class-kind filter) is not ported. It is pure convenience over Go to
  Symbol plus typing a class name — the roadmap's own phrasing names only
  "files/symbols," not a third filtered variant, and `ide-tui` has no
  toolbar/checkbox UI to expose the filter as a toggle the way `ide-ui`'s
  tab strip does. Skipped rather than half-built.
- **Go to Line** (`ide-ui`'s `⌘L`/`Ctrl+G`) is not ported this phase. Its
  only non-mac binding, `Ctrl+G`, is already `T20`'s `AddNextOccurrence`
  in this crate (`commands.rs`) — a real, audited JetBrains macOS
  binding this crate committed to first. Unlike `SelectAllOccurrences`'s
  own mac-binding collision (resolved by falling back to a second, real
  Windows/Linux JetBrains chord for the *same* action), Go to Line has no
  third published keymap variant to fall back to — its only two bindings
  are `⌘L` (unreachable, no `Cmd` in a terminal) and `Ctrl+G` (taken).
  Rather than invent a chord `CLAUDE.md` forbids, or silently reassign
  `Ctrl+G` away from an already-shipped `T20` binding, this single small
  feature is deferred rather than shipped with an artificial, non-real
  binding. It has no dependency on the fuzzy/symbol infra this doc adds,
  so nothing here blocks adding it later under its own doc once a binding
  question is resolved (or it's accepted as palette-only).
- **The `⇧⇧` double-tap gesture** is not ported — both new overlays get
  their own standalone `Ctrl`-based bindings instead (§1.2), the same way
  every other `ide-tui` feature reaches its own dedicated shortcut rather
  than a shared modal-popup gesture. `ide-tui` also has no
  `editor::double_tap`-equivalent detector to reuse; introducing one for
  a single gesture this crate doesn't otherwise need would be new
  machinery for a UX pattern (an ambient double-modifier-tap firing
  regardless of focus) that fits a mouse-and-window GUI more naturally
  than a terminal.

### 1.2 Bindings

| Action | `ide-ui` mac | `ide-ui` other | `ide-tui` |
|---|---|---|---|
| Go to File | `⌘⇧O` | `Ctrl+Shift+N` | `Ctrl+Shift+N` (mac `⌘` unreachable in a terminal; same "use the `other` half" rule this crate has followed since `T1`) |
| Go to Symbol | `⌘⌥O` | `Ctrl+Alt+Shift+N` | `Ctrl+Alt+Shift+N` (same reason) |

Neither chord collides with anything already in `commands.rs`'s table
(verified by reading the full table before choosing these, and by the
existing `no_two_bound_commands_share_the_same_chord` test, which covers
both new entries once added).

## 2. Interface

### 2.1 New module: `crates/tui/src/files_search.rs`

Structurally a sibling to `search_panel.rs` (background thread + a
generation counter + a channel polled once per frame), wrapping
`ide_core::fuzzy_match_files` instead of `ide_core::search_tree` — the
same "three similar lines is better than a premature generic merge"
precedent `search-everywhere.md` §4 already established for `ide-ui`'s
own `files_search.rs`, applied here for the same reason (a tree-walk
producer + a background-thread consumer, not meaningfully generic-able
without touching `search_panel.rs` itself for a feature that doesn't need
to).

```rust
#[derive(Default)]
pub(crate) struct FilesSearchPanel {
    pub(crate) results: Option<ide_core::FuzzyFileResults>,
    pub(crate) searching: bool,
    generation: u64,
    rx: Option<Receiver<(u64, ide_core::FuzzyFileResults)>>,
}

impl FilesSearchPanel {
    /// No-op if already searching. Otherwise spawns a thread running
    /// `ide_core::fuzzy_match_files(&tree, &query)`, same generation-
    /// tagging discipline as `SearchPanel::run`.
    pub(crate) fn run(&mut self, tree: ide_core::DirEntry, query: String);

    /// Same shape as `SearchPanel::poll`.
    pub(crate) fn poll(&mut self) -> bool;
}
```

No `discard_in_flight` — same reason `search_panel.rs`'s own doc comment
already gives (no project-switch feature in this crate).

### 2.2 `crates/tui/src/lsp_bridge.rs`

`LspBridge` gains:

```rust
pub(crate) document_symbols: Vec<Symbol>,
pub(crate) document_symbols_path: Option<PathBuf>,
pub(crate) workspace_symbols: Vec<Symbol>,
```

(No `finding_*`/`*_ready` flag pair for either, matching `search-
everywhere.md` §3.1's own reasoning verbatim: the overlay just renders
whatever these fields currently hold, the same way `code_actions` already
works in this bridge.)

Two new methods, same no-op-if-no-client shape as every existing request
method:

```rust
/// No-op with no client running. Doesn't clear `document_symbols` at
/// send-time (stale-but-plausible-until-replaced, same convention
/// `semantic_tokens`/`inlay_hints` already follow).
pub(crate) fn request_document_symbols(&mut self, path: &Path) {
    if self.client.is_none() {
        return;
    }
    self.send(LspRequest::DocumentSymbol { path: path.to_path_buf() });
}

pub(crate) fn query_workspace_symbols(&mut self, query: &str) {
    if self.client.is_none() {
        return;
    }
    self.send(LspRequest::WorkspaceSymbol { query: query.to_string() });
}
```

`poll()` gains two arms, moved out of the existing catch-all comment
(which is edited to name only `FormatReady` as the remaining unhandled
kind):

```rust
LspEvent::DocumentSymbol { path, symbols } => {
    self.document_symbols = symbols;
    self.document_symbols_path = Some(path);
}
LspEvent::WorkspaceSymbol { symbols } => {
    self.workspace_symbols = symbols;
}
```

`start_with_command` and `ServerExited` both gain clearing of all three
fields, alongside every other per-query field they already clear.

### 2.3 `crates/tui/src/app.rs`

```rust
pub(crate) struct GoToFileState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    /// The query `files_search.results` currently reflects, or that a
    /// still-in-flight run is answering -- same "track what was last
    /// asked for" shape `SearchOverlayState::ran_query` already uses,
    /// but driving a **live per-keystroke** refresh (§3.1) rather than
    /// Find in Path's submit-then-open model: fuzzy-matching an
    /// already-scanned tree by file name is cheap enough (no per-file
    /// disk I/O, unlike `search_tree`'s content scan) that a JetBrains-
    /// style "results update as you type" feel is both affordable and
    /// expected here.
    pub(crate) ran_query: Option<String>,
}

pub(crate) struct GoToSymbolState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    pub(crate) last_workspace_query: Option<String>,
    /// The path `request_document_symbols` was last *sent* for -- gates
    /// the empty-query branch the same way `search-everywhere.md` §3.2's
    /// `document_symbols_requested_for` does, and for the identical
    /// reason: `LspBridge::document_symbols_path` only updates once a
    /// *response* lands, so gating on it alone would re-send a fresh
    /// request every frame for the whole duration a slow server takes to
    /// answer.
    pub(crate) requested_for: Option<PathBuf>,
}
```

`App` gains `go_to_file: Option<GoToFileState>`, `go_to_symbol:
Option<GoToSymbolState>`, `files_search: files_search::FilesSearchPanel`.
Both new `_state` options join `close_all_overlays`'s reset list and
`handle_key`'s interception chain (immediately after `self.search_open`,
before `self.code_actions.is_some()` — position among sibling overlays is
arbitrary since they're mutually exclusive by construction, same as every
existing entry in that chain).

Methods (mirroring `toggle_search_panel`/`handle_search_key`/
`submit_or_open_search_result`'s existing three-part shape):

```rust
fn toggle_go_to_file(&mut self);
fn handle_go_to_file_key(&mut self, key: KeyEvent) -> LoopSignal;
fn confirm_go_to_file(&mut self);

fn toggle_go_to_symbol(&mut self);
fn handle_go_to_symbol_key(&mut self, key: KeyEvent) -> LoopSignal;
fn confirm_go_to_symbol(&mut self);

/// Called once per frame from `lib.rs`'s main loop, right after
/// `poll_lsp()`/alongside `sync_document_highlights` (§3.1).
fn sync_go_to_file(&mut self);
fn sync_go_to_symbol(&mut self);
```

## 3. Behaviour

### 3.1 Go to File

`toggle_go_to_file` opens/closes `go_to_file` (reset `query`/`selected`/
`ran_query` on open, same as `toggle_search_panel`). While open:

- Typing/Backspace edit `query` freely (`handle_go_to_file_key`, same
  `Ctrl`-guard on the char arm `handle_search_key` already uses).
- `Up`/`Down` move `selected`, clamped to the current result count.
- `Enter` (`confirm_go_to_file`) opens the selected row via
  `open_or_focus_tab` + placing the cursor at offset 0 (a file match
  carries no target position, unlike a search/symbol match — Go to File
  in JetBrains IDEs also just opens at the top), then closes the overlay.
- `Esc` closes without opening anything.

`sync_go_to_file` (no-op unless `go_to_file.is_some()`): if
`go_to_file.query.trim()` is empty, does nothing further (matches
`fuzzy_match_files`'s own empty-query short-circuit — no reason to spawn
a thread for a result the function would hand back empty anyway). Else,
if the trimmed query differs from `ran_query` **and** `!files_search.
searching`, calls `files_search.run(self.tree.clone(), query)` and
updates `ran_query`. This is a **live** refresh, unlike Find in Path's
submit-based `Enter` — driven every frame, not just on `Enter` — but
still bounded to one in-flight background search at a time via
`searching`'s existing guard, and still never touches
`fuzzy_match_files` synchronously on this thread (`files_search.rs`'s own
background-thread wrapper does the work, same as `search-everywhere.md`
§4's own constraint for `ide-ui`).

### 3.2 Go to Symbol

`toggle_go_to_symbol` opens/closes `go_to_symbol` (reset `query`/
`selected`/`last_workspace_query`/`requested_for`).

- Empty `query`: shows the active tab's own file outline
  (`lsp.document_symbols`, seeded automatically — §3.2's `sync_go_to_
  symbol` below).
- Non-empty `query`: shows `lsp.workspace_symbols`, server-ranked as-is
  (no client-side re-sort — `search-everywhere.md` §4's own "fuzzy
  scoring is never applied to LSP symbol results" constraint carries
  over unchanged: `workspace/symbol` matching is entirely server-side).
- `Enter` (`confirm_go_to_symbol`): calls `self.open_location(symbol.
  location.clone())` on the selected row (reusing the existing Goto/Find
  Usages jump helper — a symbol jump is not meaningfully different from a
  `Goto` result), then closes.
- `Esc` closes without jumping.

`sync_go_to_symbol` (no-op unless `go_to_symbol.is_some()`):

- If `query` is empty, there is an active tab with a path, and
  `requested_for.as_deref() != Some(path)`, calls
  `self.lsp.request_document_symbols(path)` and sets `requested_for =
  Some(path.clone())`.
- If `query` is non-empty and differs from `last_workspace_query`, calls
  `self.lsp.query_workspace_symbols(&query)` and updates
  `last_workspace_query`.

### 3.3 Rendering (`crates/tui/src/ui.rs`)

`render_go_to_file_popup` — structurally `render_goto_popup`'s existing
shape (a centered `Rect`, a bordered `List`), with each row showing
`relative` (the match's tree-relative path) and, when `truncated`, a
"+N more, refine your search" footer line — same truncation-footer
convention `search-everywhere.md` §3.4 documents for `ide-ui`'s own
popup, adapted to this crate's plain-`Line` row style (no bolded-index
highlighting of the matched characters — this crate's existing goto/
search popups don't highlight match spans either, e.g. `render_goto_
popup`'s own rows are plain path:line text, so this doesn't introduce an
inconsistency).

`render_go_to_symbol_popup` — same popup shape; each row shows `name`
with `kind` and, if present, `container_name` as trailing context (e.g.
`"new (Function) — Selections"`), mirroring `search-everywhere.md` §3.4's
`Symbol` row description at the level of detail a plain-text `ratatui`
`ListItem` can express (no separate subtitle line the way an `egui`
window can lay out — this crate's popups are already single-line-per-row
throughout, e.g. `render_notifications_panel`).

Both are called from the same per-frame popup-rendering call site every
other `render_*_popup` already is, gated on their respective `Option`.

## 4. Constraints & invariants

- **`fuzzy_match_files` never runs on the main thread inside a per-frame
  method** — `sync_go_to_file` only ever starts/polls `files_search`'s
  background thread or reads its already-computed `results`, matching
  `search-everywhere.md` §4's identical constraint on `ide-ui`'s own
  `sync_search_everywhere`.
- **Fuzzy scoring is never applied to LSP symbol results** (§3.2) —
  `workspace/symbol`'s ranking is entirely server-side, unchanged from
  `search-everywhere.md`'s own invariant.
- **The Symbols empty-query outline can go stale** the same documented
  way `search-everywhere.md` §4 already accepts for `ide-ui`: `requested_
  for` only re-triggers on a path change, not a content change, so
  editing the active file while Go to Symbol is closed, then reopening it
  with an empty query on that same file, can briefly show the pre-edit
  outline until a keystroke or file switch clears the guard. Accepted as
  the same known, self-correcting limitation `ide-ui`'s own phase already
  carries — not a new gap this port introduces.
- **`Symbol::location` path validation already happened in `ide-lsp`**
  (`client.rs`'s per-entry fail-open validation, C2's own invariant) — no
  further validation needed at this layer before calling `open_location`.
- **Go to File / Go to Symbol are mutually exclusive with every other
  overlay** by construction (`close_all_overlays`, `handle_key`'s
  chain) — same invariant every existing overlay pair already has.

## 5. Examples

```rust
app.toggle_go_to_file();               // go_to_file = Some(..)
app.go_to_file.as_mut().unwrap().query = "appuirs".to_string();
// next frame: sync_go_to_file() runs a background fuzzy match; once
// `files_search.poll()` (called from the same per-frame `poll_search`
// call site) yields a result, `crates/ui/src/app.rs` ranks near the top.
app.confirm_go_to_file();              // opens the top match, closes
```

```rust
app.toggle_go_to_symbol();             // go_to_symbol = Some(..), empty query
// sync_go_to_symbol() requests the active file's outline immediately
app.go_to_symbol.as_mut().unwrap().query = "new".to_string();
// next frame: a `workspace/symbol` query for "new" replaces the outline
app.confirm_go_to_symbol();            // jumps to the selected symbol
```

## 6. Dependencies & integration points

- Depends on `ide_core::fuzzy_match_files`/`FuzzyFileResults` (C2, already
  merged) and `ide_lsp::Symbol`/`SymbolKind`/`DocumentSymbol`/
  `WorkspaceSymbol` request-response plumbing (C2, already merged) — zero
  new `ide-core`/`ide-lsp` API.
- Reuses `open_or_focus_tab` (file open), `open_location` (symbol jump),
  and `self.tree`/`project_root` (already how Find in Path feeds
  `search_tree`).
- New files: `crates/tui/src/files_search.rs`. Modified:
  `crates/tui/src/{lsp_bridge,app,commands,ui,lib}.rs`.
- Not security-sensitive per `CLAUDE.md`'s list: no subprocess, no new
  path-validation surface (symbol locations are already validated inside
  `ide-lsp`; file matches come from the already-scanned, already-trusted
  project tree). `hacker` pass not expected.

Tests required:
1. `files_search.rs`: same test shape as `search_panel.rs`'s own suite
   (run-while-searching no-op, poll-with-nothing-running, run-and-poll
   yields matches, generation-mismatch drop, disconnected-channel
   handling) — one-for-one, swapping `search_tree` for `fuzzy_match_
   files`.
2. `lsp_bridge.rs`: `request_document_symbols`/`query_workspace_symbols`
   no-op with no client running; `poll()`'s two new event arms set the
   right fields; `start_with_command`/`ServerExited` clear all three new
   fields.
3. `app.rs`: `toggle_go_to_file`/`toggle_go_to_symbol` open-with-reset and
   close; typing/backspace/Up/Down/Esc for both; `confirm_go_to_file`
   opens the selected file at offset 0 and closes; `confirm_go_to_symbol`
   calls `open_location` on the selected symbol and closes; `sync_go_to_
   file`'s live-refresh cadence (query change while not searching starts
   a run; unchanged query or already-searching is a no-op); `sync_go_to_
   symbol`'s empty-query outline request and non-empty-query workspace
   query, each gated correctly on `requested_for`/`last_workspace_query`;
   both overlays are reachable from `handle_key`'s interception chain and
   mutually exclusive with every other overlay via `close_all_overlays`.
4. `commands.rs`: the two new bindings resolve correctly and collide with
   nothing (`no_two_bound_commands_share_the_same_chord` covers this
   once the two new `Command` entries exist).

## 7. Revision notes

Self-review round (inline, no `hacker` pass — no security-sensitive path
touched):

1. Discovered mid-implementation, not anticipated at doc-drafting time:
   neither `sync_go_to_file` nor `sync_go_to_symbol` reset `selected` when
   a new query started a fresh result set — a `selected` index left
   pointing past a shorter, freshly-arrived list (e.g. row 3 selected
   against an old 5-row result, then a new query yields only 1 row) would
   make `Enter` silently do nothing (the row is out of range) with no
   visible selection highlight, since no row's index would equal the
   stale `selected` value. Fixed by resetting `selected = 0` at the same
   point each function already marks a new query/request as sent
   (`state.ran_query = Some(query)` / `state.requested_for = Some(path)` /
   `state.last_workspace_query = Some(query)`), mirroring the reset
   `submit_or_open_search_result` already applies in the older Find in
   Path overlay for the identical reason. Covered by
   `sync_go_to_file_resets_the_selection_when_a_new_query_starts` and
   `sync_go_to_symbol_resets_the_selection_when_a_new_query_starts`.
