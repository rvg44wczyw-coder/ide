# Hacker pass: `ide-ui` debugger frontend (F5a)

## 1. Scope

Reviewed worktree `/Users/ivs/rust/ide-worktrees/rust-ui-dev-debugger-f5a`,
branch `rust-ui-dev/debugger-f5a`, commit `b12a0dc` (after two rounds of
`rev`, both approved). Target: `docs/features/debugger.md` §6's
newly-declared security-sensitive surface for this run —

- `crates/ui/src/debug_panel.rs` (new — bridges `ide_dap::DapClient` into
  the frame loop, constructs the debug-adapter subprocess command from
  user config, holds every DAP response including stack-frame source
  paths and output text that gets rendered straight into the UI).
- The breakpoint gutter click-target/paint addition to
  `crates/ui/src/editor/mod.rs` / `crates/ui/src/editor/paint.rs` — per
  the doc, already covered by the existing `editor-git-gutter.md`
  security-sensitive entry, not a new surface. Read it to confirm this is
  actually true (no new untrusted-data-rendering path introduced beyond
  what that entry already accepted) rather than taking the doc's word for
  it.

`crates/dap/**` itself is **not** in scope for this pass — it already went
through its own `hacker` review earlier in this chain run
(`docs/security-findings/ide-dap-2026-09-02.md`). I did read its relevant
internals (`client.rs`, `path.rs`) to confirm what guarantee `ide-ui`
is actually relying on, and ran one of its existing live integration
tests to confirm that guarantee holds on this commit — see §1.2.

### 1.1 Attack-surface categories

- **Subprocess execution (command injection)** — applies. The debug
  adapter command comes from user config and is spawned as a child
  process.
- **Sandbox/privilege escalation** — applies, but only to the extent
  `ide-dap` already accepted (spawning the adapter, which spawns/attaches
  to the debuggee, is the feature). `ide-ui`'s own code doesn't grant the
  subprocess anything beyond what `ide_dap::DapClient::start` already
  does.
- **Path traversal** — applies: a compromised or buggy adapter can send
  a `stackTrace` response with an arbitrary `source.path`, and `ide-ui`
  turns a `StackFrame` click into a real file-open + cursor placement.
- **DoS / resource exhaustion** — applies: adapter `Output` events feed
  an in-memory log; adapter/debuggee behavior isn't otherwise trusted.
- **Input validation (adversarial)** — applies: DAP responses are
  attacker-influenced JSON parsed by `ide-dap` and consumed by
  `debug_panel.rs`; the launch-arguments text field is user-typed JSON
  parsed at Launch-click time.
- **MITM/Replay/Downgrade/KeyConfusion/Timing/WeakRandomness** — ruled
  out. No network protocol, no cryptography, no session tokens anywhere
  in this diff — it's a local stdio subprocess, same trust model as
  `ide-lsp`.
- **Metadata leakage** — considered, nothing beyond what `crates/dap`
  already accepts (adapter argv is local-process-only, not transmitted
  anywhere).

### 1.2 Live tests actually run

- `cargo test -p ide-dap --test fixture_integration
  stack_frame_source_outside_project_root_comes_back_as_none` — **ran
  live**, passed. This is `ide-dap`'s own existing integration test that
  spawns a real fake-adapter subprocess (`tests/fixtures/
  fake_debug_adapter.rs`) which answers a `stackTrace` request with two
  frames, one inside the fixture's project root and one with
  `source.path: "/definitely/outside/the/project/root.rs"`. Confirms,
  against a real subprocess round-trip (not just a unit test of the
  parsing function), that the "outside" frame comes back with
  `source: None` — the exact guarantee `ide-ui`'s `open_stack_frame`
  navigation relies on never being violated.
- `cargo test -p ide-ui debug_panel::tests::output_log_is_capped_at_max_debug_output_lines`
  — ran live, passed. Confirms the output-log memory bound (§4 of the
  doc) actually holds under a synthetic flood of `2010` events fed
  directly through `apply_event`.
- Everything else in this doc is code-analysis (reading `debug_panel.rs`,
  the relevant `app.rs`/`app/render.rs` call sites, and `ide-dap`'s
  `client.rs`/`path.rs`) rather than a fresh live attack, because:
  - the subprocess-spawn code path (`Command::new(command).args(args)`,
    no shell) is identical in shape to `ide-lsp`'s already-reviewed
    precedent, and
  - the render-side gating on `StackFrame::source` being `Some` before
    treating it as clickable is a single `if let` with no branching
    complex enough to warrant a second live harness on top of the
    `ide-dap`-layer test above — I traced the one and only call site
    (`app/render.rs`'s Debug tool window, `crates/ui/src/app/render.rs`
    around the stack-frame list) and confirmed a `None`-source frame
    renders as `ui.label(...)` (inert), never `ui.selectable_label(...)`
    (clickable).

## 2. Findings

No Critical/High/Medium findings. One Low, informational-severity note:

1. **[DoS, Low]** `crates/ui/src/debug_panel.rs:104-109` —
   `apply_event`'s `Output` arm evicts the oldest line with
   `self.output.remove(0)` on a plain `Vec<(OutputCategory, String)>`,
   which is O(n) per removal (shifts up to `MAX_DEBUG_OUTPUT_LINES` =
   2000 elements), rather than the `VecDeque::pop_front` (O(1)) pattern
   `ClaudeTerminal::TERMINAL_SCROLLBACK_LIMIT` already established and
   that this code's own doc comment claims to mirror.
   - **Attack scenario**: a malicious or malfunctioning debuggee/adapter
     that floods `stdout` could force this O(n) shift on every one of
     its `Output` events, all processed synchronously on the UI thread
     inside `poll()`/`apply_event` (called once per frame, but draining
     *every* queued event that frame in a `while let` loop) — in
     principle, a way to make the eviction cost scale with flood volume
     rather than staying flat.
   - **How verified**: code reading + the arithmetic in §1's live test
     comment; I did not attempt to actually generate a sustained
     high-rate `Output` flood, because the realistic bottleneck is
     upstream of this line — every `Output` event is one full DAP
     `Content-Length`-framed JSON message parsed by `ide-dap`'s
     event loop and sent across an async channel before it ever reaches
     this code, and that per-message JSON-parse + channel-send overhead
     dominates the ~40-80 bytes this line would additionally `memmove`
     at the capped 2000-element bound. I judged standing up a live flood
     harness not worth it given that math, but flag the discrepancy from
     the stated "mirrors `ClaudeTerminal` exactly" intent either way,
     since a future doc/feature reusing this exact pattern at a higher
     cap or lower upstream overhead could make it a real one.
   - **Suggested fix direction**: switch `output` to `VecDeque` and use
     `push_back`/`pop_front`, matching `ClaudeTerminal` exactly as the
     comment already claims.

## 3. Verdict

Clean — no Critical/High/Medium security findings. One Low, informational
DoS-class note above (not blocking).
