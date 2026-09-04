# TUI: Search and Replace in Path (T37)

## 1. Purpose

Ports `search-in-path-v2.md` (C7) to `ide-tui`: upgrades the existing Find
in Path popup (`tui-find-in-path.md`, T15 — plain-substring, `ide_core::
search_tree`) with regex/case/whole-word search, include/exclude glob
filters, `.gitignore`-aware skipping, and a new Replace in Path flow with a
preview before anything is written to disk.

**Zero new `ide-core` API.** `crates/core/src/search_in_path.rs`
(`PathSearchOptions`/`PathSearchMatch`/`PathSearchResults`/
`ReplaceInPathResult`/`PathSearchError`, `search_tree_advanced`,
`replace_in_path`) and the `Debug`/`Clone`/`PartialEq`/`Eq` derives on
`ide_core::workspace_edit::{FileEdit, WorkspaceEdit}` were both already
added for `ide-ui`'s own C7 phase and are untouched by this doc — entirely
`rust-tui-dev` scope, consuming an already-merged core surface, the same
position T16/T36 were already in for their own dependencies.

### 1.1 A deliberate divergence from `ide-ui`'s module shape

`search-in-path-v2.md` §2.2 has `ide-ui` add a **new, sibling** module
(`crates/ui/src/search_in_path_panel.rs`, `PathSearchPanel`) rather than
reworking its existing `search_panel.rs`/`SearchPanel` in place, because
`ide-ui`'s old `SearchPanel`/`search_tree` pair stays alive as Search
Everywhere's Text tab's own data source — a second, unrelated consumer
that would break if `SearchPanel` were reworked out from under it.

`ide-tui` has no such second consumer: `crates/tui/src/search_panel.rs`'s
`SearchPanel` is used **only** by the Find in Path popup itself (this
crate has no Search Everywhere concept at all — `go_to_file`/`go_to_symbol`,
T16, are separate, purpose-built popups, not a unified fuzzy-everything
picker with a Text tab). Adding a parallel sibling module here would leave
the old `SearchPanel` immediately dead code with nothing left to call it —
a `[quality]` violation this doc avoids by reworking `search_panel.rs`'s
existing `SearchPanel` **in place** into the v2 panel directly. `ide-tui`'s
`todo_panel.rs` (T24) depends on `ide_core::search_tree`'s plain-substring
behavior exactly the way `search-in-path-v2.md` §1 says `ide-ui`'s own
Search Everywhere does — that dependency is on the **`ide-core` function**,
not on this crate's `SearchPanel` struct, so it is completely unaffected
either way; this doc doesn't touch `ide-core` at all.

### 1.2 Scope cuts (explicit)

- **No line-level diff preview before Replace in Path applies.**
  `search-in-path-v2.md` §3.3 has `ide-ui` build a full `Self::render_diff`
  hunk-by-hunk preview (reusing its Refactor Preview window's own
  rendering) before Apply. `ide-tui` already has an established, narrower
  precedent for previewing a multi-file text edit before applying one:
  `pending_rename_preview`/`render_rename_preview`
  (`tui-code-actions-and-rename.md`) shows a **per-file occurrence-count
  summary** ("`foo.rs` — 3 occurrences"), not a diff — and that precedent
  was already `rev`/`hacker`-reviewed and merged for a structurally
  identical risk (an LSP-driven multi-file `WorkspaceEdit` about to hit
  `apply_workspace_edit_to_disk`). This is a real, not free, capability
  gap relative to `ide-ui` — a user applying a regex substitution across
  many files loses the ability to visually verify capture-group expansion
  before it's written — but it is continuity with, not a new departure
  from, a risk this crate has already accepted once for the closest
  available precedent, not a novel corner cut for this feature alone.
  Replace in Path's own preview (§3.3 below) follows the identical
  per-file-summary shape.
- **No keyboard mnemonic for the four boolean options** (case-sensitive,
  whole-word, regex, respect-`.gitignore`). Checked directly (fetched
  `https://www.jetbrains.com/help/idea/reference-keymap-mac-default.html`
  — see §8 Sources): the reference IDE has no keyboard shortcut for these
  at all in Find/Replace or Find in Path — they're mouse-clickable
  checkboxes there too (`search-in-path-v2.md` §2.2's own "`ui.checkbox`"
  wording confirms this for `ide-ui`). Per root `CLAUDE.md`'s "never
  invent a binding" rule, an action absent from the reference keymap gets
  **no default binding** rather than an invented one — so these four
  options are exposed as ordinary Tab-reachable popup fields (§2.3), the
  same category of interaction Up/Down/Enter/Esc/Tab already are for
  every other popup in this crate (handled inside the popup's own key
  interceptor, never through the global command registry), not as a new
  global keybinding.

