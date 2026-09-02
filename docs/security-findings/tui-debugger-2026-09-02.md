# Security review: `ide-tui` debugger port (T27)

**Scope**: worktree `rust-tui-dev-tui-debugger`, branch `rust-tui-dev/tui-debugger`, commit `8badd74`, rev-approved. Reviewed against `CLAUDE.md`'s declared security-sensitive entries for `crates/tui/src/debug_panel.rs` and `crates/tui/src/debug_config.rs`.

## Scope and rule-outs

This is a TUI port of `crates/ui/src/debug_panel.rs`, already reviewed and cleared under `docs/security-findings/ide-ui-debug-panel-2026-09-02.md`. `crates/tui/src/debug_panel.rs` was diffed byte-for-byte against that already-audited file (visibility keywords normalized first):

```
diff <(sed -E 's/pub\(crate\)/pub/g' crates/tui/src/debug_panel.rs) ../ide/crates/ui/src/debug_panel.rs
```

Zero output — the logic is identical, so this pass does not re-derive findings for that file's internal logic (DAP event handling, `MAX_DEBUG_OUTPUT_LINES` cap, breakpoint bookkeeping); it only re-verifies the cap is actually present (confirmed: `crates/tui/src/debug_panel.rs:16` `MAX_DEBUG_OUTPUT_LINES = 2000`, enforced at `:105-106`, with an existing test proving it (`output_is_capped_at_max_debug_output_lines` pushing `MAX_DEBUG_OUTPUT_LINES + 10` entries and asserting `.len() == MAX_DEBUG_OUTPUT_LINES`)).

Net-new surface reviewed:
- `crates/tui/src/debug_config.rs` (new file): persisted `~/.config/ide-tui/debug_adapters.json`.
- `crates/tui/src/app.rs`: `App::new`'s language enrichment from `debug_config::load()`, `trigger_debug`/`confirm_debug_launch`, `toggle_debug_adapter_config_popup`/`handle_debug_adapter_config_key`/`confirm_debug_adapter_config`, `toggle_breakpoint_at_caret`/`breakpoint_line_ranges`, `open_stack_frame`.
- `crates/tui/src/highlight.rs`: `LineOverlays`'s two new breakpoint-wash fields threaded through `styled_line`.
- `crates/tui/src/ui.rs`: `render_debug_panel`'s rendering of adapter-controlled output text.

Attack-surface categories from the skill checklist, ruled in/out:
- **Subprocess/sandbox escape** — applies (`ide_dap::DapClient::start` spawns the configured adapter). Reviewed by tracing the command/args path end to end; the DAP subprocess-framing layer itself is unchanged by this diff (confirmed via `git diff --name-only main...rust-tui-dev/tui-debugger`, `crates/dap/**` absent) and was already audited under `ide-dap-2026-09-02.md`.
- **Input validation (adversarial)** — applies (`debug_config.rs`'s JSON load, user-typed launch-args JSON, user-typed command/args popup). Live-tested (see below).
- **Path traversal** — applies to `open_stack_frame`/`breakpoint_line_ranges`. Reviewed; both consume already-validated/already-local paths, no new traversal surface (see Findings).
- **DoS / resource exhaustion** — applies to the config file and the debug output buffer. Live-tested (see below).
- **MetadataLeak** — adapter subprocess argv is visible to other local processes via `ps`, same as `ide-ui`'s already-accepted convention; no new secret enters argv here (command/args are developer tool paths/flags, not credentials).
- Ruled out entirely: MITM/identity spoofing, replay, downgrade, key confusion, timing side-channels, weak randomness — no network handshake, no session replay surface, no crypto/key material anywhere in this diff.

Live tests actually run (not just reasoned about):
1. Compiled a standalone scratch crate (`serde 1` / `serde_json 1.0.151`, matching this workspace's pinned version) reproducing `DebugAdapterConfig`'s exact shape, and fed it 10 adversarial payloads (truncated JSON, `null`, wrong-typed fields, a 50,000-entry map, 200,000 unmatched `{`/`[` characters). Result: `ok=3 err=7 total=10`, no panic, no hang — every malformed case degraded to a normal `Err`.
2. Extended that harness with a `std::panic::catch_unwind` around parsing a payload with 200,000 levels of array nesting inside the `args` field (`debug_adapters.json`'s only place a deeply-recursive-parse stack-overflow could plausibly matter). Result: `parse error, no panic` — the deserializer rejects at the first type mismatch (`args` typed as `Vec<String>`, not `serde_json::Value`) without ever recursing into the nested structure, because the schema has no field of unconstrained/recursive type for an attacker to force deep recursion through.
3. Read `crates/dap/src/client.rs:135-155` (`spawn_child`) to confirm the actual subprocess invocation this diff's command/args ultimately reaches is `Command::new(command).args(args)` — no shell (`sh -c` or similar) anywhere in the call chain, confirmed by grep across `crates/dap/src/client.rs` and `crates/tui/src/app.rs`/`debug_panel.rs` for any `sh`/`Command::new("sh")`/shell-string formatting — none found.

## Findings

None met the bar for a reportable finding. The one item worth naming for completeness (raised in the review brief) is explicitly **not** a security finding:

- **Naive whitespace-splitting of the "Configure Debug Adapter" popup's args field** (`crates/tui/src/app.rs:1038`, `state.args.split_whitespace().map(str::to_string).collect()`). This means an argument containing a literal space (e.g. a path with a space in it) cannot be entered through this popup — a usability gap. It is not a security issue: there is no shell interpolation anywhere downstream (confirmed above), so there is no metacharacter/injection risk from the naive split — worst case is an argument gets split into two args instead of one, which `Command::new(command).args(args)` still passes as inert, non-shell-interpreted strings to the child process's argv. No fix required from a security standpoint; a usability follow-up is the implementing role's call, not this pass's.

## Path handling verification (detail)

- `toggle_breakpoint_at_caret` (`app.rs:822-833`): `path` comes from `buf.path`, the already-open tab's own path (opened through the existing, already-validated `open_location`/tree-open paths) — no new user-supplied path parsing.
- `breakpoint_line_ranges` (`app.rs:1055+`): keyed by the same already-open-tab `path`; the `HashMap<PathBuf, Vec<u32>>` lookup performs no filesystem access and no path construction, only line-number arithmetic (`dap_line.saturating_sub(1)` — no underflow, since it's `usize` sub past `saturating_sub`) resolved through `text_buffer.lines().line_range(...)`, which itself operates on the buffer already in memory.
- `open_stack_frame` (`app.rs:947-959`): consumes a `StackFrame::source` path that `ide-dap` already canonicalizes/validates against `project_root` (per `docs/features/debugger.md` §3.6, unchanged by this diff) before this call site ever sees it — this call only converts DAP's 1-based line/column to `ide_lsp::Position`'s 0-based convention and hands it to the pre-existing `open_location`, which applies its own (also pre-existing, already-audited) path handling.

No new path-traversal surface identified.

## Verdict

Clean.
