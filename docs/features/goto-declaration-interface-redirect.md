# Go to Declaration redirects to Go to Implementation on an interface

## 1. Purpose

Real JetBrains IDEs: `Cmd+Click`/`Cmd+B` ("Go to Declaration") on a symbol
that resolves to an interface/trait -- the interface itself, or a member
declared only inside one, with no concrete body -- redirects to **Go to
Implementation** instead of landing on the bodiless interface declaration,
since there is nothing useful to look at there. `ide-ui`'s existing
`goto-definition.md` (C1) never implemented this refinement; this phase
adds it, and (per the user's explicit request while reviewing the design)
extends it to `ide-tui`'s equivalent `Ctrl+B` gesture too, even though that
crate had no separate "Go to Implementation" query at all before this.

Scope: only the "Go to Declaration" gesture (`Cmd+Click`/`Cmd+B` in
`ide-ui`, `Ctrl+B` in `ide-tui`) redirects. `ide-ui`'s `Cmd+Option+B`
(explicit Go to Implementation) and `Ctrl+Shift+B` (Go to Type
Declaration) are unaffected -- the redirect only ever substitutes what a
*Definition* query would have done.

## 2. Interface

### 2.1 `ide-lsp` (new shared pure function, `crates/lsp/src/types.rs`)

```rust
/// True if any symbol in `symbols` (a file's `documentSymbol` result) has
/// kind `Interface` and its range contains `position` -- i.e. `position`
/// falls on, or inside, a trait/interface declaration. A `documentSymbol`
/// tree's parent ranges always span their children's ranges (LSP spec:
/// `DocumentSymbol.range` covers the whole declaration, `children` nest
/// inside it), so this single check catches both "resolved directly to
/// the interface/trait's own declaration" (a symbol of kind `Interface`
/// whose range contains `position` -- the position sits on the item
/// itself) and "resolved to a member declared only inside one" (an
/// ancestor of kind `Interface` whose range still contains `position`
/// even though the *innermost* containing symbol is e.g. `Method`) with
/// the same test, no name-based parent lookup needed.
pub fn position_is_within_interface(symbols: &[Symbol], position: Position) -> bool;
```

Exported from `ide-lsp`'s `lib.rs` alongside the other `types` re-exports.
Used identically by both frontends -- this is the one piece of new logic
that would otherwise be duplicated verbatim across `ide-ui` and `ide-tui`,
so unlike most frontend-local additions in this project it belongs in the
shared crate both already depend on.

No new `LspRequest`/`LspEvent` variant: this phase is built entirely out
of the `DocumentSymbol` and `Goto{kind: Implementation}` queries both
crates already have (`search-everywhere.md`/`goto-definition.md` for
`ide-ui`; `tui-go-to-file-and-symbol.md` for `document_symbols`,
`tui-goto-and-usages.md` for `Goto`, extended here to add
`GotoKind::Implementation` on the `ide-tui` side, which previously only
ever sent `GotoKind::Definition`).

### 2.2 `ide-ui` (`crates/ui/src/lsp_bridge.rs`, `crates/ui/src/app.rs`)

`LspBridge` gains one new field:

```rust
/// `true` for exactly the one `poll()` call that processed the most-
/// recently-arrived `LspEvent::DocumentSymbol` -- reset to `false` at the
/// top of every `poll()`, mirroring `goto_ready`. The pre-existing
/// `document_symbols`/`document_symbols_path` fields have no such flag
/// (`request_document_symbols`'s own doc comment: "the Symbols tab just
/// renders whatever's here") because their one existing consumer (File
/// Structure / Go to Symbol) just live-renders the current cache with no
/// need to distinguish "fresh this frame" from "stale from before." This
/// phase's second consumer (the interface-redirect check below) genuinely
/// needs that distinction: without it, a `document_symbols` cache left
/// over from an unrelated earlier File Structure query -- for the exact
/// file this query also targets -- would be misread as this query's own
/// answer, on the very frame the request was sent, before any real
/// response could possibly have arrived.
pub document_symbols_ready: bool,
```

`IdeApp` (`app.rs`) gains:

```rust
/// The `(path, position)` a `Cmd+Click`/`Cmd+B` press was actually fired
/// from -- remembered so the interface-redirect check (below) knows where
/// to re-query `Implementation` from if the resolved declaration turns
/// out to need one. `None` whenever no redirect check is pending.
goto_declaration_origin: Option<(PathBuf, Position)>,
/// The single `Goto` result `handle_goto_response` is holding while it
/// waits on a `DocumentSymbol` response to decide whether to jump to it
/// directly or redirect to `Implementation` from `goto_declaration_origin`
/// instead. `None` whenever no check is pending.
pending_interface_check: Option<Location>,
```

`trigger_go_to_declaration` additionally sets `goto_declaration_origin =
Some((path, position))` right before calling `LspBridge::go_to_definition`
(and clears it in the no-op branch, matching how `goto_action` is already
set unconditionally by that method regardless of the no-op gating).

`handle_goto_response` gains the redirect: when `self.goto_action ==
Some(GotoKind::Definition)` and exactly one result arrived, instead of
jumping immediately it sets `pending_interface_check` to that one
location and calls `self.lsp.request_document_symbols(&location.path)`,
then returns without opening the popup either -- the existing zero/many
branches are unchanged, and every other `goto_action` (`TypeDefinition`,
`Implementation`) is unaffected, jumping/popping-up exactly as before.

A new `handle_interface_check_response(&mut self)`, called once per frame
right after `handle_goto_response` (both after `self.lsp.poll()`): no-op
unless `self.lsp.document_symbols_ready` and `pending_interface_check` is
`Some` and `self.lsp.document_symbols_path.as_deref()` matches the
pending location's path (a `DocumentSymbol` response for some *other*
file -- e.g. a stale in-flight File Structure query for a file the user
had open before -- must not be consumed here; it's left for its own
intended consumer, and this check's own request stays outstanding until
its own matching response arrives). On a match, takes
`pending_interface_check`; if
`ide_lsp::position_is_within_interface(&self.lsp.document_symbols,
location.range.start)`, calls `self.lsp.go_to_implementation` using
`goto_declaration_origin` (falling back to jumping to `location` directly
if `goto_declaration_origin` is somehow `None` -- defensive, should be
unreachable given `trigger_go_to_declaration` always sets it first);
otherwise jumps to `location` via the existing `open_definition`, exactly
what would have happened without this phase.

