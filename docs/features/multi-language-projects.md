# Multi-Language Project Support

## 1. Purpose

Every language feature shipped so far (`global-search-and-languages.md`,
`language-auto-detect.md`, `language-server-arguments.md`) assumed a
project has exactly one language: `detect_language` returns at most one
`LanguageConfig`, `Cargo.toml` at the root wins unconditionally over
everything else and suppresses every other suggestion outright, and
`LspBridge` wraps exactly one `ide_lsp::LspClient`. That's wrong for a
real polyglot repo — a Rust core with Swift/Kotlin bindings (the concrete
motivating case: a local `core-lib`-shaped project with `crates/`,
`bindings/swift/`, and a Kotlin/Compose Android app all in one tree) gets
`rust-analyzer` and nothing else; Swift and Kotlin files get syntax
highlighting only, with no LSP features ever, and no path exists to
change that short of opening each language's files in a different editor
entirely.

This feature lifts the one-language limit: a project can have **several
simultaneously active languages**, each with its own running LSP client,
each open file routed to the client matching its own extension. Rust
stops being an exclusive special case that blocks everything else — it
becomes one more entry that can coexist with others.

**Scope**: `ide-core` (`crates/core/src/language.rs`) and `ide-ui`
(`crates/ui/src/lsp_bridge.rs`, `crates/ui/src/app.rs`,
`crates/ui/src/app/render.rs`). **Not** `ide-tui` this round — it has no
`custom_languages`/Languages-settings UI yet (confirmed by reading
`crates/tui/src/app.rs`: its one `detect_language` call site always
passes `&[]`), so there's nothing there for multi-language detection to
attach to; broad TUI parity is tracked separately per `CLAUDE.md`'s
dev-chain role notes. **Not** `ide-lsp` — `LspClient` is already a plain,
independently-instantiable struct; nothing about running several at once
requires changing its public API.

## 2. Interface / API

### 2.1 `ide-core` (`crates/core/src/language.rs`)

`detect_language` (singular, existing) is **unchanged** — still used by
`ide-tui`, still "Rust wins outright, else first matching custom entry,
else `None`". A new, additive function sits alongside it:

```rust
/// Every language active for `tree`'s project: Rust first (if
/// `Cargo.toml` is at the root) -- no longer exclusive, just first --
/// followed by every `custom` entry (in `custom`'s order) whose
/// `extension` or any `extra_extensions` entry matches a file anywhere
/// in `tree`. A `custom` entry whose `extension` case-insensitively
/// equals `"rs"` is skipped (Rust's slot can't be shadowed or
/// duplicated -- defensive, since the UI's own "Add custom language"
/// dialog already rejects that at entry time, but a hand-edited
/// `preferences.json` could still contain one). Entries are also
/// deduplicated by `extension` (case-insensitive), first occurrence
/// wins, for the same hand-edited-JSON reason -- `LspBridge` keys its
/// running clients by extension and can't run two for the same key.
pub fn detect_active_languages(
    tree: &DirEntry,
    custom: &[LanguageConfig],
) -> Vec<LanguageConfig>
```

`const MAX_ACTIVE_LANGUAGES: usize = 8;` — `detect_active_languages`
stops adding entries once the result reaches this length (Rust, if
matched, counts toward it and is always considered first, so it's never
the one silently dropped). Every previous language feature ever shipped
here started at most **one** LSP subprocess; this one can start several
concurrently for the first time, so an unbounded result — a huge
polyglot tree, or a `custom_languages` list with many entries that
happen to all match — would spawn an unbounded number of language-server
processes with no cap at all. Silently capped, not an error (same
convention the existing wire-decode caps like
`MAX_SEMANTIC_TOKENS_PER_MESSAGE` already use elsewhere in this
codebase) — a real project needing more than 8 simultaneously active
languages is not a case this v1 needs to handle gracefully, just not
crash or fall over on.

