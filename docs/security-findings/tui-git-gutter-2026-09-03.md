# Security review: TUI Git Gutter (T30)

**Scope:** `crates/tui/src/git_gutter.rs` (new), `crates/tui/src/git_panel.rs`
(`hunks_for`/`gutter_marks_for`), `crates/tui/src/app.rs`
(`trigger_revert_hunk`, `git_gutter_popup_target`, `sync_git_gutter`).
Branch `rust-tui-dev/tui-git-gutter`, commit `ece78f9` (rev-approved, code
review round 2).

This is a TUI port of `docs/features/editor-git-gutter.md` (E7), covered by
`CLAUDE.md`'s security-sensitive-paths list under the generic "any
`*_gutter.rs` file... in either crate" wording, which specifically calls
out diff text among the repository-sourced content such files render.

## Attack-surface categories

- **Input validation (adversarial)** — applies. `revert_hunk_change`
  reconstructs pre-image text from `DiffLine::Context`/`Removed` entries
  (historical commit content, potentially adversarial) and splices it into
  the live, editable text buffer via an ordinary `Transaction`/
  `Buffer::apply` — a more consequential surface than blame's read-only
  popup. Live-tested below.
- **DoS / resource exhaustion** — applies. `marks_from_hunks`/
  `revert_hunk_change` run on every hunk in a diff; a maliciously large diff
  (e.g. from a cloned, untrusted repo) could in principle try to blow up
  CPU/memory. Live-tested below.
- **Path traversal** — applies to `hunks_for`/`gutter_marks_for`'s
  canonicalize+`strip_prefix` path handling. Code-analysis only (identical,
  already-audited pattern — see below).
- **Sandbox/privilege escalation, MITM, replay, downgrade, key confusion,
  timing, weak randomness, metadata leakage** — ruled out. This diff spawns
  no subprocess, does no crypto/key handling, and has no network/IPC
  protocol of its own (`GitRepo::diff_file` is local `git2`/libgit2 already
  audited in prior rounds).

**Live tests run:** a standalone scratch harness
(`/private/tmp/.../scratchpad/hacker-git-gutter`) linking directly against
the worktree's real `ide-core` crate, with `marks_from_hunks`/
`revert_hunk_change`'s function bodies copied verbatim from
`crates/tui/src/git_gutter.rs` (that module is private (`mod`, not
`pub mod`) in `lib.rs`, so an external harness can't link against it
directly — same technique used for the T29 `tui-blame` hacker pass) —
exercising the real `ide_core::text::TextBuffer`/`Change`/`DiffHunk`/
`DiffLine` types, not reimplemented stand-ins. Six live adversarial cases,
covered below. `hunks_for`/`gutter_marks_for`'s path handling and the
popup-staleness gate were reviewed by direct code reading and cross-checked
against the project's own already-merged, already-tested behavior (the
identical pattern `blame_for`/`show_working_tree_diff` already use), not
re-exercised live, since the pattern itself was already live-tested in
prior rounds (T11, T29) and this diff introduces no new logic there.

## Findings