## 2. Interface

### 2.1 `crates/tui/src/search_panel.rs` (reworked in place)

```rust
pub(crate) struct SearchPanel {
    pub(crate) results: Option<ide_core::PathSearchResults>,
    pub(crate) error: Option<ide_core::PathSearchError>, // mutually exclusive with `results`
    pub(crate) searching: bool,
    generation: u64,
    rx: Option<Receiver<(u64, Result<ide_core::PathSearchResults, ide_core::PathSearchError>)>>,

    pub(crate) replace_preview: Option<ide_core::ReplaceInPathResult>,
    pub(crate) replace_error: Option<ide_core::PathSearchError>,
    pub(crate) replacing: bool,
    replace_generation: u64,
    replace_rx: Option<Receiver<(u64, Result<ide_core::ReplaceInPathResult, ide_core::PathSearchError>)>>,
}

impl SearchPanel {
    pub(crate) fn run(&mut self, tree: ide_core::DirEntry, query: String, options: ide_core::PathSearchOptions);
    pub(crate) fn poll(&mut self) -> bool;

    pub(crate) fn run_replace(&mut self, tree: ide_core::DirEntry, query: String, replacement: String, options: ide_core::PathSearchOptions);
    pub(crate) fn poll_replace(&mut self) -> bool;
}
```

`run`/`poll` keep T15's exact generation-counter contract (no-op while
already `searching`; a spawned thread calls `ide_core::search_tree_advanced`
and sends `(generation, Result<PathSearchResults, PathSearchError>)`;
`poll` drains the channel, clears `searching` unconditionally, and writes
the arriving payload into `results` (clearing `error`) or `error` (clearing
`results`) only if its generation still matches current — an error from a
now-superseded search is discarded exactly like a stale success would be).
`run_replace`/`poll_replace` are a second, fully independent instance of
the identical shape (own `replace_generation`/`replace_rx`/`replacing`),
calling `ide_core::replace_in_path` — kept as a second pair of methods
rather than a generic "run one of two ops" abstraction, same reasoning
`search-in-path-v2.md` §2.2 already gives for `ide-ui`'s own two pairs
(different inputs, different result types, exactly one existing precedent
to extend). `discard_in_flight` is **not** ported for either op — same
`tui-find-in-path.md` §2.1 reasoning: `ide-tui` has no project-switch
feature, the only condition that would ever need to discard an in-flight
op out from under itself.

### 2.2 `crates/tui/src/app.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SearchInPathField {
    #[default]
    Query,
    Include,
    Exclude,
    Replacement,
    CaseSensitive,
    WholeWord,
    Regex,
    RespectGitignore,
}

impl SearchInPathField {
    pub(crate) fn next(self) -> Self { /* wraps Query -> Include -> ... -> RespectGitignore -> Query */ }
    pub(crate) fn prev(self) -> Self { /* reverse of next */ }
}

pub(crate) struct SearchOverlayState {
    query: String,
    include: String,
    exclude: String,
    replacement: String,
    field: SearchInPathField,
    selected: usize,
    ran_query: Option<String>, // now compares (query, options) — see §3.2
    ran_options: Option<ide_core::PathSearchOptions>,
}

