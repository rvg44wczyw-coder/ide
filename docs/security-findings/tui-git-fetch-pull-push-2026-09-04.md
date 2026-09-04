# Hacker pass: `ide-tui` Fetch/Pull/Push (rust-tui-dev, git-fetch-pull-push.md §2.3)

## Scope

Reviewed: worktree `/Users/ivs/rust/ide-worktrees/rust-tui-dev-git-fetch-pull-push`,
branch `rust-tui-dev/git-fetch-pull-push`, commit `05fc3a0`, just `rev`-approved.
This is `rust-tui-dev`'s port of `ide-ui`'s already-hacker-tested `RemoteOpState`
Fetch/Pull/Push implementation into `crates/tui/src/git_panel.rs`, plus the
`commands.rs`/`app.rs`/`lib.rs`/`ui.rs` wiring around it. `git diff --name-only
main...rust-tui-dev/git-fetch-pull-push` confirms the diff is confined to
`crates/tui/**` — `crates/core/**` (including `GitRepo::push`/`fetch`, the
`catch_unwind`/`MAX_ADVERTISED_REFS` fixes from the earlier `rust-core-dev`
hacker round) is untouched by this role.

Two earlier hacker rounds already ran against `ide-ui`'s identical code shape
(`docs/security-findings/git-fetch-pull-push-ui-2026-09-04.md`) and found (both
fixed, merged into `main` before this worktree forked):

1. `RemoteOpState.error` could carry a hostile remote's attacker-controlled,
   valid-UTF-8 push-rejection text (a bidi-override control character, an
   unbounded tail) straight into the rendered UI.