**Fallback, never stuck**: if the `DocumentSymbol` query never answers at
all (server doesn't support the capability, server exits, or the response
is simply slow) the user only loses the redirect, never the jump itself
outright -- but see §3.4 for the one case this doesn't cover for free.

### 2.3 `ide-tui` (`crates/tui/src/lsp_bridge.rs`, `crates/tui/src/app.rs`)

`LspBridge` gains:

```rust
/// `Ctrl+Option+B`'s v1 in `ide-tui`... actually not bound to any key yet
/// (this crate never exposed Go to Implementation before this phase) --
/// exists now purely so the interface-redirect can send it. No new
/// binding is added; `Ctrl+B`'s existing behavior is what changes.
pub(crate) fn go_to_implementation(&mut self, path: &Path, position: Position);
/// Mirrors `ide-ui`'s own field, same reset-at-top-of-`poll()` semantics.
pub(crate) document_symbols_ready: bool,
```

`App` gains the same `goto_declaration_origin`/`pending_interface_check`
pair (private fields), and `trigger_go_to_declaration`/`handle_goto_
results`/`poll_lsp` are extended the same way `ide-ui`'s equivalents are
in §2.2 -- `handle_goto_results` only takes the redirect branch when
`title == "Declaration"` (this crate's `handle_goto_results` is shared
between Declaration and Usages, keyed by that title string rather than a
`GotoKind` field the way `ide-ui`'s `goto_action` is; `"Usages"` never
takes this path).

## 3. Behaviour

### 3.1 The common case

Cmd+Click/Cmd+B (`ide-ui`) or `Ctrl+B` (`ide-tui`) on a plain function,
struct, class, or concrete method: **unchanged**. Go to Declaration
resolves, the redirect check finds no `Interface`-kind symbol containing
the target, and the jump happens exactly as it always did -- with one
extra `DocumentSymbol` round-trip most users will never notice (rust-
analyzer answers it in well under a frame on any real project).

### 3.2 The redirect case

Resolves to (or a member declared only inside) a trait/interface:

- **Exactly one implementor** -- jumps straight there, no popup, same
  single-result UX as an ordinary Go to Declaration jump.
- **Multiple implementors** -- opens the existing ambiguous-result popup
  (`ide-ui`'s `render_goto_popup`; `ide-tui`'s `GotoState` picker),
  labelled for Implementation the same way an explicit `Cmd+Option+B`
  press's popup already is.
- **Zero implementors** -- opens that same popup showing "no results"
  (`ide-ui`) / a notification (`ide-tui`), rather than silently landing on
  the bodiless interface declaration.

### 3.3 Detection

`position_is_within_interface` is checked against the *resolved
Definition target*, not the original click position: clicking a call
site that resolves to a trait method's own signature (declared only
inside the trait body, kind `Method`, `container` an `Interface`-kind
symbol whose range still contains it) is caught by this because the
containing `Interface` symbol's own range -- not just the innermost
`Method` symbol's -- is checked; clicking the trait/interface name itself
resolves directly to the `Interface`-kind symbol, caught the same way.