pub struct App {
    // ... existing ...
    pub(crate) search: SearchPanel,
    pub(crate) search_state: SearchOverlayState,
    pub(crate) search_open: bool,
    pub(crate) search_options: ide_core::PathSearchOptions, // default: respect_gitignore: true, else Default::default()
    pub(crate) search_replace_open: bool, // reveals `Replacement`/`CaseSensitive`.../is additive to search_open, see §3.1
    pub(crate) pending_replace_in_path_preview: Option<ide_core::ReplaceInPathResult>,
}
```

Only `pending_replace_in_path_preview` joins `close_all_overlays`'s reset
list — mirroring `pending_rename_preview`'s own already-established
precedent, not `ClonePanel`'s "background op survives close" one: a
finished preview is a true modal waiting on a decision, not background
state to preserve. This matters concretely because `pending_replace_in_
path_preview.is_some()` is checked at a tier *above* every other overlay
in `handle_key` (see below) — if it were left `Some` across a
`close_all_overlays` call triggered by some *other* overlay opening (which
also sets `search_open = false` but, before this correction, would have
left the preview `Some`), that higher-priority check would keep
intercepting every key indefinitely, permanently blocking the very overlay
the user just tried to open. `search`/`search_state`/`search_options`/
`search_replace_open` are not reset — mirroring `ClonePanel`'s/T34's
"background op survives close" precedent, applied here to `search`/
`search_state` exactly as `tui-find-in-path.md` §3.1 already established
for the plain-search case: `close_all_overlays` touches only `search_open`
and `pending_replace_in_path_preview`, never `search_replace_open` itself,
so reopening still shows exactly what was left (query, options, and the
replace-reveal state). `pending_replace_in_path_preview.is_some()` is its
own, higher-priority interception tier (a true nested modal *within* the
Search in Path popup, the same "confirm popup on top of a non-modal panel"
shape `tui-tool-window-docking.md` §2.4 already established for Docker's/
K8s's own confirm popups) — checked **before** `search_open` in
`handle_key` so `Enter`/`Esc` reach the preview first while it's showing,
and joins `any_popup_open`.

New methods:

```rust
impl App {
    fn toggle_search_panel(&mut self);                 // unchanged from T15
    fn trigger_replace_in_path(&mut self);              // NEW
    fn handle_search_key(&mut self, key: KeyEvent) -> LoopSignal; // reworked
    fn submit_or_open_search_result(&mut self);          // reworked (§3.2)
    fn run_replace_preview(&mut self);                   // NEW
    fn handle_replace_preview_key(&mut self, key: KeyEvent) -> LoopSignal; // NEW
    fn confirm_replace_in_path_preview(&mut self);       // NEW
    fn cancel_replace_in_path_preview(&mut self);        // NEW
    fn apply_file_edits(&mut self, edits: Vec<ide_core::FileEdit>, what: &str) -> Result<usize, String>; // NEW, extracted
}
```

- `toggle_search_panel` — unchanged from T15: opens/closes `search_open`
  only, never touches `search`/`search_state`/`search_options`/
  `search_replace_open`.
- `trigger_replace_in_path` — new `Action::ReplaceInPath` entry point
  (§2.4). Opens the panel (`search_open = true`, via the same
  `close_all_overlays`-then-set-true sequence `toggle_search_panel` uses)
  **and** sets `search_replace_open = true`, revealing the `Replacement`/
  boolean fields in the Tab cycle — mirrors `search-in-path-v2.md` §2.2's
  own `trigger_replace_in_path` ("opens the Search view and reveals the
  replacement field... does not itself compute a preview"). Does *not*
  force `search_replace_open` back to `false` if the panel is already
  open — pressing it again while already in replace mode is a no-op
  re-open, same idempotence `toggle_search_panel` already has for a
  repeat `Ctrl+Shift+F`.
- `handle_search_key` — while `search_open` and no preview is pending:
  `Tab`/`BackTab` move `search_state.field` via `next`/`prev` (wrapping);
  `Esc` closes the panel (`search_open = false`, `search_replace_open`
  left as-is — reopening later with `Ctrl+Shift+F` alone still shows
  whichever fields were last revealed, matching "nothing is reset by
  closing" for every other piece of this state); `Backspace`/`Char(c)`
  (not holding `Ctrl`) edit whichever of `query`/`include`/`exclude`/
  `replacement` is `field`'s current target, no-op if `field` names one of
  the four boolean fields; `Space` flips the boolean named by `field`,
  no-op if `field` names one of the four string fields; `Up`/`Down` move
  `selected`, clamped to `self.search.results`' current match count (`0`
  with no results yet) — same as T15, and a no-op while `field` doesn't
  matter for these two keys either way; `Enter`'s behavior depends on
  `field` — see below.
- `submit_or_open_search_result` — `Enter` while `field` is `Query`,
  `Include`, or `Exclude`: T15's exact submit-vs-open disambiguation,
  extended to key on `(query, options)` together, not `query` alone
  (§3.2) — a field/option edit that leaves the query text itself unchanged
  still counts as "the results shown no longer answer the current
  request," a case T15 itself never had to handle since it had no options
  to vary independently of the query string.
- `run_replace_preview` — `Enter` while `field` is `Replacement`. No-op if
  `self.search.replacing`, or if `search_state.query.trim()`/
  `search_state.replacement` is empty, or no project (mirrors
  `search-in-path-v2.md` §2.2's own `run_replace_preview` guard); otherwise
  calls `self.search.run_replace(self.tree.clone(), query, replacement,
  self.search_options.clone())`.
- `handle_replace_preview_key` — while `pending_replace_in_path_preview`
  is `Some`: `Enter` calls `confirm_replace_in_path_preview`, `Esc` calls
  `cancel_replace_in_path_preview`, every other key ignored (matches
  `handle_rename_preview_key`'s exact shape).
- `confirm_replace_in_path_preview` — takes `pending_replace_in_path_
  preview`, calls `self.apply_file_edits(preview.edit.edits, "Replace in
  Path")`, sets `self.status`/`self.notify` to a one-line summary (file
  count on success, the error string on failure — same convention
  `handle_workspace_edit_ready` already uses), closes the whole Search in
  Path panel on success (`search_open = false`) since the operation the
  user opened it for is now done; leaves the panel open on failure so the
  query/options/preview context isn't lost mid-error. Mirrors
  `search-in-path-v2.md` §3.3's exact "Apply" body, with `apply_file_edits`
  in place of `ide-ui`'s own copy of the same extraction.
