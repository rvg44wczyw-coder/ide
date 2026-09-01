# `ide-tui`: Semantic highlighting (T14)

## 1. Purpose

Third item of the TUI-parity backlog (`docs/roadmap.md` §10). Ports
`semantic-highlighting.md` (A10) to `ide-tui`: layers `rust-analyzer`'s
`textDocument/semanticTokens/full` answer on top of the existing regex
tokenizer (`crates/core/src/syntax.rs`, `tui-syntax-highlighting.md`), so
`ide-tui` gets the same "type vs. variable of the identical shape" and
"call vs. definition" disambiguation `ide-ui` already has. No new
`ide-core`/`ide-lsp` surface needed -- both already carry this feature's
entire protocol machinery (`TokenKind::Variable`,
`SemanticTokenKind`/`SemanticToken`, `LspRequest::SemanticTokensFull`,
`LspEvent::SemanticTokens`, capability negotiation, bounded delta-decode)
from A10's own `ide-core`/`ide-lsp` work, already merged into `main`. This
doc covers only the `ide-tui`-side consumption, the same split
`tui-goto-and-usages.md`/`tui-problems.md` already established between
already-merged shared infrastructure and this crate's own wiring.

Same priority rule as A10: wherever the server has an opinion about a
span, its opinion wins over the regex tokenizer's guess for that span.
Everywhere it has none, the regex tokenizer's own output renders
unchanged.

## 2. Interface / API

### 2.1 `src/lsp_bridge.rs`

```rust
pub(crate) semantic_tokens: HashMap<PathBuf, Vec<ide_lsp::SemanticToken>>,

impl LspBridge {
    pub(crate) fn request_semantic_tokens(&mut self, path: &Path);
}
```

Same per-file-map, replace-wholesale-per-path convention `diagnostics`
already uses (`tui-problems.md` §2.1) -- raw, `Position`-based, exactly as
`ide-lsp` decoded them (this bridge has no buffer text of its own to
convert against, same reasoning `ide-ui`'s `LspBridge::semantic_tokens`
field doc already gives). `request_semantic_tokens` is a no-op with no
client running, doesn't clear the existing entry at send-time (stale-but-
plausible until replaced, same as every other query this bridge sends).
`poll()`'s match gains a `LspEvent::SemanticTokens { path, tokens }` arm
(`self.semantic_tokens.insert(path, tokens);`) and `start_with_command`/
`ServerExited` both clear `semantic_tokens`, alongside every other
query-state field.

### 2.2 `src/app.rs`

```rust
impl App {
    pub(crate) fn active_semantic_tokens(&self) -> &[ide_lsp::SemanticToken];
}
```

The active tab's entry in `lsp.semantic_tokens`, or `&[]` with no active
tab or no entry yet -- same shape `flattened_diagnostics` established for
reading LSP-sourced per-file state. `sync_lsp_did_change` (the one
function every edit path already funnels through -- Undo/Redo/typing/
find-replace, per its own doc comment) gains one more line,
`self.lsp.request_semantic_tokens(&path);`, right after sending
`DidChange` for the same path. `open_or_focus_tab` gains the same call
right after sending `DidOpen` for a freshly-opened tab. Unlike `ide-ui`
(which fires `sync_semantic_tokens` from two named call sites alongside
`sync_inlay_hints`), `ide-tui` doesn't have an inlay-hints call site to
mirror yet (`T12`, not built) -- folding the request into
`sync_lsp_did_change` itself, rather than duplicating a call at each of
that function's five call sites, gets the same "refetch on every DidOpen/
real DidChange" behaviour with one line instead of five.

### 2.3 `src/highlight.rs`

```rust
pub fn semantic_token_marks(text: &str, tokens: &[ide_lsp::SemanticToken]) -> Vec<Token>;
pub fn tokens_in_range(tokens: &[Token], range: Range<usize>) -> &[Token];
pub fn merge_semantic_tokens(regex: &[Token], semantic: &[Token]) -> Vec<Token>;

pub fn styled_line(
    text_buffer: &TextBuffer,
    line: usize,
    semantic_tokens: &[Token],
) -> Line<'static>; // signature change: new third parameter
```