1. **[InputValidation: Informational, not a vulnerability]**
   `crates/tui/src/git_gutter.rs`'s `revert_hunk_change` performs **no**
   sanitization of the historical commit text it reconstructs before
   handing it to `Transaction`/`Buffer::apply` — confirmed by reading the
   function (it does a plain `push_str`/`push('\n')` loop over
   `DiffLine::Context`/`Removed` text) and by live-testing:
   - An unterminated U+202E (RIGHT-TO-LEFT OVERRIDE) payload
     (`"\u{202E}evil(); //"`) round-tripped through `revert_hunk_change`
     byte-for-byte identical to the input.
   - A Trojan-Source-style mixed-isolate payload
     (`"if (access_level != \u{2066}\u{202E} \u{2069}) {"`) round-tripped
     verbatim.
   - NUL bytes and raw ANSI escape sequences (`\u{0000}`, `\u{001B}[31m`)
     round-tripped verbatim.
   - Zero-width-space and standalone combining-character-only content
     round-tripped verbatim.

   This is a real behavioral difference from `blame_gutter.rs`'s commit
   summary/body path, which **does** sanitize-then-truncate before
   rendering (`docs/security-findings/git-branches-and-blame-ui-
   2026-09-01.md`). However, the risk calculus is different, not merely
   unaddressed: blame's sanitization exists because that surface is a
   **read-only display** of commit metadata the user is implicitly asked
   to trust while reading it — bidi overrides there are a spoofing
   primitive (make displayed text lie about its own content/order).
   `revert_hunk_change`'s target is the **live, editable text buffer**,
   which already accepts arbitrary bidi/control/zero-width content from
   every other entry point this editor has — opening any file from disk,
   typing, pasting — with **zero** sanitization at any of them (confirmed:
   `crates/tui/src/ui.rs`, `crates/tui/src/highlight.rs`, and
   `crates/core/src/buffer.rs` contain no bidi-stripping logic of any
   kind). Stripping such content specifically on the revert-hunk path
   would be inconsistent (every other way to get the same bytes into the
   same buffer is left alone) and would actively break legitimate use
   (RTL-language text embedded in LTR source comments/strings uses these
   same override characters correctly). A user who opens a file already
   containing this exact payload gets identical buffer content today, with
   or without this feature.

   **Verdict on this finding: not a vulnerability, no fix required.** Flagging
   only because the "no sanitization" fact is real and worth having on
   record for anyone auditing this path later — the correct fix, if this
   editor ever wants to defend against maliciously-crafted file content in
   general, is a buffer-wide (not revert-specific) policy, which is out of
   this feature's scope.

2. **[DoS: Clean]** Constructed a single 10 MiB line and a 200,000-line
   (400,000 total `DiffLine`) synthetic hunk. `marks_from_hunks` and
   `revert_hunk_change` both completed in under 10ms in a debug build (sub-
   2ms in release) for every case — no hang, no panic, no disproportionate
   memory use (output size tracked input size linearly, as expected for a
   single linear segment-walk + string-concatenation algorithm with no
   nested loops over the full hunk). No DoS finding.

3. **[PathTraversal: Clean, code-analysis only]** `GitPanel::hunks_for`/
   `gutter_marks_for` (`crates/tui/src/git_panel.rs`) use the exact same
   `std::fs::canonicalize` + `strip_prefix(repo.workdir())` pattern already
   audited for `blame_for` (T29 hacker pass) and `show_working_tree_diff`
   (T11) — no new path-construction logic, no string concatenation, no new
   escape vector. Confirmed by direct diff read, not re-exercised live
   since the identical code shape was already live-tested in those prior
   rounds.

4. **[InputValidation/race: Clean, code-analysis only]** `trigger_revert_
   hunk`'s staleness gate (`git_gutter_popup_target`, comparing
   `git_gutter_path` against the active tab's current path before acting)
   cannot currently be raced through the real UI: `any_popup_open()`
   includes `git_gutter_popup_line.is_some()`, and both `handle_key`'s
   popup-precedence chain and `handle_mouse_click`'s early-return on
   `any_popup_open()` fully gate keyboard *and* mouse input while the popup
   is open — there is no reachable key or click that changes `active_tab`
   or triggers a `sync_git_gutter` recompute while the popup is showing.
   The gate is therefore defense-in-depth against a future caller/state
   change that doesn't yet exist, not a currently-exploitable race. Verified
   by reading `any_popup_open`'s full boolean chain and both gating sites in
   `crates/tui/src/app.rs`; the implementer's own test
   `trigger_revert_hunk_with_stale_marks_is_a_noop` exercises this gate
   directly via API state manipulation and passes.

## Verdict

**Clean.** No Critical/High/Medium/Low vulnerability found. Finding 1 is
recorded as informational context (a genuine, verified fact about the
code) rather than a defect requiring a fix — the "no sanitization" gap
already exists uniformly across every path that puts content into an
editable buffer in this editor, and narrowly patching just this one path
would be inconsistent without addressing the others, which is out of this
feature's scope.
