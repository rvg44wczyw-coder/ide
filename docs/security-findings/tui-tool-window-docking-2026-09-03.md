# Security review: T33 TUI Tool Window Docking

## Scope

Reviewed `crates/tui/src/app.rs`, `crates/tui/src/commands.rs`,
`crates/tui/src/ui.rs` as changed on `rust-tui-dev/tui-tool-window-docking`
(commit `0fcedcd`), against `main`. `hacker` was invoked here only because
CLAUDE.md unconditionally lists `crates/tui/src/docker_panel.rs`,
`crates/tui/src/k8s_panel.rs`, and `crates/tui/src/git_panel.rs` as
security-sensitive — `git diff --stat main..HEAD` confirms none of those
three files themselves changed in this diff. The change is a pure UI-
architecture refactor: five previously `Option<T>`-based full-screen modal
panels (Docker, Kubernetes, Cargo, Problems, and a new Git Log view) moved
into an always-alive-`T` left/bottom dock system, with visibility derived
from dock state instead of `Option::is_some()`.

**Attack-surface categories ruled out, with why:**
- Cryptography/auth, network protocol parsing, replay/downgrade/key
  confusion, weak randomness, MITM — no such surface exists anywhere in
  this crate; N/A.
- Sandbox/privilege escalation, subprocess argument construction — `docker_panel.rs`/`k8s_panel.rs`'s own command-construction code is
  byte-for-byte unchanged in this diff (confirmed via `git diff --stat`);
  nothing in `app.rs`/`ui.rs`'s changes touches how those subprocesses are
  invoked. N/A for this diff specifically (the files themselves remain in
  scope for a future diff that does touch them).
- Metadata leakage, timing side-channels — no secret/credential material
  flows through any of the changed code. N/A.

**Categories actually assessed:**
- **InputValidation / DoS via crafted key-event sequences** — the new
  dock-focus/visibility/resize state machine is reachable from arbitrary
  keyboard input; checked for panics, out-of-bounds indexing, and integer
  overflow.
- **InputValidation — untrusted git-repository content reaching the UI
  through a new code path** — the new `BottomDockTab::GitLog` view renders
  commit graph/diff data that CLAUDE.md's `crates/core/src/git/**` entry
  already flags as untrusted (attacker-controlled commit messages, branch
  names, diff text from a possibly-malicious cloned repo).

**Live tests actually run vs. code-analysis-only:** No live/black-box
harness was run against this specific diff. `ide-tui`'s `mod app;` is
private and `App` is not re-exported from `crates/tui/src/lib.rs` (no
`pub use`), so — unlike an `ide-core`-based harness used in a prior pass —
an external test crate cannot construct an `App` or call `handle_key` at
all without editing `lib.rs`, which is outside `hacker`'s "never edit
files other than this findings doc" rule. Substituted with targeted
code-analysis: traced every new/changed match arm and arithmetic
expression by hand rather than relying on the existing (already
rev-verified) unit-test suite alone.

## Findings

No findings. Verdict-relevant reasoning:

1. **DoS/InputValidation** — `handle_git_log_dock_key` (`app.rs:5108`+):
   `graph_selected` is only ever advanced via `if self.git_log_dock.graph_selected + 1 < self.git.graph.len() { ... += 1 }` (bounds-checked
   before increment, no overflow since both sides are `usize`) and only
   ever read via `self.git.graph.get(self.git_log_dock.graph_selected)`
   (`Option`-returning, never an indexing panic). `diff_scroll` uses
   `saturating_add`/`saturating_sub` throughout. `focus` toggles between
   exactly `Graph`/`Diff` via `_ => GitPanelFocus::Diff` — the dock never
   assigns `Conflicts`/`Filter` to this field, so the `unreachable!("handled
   above")` arms that exist elsewhere in this file for the *full* Git
   Panel's own `GitPanelFocus` match are never reachable through this new
   code path. No crafted key sequence can panic or corrupt state here.

2. **DoS/InputValidation** — `resize_focused_dock`/`clamp_pct`
   (`app.rs:1943`, `778`): `clamp_pct` widens `current: u16` and
   `delta: i16` to `i32` before adding, then clamps into range before
   narrowing back to `u16` — no overflow/underflow is possible regardless
   of how many `GrowFocusedDock`/`ShrinkFocusedDock` actions are fired in a
   row (verified the arithmetic directly; also covered by
   `grow_and_shrink_focused_dock_clamp_to_their_range`, which already
   drives 20 iterations past each bound).

3. **InputValidation (untrusted git content)** — the new `GitLog` dock tab
   (`ui.rs`'s `render_bottom_dock`, `BottomDockTab::GitLog` arm) does not
   introduce a new rendering path for commit/diff content: it constructs a
   synthetic `GitPanelState { view: GitPanelView::Log, .. }` and calls the
   existing `render_git_log_view` — the *same* function the full modal Git
   Panel already uses. Whatever sanitization or lack thereof already
   applies to commit messages/diff text in that shared function is
   unchanged by this diff; this refactor adds a second call site, not a
   second implementation. Nothing here regresses or bypasses existing
   handling.

4. **Noted but not a new issue** — `rev` (this same chain run) flagged
   that `self.git.selected_commit`/`graph`/diff cache is shared mutable
   state between the full modal Git Panel and the new `GitLog` dock tab,
   making the doc's "fully independent" claim inaccurate. This is a
   pre-existing data-sharing pattern (the full modal already worked this
   way before T33: `toggle_git_panel`'s own doc comment states
   `self.git`'s fields persist across the toggle) — T33 adds a second
   *reader/writer* of that cache, not a new *trust boundary*. Both
   consumers are trusted, same-process, single-user UI state; there is no
   scenario where one op's already-local, already-trusted git data
   becomes attacker-reachable data through this sharing that it wasn't
   already. Not a security finding — tracked as `rev`'s `[docs]` item
   instead.

## Verdict

Clean.