- `cancel_replace_in_path_preview` — drops `pending_replace_in_path_
  preview`; no I/O has happened yet at that point (`replace_in_path`
  itself never writes to disk — §4), identical to `cancel_rename_preview`.
- `apply_file_edits` — **refactor, behavior-preserving.** `apply_workspace_
  edit`'s existing disk-then-buffer body (app.rs, ported from `ide-ui` for
  T-code-actions-and-rename) is split at its `ide_lsp::WorkspaceEdit` →
  `Vec<ide_core::FileEdit>` conversion boundary: the conversion loop stays
  in `apply_workspace_edit` (still the only caller that has LSP-typed
  input at all), and everything from `disk_edits`/`buffer_edits`
  partitioning onward moves into this new function, taking `Vec<ide_core::
  FileEdit>` directly. `apply_workspace_edit` becomes a thin wrapper:
  convert, then call `apply_file_edits`. `confirm_replace_in_path_preview`
  calls `apply_file_edits` directly — its edits are already `ide_core::
  FileEdit`s (`replace_in_path` never touches LSP), no conversion needed.
  Every existing `apply_workspace_edit` caller (code actions, rename,
  rename preview) keeps its exact current behavior and passes its existing
  tests unmodified — this split changes nothing observable for them.

### 2.3 New command (`crates/tui/src/commands.rs`)

```rust
Command {
    id: "ReplaceInPath",
    title: "Replace in Path",
    // ⌘⇧R / Ctrl+Shift+R -- confirmed against the reference macOS keymap
    // (§8 Sources): "Replace in Files..." is a real, same-on-every-
    // platform binding, and `Char('r')` + `SHIFT` collides with nothing
    // already registered (`Ctrl+R` alone is the existing `Replace`
    // command; `commands.rs`'s own module doc already documents this
    // lowercase-letter-plus-`SHIFT`-bit disambiguation convention, e.g.
    // `Undo`/`Redo`).
    binding: Some((
        KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
        KeyCode::Char('r'),
    )),
    action: Action::ReplaceInPath,
},
```

Unlike `ide-ui`'s own C7 phase, **no existing binding needs fixing here**:
`search-in-path-v2.md` §2.2 found and fixed a pre-existing `ReplaceAll`
mis-binding in `ide-ui`'s `command.rs` (`⌘⇧R` claimed for a JetBrains
action that doesn't actually exist). `ide-tui` has no `ReplaceAll` command
at all — checked (`grep ReplaceAll crates/tui/src/commands.rs` returns
nothing) — its in-buffer Replace All is reached only via the Find/Replace
bar's own key (`tui-replace-all.md`), never a standalone global command, so
there is nothing analogous to fix here.

### 2.4 `crates/tui/src/ui.rs`

```rust
fn render_search_panel(frame: &mut Frame, app: &App, area: Rect);       // reworked
fn render_replace_in_path_preview(frame: &mut Frame, app: &App, area: Rect); // NEW
```

