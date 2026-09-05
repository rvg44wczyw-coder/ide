# TUI Code Generation (T39)

## 1. Purpose

Ports `docs/features/code-generation.md` (D4, `ide-ui`) into `ide-tui`:
**Generate menu**, **Implement Methods**, **Override Methods**, **Create
Test**, and **Optimize Imports** — all five read or drive the same
`ide_lsp::CodeAction`/`WorkspaceEdit` machinery `ide-tui` already has from
`docs/features/tui-code-actions-and-rename.md` (T13): `lsp.code_actions`
(the ambient per-caret cache), `LspBridge::apply_code_action(index)`, and
`handle_workspace_edit_ready`. **Zero new `ide-core`/`ide-lsp` API** — this
is entirely a `crates/tui/**` port, the same "zero new `ide-core` API"
pattern every prior LSP-feature TUI port already followed
(`tui-formatting.md`/T38 most recently). Auto Import needs no port at all
(§3.5) — it was already fully delivered by T13's `⌥Enter` menu, the same
zero-new-code conclusion `code-generation.md` §1/§3.5 reaches for `ide-ui`.

### 1.1 Deliberate divergences from `code-generation.md`

- **No `is_command_enabled` gating.** `ide-ui` greys out all five commands
  in the palette unless the active tab's path has a running language
  server (`code-generation.md` §2.2's shared `is_command_enabled` arm).
  `ide-tui` has no command-gating mechanism of any kind — confirmed by
  grepping `crates/tui/src/*.rs` for `is_command_enabled`: zero hits. This
  divergence was already established and documented by `tui-formatting.md`
  (T38) §1.1/§2.3's Revision notes for exactly the same reason; this phase
  follows the same precedent rather than re-arguing it. Every command below
  self-guards internally instead (no active tab → no-op; no cached/matching
  action → `self.status = Some("<name>: not available here")`).
- **No `Option<PathBuf>` guard.** `OpenBuffer::path` is a plain `PathBuf`
  in `ide-tui`, never `Option<PathBuf>` — `trigger_optimize_imports` reads
  `self.active_buffer().map(|b| b.path.clone())` directly instead of
  `ide-ui`'s `find_usages_target()` (which exists specifically to unwrap an
  `Option<PathBuf>` `ide-tui` doesn't have). Same divergence shape T38
  §1.1 already established for `trigger_reformat_code`.
- **`LspBridge::request_organize_imports` needs no new bridge state at
  all** — unlike `ide-ui`'s own version, which checks
  `self.is_running_for(path)` (multi-client, multi-language routing).
  `ide-tui`'s `LspBridge` wraps exactly one `LspClient`; its existing
  `send(request)` primitive already no-ops when `self.client.is_none()`
  (every other `request_*` method already relies on this), so
  `request_organize_imports` is a one-line forwarding call with no
  target-tracking field of its own — the response reuses the *already
  existing* `workspace_edit`/`workspace_edit_label`/`workspace_edit_ready`
  fields T13 introduced for `ApplyCodeAction`'s own `WorkspaceEditReady`,
  unchanged.
- **Generate menu's popup is a new, small, filtered sibling of the
  existing Show Intention Actions popup — not a reuse of
  `CodeActionsState`/`code_actions`.** T13's `CodeActionsState { selected:
  usize }` indexes directly into the *unfiltered* `lsp.code_actions`
  (`handle_code_actions_key`'s `Enter` arm applies `state.selected`
  directly as the response index). Generate menu needs a *filtered* view
  (`kind == Some("")`, §3.1) whose visible row count and row-to-response-
  index mapping differ from the unfiltered list — reusing
  `CodeActionsState` as-is would apply the wrong action the moment the
  filtered and unfiltered lists diverge in length. `GenerateMenuState {
  selected: usize }` is therefore its own type, and its key handler
  recomputes the filtered `Vec<&ide_lsp::CodeAction>` fresh each time
  (cheap — `lsp.code_actions` is bounded and already in memory, no new
  I/O), applying via `filtered[selected].index` (the action's own absolute
  index, `ide_lsp::CodeAction::index` — never the filtered position).
- **Keybindings.** `code-generation.md` §2.2's table has two shapes this
  project's established translation conventions (`commands.rs`'s own
  module doc, extended by T38) already cover without a new rule:
  - **Implement Methods (`⌃I`/`Ctrl+I`), Override Methods (`⌃O`/`Ctrl+O`),
    Create Test (`⌘⇧T`/`Ctrl+Shift+T`)** are all `Binding::same` in
    `ide-ui` (`other` is a mechanical Cmd/Ctrl-preserving translation of
    `mac`) — translated the same way `ToggleBlockComment`/`ReformatCode`
    already are: `Ctrl+I`, `Ctrl+O`, `Ctrl+Shift+T` respectively. All three
    chords are free in `commands.rs` today (checked: no existing binding
    uses `Char('i')`/`Char('o')` at all, and the one existing `Char('t')`
    binding is bare `Ctrl+T` — `ToggleProjectToolWindow` — a different
    chord from `Ctrl+Shift+T`).
  - **Generate (`⌘N`/`Alt+Insert`) is a genuine two-chord divergence, not a
    modifier substitution** (`code-generation.md` §2.2 says so explicitly:
    "a real divergence, not a modifier substitution"). This is the exact
    shape `GoToFile` (`⌘⇧O`/`Ctrl+Shift+N`) and `GoToSymbol`
    (`⌘⌥O`/`Ctrl+Alt+Shift+N`) already established a convention for in
    this same file: when `mac` and `other` are genuinely different keys
    (not a Cmd→Ctrl substitution of the same chord), `ide-tui` uses
    `ide-ui`'s own **`other`** value directly, since `other` is already a
    real, terminal-native (non-Cmd) JetBrains binding rather than something
    this project would need to invent. Generate's `ide-tui` binding is
    therefore `Alt+Insert`, `ide-ui`'s own literal `other` value — not a
    Ctrl-substitution of `⌘N` (which would collide with nothing today, but
    would also not be a real JetBrains binding on any platform, violating
    `CLAUDE.md`'s "never invent a binding" rule the same way a naive
    substitution would for `GoToFile`/`GoToSymbol`). `KeyCode::Insert` is
    already a first-class variant in `keymap.rs`'s label/parse round-trip
    (added for keymap-serialization completeness, not yet bound to any
    command by default) — this is its first default binding. `Alt+Insert`
    is free today (no existing binding uses `KeyCode::Insert` at all).
  - **Optimize Imports has no binding on either platform in `ide-ui`** —
    ported as `binding: None`, palette-only, identically.

Everything else — which rust-analyzer assist backs which command, the
`AssistKind::Generate → CodeActionKind::Empty` wire fact, the "no
`create_test`/`source.organizeImports` assist exists for Rust today"
findings, the Auto Import already-delivered conclusion — is unchanged from
`code-generation.md` §1 and not re-verified here; that research doesn't
depend on which frontend is asking the same already-connected language
server the same `textDocument/codeAction` question.

Not security-sensitive per `CLAUDE.md`'s declared list: this diff touches
none of `crates/lsp/**` (the `OrganizeImports` wire request/`to_proto`
capability plumbing already exists, unchanged, from `ide-ui`'s D4 round),
invokes no external formatter/subprocess, and its one call into
`apply_workspace_edit`/`apply_file_edits` (via the *already-existing*
`handle_workspace_edit_ready`, unchanged by this phase — every command
here produces a `WorkspaceEditReady` event the same already-shipped
handler already consumes) carries the same "single already-open-tab
target, never a glob/regex-driven candidate set" property
`tui-formatting.md` §1's own analysis already established for why that
listing doesn't extend to a caller shaped this way. `hacker` is skipped
for this role on that basis, subject to `rev`'s own security-checklist
pass independently confirming it against the actual diff — the same
confirm-don't-assume posture `tui-formatting.md` and `code-generation.md`
itself already took for their own no-`hacker` calls.

## 2. Interface / API

### 2.1 `ide-lsp`

No changes. `LspRequest::OrganizeImports`, the resulting
`LspEvent::WorkspaceEditReady { edit, label }`, and rust-analyzer's
`source.organizeImports`-scoped `textDocument/codeAction` handling
(`crates/lsp/src/client.rs`'s `OrganizeImportsOutcome`/
`parse_organize_imports_response`) are reused exactly as `ide-ui` already
uses them — a second, independent caller of an unchanged public API, the
same relationship every prior TUI LSP port has to its `ide-lsp` half.

### 2.2 `ide-core`

No changes.

### 2.3 `ide-tui`

```rust
// crates/tui/src/lsp_bridge.rs -- one new method on the existing LspBridge.
impl LspBridge {
    /// Sends `LspRequest::OrganizeImports` -- no target-tracking field of
    /// its own (§1.1): the response lands in the same `workspace_edit`/
    /// `workspace_edit_label`/`workspace_edit_ready` fields
    /// `ApplyCodeAction`'s `WorkspaceEditReady` already fills, since both
    /// produce the exact same event shape. No-op with no client running,
    /// same as every other `request_*` method (`self.send` already
    /// no-ops in that case).
    pub(crate) fn request_organize_imports(&mut self, path: &Path) {
        self.send(LspRequest::OrganizeImports {
            path: path.to_path_buf(),
        });
    }
}
```

```rust
// crates/tui/src/app.rs -- new state/logic types.

/// Generate menu's list-selection state (§1.1) -- mirrors
/// `CodeActionsState`'s shape exactly (presence is visibility, only a
/// `selected` index), but is its own type since it indexes into a
/// *filtered* view of `lsp.code_actions`, not the raw list.
pub(crate) struct GenerateMenuState {
    pub(crate) selected: usize,
}

/// The three direct-invoke commands' matching heuristic (`code-generation
/// .md` §1/§2.2's `DirectGenerateKind`, ported verbatim -- the matching
/// logic is server-response-shaped, not frontend-shaped, so it transfers
/// with no change). Generate menu's own filter is simpler still
/// (kind-equals-empty-string alone, §3.1) and isn't a fourth variant here,
/// exactly as `ide-ui`'s own doc already notes for its version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectGenerateKind {
    ImplementMethods,
    OverrideMethods,
    CreateTest,
}

impl DirectGenerateKind {
    fn name(self) -> &'static str;

    /// Identical to `ide-ui`'s own `matches` (`code-generation.md` §2.2):
    /// `ImplementMethods`/`OverrideMethods` require `kind ==
    /// Some("quickfix")` and a title containing rust-analyzer's exact
    /// "implement missing members"/"implement default members" strings
    /// (case-insensitive); `CreateTest` matches on a title containing
    /// "test" alone, kind-agnostic, deliberately permissive since no
    /// server this project configures today has an observed kind to
    /// narrow on (`code-generation.md` §1, §4).
    fn matches(self, action: &ide_lsp::CodeAction) -> bool;
}
```

`App` gains:

- `generate_menu: Option<GenerateMenuState>` — presence is visibility,
  same convention `code_actions`/`rename_popup`/every other popup-shaped
  field in this struct already uses. Added to **both** `close_all_
  overlays()`'s reset list (`self.generate_menu = None;`, alongside its
  existing `self.code_actions = None;`) and `any_popup_open()`'s
  disjunction (`|| self.generate_menu.is_some()`) — omitting either would
  be a real bug, not just an inconsistency: without the first, a still-open
  Generate menu could survive an unrelated overlay-closing action that
  every other popup already correctly gets closed by; without the second,
  `handle_mouse_click`'s `any_popup_open()` guard (`app.rs:5348-5351`)
  would let a mouse click fall through to the tree/editor underneath an
  open Generate menu instead of being swallowed by it, the same way every
  other popup already is.
- `fn generate_menu_actions(&self) -> Vec<&ide_lsp::CodeAction>` — the
  shared filter: `self.lsp.code_actions.iter().filter(|a| a.kind.as_deref()
  == Some("")).collect()`. Called fresh by `trigger_generate_menu` (to
  decide whether to open at all) and by the popup's key handler/render
  function (to know what to show and what `Enter` should apply) — never
  cached, so it can never go stale between an ambient `sync_code_actions`
  re-query and the popup already being open (the same "always read
  `lsp.code_actions` live" property the existing intention-actions popup
  already has).
- `fn trigger_generate_menu(&mut self)` — `Alt+Insert`'s entry point
  (`Action::GenerateMenu`, §1.1). No-op-with-status if
  `self.generate_menu_actions().is_empty()`
  (`self.status = Some("Generate: nothing to generate here")`); otherwise
  calls `self.close_all_overlays()` first, then
  `self.generate_menu = Some(GenerateMenuState { selected: 0 })` —
  mirroring `trigger_show_intention_actions`'s own precedent exactly
  (`app.rs:4681-4684`: `close_all_overlays()` unconditionally, then set the
  popup state), not a deliberate divergence from it.
- Key-dispatch wiring: the main key-routing chain (`app.rs`, around the
  existing `if self.code_actions.is_some() { return self.handle_code_
  actions_key(key); }` check) gains a matching
  `if self.generate_menu.is_some() { return self.handle_generate_menu_key
  (key); }` arm at the same priority tier.
- `fn handle_generate_menu_key(&mut self, key: KeyEvent) -> LoopSignal` —
  the same `Esc`/`Up`/`Down`/`Enter` shape as `handle_code_actions_key`,
  operating on `self.generate_menu_actions()`'s length/entries instead of
  `self.lsp.code_actions` directly: `Enter` applies
  `self.lsp.apply_code_action(actions[state.selected].index)` (the
  matched action's own absolute index — never `state.selected` itself,
  which only indexes the filtered view) and closes the popup, mirroring
  `handle_code_actions_key`'s own "apply regardless of `disabled_reason`"
  behavior exactly (no new disabled-guard introduced here that the sibling
  popup doesn't already have).
- `fn trigger_direct_generate(&mut self, kind: DirectGenerateKind)` —
  `Ctrl+I`/`Ctrl+O`/`Ctrl+Shift+T`'s shared entry point. Searches
  `self.lsp.code_actions` for a non-disabled entry matching `kind`; found →
  `self.lsp.apply_code_action(action.index)`; not found →
  `self.status = Some(format!("{}: not available here", kind.name()))`.
  Ported from `ide-ui`'s `trigger_direct_generate` with the sole change of
  `self.error` → `self.status` (`ide-tui`'s single transient-message field,
  the same substitution T38's own port already made for every `self.error`
  call site it touched).
- `fn trigger_optimize_imports(&mut self)` — Optimize Imports' entry point
  (palette-only, §1.1). No-op with no active tab
  (`self.active_buffer().map(|b| b.path.clone())` is `None`); otherwise
  `self.lsp.request_organize_imports(&path)` unconditionally — reaches for
  nothing in `lsp.code_actions` (§3.4: the ambient per-caret cache is the
  wrong data source for this whole-file, `context.only`-scoped request,
  identical reasoning to `ide-ui`'s own).

`handle_workspace_edit_ready` (T13, unchanged by this phase) already
correctly renders every outcome `OrganizeImports` can produce: `edit:
Some(..)` applies through the existing `apply_workspace_edit` path;
`edit: None` — which is what rust-analyzer's response always is (§1) —
falls through to `self.status = Some(format!("{what}: nothing to
apply"))`, where `what` is `self.lsp.workspace_edit_label.clone()
.unwrap_or_else(|| "Code action".to_string())`. **Read directly from
`ide-lsp`'s source, not assumed from `code-generation.md`'s own prose**:
the `Empty` outcome (§1's guaranteed-for-rust-analyzer case) sends
`WorkspaceEditReady { edit: None, label: None }` — `label` is `None`, not
`Some("Optimize Imports")` — so the actual rendered message for that case
is the generic fallback, **"Code action: nothing to apply"**, not
"Optimize Imports: nothing to apply" (§5 corrects this against
`code-generation.md`'s own Examples section, which states the latter;
that inaccuracy is pre-existing in the already-shipped `ide-ui` doc and
out of this phase's scope to fix — noted here only so this port's own
Examples section doesn't repeat it). A response with a real edit
(`OrganizeImportsOutcome::Ready(edit)`) does send `label: Some("Optimize
Imports")` (`crates/lsp/src/client.rs`'s `Ready` arm), so that case's
message is exactly as named.

`commands.rs` gains five entries:

```rust
Command {
    id: "GenerateMenu",
    title: "Generate",
    // `ide-ui`'s own `other` binding (`Alt+Insert`), used directly -- a
    // genuine two-chord divergence from `⌘N`, not a Cmd->Ctrl
    // substitution (§1.1).
    binding: Some((KeyModifiers::ALT, KeyCode::Insert)),
    action: Action::GenerateMenu,
},
Command {
    id: "ImplementMethods",
    title: "Implement Methods",
    // `⌃I` translated -- literal Control on both platforms already.
    binding: Some((KeyModifiers::CONTROL, KeyCode::Char('i'))),
    action: Action::ImplementMethods,
},
Command {
    id: "OverrideMethods",
    title: "Override Methods",
    binding: Some((KeyModifiers::CONTROL, KeyCode::Char('o'))),
    action: Action::OverrideMethods,
},
Command {
    id: "CreateTest",
    title: "Create Test",
    // `⌘⇧T` translated.
    binding: Some((
        KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
        KeyCode::Char('t'),
    )),
    action: Action::CreateTest,
},
Command {
    id: "OptimizeImports",
    title: "Optimize Imports",
    // No default binding on either platform in `ide-ui` -- palette-only.
    binding: None,
    action: Action::OptimizeImports,
},
```

`Action` gains `GenerateMenu`, `ImplementMethods`, `OverrideMethods`,
`CreateTest`, `OptimizeImports` variants, dispatched in `run_action`:
`GenerateMenu => self.trigger_generate_menu()`; `ImplementMethods =>
self.trigger_direct_generate(DirectGenerateKind::ImplementMethods)`;
`OverrideMethods => self.trigger_direct_generate(DirectGenerateKind::
OverrideMethods)`; `CreateTest => self.trigger_direct_generate
(DirectGenerateKind::CreateTest)`; `OptimizeImports => self.trigger_
optimize_imports()`.

`ui.rs` gains one new popup render function:

```rust
/// `Alt+Insert`'s popup. Identical row rendering to `render_code_actions_
/// popup` (title/`(disabled)` suffix, `Modifier::REVERSED` for the
/// selected row) -- the only differences are the row source
/// (`app.generate_menu_actions()` instead of `&app.lsp.code_actions`
/// wholesale) and the window title/empty-state text ("Generate  (Enter:
/// apply, Esc: close)" / "Nothing to generate here.", per `code-
/// generation.md` §2.2's own wording for `ide-ui`'s equivalent popup).
fn render_generate_menu_popup(frame: &mut Frame, app: &App, area: Rect);
```

Wired into the render dispatch and the key-routing `match` the same way
`code_actions`/`render_code_actions_popup` already are (an `if let
Some(_) = self.generate_menu` branch ahead of ordinary editor key
handling, mirroring `handle_code_actions_key`'s own call site).

## 3. Behaviour

### 3.1 Generate menu (`Alt+Insert`)

Opens a popup listing every entry in `lsp.code_actions` whose `kind` is
`Some("")` (§1's `AssistKind::Generate → CodeActionKind::Empty` wire fact,
unchanged from `code-generation.md` §1/§3.1 — this phase does not
re-verify rust-analyzer's source again, since that fact is protocol-level,
not frontend-level). Selecting a row applies it immediately via
`apply_code_action` — no preview (`ide-tui` has no Refactor Preview
machinery at all; even `ide-ui`'s own D4 doc already argues immediate
apply is correct for every command in this phase regardless, §4). Opening
with nothing available shows the popup anyway with an inline "Nothing to
generate here." row, matching `render_code_actions_popup`'s own "No
actions available." convention — **this diverges from `trigger_generate_
menu`'s own no-op-with-status behavior stated above**: the *trigger*
checks emptiness up front and refuses to open at all
(`self.status = Some("Generate: nothing to generate here")`), so the
popup's own "Nothing to generate here." empty-state row only matters for
the (rare, but possible) case where every previously-matching action is
cleared out of `lsp.code_actions` by an ambient re-query firing while the
popup is already open — the same TOCTOU window `handle_code_actions_key`'s
own popup already tolerates for the unfiltered list.

### 3.2 Implement Methods (`Ctrl+I`) / Override Methods (`Ctrl+O`)

Unchanged in substance from `code-generation.md` §3.2 — searches the same
ambient `lsp.code_actions` cache (no new request) for a non-disabled entry
with `kind == Some("quickfix")` and the exact rust-analyzer title
substring. Found → applied immediately via `apply_code_action` (resolving
first if the server marked it unresolved — unchanged, existing
`ApplyCodeAction` logic, identical to what `ide-ui` already relies on and
what T13's own `Enter`-on-intention-actions path already exercises). Not
found → `self.status = Some("Implement Methods: not available here")` (or
"Override Methods:").

### 3.3 Create Test (`Ctrl+Shift+T`)

Same shape as §3.2: searches for any non-disabled entry whose title
contains "test", kind-agnostic. For every language this project ships a
server config for today, this always reports "Create Test: not available
here" (§1 — no rust-analyzer assist exists to ever match) — registered
and bound anyway per `CLAUDE.md`'s "never invent a binding, use it
verbatim even if this server never satisfies it" rule, the identical
precedent `code-generation.md` §3.3 already set for `ide-ui`.

### 3.4 Optimize Imports (palette-only, no default binding)

The one behavior in this phase that isn't "filter the existing ambient
cache differently," identical in shape to `code-generation.md` §3.4:
sends `LspRequest::OrganizeImports { path }` unconditionally (no ambient
cache check — this is a rare, direct action). `ide-lsp` issues
`textDocument/codeAction` scoped to `context.only:
["source.organizeImports"]` covering the whole document, resolving first
if needed, exactly as already implemented and unchanged by this phase.
The result reaches `handle_workspace_edit_ready` (T13, unchanged) exactly
as any other `WorkspaceEditReady` does — see §2.3's note on the actual
"nothing to apply" message text for the guaranteed-empty rust-analyzer
case.

### 3.5 Auto Import — already delivered, nothing added

Identical conclusion to `code-generation.md` §3.5: rust-analyzer's
`auto_import`/`qualify_path` assists are ordinary `QuickFix`-kind,
caret-position-triggered code actions already returned by the exact same
`textDocument/codeAction` request T13's `⌥Enter` (Show Intention Actions)
already sends and already displays in `ide-tui`. This phase makes no code
change for it.

## 4. Constraints & invariants

- **Kind-empty-string filtering is exact, not "falsy."**
  `kind.as_deref() == Some("")`, never `kind.is_none() || kind.as_deref()
  == Some("")` — identical reasoning to `code-generation.md` §4 (never
  observed from rust-analyzer, not something this phase should treat as
  "must be Generate" by default).
- **`DirectGenerateKind::CreateTest`'s permissive match is deliberate, not
  a placeholder** — same reasoning as `code-generation.md` §4: no kind
  exists to narrow on today, and narrowing anyway risks excluding a future
  server this command is written to eventually work with.
- **Optimize Imports never touches `lsp.code_actions`/`code_actions_
  target` in either direction** — it neither reads the ambient cache to
  decide whether to fire nor leaves anything behind in it afterward. An
  ambient intention-actions/Generate-menu re-query already in flight when
  Optimize Imports is invoked, or vice versa, resolves independently and
  correctly regardless of order — `ide-tui`'s `LspBridge` has no per-
  request pending-id slot at all (it forwards to a single `LspClient` and
  trusts `ide-lsp`'s own internal id bookkeeping, unlike `ide-ui`'s
  multi-client bridge), so this invariant is actually simpler to state
  here than in `code-generation.md` §4: there is no shared slot to
  misdeliver in the first place, only `ide-lsp`'s own already-correct,
  already-tested `pending_organize_imports_id`/`pending_code_action_id`
  separation (`crates/lsp/src/client.rs`), unchanged by this phase.
- **Immediate apply, never preview, for every command in this phase** —
  same as `code-generation.md` §4; `ide-tui` has no preview machinery of
  any kind to route through instead, so this isn't even a choice this
  phase makes, merely an observation of what's already true.
- **`GenerateMenuState.selected` indexes the filtered view, never
  `lsp.code_actions` directly** — the one invariant this phase's own
  design (§1.1) introduces beyond what `code-generation.md` states: a bug
  here (applying `state.selected` as if it were the response index)
  would silently apply the *wrong* code action whenever the filtered and
  unfiltered lists have different lengths, which is the common case
  whenever any non-Generate action is also cached. Tests must construct a
  `lsp.code_actions` mix where the two lengths and orderings genuinely
  differ, not just a list that happens to be all-Generate or all-empty.
- **A server that never responds to `OrganizeImports`'s request behaves
  like any other unanswered LSP request already does** — no new timeout
  logic, identical to `code-generation.md` §4.

## 5. Examples

**Generate a getter (caret on a struct field):**

```rust
// caret on `age` inside:
struct Player { age: u32 }
```

`Alt+Insert` → `generate_menu_actions()` returns whatever Generate-kind
actions apply at that exact caret position (e.g. "Generate getter",
"Generate setter", both `kind: Some("")`) → popup opens. `Enter` on
"Generate getter" applies it immediately via `apply_code_action`; no
preview dialog.

**Implement Methods:**

```rust
trait Shape { fn area(&self) -> f64; fn name(&self) -> &str { "shape" } }
impl Shape for Circle { }
//                     ^ caret anywhere inside these braces
```

`Ctrl+I` → finds the cached `"Implement missing members"` action → applies
immediately, inserting a `todo!()`-bodied `fn area(&self) -> f64` stub.

**Optimize Imports on a Rust file:**

Invoking Optimize Imports anywhere in a `.rs` file → `ide-lsp` sends the
`source.organizeImports`-scoped request → rust-analyzer's response is
empty (§1, guaranteed) → `WorkspaceEditReady { edit: None, label: None }`
→ status line reads **"Code action: nothing to apply"** (the generic
`handle_workspace_edit_ready` fallback, since `label` is `None` for this
specific outcome — §2.3's note; not "Optimize Imports: nothing to apply",
which is what `code-generation.md`'s own Examples section says for
`ide-ui`'s identical case, an inaccuracy in that already-shipped doc this
phase does not inherit).

**Create Test on any file today:**

`Ctrl+Shift+T` → no cached action's title contains "test" for any
currently configured server → `self.status = Some("Create Test: not
available here")`. Documented, expected, not a defect (§3.3).

**Generate menu with a stale filtered index (TOCTOU):**

```rust
app.lsp.code_actions = vec![generate_action(0), quickfix_action(1)];
app.trigger_generate_menu(); // generate_menu_actions() == [action 0] -> opens
// ... an ambient re-query lands, replacing lsp.code_actions with a new,
// differently-ordered list before Enter is pressed ...
app.lsp.code_actions = vec![quickfix_action(0), generate_action(1)];
// handle_generate_menu_key(Enter) recomputes generate_menu_actions() ==
// [action 1] fresh -- applies index 1, the CURRENT Generate action, never
// stale index 0 from the list that existed when the popup opened.
```

## 6. Dependencies & integration points

- `tui-code-actions-and-rename.md` (T13) — every mechanism this phase
  reuses (`lsp.code_actions`, `apply_code_action`,
  `handle_workspace_edit_ready`) already exists there; this phase adds one
  new `ide-tui`-side request method (`request_organize_imports`) and
  several new consumers of the existing cache, nothing to T13 itself.
- `code-generation.md` (D4, `ide-ui`) — the direct source this phase
  ports from; §1's rust-analyzer research (verified against actual
  rust-analyzer source, not assumed) is reused without re-verification,
  since it's a protocol-level fact independent of which frontend asks.
- `tui-formatting.md` (T38) — direct precedent for two conventions this
  phase reuses without re-arguing: no `is_command_enabled` (§1.1) and
  `self.status` in place of `ide-ui`'s `self.error` (§2.3).
- `docs/roadmap.md` §10 — this document's own destination row (`T39`),
  the fourth and final item in the TUI-parity sequence the user specified
  (File Structure + Breadcrumbs / T36, Search and Replace in Path / T37,
  Reformat Code + Format on Save / T38, Code Generation / T39).
- Not security-sensitive per `CLAUDE.md`'s declared list — this phase
  touches no subprocess, no path outside the already-audited
  `apply_workspace_edit`/`apply_workspace_edit_to_disk` write path
  (unchanged here, reached only through the already-existing, unchanged
  `handle_workspace_edit_ready`), and no network I/O beyond the
  already-established, already-audited LSP JSON-RPC channel `crates/lsp
  /**` already is. `hacker` is expected to be skipped for this role's
  diff on that basis, subject to `rev`'s own security-checklist pass
  confirming no new surface was introduced.

## 7. Diagram

Skipped — the sequence is byte-for-byte the same shape `code-generation
.md`'s own Optimize Imports diagram already shows (`ide-ui` node relabeled
`ide-tui`); a second diagram would duplicate it without adding
information, the same reasoning `tui-formatting.md` §7 already gave for
its own skip.

## Revision notes

- §2.3: `trigger_generate_menu`'s original text both contradicted itself
  and misdescribed the actual codebase (`rev` finding) — it claimed
  Generate deliberately skips `close_all_overlays()` "unlike
  `trigger_show_intention_actions`," then claimed in the same breath that
  intention-actions *also* never calls it; neither is true
  (`app.rs:4681-4684` shows `trigger_show_intention_actions` calling
  `close_all_overlays()` unconditionally). Corrected: `trigger_generate_
  menu` now calls it too, mirroring its sibling exactly, no divergence.
- §2.3: added two integration points `rev`'s independent source check
  found missing entirely — `close_all_overlays()` must reset
  `self.generate_menu = None;` (alongside its existing `code_actions`
  reset) and `any_popup_open()` must gain `|| self.generate_menu.is_some()`
  (it gates mouse-click routing, `app.rs:5348-5351` — without it, a click
  while the Generate menu is open would fall through to the tree/editor
  underneath instead of being swallowed by the popup, a real input bug,
  not just a documentation nicety). Also added the key-dispatch wiring
  note (a `generate_menu.is_some()` arm alongside the existing
  `code_actions.is_some()` one in the main key-routing chain).