Implementation note: an extension is only added to the "claimed" set once
an entry with it actually matches and gets included, not merely for
appearing in `custom` — though since matching is a pure function of the
extension string itself, two entries sharing an `extension` always match
or don't match identically, so this distinction can't currently be
observed by any real input. It's kept this way anyway because it's the
more obviously correct rule to read, not because there's a reachable case
that depends on it.

```rust
/// The first entry in `active` whose `extension` or any
/// `extra_extensions` entry matches `path`'s extension
/// (case-insensitive), or `None` if nothing in `active` covers it.
/// The per-file routing primitive `LspBridge` uses to pick which
/// running client answers a request about a given path.
pub fn language_for_path<'a>(
    active: &'a [LanguageConfig],
    path: &Path,
) -> Option<&'a LanguageConfig>
```

### 2.2 `ide-ui` — `LspBridge` (`crates/ui/src/lsp_bridge.rs`)

The single `client: Option<LspClient>` field becomes a map:

```rust
struct RunningLanguage {
    config: LanguageConfig,
    client: LspClient,
}

pub struct LspBridge {
    clients: HashMap<String, RunningLanguage>, // key: config.extension.to_lowercase()
    // every other existing field (diagnostics, inlay_hints, semantic_tokens,
    // references, goto, hover, document_highlights, code_actions,
    // workspace_edit*, document_symbols*, workspace_symbols, format_*,
    // prepare_rename*, rename_*) is UNCHANGED IN SHAPE -- see §3.2 for how
    // each is populated/cleared now that there can be more than one client.
    code_actions_client: Option<String>, // NEW -- see §3.2
    ...
}
```

Per-path maps (`diagnostics`, `inlay_hints`, `semantic_tokens`) need no
shape change: every path belongs to at most one active language (routing
is a partition, not an overlap), so one flat `HashMap<PathBuf, _>` shared
across every running client is still correct — an event from client A can
never collide with a path client B owns.

Public surface changes:

- `is_running(&self) -> bool` — unchanged meaning, now "at least one
  client is running" (was "the one client is running"). Still used by
  `smart_mode_state`'s aggregate On/Off/Error and by tests that just
  assert "something started."
- `is_running_for(&self, path: &Path) -> bool` — **new**. Routes `path`
  through `language_for_path` logic against the running set and reports
  whether that specific language's client is up. Replaces the two
  call sites in `app.rs` (`trigger_rename`, the command-palette
  enablement check for the Rename/Refactor family) that were checking
  "is the server running" when they meant "is the server for *this
  file* running."
- `is_running_for_extension(&self, extension: &str) -> bool` — **new**,
  small helper for the Languages… settings window (§2.4): an exact-key
  lookup (`extension.to_lowercase()` against the map), distinct from
  `is_running_for` which resolves through `extra_extensions` too.
- `sync_active_languages(&mut self, project_root: &Path, active: &[LanguageConfig])`
  — **new**, replaces most `start_with_command`/`stop` call sites.
  Diffs `active` against the running set: a `config` with no entry keyed
  by its extension, or whose stored entry's `config` differs from the
  new one (name/command/args/extra_extensions changed), gets
  (re)started; a running key no longer present in `active` gets
  stopped. A key present in both with an unchanged `config` is left
  running untouched — restarting it would kill in-flight LSP state
  (diagnostics, open-document sync) for no reason. This diffing
  subsumes what `IdeApp::poll_tree_scan`'s `Refresh` arm used to hand-roll
  itself (§3.3).
- `restart_all(&mut self, project_root: &Path, active: &[LanguageConfig])`
  — **new**, the "Restart Language Server" action's primitive: stops
  every running key not in `active`, then (re)starts every entry in
  `active` unconditionally, even ones already running unchanged — an
  explicit user action requesting fresh processes, unlike the diff-only
  `sync_active_languages`.
- `stop_all(&mut self)` — renamed from `stop`; same "drop every client,
  clear every field" full reset, used when a project closes or Smart
  Mode is toggled off.