- `render_search_panel` — same centered-`Rect`-plus-bordered-`List` popup
  shape T15 already establishes, with a new header block above the results
  list: the `Query`/`Include`/`Exclude` fields (and, only while
  `search_replace_open`, `Replacement`) each as one line, `field`'s current
  target reverse-styled; the four boolean fields each as one line reading
  `[x] Case-sensitive` / `[ ] Whole word` / `[ ] Regex` / `[x] Respect
  .gitignore` (`[x]`/`[ ]` literal glyphs — this crate's established
  boolean-affordance convention, matching e.g. `render_git_panel`'s own
  conflict-list checkmarks), `field`'s current target reverse-styled the
  same way. Below the header: `"Searching…"` while `self.search.searching`;
  `self.search.error`'s `Display` text in place of the list (§3.5, same
  "error replaces content" convention `search-in-path-v2.md` §3.5 itself
  cites from `ide-ui`'s `render_find_bar`); otherwise the existing T15
  match-list rendering unchanged (`{path}:{line+1}:{column+1} {line_text}`,
  `"No results."` for an empty completed search, a trailing truncation
  note when `results.truncated`). Title bar hints extend to mention `Tab`/
  `Space` alongside the existing `Enter`/`Esc`.
- `render_replace_in_path_preview` — same shape `render_rename_preview`
  already establishes (§1.2): a centered popup, "Replace: N occurrence(s)
  across M file(s)" summary line, then one line per `FileEdit` ("`path` —
  n occurrence(s)"), title "Replace in Path Preview  (Enter: apply, Esc:
  cancel)" — deliberately not "Rename Preview"'s title, same reasoning
  `search-in-path-v2.md` §2.2 gives for `ide-ui`'s equivalent naming
  choice. Wired into `render`'s existing flat popup-dispatch list, checked
  before the plain `search_open` popup (matches §2.2's interception-order
  note) so a pending preview visually covers the panel behind it.

## 3. Behaviour

### 3.1 Opening, closing, and reveal state