Direct ports of `ide-ui`'s `crates/ui/src/editor/paint.rs` functions of
the same name (`semantic-highlighting.md` §2.3) -- byte-range conversion
via `ide_lsp::position_to_byte_offset` (dropping any token whose start or
end doesn't resolve to a valid offset), the exact nine-way
`SemanticTokenKind` → `TokenKind` table, the same `partition_point`
binary search `tokens_in_lines` already uses internally, and the same
overlap-drop-and-replace merge with its no-overlap postcondition
(§4 below). `styled_line` itself changes shape rather than gaining a
sibling: it now takes the whole buffer's already-byte-converted semantic
tokens (computed once per frame by the caller, not per line -- see §3.3)
and, per line, slices them via `tokens_in_range` and merges them over the
line's regex tokens via `merge_semantic_tokens` before building spans,
exactly where `regex`-only tokens were read directly before.

## 3. Behaviour

1. Opening a file (`DidOpen`) or making a real edit (`DidChange`, via
   `sync_lsp_did_change`) requests a fresh full semantic-tokens re-tag for
   that file, mirroring `ide-ui`'s "no delta support, always request a
   full re-tag" v1 choice (A10 §1).
2. `LspBridge::poll` stores the response, replacing that file's previous
   entry wholesale -- the old highlighting keeps rendering, unchanged,
   until the fresh answer arrives (same latency-tolerant convention as
   every other per-file map this bridge keeps).
3. `render_editor` converts `app.active_semantic_tokens()` to
   absolute-byte-range `Token`s once per frame (`semantic_token_marks`),
   then passes that slice into every visible line's `styled_line` call.
   Each line slices its own overlapping semantic tokens
   (`tokens_in_range`) and merges them over that line's regex tokens
   (`merge_semantic_tokens`) before painting -- a semantic token's color
   wins for its exact span; the regex tokenizer's guess still renders
   everywhere the server said nothing (punctuation, comments, strings,
   most keywords).
4. No client running, capability absent, or a response that hasn't
   arrived yet (or dropped every entry via §4's fail-closed decode) all
   degrade to the same thing: an empty semantic-token slice, so
   `merge_semantic_tokens` returns the regex tokenizer's own output
   completely unchanged -- identical to `ide-tui`'s behaviour before this
   batch.

## 4. Constraints & invariants

- **No new `ide-core`/`ide-lsp` surface.** Both already carry everything
  this feature needs from A10; this doc's diff is entirely inside
  `crates/tui/**`.
- **No-overlap postcondition on the merged per-line token list.**
  Load-bearing for the same reason A10 §2.3/§4 states it is in `ide-ui`:
  `styled_line`'s span-building loop resolves each stretch of text to
  *one* style by walking tokens in order with no priority field to break
  a tie otherwise -- `merge_semantic_tokens` guaranteeing the result never
  contains two overlapping tokens is what makes that walk correct rather
  than order-dependent by accident.
- **Whole-buffer conversion once per frame, not per line.** Same
  performance shape `tui-syntax-highlighting.md`'s own Revision notes
  already chose for the regex tokenizer's viewport-limited calls --
  `semantic_token_marks` runs once per frame over the active buffer's
  full semantic-token list (typically a few hundred entries at most, not
  per visible row), then `tokens_in_range`'s binary search does the
  per-line narrowing cheaply.
- **`request_semantic_tokens` folded into `sync_lsp_did_change`, not a
  separate call site.** See §2.2 -- a deliberate, documented departure
  from `ide-ui`'s "two call sites" shape, since `ide-tui` has no sibling
  call site to mirror yet.
- **Not on `CLAUDE.md`'s security-sensitive path list.** `ide_lsp::
  SemanticToken` values flowing through this bridge are already-validated,
  already-bounded-decoded data (A10's own `hacker` pass, `crates/lsp/**`,
  covered the untrusted-server-response surface) -- this diff only
  consumes that already-safe shape, the same way `tui-problems.md` §6
  reasoned about `ide_lsp::Diagnostic`.

## 5. Examples

```
$ ide-tui ~/code/my-rust-project
```

Opening a file with a local variable `foo` and a type `Foo` in the same
scope now colors them distinctly once `rust-analyzer` answers -- before
this batch, the regex tokenizer colored both identically (an identifier
is an identifier, by shape alone).

## 6. Dependencies & integration points

No new dependencies. Touches `crates/tui/src/{lsp_bridge,app,highlight,ui}.rs`.

## 7. Diagrams

None -- same reasoning as `tui-problems.md` §7: an established request/
response/render shape (`tui-goto-and-usages.md` for the query lifecycle,
`tui-syntax-highlighting.md` for the render path), no new component
boundary a diagram would clarify.

## Revision notes

Implemented as the third item of the TUI-parity backlog, directly porting
A10's already-reviewed `ide-ui`-side algorithms (`semantic_token_marks`/
`tokens_in_range`/`merge_semantic_tokens`) rather than redesigning them --
their correctness (the no-overlap postcondition in particular) was already
established during A10's own review, so this batch's own review focus was
the port's faithfulness and `ide-tui`'s different call-site shape
(`sync_lsp_did_change` folding, §2.2/§4), not the algorithms themselves.
Self-reviewed inline (`rev`-style pass only -- not on the security-
sensitive path list, so no `hacker` pass, per §4's own reasoning): no
controversial findings.