2. A finished remote op's result could be misapplied after a mid-flight
   project switch (`ide-ui` only, via `App`'s Project menu).

Attack-surface categories applicable here: **InputValidation** (untrusted
remote-supplied rejection text reaching the TUI's rendered status/notification
text) and a **logic/state-confusion** class matching finding 2 above, assessed
for applicability to this crate's different architecture. Ruled out as N/A:
MITM/Replay/Downgrade/KeyConfusion (no new protocol negotiation — this port
reuses `ide-core::git::GitRepo::fetch/pull/push` verbatim, unchanged),
Timing/WeakRandomness (no crypto/token material here), SandboxEscape (no new
subprocess/plugin surface — `GitRepo::open` opens a repo handle, not a
subprocess), Metadata leakage (no new argv/env exposure).

Live tests actually run (not just code-analysis):

- Ran the existing regression test
  (`git_panel::tests::remote_op_error_from_a_malicious_push_rejection_is_sanitized_and_bounded`)
  fresh, myself, on this exact commit — passed.
- Built a **new**, independent from-scratch git-receive-pack pkt-line wire
  server (a standalone external Cargo project outside the repo, path-depending
  on this worktree's `ide-core`, deleted after the run) with a payload
  different from the existing test's: `U+2066`/`U+2069` (LEFT-TO-RIGHT
  ISOLATE / POP DIRECTIONAL ISOLATE — a different bidi-control subclass than
  the existing test's `U+202E` override), `U+200B` (ZERO WIDTH SPACE, not a
  bidi control at all), and a 600-repeat multi-byte (3-byte CJK) tail —
  pushed against a real local repo via `ide_core::git::GitRepo::push` and
  captured the raw, pre-sanitization `GitError::to_string()` output directly
  (see Finding 1 discussion below for the result).
- Built a second standalone probe (ratatui `Layout::split`, no repo
  dependency) exercising the exact `[Constraint::Length(1), Constraint::Min(3)]`
  split this diff adds to `render_git_panel`, across `Rect`s from `0x0` up to
  `20x4`, wrapped in `catch_unwind` — no panic for any size, confirming the
  layout-carving change is safe on degenerate/tiny terminal sizes.
- Code-analysis-only: `strip_bidi_controls`/`truncate_display`
  (`crates/tui/src/blame_gutter.rs`) themselves — confirmed via `git diff
  --stat` that this diff does not touch that file at all (it's pre-existing,
  already independently tested infrastructure this role only calls into), and
  read the exact character-class filter and the `.chars().take(n)`
  truncation logic directly rather than re-deriving char-boundary safety via
  a new live test.
- Code-analysis + targeted `grep`: independently verified (not just trusting
  `rev`'s account) that `App::project_root` in `crates/tui/src/app.rs` is set
  exactly once (`App::new`, line 795/835 depending on count) and never
  reassigned — `grep -n "self\.project_root *="` returns zero matches, and
  the one other site that touches it (`refresh_tree`, line 1471,
  `Project::open(&self.project_root)`) re-opens the *same* root purely to
  rescan the directory tree, never changing which path it points at.
  `App::new` itself has exactly one production call site
  (`crates/tui/src/lib.rs:116`, this process's own startup), confirming there
  is no "reconstruct `App` against a different project" path anywhere in this
  binary.

Not re-run live: a real `git daemon --enable=receive-pack` + `pre-receive`
hook test proving stock git/hooks can't inject custom text into this field.
Judged redundant rather than skipped out of laziness — `GitRepo::push`
(`crates/core/src/git/mod.rs`) is byte-for-byte unchanged by this diff (it's
outside `crates/tui/**` entirely), so the "can real git reach this field with
attacker-chosen text" question is a property of `ide-core`'s already-hardened
push path and real git's own hardcoded rejection strings, not of anything
`rust-tui-dev` touched. That property was already established live in the
`ide-ui` hacker round using the identical `GitRepo::push` call.

## Findings

None.

**Finding 1 candidate, ruled not a finding: isolate-class bidi controls and
zero-width space.** The new probe's raw `GitError::to_string()` output
(captured live) was:

```
push rejected by remote: safe-looking\u{2066}REVERSED\u{2069}\u{200b}-hidden-zwsp 日日日...(600×)
```

i.e. the raw, pre-sanitization string genuinely carries both the isolate
pair and the ZWSP through to `RemoteOpState::start`'s `.map_err(|e|
e.to_string())` — confirming the injection point is real and reachable, same
as the `ide-ui` round already established. But `strip_bidi_controls`'s filter
(`crates/tui/src/blame_gutter.rs:74-83`) is:

```rust
!matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
```

`\u{2066}'..='\u{2069}'` explicitly covers the isolate range this probe used
— so this payload is caught by the existing mitigation, not a gap. `\u{200B}`
(ZWSP) is *not* stripped, but ZWSP has no directional-rendering effect (it
isn't part of the Unicode bidi algorithm's control-character set at all,
unlike the embedding/override/isolate/mark characters `strip_bidi_controls`
targets) — it cannot be used for the "Trojan Source"-style visual-order
spoofing this mitigation exists to prevent, only for an invisible-character
nuisance (e.g. splitting a word for a naive string search), which is out of
this mitigation's stated scope and matches `ide-ui`'s identical, already-
accepted implementation verbatim (this function is shared/pre-existing
infrastructure, not new code from this role). Not filing as a finding.

**`MAX_REMOTE_OP_ERROR_CHARS` truncation on the multi-byte tail**: the probe's
636-char payload (600 3-byte CJK characters) exceeds the 500-char cap.
`truncate_display`'s `s.chars().take(max_chars.saturating_sub(1)).collect()`
operates per `char` (Unicode scalar value), which by construction can never
land mid-codepoint regardless of UTF-8 byte width — confirmed by reading the
implementation directly; `crates/tui/src/blame_gutter.rs`'s own existing test
suite (`truncate_display_is_char_boundary_safe_on_multi_byte_text`) already
covers this generically. No panic risk.

**Layout-carving panic risk (`render_git_panel`'s new
`Constraint::Length(1)`/`Constraint::Min(3)` split)**: live-tested via a
standalone `ratatui::layout::Layout::split` probe across `Rect` sizes from
`0×0` through `20×4` (every combination), wrapped in `catch_unwind` — no
panic in any case; ratatui's Cassowary-based solver degrades constraints
gracefully on undersized areas rather than panicking. No finding.

**`started_for` omission**: independently confirmed sound — see Scope's
live-test bullet above. No code path reassigns `self.project_root` or
reconstructs `App` against a different root while a remote op could be
in flight.

## Verdict

Clean.