- `send(&self, path: &Path, request: LspRequest)` — **signature change**
  (was `send(&self, request: LspRequest)`). Routes through
  `language_for_path`-equivalent matching against the running set,
  sends to that client if found, silent no-op otherwise (same
  "callers don't need to check first" convention every method here
  already follows). Every existing caller already has `path` in scope
  wherever it builds a path-carrying `LspRequest`, so this is a
  mechanical update, not a new burden on callers.
- `fn send_to_key(&self, key: &str, request: LspRequest)` — **new**,
  private. The low-level primitive both `send` and `apply_code_action`
  eventually reduce to: look `key` up in `self.clients`, call
  `.client.send(request)` if present, no-op otherwise. `send(path,
  request)` resolves `path` to a key via the same matching
  `language_for_path` uses, then delegates here; `apply_code_action`
  (below) resolves a key from `code_actions_client` instead of a path
  and delegates here directly, since `ApplyCodeAction` carries no path
  of its own to route by.
- `apply_code_action(&self, index: usize)` — unchanged signature.
  `ApplyCodeAction` carries no path (it applies to whatever the last
  `code_actions` answer was), so it routes via the new
  `code_actions_client: Option<String>` field and `send_to_key` instead
  of the path-based `send`. No-op if `code_actions_client` is `None`.
- `query_workspace_symbols(&mut self, query: &str)` — **behavior
  change**, not signature: broadcasts `LspRequest::WorkspaceSymbol` to
  *every* running client instead of the one client, since a workspace
  symbol search is meant to cover the whole project regardless of which
  language each hit is in. See §3.2 for how results from multiple
  clients are combined and §4 for the accepted race this introduces.
- Every other `request_*` method (`find_references`, `go_to_*`,
  `request_hover`, `request_document_highlight`, `request_inlay_hints`,
  `request_semantic_tokens`, `request_code_actions`,
  `request_document_symbols`, `request_format`/`request_format_range`,
  `request_prepare_rename`, `request_rename`) keeps its existing
  signature (`path` was already a parameter on all of them) — only the
  internal "is a client running" / "which client do I send to" logic
  changes, from `self.client.is_some()`/`self.send(request)` to
  `self.is_running_for(path)`/`self.send(path, request)`.

### 2.3 `ide-ui` — `IdeApp` (`crates/ui/src/app.rs`)

- `active_language: Option<LanguageConfig>` → `active_languages: Vec<LanguageConfig>`.
- `redetect_language` → renamed `resync_active_languages`: computes
  `self.active_languages = ide_core::detect_active_languages(tree, &self.custom_languages)`,
  then `self.lsp.sync_active_languages(project.root(), &self.active_languages)`.
  Same no-op-if-no-project guard as before.
- `restart_lsp` (the "Restart Language Server" action): now calls
  `self.lsp.restart_all(project.root(), &self.active_languages)` — no
  longer special-cases "no active language" as a no-op via `if let
  Some`, since `active_languages` can legitimately be empty (nothing to
  restart, `restart_all` on an empty slice is already a correct no-op
  through the same diff logic).
- `poll_tree_scan`'s `Refresh` arm: the hand-rolled inline
  detect-then-conditionally-start block is deleted and replaced with a
  call to `resync_active_languages` — `sync_active_languages`'s own
  diffing already produces the "leave an unchanged, still-running
  language alone" behavior that inline block existed for (§2.2).
- `refresh_language_suggestions`: the early-return block that cleared
  `pending_language_suggestions` whenever `active_language` was Rust is
  **deleted**. Rust no longer prevents other suggestions from mattering
  — a `go.mod` next to a Rust-rooted project's `Cargo.toml` can now
  actually activate, so suppressing its suggestion was only ever correct
  under the old exclusive model. The existing `.retain(...)` filtering
  (already dismissed, already configured by extension) is untouched and
  is sufficient on its own.