`Ctrl+Shift+F` (existing `Action::FindInPath`) opens/closes the panel
without touching `search_replace_open` — pressing it after `Ctrl+Shift+R`
(`ReplaceInPath`) was used earlier in the session still shows the
replacement field and boolean rows revealed, since nothing about find-only
reopening resets them. `Ctrl+Shift+R` (`Action::ReplaceInPath`) opens the
panel and forces `search_replace_open = true` unconditionally — the one
asymmetry in an otherwise fully symmetric pair, matching `search-in-path-
v2.md`'s own `trigger_replace_in_path` (it reveals, nothing un-reveals).
Closing (`Esc`, or reopening via `Ctrl+Shift+F` while already open, which
still just flips to closed the same as T15) never resets `search`/
`search_state`/`search_options`/`search_replace_open` — each persists
across a close/reopen cycle, extending T15's "nothing is reset by closing"
rule to every new field this phase adds *except* one:
`pending_replace_in_path_preview` **is** reset by `close_all_overlays` (see
§2.2's fuller rationale) — a pending preview is a decision still owed to
the user, not state whose loss would be a regression, and leaving it set
across an unrelated overlay open would otherwise wedge the whole overlay
system.

### 3.2 Query/options lifecycle and re-run detection

T15's `ran_query` becomes two fields, `ran_query`/`ran_options`, compared
together: `Enter` on `Query`/`Include`/`Exclude` re-runs the search (and
resets `selected` to `0`) whenever `(search_state.query.trim(), &self.
search_options)` differs from `(ran_query, ran_options)` **or** nothing has
run yet; otherwise it opens the selected row and closes the panel, exactly
as T15 already does for the single-field case. This means toggling a
boolean (`Space`) or editing `include`/`exclude` immediately desyncs the
next `Query`/`Include`/`Exclude` `Enter` from being a "just open the
selected row" no-op — the same "any relevant change immediately goes
stale" property T15 already gives the query string alone, now covering
every input that actually changes what `search_tree_advanced` was asked.
`PathSearchOptions: Clone` (already derived in `ide-core`, §2.1's own
comment on why) makes storing a whole extra copy in `ran_options` cheap
and correct — no partial-field comparison to keep in sync by hand.

### 3.3 Replace in Path: preview and apply

Identical engine and threading model to `search-in-path-v2.md` §3.3:
`ide_core::replace_in_path` walks the same filtered file set
`search_tree_advanced` would, builds one `ide_core::WorkspaceEdit` in
memory, and **never writes to disk itself**. `run_replace_preview` sends
this off-thread exactly like `run` does for a plain search; once `poll_
replace` reports a result, it's stored directly as `self.pending_replace_
in_path_preview` (no re-read/re-diff step here, unlike `ide-ui`'s
`show_replace_in_path_preview` — see §1.2 for why this crate's own preview
is a per-file occurrence count, not a diff, so there is no post-processing
step that would need a fresh disk/buffer re-read the way computing a diff
would). **Apply** (`confirm_replace_in_path_preview`) calls `apply_file_
edits` directly with the preview's own `edit.edits` — the same all-or-
nothing `apply_workspace_edit_to_disk`-then-buffer-apply sequence every
other multi-file apply path in this crate already uses (§2.2). **Cancel**
just drops the preview; no I/O has happened yet either way.

Each `FileEdit`'s `Transaction` was computed off-thread against a
search-time disk snapshot, not against whatever text is live at Apply
time. For the disk-write branch this is safe by construction —
`apply_workspace_edit_to_disk` re-reads each file fresh immediately before
writing and rejects a `Transaction` that no longer fits
(`WorkspaceEditError::OffsetOutOfRange`), rolling back every file already
written. For the open-tab branch, `Buffer::apply` → `TextBuffer::edit`
instead **clamps** an out-of-range offset to the nearest valid position
(`crates/core/src/text/mod.rs`'s own `clamp`, this crate's established "a
caller got an offset slightly wrong" convention) rather than rejecting it —
never panics, but a tab with unsaved edits made since the search ran could
receive the replacement at a shifted position instead of a clean error.
This is not a new risk this port introduces: `ide-ui`'s own `confirm_
replace_in_path_preview` applies `preview.edit.edits` the same direct way,
with the identical buffer-side behavior. Documented here rather than
silently inherited.

A file rewritten on disk by Replace in Path that's *also* open in a tab
elsewhere and *wasn't* matched (so it went through the disk path, not the
buffer path) is picked up by this crate's existing file-watcher/external-
change mechanism exactly like any other external edit — no new buffer-sync
code needed, identical to `search-in-path-v2.md` §3.3's own note for
`ide-ui`.

### 3.4 Results list: unchanged flat shape

T15's flat, file-by-file match list (no per-file heading/expand-collapse
widget) is kept as-is — `search-in-path-v2.md` §3.4 gives `ide-ui` a
clickable per-file heading specifically to satisfy the roadmap's original
"results as a file tree with expand" wording via a mouse interaction this
crate has no equivalent input channel for anyway (no click, and this
crate's `List` widget has no established non-selectable-heading-row
convention — `tui-find-in-path.md`'s own Revision notes already reached
this exact conclusion for T15's identical flat list, unchanged by this
phase's added options).

### 3.5 Errors

A bad regex (`SearchQuery::compile` failure) or bad glob
(`OverrideBuilder::add`/`build` failure) surfaces as `ide_core::
PathSearchError` through `poll`/`poll_replace` exactly as `search-in-path-
v2.md` §3.5 describes for `ide-ui`; `render_search_panel` shows it in place
of the results list (§2.4). A search error never clears a still-showing
previous success (or vice versa) beyond the ordinary "arriving payload
replaces whichever of `results`/`error` it is" swap already described in
§2.1 — there's no scenario where both a stale `results` and a fresh `error`
would need to coexist, since every `poll`/`poll_replace` call writes
exactly one of the two.

### 3.6 `.gitignore`/glob scope, directory skip list

Unchanged from `search-in-path-v2.md` §3.6/§3.7 — both are `ide_core::
search_in_path`'s own behavior, already merged, not re-described here.
`search_options`'s default (`respect_gitignore: true`, everything else
`Default::default()`/empty) matches `ide-ui`'s own default exactly.

## 4. Constraints & invariants

- **No new `ide-core`/`ide-lsp` surface** — see §1.
- `search_tree_advanced`/`replace_in_path` are called with `self.tree`
  (already `Project::scan_tree`-produced, already root-validated) exactly
  as `search`/`files_search`/`todo` already do — no new filesystem walk
  logic in this crate.
- `replace_in_path` never writes to disk — only `apply_file_edits`, gated
  behind the user reaching the preview and pressing `Enter`, ever calls
  `ide_core::apply_workspace_edit_to_disk`.
- `apply_file_edits`'s disk-then-buffer ordering (preserved unchanged from
  `apply_workspace_edit`) means a disk failure never leaves any buffer
  edited — the all-or-nothing property already holds for this path exactly
  as it already does for code actions/rename.
- **Security-sensitive — root `CLAUDE.md` gains a line for this diff.**
  `confirm_replace_in_path_preview`'s call to `apply_file_edits` is a
  regex/glob-driven, user-typed-pattern-fed multi-file write to arbitrary
  project files via `ide_core::apply_workspace_edit_to_disk` — the exact
  surface `crates/core/src/workspace_edit.rs`'s existing `CLAUDE.md` entry
  already covers, reached from this new call site (mirrors `search-in-
  path-v2.md`'s own "CLAUDE.md follow-up" note for `ide-ui`'s equivalent
  path, and the `git_panel.rs`-family "same surface, new call site" pattern
  already established for `*_gutter.rs`). §6 below makes the exact
  `CLAUDE.md` edit. `hacker` is expected for this role's diff.

## 5. Examples

```
$ ide-tui ~/code/my-rust-project
```

`Ctrl+Shift+F` opens Search in Path; `Tab` to `Regex`, `Space` to enable
it, `Tab` back to `Query`, type `foo_(\w+)`, `Enter` searches. `Ctrl+Shift+R`
instead opens with `Replacement` revealed; `Tab` to it, type `bar_$1`,
`Enter` previews the replace (capture-group expansion identical to
in-buffer Replace All, §3.3); the preview lists every affected file with
its occurrence count; `Enter` applies, `Esc` cancels with nothing written.

## 6. Dependencies & integration points

No new dependencies (`ignore` is already an `ide-core` dependency from
`ide-ui`'s own C7 phase). Touches `crates/tui/src/{search_panel,app,
commands,ui}.rs`.

### `CLAUDE.md` follow-up

Root `CLAUDE.md`'s security-sensitive-paths list gains a line noting that
`crates/tui/src/app.rs`'s `confirm_replace_in_path_preview`/`apply_file_
edits` path is covered by the same rationale as its existing `crates/
core/src/workspace_edit.rs` entry, reached from this new call site — made
once this role's diff exists, same "made once the diff exists" precedent
`search-in-path-v2.md` §6 itself documents for `ide-ui`'s equivalent note.

## 7. Diagrams

None — a direct extension of T15's already-simple state machine plus this
crate's already-established preview-popup pattern (`pending_rename_
preview`); nothing new enough in shape to warrant one.

## 8. Sources

- `https://www.jetbrains.com/help/idea/reference-keymap-mac-default.html`
  — fetched directly to confirm "Replace in Files..." = `⌘⇧R` (this doc's
  new `ReplaceInPath` binding) and, separately, that no keyboard shortcut
  exists anywhere in the reference keymap for toggling case-sensitivity/
  whole-word/regex matching (the basis for §1.2's "no keyboard mnemonic"
  scope note — the reference IDE itself only exposes these as clickable
  checkboxes, matching `search-in-path-v2.md` §2.2's own description of
  `ide-ui`'s identical checkboxes).

## Revision notes

- §3.3: added an explicit note on the clamp-vs-reject asymmetry between
  the disk-apply and open-tab-apply branches for a stale off-thread
  `Transaction` (`rev`'s doc-review finding) — traced `TextBuffer::edit`'s
  `clamp` directly to confirm it never panics, and confirmed `ide-ui`'s
  own equivalent path has the identical behavior already, so this is a
  documentation gap, not a new defect to design around.
- §2.2/§3.1: fixed a self-contradiction found during implementation —
  §2.2 originally said `pending_replace_in_path_preview` "joins
  `close_all_overlays`'s reset list" while §3.1 simultaneously listed it
  among the fields closing "never resets". Traced the actual runtime
  consequence of *not* resetting it (a stuck modal: the preview's
  higher-priority `handle_key` check would keep firing even after some
  other overlay's own opening set `search_open = false`, permanently
  blocking that overlay) and corrected both sections to match the
  implemented, correct behavior — `pending_replace_in_path_preview` is the
  one field in this group that *does* reset on `close_all_overlays`,
  matching `pending_rename_preview`'s precedent; `search`/`search_state`/
  `search_options`/`search_replace_open` still do not.
