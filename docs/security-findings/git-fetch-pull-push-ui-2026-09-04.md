# Hacker pass: git-fetch-pull-push, ide-ui layer (2026-09-04)

## Scope

Reviewed: `crates/ui/src/git_panel.rs` (`RemoteOpKind`/`RemoteOpProgress`/
`RemoteOpOutcome`/`RemoteOpPollResult`/`RemoteOpEvent`/`RemoteOpState`),
`crates/ui/src/app.rs` (`is_command_enabled`/`run_command`/`poll_remote_op`),
`crates/ui/src/app/render.rs` (`render_remote_op_toolbar`), `crates/ui/src/
command.rs` (three new commands), `crates/ui/src/main.rs` (startup
`configure_network_timeouts` call). Branch `rust-ui-dev/git-fetch-pull-push`,
commit `f6a1643`, just `rev`-approved.

This is on `CLAUDE.md`'s unconditional security-sensitive list because
`crates/ui/src/git_panel.rs` is named there. The actual network-transport
and wire-protocol-parsing surface (fetch/pull/push, panic hardening on
malformed remote data, `configure_network_timeouts`) lives in and was
already hacker-reviewed at the `ide-core` layer (see `docs/security-
findings/git-fetch-pull-push-2026-09-04.md`, three rounds, clean as of
round 3 / commit `b58cd69`). This pass is scoped specifically to what the
UI layer adds on top: the background-thread/channel wiring itself, and how
`ide-core`'s (already-established) attacker-controlled error strings get
surfaced into the actual UI.

**Attack-surface categories applied:**
- `InputValidation` — how untrusted remote-supplied text flows into egui.
- `DoS` — resource pile-up from repeated command invocation; render-cost
  from unbounded error text.
- Ruled out: MITM/Replay/Downgrade/KeyConfusion/Timing/WeakRandomness/
  SandboxEscape (none of this diff touches key handling, protocol
  negotiation, or subprocess/plugin execution — it only spawns a plain
  `std::thread` calling already-reviewed `ide-core` functions).
  `PathTraversal` — N/A, `project_root` comes from an already-open,
  already-validated `Project`, never touched or reconstructed by this
  diff.

**Live tests actually run** (all against instances I started myself on
`127.0.0.1`, cleaned up afterward):
1. A real `git daemon --enable=receive-pack` on loopback serving a bare
   repo with a malicious `pre-receive` hook, pushed to via `ide_core::git::
   GitRepo::push` — to check whether a hostile hook's custom rejection
   text reaches `GitError::PushRejected`.
2. A from-scratch, minimal git-receive-pack wire-protocol server (same
   technique `ide-core`'s own `push_with_a_non_utf8_rejection_reason_...`
   test already uses) on loopback, sending a crafted, **valid**-UTF-8 "ng"
   rejection line containing a Unicode right-to-left-override character
   and a 2000-byte tail, pushed to via `ide_core::git::GitRepo::push` from
   a standalone scratch harness (outside the repo, in my own scratchpad,
   depending on `ide-core` by path — `RemoteOpState` itself is private to
   the `ide_ui` crate and unreachable from outside it, so this is the
   deepest layer of the actual vulnerable data flow I could drive directly
   with a controlled payload; the remaining hop, `ide_core::GitError::
   to_string()` → `RemoteOpState.error` → `ui.colored_label`, is a
   direct, unconditional string copy verified by reading the code, not
   independently re-derivable by a live test without editing the target
   crate).