- `smart_mode_state`: `self.active_language.is_none()` →
  `self.active_languages.is_empty()`; the On/Off/Error resolution from
  `self.lsp.is_running()`/`self.lsp.server_error` is otherwise unchanged
  (an aggregate indicator: On if *any* client is up).
- `toggle_smart_mode`: `self.lsp.stop()` → `self.lsp.stop_all()`;
  `self.restart_lsp()` unchanged (now restarts every active language,
  not just one).
- `add_custom_language`/`remove_custom_language`/
  `enable_language_suggestion`: their trailing `self.redetect_language()`
  call becomes `self.resync_active_languages()`.
- `trigger_rename` and the command-palette enablement match arm for
  `Rename | RefactorThis | ExtractVariable | ExtractMethod |
  ExtractConstant | ExtractField | Inline`: `self.lsp.is_running()` →
  `self.lsp.is_running_for(path)` (both call sites already have `path`
  in scope).
- `open_file`/`notify_lsp_changed`/`close_tab_now`: their
  `self.lsp.send(LspRequest::DidOpen/DidChange/DidClose { path, .. })`
  calls become `self.lsp.send(&path, LspRequest::...)`.
- `CommandAction::ToggleSmartMode`'s palette-enablement check:
  `self.active_language.is_some()` → `!self.active_languages.is_empty()`.

### 2.4 `ide-ui` — rendering (`crates/ui/src/app/render.rs`)

`render_language_settings_window`'s per-entry row gains a running/stopped
indicator using the new `is_running_for_extension`:

```rust
ui.label(format!(
    "{} (.{}) — {} [{}]",
    lang.name, lang.extension, command_line(&lang.command, &lang.args),
    if self.lsp.is_running_for_extension(&lang.extension) { "running" } else { "stopped" },
));
```

Purely additive — the row's existing content and the Remove button are
untouched.

## 3. Behavior notes

### 3.1 Worked example: the motivating polyglot project

A project has `Cargo.toml` at its root, a `Package.swift` under
`bindings/swift/`, and a `build.gradle.kts` under an Android app
directory — the shape of a real Rust-core-plus-Swift-plus-Kotlin repo.
`custom_languages` already has Swift and Kotlin entries (enabled earlier
via the auto-detect popups, `language-auto-detect.md`). `Cargo.toml`
existing makes `detect_active_languages` include Rust; Swift/Kotlin files
existing anywhere in the tree makes their `custom_languages` entries
match too — result: `[Rust, Swift, Kotlin]` (Rust first, then `custom`'s
order). `sync_active_languages` starts three clients: `rust-analyzer`,
`sourcekit-lsp`, `kotlin-language-server`, keyed `"rs"`/`"swift"`/`"kt"`.

Opening a `.rs` file sends `DidOpen` to the `"rs"`-keyed client only;
opening a `.swift` file sends it to `"swift"` only. A hover request on
the open Swift tab routes to `sourcekit-lsp`; the same request on the
Rust tab routes to `rust-analyzer`. Diagnostics from both stream into the
same flat `diagnostics: HashMap<PathBuf, _>` — each path only ever
receives diagnostics from the one client that has it open, so nothing
collides. A workspace-symbol search for "Session" queries all three
clients and merges whatever each answers.

### 3.2 `poll()`'s multi-client drain

```rust
pub fn poll(&mut self) -> bool {
    // one-frame-true flags reset, unchanged
    let mut changed = false;
    let mut exited = Vec::new();
    for (key, running) in self.clients.iter_mut() {
        while let Some(event) = running.client.try_recv() {
            changed = true;
            match event {
                // Diagnostics/References/Goto/Hover/DocumentHighlight/
                // InlayHint/SemanticTokens/WorkspaceEditReady/
                // DocumentSymbol/FormatReady/PrepareRenameReady/
                // RenameReady: same bodies as today, unchanged -- none of
                // them need to know which client they came from.
                LspEvent::CodeAction { path: _, actions } => {
                    self.code_actions = actions;
                    self.code_actions_client = Some(key.clone());
                }
                LspEvent::WorkspaceSymbol { symbols } => {
                    self.workspace_symbols.extend(symbols); // was `=`
                }
                LspEvent::ServerExited { message } => {
                    self.server_error = Some(message);
                    exited.push(key.clone());
                    break;
                }
            }
        }
    }
    for key in exited {
        self.stop_language(&key); // see §3.3
    }
    changed
}
```

