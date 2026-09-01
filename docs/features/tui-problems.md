# `ide-tui`: Problems panel (T9)

## 1. Purpose

Part of the TUI-parity backlog (`docs/roadmap.md` §10, driven by the
user's explicit "all ide-gui features must exist in tui version"). A user
asked directly ("where is problems section") right after `tui-goto-and-
usages.md` shipped. `LspBridge::poll` already receives
`LspEvent::Diagnostics` from `rust-analyzer` (it's published unsolicited,
the moment a file is analyzed) but dropped it into the wildcard arm --
this batch collects and surfaces it, mirroring `ide-ui`'s Problems tool
window at a fraction of its size (no build/cargo diagnostics yet, LSP
diagnostics only -- `T10`'s cargo panel is a separate, not-yet-built
piece per the roadmap).

## 2. Interface / API

### 2.1 `src/lsp_bridge.rs`

```rust
pub(crate) diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
```

Replaced wholesale per path on each `LspEvent::Diagnostics` (LSP's
`publishDiagnostics` is always a full snapshot for that file, never a
delta) -- same convention `ide-ui`'s own `LspBridge::diagnostics` already
uses. Cleared in `start_with_command` and on `ServerExited`, alongside
every other query-state field.

### 2.2 `src/app.rs`

```rust
pub(crate) struct ProblemsState { pub(crate) selected: usize }

impl App {
    pub(crate) fn flattened_diagnostics(&self) -> Vec<(&PathBuf, &Diagnostic)>;
}
```

`flattened_diagnostics` flattens `lsp.diagnostics`'s per-file map into one
list, sorted by path then range start -- computed fresh on every call
(not cached) so it's always current; the sort gives `ProblemsState.selected`
a stable meaning across two calls despite `HashMap`'s own unstable
iteration order. `App` gains `problems: Option<ProblemsState>`, and a new
`close_all_overlays` helper that Goto/Notifications/Problems each call
before opening, so at most one of the three overlay panels is ever open
at once (§4).

### 2.3 `src/commands.rs`

One new `Command`: `ToggleProblems`, title "Problems", bound to `Ctrl+P`.
`ide-ui`'s own binding is `⌘6` (a `Cmd`+digit chord) -- per this crate's
"never invent a binding, but don't force a translation that collides"
rule (`commands.rs`'s own module doc comment, same reasoning as
`ToggleProjectToolWindow`'s `Ctrl+T` and `FindUsages`'s `Ctrl+U`), `Ctrl+P`
is used instead of the naive, digit-ambiguous `Ctrl+6`.

## 3. Behaviour

1. `rust-analyzer` publishes diagnostics for a file (on open, on every
   `DidChange`); `LspBridge::poll` stores them, replacing that file's
   previous set wholesale.
2. `Ctrl+P` opens the panel: every current diagnostic across every open-or-
   analyzed file, one per row, prefixed with a one-letter severity marker
   (`E`/`W`/`I`/`H`) and formatted `path:line: message`.
3. `Up`/`Down` moves the selection (clamped at both ends); `Enter` opens
   that diagnostic's file and jumps the cursor to its range's start,
   reusing `open_location` (the same jump `Goto`/`Find Usages` already
   use -- a `Diagnostic`'s `range` plus its owning path is exactly a
   `Location`'s shape) and closes the panel. `Esc` closes without jumping.
4. The status bar gets a `[N problems]` suffix (alongside the existing
   `[N unread]` notifications suffix) whenever the flattened list is
   non-empty, so problems are visible without opening the panel.

## 4. Constraints & invariants

- **LSP diagnostics only, no cargo/build diagnostics.** `ide-ui`'s
  Problems tool window also aggregates `cargo`'s own JSON diagnostics
  (`build-integration.md`, not yet built in `ide-ui` either per
  `docs/roadmap.md` §6 track F) -- nothing to port there yet. This
  crate's own `T10` (cargo panel) is the natural place to add that later.
- **Mutual exclusion via `close_all_overlays`.** Opening Problems closes
  an open Goto picker or Notifications panel and vice versa -- extending
  `tui-goto-and-usages.md` §4's two-panel rule to three. `find`/`palette`
  are unaffected (different, outer interception tier in `handle_key`,
  same as before).
- **No mouse, no scrollbar markers.** `ide-ui`'s Problems panel is a real
  `egui` list with click-to-jump; this is `Up`/`Down`/`Enter` only,
  consistent with every other picker this crate has built (`GotoState`,
  the palette).

## 5. Examples

```
$ ide-tui ~/code/my-rust-project
```

A file with an unresolved import shows `[1 problems]` in the status bar
as soon as `rust-analyzer` finishes analyzing it. `Ctrl+P` opens the list;
`Enter` on that row jumps straight to the offending line.

## 6. Dependencies & integration points

No new dependencies. Touches `crates/tui/src/{lsp_bridge,app,commands,ui}.rs`.
Not on `CLAUDE.md`'s security-sensitive path list (LSP diagnostics are
already-validated `ide_lsp::Diagnostic` values flowing through the same
bridge `tui-goto-and-usages.md` already covered) -- no `hacker` pass.

## 7. Diagrams

None -- same reasoning as `tui-goto-and-usages.md` §7: a small,
already-established request/response/render shape, no new component
boundaries.

## Revision notes

Implemented directly in response to a live user question ("where is
problems section") as the first item of the TUI-parity backlog
(`docs/roadmap.md` §10, `T9`). Self-reviewed before merge: no
controversial findings -- `flattened_diagnostics` recomputing on every
call rather than caching was considered and is the right call at this
scale (a handful to a few hundred diagnostics, recomputed only while the
panel is open or the status bar renders, both already once-per-frame
operations).