3. Code-tracing/manual reasoning for the `RemoteOpState::start`/`poll`/
   `is_running` concurrency model (confirmed single-threaded-caller
   invariant against `eframe`'s execution model) and for the project-
   switch race described in finding 3.

No process was left running; the git daemon (port 9723) and all temp
directories were torn down at the end of the pass.

## Findings

### 1. [InputValidation, Low-Medium] Attacker-controlled push-rejection text renders unsanitized and unbounded in the Git Panel toolbar

**Location:** `crates/ui/src/app/render.rs`, `render_remote_op_toolbar`
(`ui.colored_label(self.theme.tokens().color.warning, &err)`), fed by
`crates/ui/src/git_panel.rs`'s `RemoteOpState::start`/`poll`
(`Err(e) => e.to_string()` → `self.error`), fed in turn by `ide_core::
git::GitRepo::push`'s `GitError::PushRejected(String)` (`crates/core/src/
git/mod.rs:1264-1272`), whose payload is the *verbatim* rejection-reason
string the remote's `push_update_reference` status line supplies.

**Attack scenario:** A user adds (or is socially engineered/supply-chain'd
into adding) a git remote pointing at a server the attacker controls, or
an attacker performs a downgrade/MITM to an unencrypted `git://` transport
and substitutes their own responder (this project's own `configure_network_
timeouts` doc and `ide-core`'s `git-fetch-pull-push-2026-09-04.md` findings
doc already establish "malicious/misbehaving remote" as squarely in scope
for this feature). Such a server does not need to run real `git`/hooks at
all — it can implement just enough of the smart `git://` wire protocol to
answer the client's push with a crafted "ng &lt;ref&gt; &lt;reason&gt;"
line, where `&lt;reason&gt;` is any valid-UTF-8 byte string of the
attacker's choosing (up to the protocol's own pkt-line framing limit,
~64KB). Clicking **Push** in the GUI against such a remote renders that
entire string, completely unmodified, in the Git Panel:
- **Bidi/visual spoofing**: the string can embed Unicode bidi-override
  control characters (e.g. U+202E) to visually reverse or otherwise
  scramble the displayed text, the same "Trojan Source" class of attack
  this very file already defends against for commit metadata (`strip_
  bidi_controls`, used for `detail.summary`/`.body`/`.author`/`.email` at
  `git_panel.rs:944-957`) but not applied here.
- **Unbounded render cost**: nothing truncates the string before it
  reaches `egui::Ui::colored_label`, unlike the same file's `truncate_
  display` precedent for the same kind of untrusted content. A remote
  can return a message close to the wire protocol's own per-line cap on
  every rejected push, and `render_remote_op_toolbar` re-`.clone()`s and
  re-renders that string every single frame while it's displayed (no
  caching, no length cap), which is a minor but real repeated per-frame
  cost/visual-layout blowup from a single hostile response, not from a
  flood of requests.

**Verified live:** test #2 above. Captured output from the crafted server
(reason = a bidi-override character plus a 2000-`'A'` tail, all valid
UTF-8):

```
RENDERED_ERROR_START>>>push rejected by remote: SECURITY ALERT: your
credentials have expired ‮pinif dna won retnE‮ click here:
evil.example/reset AAAA...(2000 more)...<<<RENDERED_ERROR_END
byte_len=2128
contains_U+202E=true
```

Also verified (test #1) that a *real* git server's stock `pre-receive`
hook can **not** reach this field — git's own `receive-pack` reports a
fixed `"pre-receive hook declined"` string regardless of what the hook
writes to stderr, confirming the practical threat model is specifically a
non-standard/custom server implementation (or a MITM'd/attacker-run
endpoint), not "any repo host with a hook," and matching `ide-core`'s own
already-documented threat model for this exact code path (its doc comment
at `crates/core/src/git/mod.rs:1194` frames the rejection-reason field as
attacker-controlled precisely because it bypasses real git/hooks).

**Suggested fix direction:** apply the same `strip_bidi_controls`/
`truncate_display` treatment `git_panel.rs` already uses for commit
metadata to `remote_op.error` before rendering — either at the point
`RemoteOpState::poll` sets `self.error`, or in `render_remote_op_toolbar`
right before the `colored_label` call. This is a UI-layer-only fix
(`ide-core`'s `GitError` strings themselves stay as-is); no `ide-core`
change needed.

### 2. [InputValidation, Low] Same unsanitized-text sink applies to fetch's `GitError::Git2` messages, not just push rejections

**Location:** same `render_remote_op_toolbar` sink as finding 1.
`RemoteOpKind::Fetch`'s failure path (`git_panel.rs`'s `RemoteOpState::
start`) returns `Err(GitError::Git2(e))` for any libgit2-level fetch
failure whenever the fetch doesn't hit the `FetchFailed` panic-recovery
path; `git2::Error::message()` can itself echo raw bytes taken from the
wire (e.g. a server's `ERR` packet-line text, or other diagnostic strings
libgit2 surfaces from the remote side of the negotiation) rather than
being a purely local/internal string in every case.

**Verified:** code-analysis only — I did not build a second from-scratch
fetch-side wire-protocol server to independently confirm an `ERR`-line
payload reaches `Error::message()` unmodified (finding 1's push-side test
already demonstrates the same `RemoteOpState`→`render_remote_op_toolbar`
sink is unsanitized regardless of which `GitError` variant feeds it, so a
second live repro on the fetch side would confirm the same sink again, not
a materially different code path).

**Suggested fix direction:** the same fix as finding 1, applied once at
the shared sink, covers both.

### 3. [Low, logic/state-confusion — not cleanly one of the standard attack classes] Completing a Fetch/Pull/Push after the user has switched to a different project applies the result to the wrong project's UI state

**Location:** `crates/ui/src/app.rs`, `poll_remote_op` (`crates/ui/src/
app.rs:~1666-1699`).

**Scenario:** `RemoteOpState::start` correctly captures `project_root` by
value at the moment the user clicks Fetch/Pull/Push, so the actual
network operation and any on-disk merge/conflict state always land in the
*originally* targeted project's repository — that part is sound. But
`poll_remote_op`'s completion handler reads `self.project.as_ref()...
root()` — the **currently** open project, evaluated when the background
thread's result is drained, not when the operation was started. If the
user opens a different project (`load_project` reassigns `self.project`,
with no gate against doing so while `self.git.remote_op.is_running()`)
before the in-flight operation completes, the eventual `Done` result is
applied against the *new* project's root: `self.git.refresh(&new_root)`
for Fetch/Push (harmless — just an extra, redundant refresh of whatever
project is actually open), but for Pull's `Merged(MergeOutcome::Conflicts(_))`
outcome, `apply_merge_outcome(&new_root, ...)` sets `self.merging = true`
and pre-fills `commit_message` with wording built from `self.git.
current_branch` (by then already refreshed to the *new* project's branch
name by `load_project`'s own refresh), while the actual conflicted
files/`MERGE_HEAD` were written to the *old* project's working tree on
disk. The user is left looking at a "resolve merge conflict" prompt for a
conflict that doesn't exist in the project currently open, while the old
project silently sits mid-merge with no on-screen indication.

**Why this is worth flagging even though no external attacker forces the
project switch:** a malicious/stalling remote controls how *long* the
window for this race stays open (this feature's own `configure_network_
timeouts` allows up to 30s of I/O stall before erroring), so a hostile
remote can deliberately maximize the chance of the user navigating away
mid-operation. This isn't independently live-testable without adding a
test to the crate (forbidden under this skill's rules, and `RemoteOpState`
is private to `ide_ui` so an external harness can't drive it either) —
confirmed via direct code tracing of `poll_remote_op`/`apply_merge_
outcome`/`load_project` instead.

**Suggested fix direction:** either (a) gate project-switching (`load_
project`'s caller) on `!self.git.remote_op.is_running()`, surfacing a
"finish or cancel the current git operation first" message, or (b) have
`RemoteOpState::start` capture enough identity (e.g. the `project_root`
itself, already captured) and have `poll_remote_op` compare it against the
*current* `self.project` root before applying any UI-state side effect,
discarding (with perhaps a toast/log, not silence) a stale result whose
project no longer matches.

## Verdict

Findings (highest severity: Medium — finding 1's Low-Medium is the
ceiling; nothing Critical/High).