### 3.4 What this doesn't cover

Not every "resolves to an abstract member with several real
implementations" case is reachable through `SymbolKind::Interface`: an
abstract method on a Rust `struct` isn't a thing (Rust has no abstract
classes), but another language's LSP server modeling e.g. a Java abstract
class the same way would need `SymbolKind::Class`, not `Interface`, to
carry the same signal -- out of scope for v1, which only ever checks
`Interface` (the case the user asked for, and the case rust-analyzer
itself actually produces for Rust `trait`s). Extending the check to cover
abstract classes in other languages is a natural, additive follow-up (a
second `SymbolKind` in the same check) if the project ever exercises this
against a non-Rust language server in practice.

If the `DocumentSymbol` query never answers (unsupported capability,
server exit, a response for some other file arrives instead and this
one's stays pending forever), the user is left waiting rather than
jumping: `pending_interface_check` never resolves and `handle_goto_
response`'s early return already fired, so no fallback jump happens on
its own. `ServerExited` clears `pending_interface_check`/`goto_declaration_
origin` (added to both crates' existing `ServerExited` clear-lists) so at
least a dead server doesn't leave the state permanently stuck across a
restart, but a *live* server that simply never answers this specific
query is an accepted, narrow gap -- the same class of gap `finding_hover`/
`finding_goto` already have (nothing times out a query in this codebase
today), not a new one this phase introduces.

## 4. Constraints

- Not security-sensitive on its own: no new subprocess, path, or network
  surface -- purely additional client-side interpretation of data
  (`DocumentSymbol` responses) both crates already fetch and trust for
  other features. `hacker` skipped.
- `position_is_within_interface`'s range-containment comparison is
  `(line, character)` tuple ordering, both fields already `Copy`/`Ord`-
  free-standing `u32`s -- no `PartialOrd`/`Ord` derive needed on
  `Position`/`Range` themselves, this function does the comparison
  directly.

## 5. Examples

- `Cmd+Click` on `Box<dyn Logger>`'s `Logger` name where `trait Logger`
  has three `impl Logger for ...` blocks -- opens the picker with all
  three, instead of jumping to `trait Logger { ... }`'s bodiless
  declaration.
- `Cmd+Click` on `logger.log(...)` where `logger: &dyn Logger` and there
  is exactly one `impl Logger` in the whole project -- jumps straight to
  that one impl's `fn log` body, no popup.
- `Cmd+Click` on a plain `fn helper()` call -- jumps to `fn helper()`'s
  body exactly as before this phase; the `DocumentSymbol` round-trip finds
  no `Interface` symbol containing the target and changes nothing.

## 6. Dependencies / integration

No new external dependency. Touches `crates/lsp/src/{types.rs,lib.rs}`,
`crates/ui/src/{lsp_bridge.rs,app.rs}`, `crates/tui/src/{lsp_bridge.rs,
app.rs}` -- three roles, `rust-lsp-dev` merges first, `rust-ui-dev`/
`rust-tui-dev` after (independent of each other).

## Revision notes

- **Re-entrancy on the redirected query itself.** The obvious first
  implementation left `ide-ui`'s `goto_action` at `Definition` (and
  `ide-tui` had no equivalent marker at all) while the redirect's own
  `go_to_implementation` request was in flight. When that response landed,
  `handle_goto_response`/`handle_goto_results` would have treated it as
  *another* Declaration result and deferred it into a second
  `DocumentSymbol` check -- for the same interface, forever. Fixed in both
  crates: `ide-ui` sets `goto_action = Some(GotoKind::Implementation)`
  right before sending the redirected query (mirroring what
  `trigger_go_to_implementation` already does for the explicit gesture);
  `ide-tui`, which has no persistent `goto_action` field, gained a new
  one-frame `expect_implementation_next` flag that `poll_lsp` consults to
  label the next `goto_ready` response `"Implementation"` instead of the
  hardcoded `"Declaration"` -- `handle_goto_results`'s redirect branch is
  gated on `title == "Declaration"`, so that label alone is enough to keep
  the redirected result from being checked a second time.
- Both crates' single-result jump was refactored into a small shared
  helper (`IdeApp::open_definition`'s call site stayed a one-liner already;
  `ide-tui` gained `jump_to_goto_result`) so `handle_interface_check_
  response`'s own resolution -- direct jump on a plain symbol, or the
  fallback when `goto_declaration_origin` is unexpectedly `None` -- reuses
  the exact same notify-then-open behaviour the ordinary single-result
  path already had, rather than a second hand-rolled copy.
- No `docs/roadmap.md` update: this is a refinement of the already-shipped
  C1 `goto-definition.md` phase, not a new lettered/numbered roadmap item.