Iterating `self.clients.iter_mut()` while wanting to remove entries on
`ServerExited` is why exited keys are collected and removed in a second
pass after the loop, rather than removed in place.

### 3.3 Scoped teardown: `stop_language`

`sync_active_languages`'s stop path, `restart_all`'s stop-before-restart
path, and `poll()`'s `ServerExited` handling all funnel through one
private helper:

```rust
fn stop_language(&mut self, key: &str) {
    let Some(running) = self.clients.remove(key) else { return };
    let config = running.config; // drops `running.client`, tearing the process down
    self.diagnostics.retain(|p, _| !path_matches(&config, p));
    self.inlay_hints.retain(|p, _| !path_matches(&config, p));
    self.semantic_tokens.retain(|p, _| !path_matches(&config, p));
    if self.code_actions_client.as_deref() == Some(key) {
        self.code_actions.clear();
        self.code_actions_target = None;
        self.code_actions_client = None;
    }
    if self.prepare_rename_target.as_ref().is_some_and(|(p, _)| path_matches(&config, p)) {
        self.prepare_rename_target = None;
        self.prepare_renameable = None;
    }
    if self.format_path.as_ref().is_some_and(|p| path_matches(&config, p)) {
        self.format_edit = None;
        self.format_path = None;
        // format_ready is deliberately left untouched -- same
        // never-reset-except-by-its-consumer rule its own doc comment
        // already documents, unaffected by which client answered it.
    }
    if self.document_symbols_path.as_ref().is_some_and(|p| path_matches(&config, p)) {
        self.document_symbols_path = None;
        self.document_symbols.clear();
    }
    self.finding_references = false;
    self.finding_goto = false;
    self.finding_hover = false;
}
```

`path_matches(config, path)` is `ide_core::language_for_path(&[config.clone()], path).is_some()`
in spirit (a one-element-slice check) — the real implementation just
inlines the extension comparison against `config.extension`/
`config.extra_extensions` directly, to avoid a throwaway `Vec` allocation
per call.

**Deliberately not scoped, matching this file's existing behavior for a
full-teardown `ServerExited` today**: `hover`, `references`, `goto`,
`document_highlights`, `rename_edit`/`rename_new_name`/`rename_ready`,
`workspace_symbols` are left untouched by `stop_language` regardless of
which language exited — the *existing* single-client `ServerExited`
handler already leaves `hover` (and, on inspection while writing this
doc, `references`/`goto`/`rename_edit` too) untouched today, exactly
because "the popup/panel already showing this text isn't made misleading
by the server dying, the way stale highlights/hints would be"
(`hover`'s own doc comment). This feature extends that same accepted
principle from "the one server" to "any one of several servers" rather
than introducing a new compromise. `finding_references`/`finding_goto`/
`finding_hover` are still cleared on *any* client's exit regardless of
whether that client owned the in-flight query (a narrow, pre-existing-
shaped simplification: the alternative requires tracking which client
every in-flight query without a path target was sent to, for a payoff of
"a spinner keeps spinning a little longer in the rare case a different
language's server happened to die mid-query" — accepted as-is, called
out here rather than silently carried forward).

### 3.4 `query_workspace_symbols`'s merge race (accepted, v1)

Broadcasting to every client and appending each `LspEvent::WorkspaceSymbol`
response as it arrives (§3.2) has one accepted race: if a second query is
sent before every client has answered the first, a straggling response to
the *first* query can still arrive and gets appended into what the user
now sees as the *second* query's results, since responses aren't tagged
with which query they answered (`ide_lsp`'s `LspEvent::WorkspaceSymbol`
carries no request id). `workspace_symbols` is cleared at send-time in
this feature — a **deliberate reversal** of the single-client version's
"stale-but-plausible, cleared only by the fresh response" convention
(the same one `document_symbols`/`inlay_hints` still use): with several
clients appending into the same accumulator, leaving old results in
place until the first new one arrives would mean *every* re-query starts
by briefly showing a mix of old and new results even in the common case,
not just the rare straggler case. Clearing at send-time fixes the common
case at the cost of the accepted straggler race described here, which
only bites once per overlapping re-query rather than every time.
Given `ide_lsp::LspClient`'s internal
pending-request-slot model already prevents a *single* client from ever
answering an old query after a newer one superseded it
(`docs/features/rename-refactoring.md`-era precedent), this race is
narrower than it sounds — it only bites when the *same* project has
multiple language servers and the user re-searches faster than the
slowest of them can answer. Documented here as an accepted v1 gap rather
than solved with per-query generation tracking across every client,
consistent with this project's general practice of shipping a reasonable
v1 and naming the cut rather than either hiding it or gold-plating it.

## 4. Invariants

- A path is routed to **at most one** running client, ever —
  `language_for_path`/`stop_language`'s `path_matches` and
  `LspBridge.clients`'s extension-keyed map both derive from the same
  `LanguageConfig.extension`/`extra_extensions` matching rule
  `detect_active_languages` already establishes, so there's one source
  of truth for "which language owns this file," not two that could
  drift apart.
- `detect_active_languages` never returns more than
  `MAX_ACTIVE_LANGUAGES` (8) entries, so `LspBridge` never has to spawn
  an unbounded number of subprocesses for one project.
- `detect_active_languages` never returns two entries with the same
  (lowercased) `extension` — `LspBridge.clients`'s `HashMap<String, _>`
  key would silently collapse a duplicate otherwise, dropping one
  language's client without any signal that it happened.
- Rust's slot (`"rs"`) can never be taken by a `custom` entry, defensively
  enforced in `detect_active_languages` itself, not just relied upon from
  the UI's own duplicate-extension check at Add-time.
- `stop_language`/`sync_active_languages`/`restart_all` are the only
  three places that mutate `self.clients` — every public
  start/stop-adjacent method on `LspBridge` funnels through one of them,
  so there's one place that owns "what does tearing a language down
  clear" (§3.3), not one copy per call site the way the pre-feature code
  had it (`start_with_command` and `stop` each independently listed
  every field to clear).

## 5. Tests (required, per the implementing roles' own skills)

**`ide-core`**: `detect_active_languages` — no project (empty tree, no
markers) returns `[]`; Rust-only returns `[rust()]`; Rust-plus-matching-
custom returns `[rust(), custom...]` in that order; a `custom` entry
with no matching file in the tree is excluded; a `custom` entry with
`extension: "rs"` is excluded even though nothing else would filter it;
two `custom` entries sharing an extension keep only the first
(dedup-by-extension); a `custom` list with more than
`MAX_ACTIVE_LANGUAGES` matching entries is truncated to the cap, with
Rust (if matched) never being the one dropped; the polyglot worked
example from §3.1 end to end
(three real marker/extension combinations in one fixture tree, asserting
all three configs come back). `language_for_path` — matches on primary
extension, matches on an `extra_extensions` entry (the C/C++ case),
case-insensitive, returns `None` for an uncovered extension and for a
path with no extension at all.

**`ide-ui`**: every existing `LspBridge`/`IdeApp` language/LSP test needs
its shape updated for the plural `active_languages`/multi-client
`clients` map, per the signature changes in §2.2/§2.3 — not new coverage,
mechanical adaptation. New coverage specifically for the multi-client
behavior: two real (spawn-failing, same "definitely-not-a-real-lsp-
binary-xyz" convention this file already uses) configs started via
`sync_active_languages` land in two independent map entries; a third
config replacing one of the two (same extension, different command)
triggers a restart of only that one, leaving the other's entry
untouched (assert via a marker in `server_error`/timing isn't available
here — assert instead that the *unrelated* extension's entry survives by
checking `is_running_for_extension` before/after, or a dedicated
"still-running-ness" probe if a cleaner one exists once implementing);
`is_running_for`/`is_running_for_extension` return correctly for a
covered vs. uncovered path/extension across a two-language running set;
`stop_language`'s scoped clearing — a `diagnostics`/`inlay_hints`/
`semantic_tokens` entry for language A survives language B's teardown,
and is removed by language A's own teardown; `code_actions_client`
routing — `apply_code_action` is a no-op if `code_actions_client` is
`None`, and (via the `cat`-stand-in convention `request_methods_forward_
to_a_running_client` already uses) is attempted against the right
client when set; `query_workspace_symbols` broadcasts to every running
client (assert via `cat` standing in for two "servers," confirming both
receive the request — same forwarding-only assertion level
`request_methods_forward_to_a_running_client` already uses for its own
single-client case, not a full round-trip, which needs `ide-lsp`'s own
fixture-backed integration tests, not this bridge's unit tests).

## Revision notes

**Self-review (before implementation):** caught three gaps in the first
draft before writing code — (1) `apply_code_action`'s routing described
`code_actions_client` without ever defining the `send_to_key` primitive it
depends on, fixed by specifying it in §2.2; (2) no cap on how many
languages could be concurrently active, a genuinely new resource-
exhaustion consideration this feature introduces (every prior language
feature started at most one LSP subprocess) — fixed by adding
`MAX_ACTIVE_LANGUAGES = 8` and wiring it into `detect_active_languages`,
§4's invariants, and the test list; (3) a planned test for "a non-matching
earlier entry sharing an extension doesn't block a later matching one"
turned out to be unconstructible — matching depends only on the extension
string, not entry identity, so any two entries sharing an extension always
match or don't match identically. Removed the test, reworded §2.1's dedup
rule to note the distinction is unobservable by any real input.

**Implementation:** `ide-core` (`detect_active_languages`,
`language_for_path`, `MAX_ACTIVE_LANGUAGES`) — 100% line coverage
(99.91% region, one unreachable sub-branch, not blocking), 43 tests in
`language::` passing. `ide-ui` (`LspBridge` multi-client rewrite,
`IdeApp` field rename and call-site updates, the Languages… settings
window's `[running]`/`[stopped]` row suffix) — `lsp_bridge.rs` 92.29%
line coverage, `app.rs` 96.63%; `app/render.rs`'s one-line change is
rendering code, exempt from the floor per this project's convention.
Full workspace `fmt`/`clippy -D warnings`/`build --all-targets`/`test`
all green (2,378 tests total across the workspace).

**Hacker pass** (`docs/security-findings/rust-ui-dev-multi-language-projects-2026-08-28.md`):
one Medium DoS finding, non-blocking. `detect_active_languages` re-walks
the whole project tree once per `custom_languages` entry with no bound on
that array's length, and now fires from five UI-thread call sites
(project load, tree refresh, language-suggestion accept, language-settings
save/remove) instead of the old single-language design's narrower set —
live-measured at ~4.7s for a 50,000-entry array against a 9,331-node tree,
scaling linearly. This amplifies (doesn't introduce) the already-documented,
already-unfixed "unbounded `custom_languages` array size on deserialization"
gap from `project_settings.rs` — capping that array's length at deserialize
time closes both at once. Queued as the first item in the risk-fix pass
that follows this feature, per the user's standing instruction. Every other
checked area (the `MAX_ACTIVE_LANGUAGES` cap's live enforcement, scoped-
teardown correctness across `ServerExited`, no-shell verification for the
multi-spawn path, restart-overwrite semantics, the `workspace_symbols`
broadcast/merge race already accepted in §3.4) came back clean.
