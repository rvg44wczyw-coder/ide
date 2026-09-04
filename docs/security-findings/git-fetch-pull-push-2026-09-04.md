# Adversarial security pass — `GitRepo::fetch`/`pull`/`push`

**Scope:** `crates/core/src/git/mod.rs`'s new `fetch`/`pull`/`push`/`merge_oid`/
`current_branch_name`/`credential_callback` code, worktree
`/Users/ivs/rust/ide-worktrees/rust-core-dev-git-fetch-pull-push`, branch
`rust-core-dev/git-fetch-pull-push`, commit `e2357c1`. Checked against
`docs/features/git-fetch-pull-push.md`, which had already been `rev`-approved
for both docs and code.

Attack-surface categories from the skill's checklist, applied vs. ruled out:

- **Input validation (adversarial)** — applies directly: `fetch`/`push` parse
  data supplied by whatever the configured remote sends over the wire
  (ref advertisements, push-rejection status text). Live-tested.
- **DoS / resource exhaustion** — applies: `fetch`'s ref-list handling has no
  client-side cap on what a remote can advertise. Live-tested.
- **MITM / identity spoofing** — **not re-tested from scratch.** `rev`'s
  code-review diff confirmed `credential_callback` is a byte-for-byte
  extraction of `clone_repo`'s original inline closure (verified against
  `ac9e7a9`'s pre-refactor body), and `fetch`/`push` never set
  `RemoteCallbacks::certificate_check` (grep-confirmed, zero occurrences in
  the file). `docs/features/git-remote.md`'s own prior `hacker` pass on
  `clone_repo` already exercised this exact code path with a MITM/spoofed-
  cert harness. No new surface here — ruling out re-running that harness.
- **Metadata leakage (path disclosure)** — checked: all four new `GitError`
  variants' `#[error(...)]` formats (`RemoteNotFound`, `DetachedHead`,
  `NoUpstream`, `PushRejected`) carry only a remote name (user-configured,
  not attacker data) or the remote's own rejection text — never
  `self.workdir` or any local filesystem path. Code-analysis only (no live
  test needed — the format strings are static and inspectable directly).
- **Replay / Downgrade / KeyConfusion / Timing / WeakRandomness /
  SandboxEscape** — N/A. No stateful session/protocol version negotiation
  this module owns, no key derivation, no timing-sensitive comparison, no
  RNG use, no subprocess/plugin execution anywhere in this diff (pure
  `git2`/libgit2 library calls, no `Command::new` anywhere in this file).

Live tests actually run (all against instances started by this pass, on
`127.0.0.1`, from inside this worktree):

1. A loopback `git daemon --enable=receive-pack` serving a bare repo with an
   adversarial `pre-receive` hook, to see whether a hostile hook's own
   output reaches `PushRejected`'s payload. (Result: no — see Finding 3's
   discussion; real git's report-status protocol uses a fixed
   `"pre-receive hook declined"` string regardless of hook output, so this
   vector doesn't reach the code under test at all.)
2. A hand-rolled minimal git-wire-protocol server (`fake_git_server.py`,
   plain Python `socket`, no real git) speaking just enough of
   git-receive-pack's ref-advertisement/report-status framing to send a
   **fully attacker-controlled** rejection reason string back through
   `push_update_reference` — the actual malicious-server scenario the
   feature doc's §6 asked for. Used to confirm Finding 1.
3. A second hand-rolled server (`fake_upload_pack_server.py`) advertising
   300,000 and then 2,000,000 fake refs during the git-upload-pack
   ref-advertisement phase, timing/measuring `GitRepo::fetch`'s
   `/usr/bin/time -l` wall-clock and peak-RSS cost. Used to confirm
   Finding 2.

All harnesses and their PIDs were torn down at the end of the pass; verified
no orphaned process remains on ports 9799/9800/9801.

## Findings

### 1. `[security: Medium]` `InputValidation` — a malicious/misbehaving remote's push-rejection message crashes the calling thread instead of producing `PushRejected`

**Location:** `crates/core/src/git/mod.rs`, `GitRepo::push`
(`callbacks.push_update_reference(...)`, ~line 1164), via
`git2-0.21.0/src/remote_callbacks.rs:455`
(`str::from_utf8(CStr::from_ptr(status).to_bytes()).unwrap()` inside
`push_update_reference_cb`, the C-callback trampoline `push_update_reference`
registers into).

**Attack scenario:** a remote git server `origin` points at — malicious, or a
legitimate server compromised, MITM'd, or simply misconfigured to emit a
rejection reason in a non-UTF-8 encoding (e.g. a locale-specific error
message, or a proxy/hook mangling the response) — sends a `report-status`
`ng <ref> <reason>` line whose `<reason>` bytes are not valid UTF-8. Real
`git-receive-pack` never does this on its own initiative (its own hook-
rejection reason strings are fixed, ASCII, e.g. `"pre-receive hook
declined"` — confirmed via test #1 above, a hostile *hook* cannot reach this
field at all through stock git). A server that speaks the wire protocol
directly — not necessarily "hostile" in intent, could be a buggy or
non-standard git-hosting implementation — can.

**Verified live:** test #2's fake server sent
`"ng refs/heads/master \x1b[31mFAKE\x1b[0m\xff\xfe\xfd\rAAAA…"` (ANSI escape,
then three invalid-UTF-8 lead/continuation bytes, no embedded NUL). Running
the in-worktree `ide-core` crate's `GitRepo::push` against it produced:

```
thread 'main' (31124436) panicked at .../git2-0.21.0/src/remote_callbacks.rs:455:68:
called `Result::unwrap()` on an `Err` value: Utf8Error { valid_up_to: 13, error_len: Some(1) }
```

— a genuine Rust panic, not a returned `Err(GitError::PushRejected(_))`. (A
second run with the same payload but an embedded NUL byte earlier in the
string was silently truncated by `CStr::from_ptr`'s own NUL-termination
*before* reaching the invalid bytes — that path is safe; the panic only
fires when invalid UTF-8 bytes appear with no preceding NUL.)

**Impact:** this is upstream `git2`-rs's own defect (the `.unwrap()` in its
FFI trampoline, not code this diff wrote), but `GitRepo::push` has no way to
avoid triggering it — libgit2 hands `git2`-rs the raw bytes, and `git2`-rs
panics before `push`'s own code ever sees a `Result`. The panic is caught by
`git2`-rs's own `panic::wrap`/`catch_unwind` at the FFI boundary (so this is
memory-safe — no UB, no unwind across the `extern "C"` boundary) but is then
*re-raised* (`resume_unwind`) once control returns to safe Rust, meaning it
propagates out of `GitRepo::push` as a real panic to whatever thread called
it. Per this doc's own §3.1 threading design, that's the dedicated
background thread `RemoteOpState::start` spawns — so the panic is contained
to that one thread (not the whole app) *if* the UI/TUI roles' completion
handling actually notices the thread died. Nothing in this doc's §2.2/§2.3
currently accounts for the mpsc `Sender` simply never sending `Done` because
its thread panicked instead of returning — every future `RemoteOpState::
poll()` call would see the channel disconnected with no `Done` variant ever
delivered, which (depending on how `ui`/`tui` implement the "disconnected
with no Done" case) most likely means `is_running()` stays `true` forever,
requiring an app restart to push/fetch/pull again.

**Suggested fix direction:** wrap the `remote.push(...)` call (or, more
narrowly, just the closure passed to `push_update_reference`) in
`std::panic::catch_unwind` inside `GitRepo::push`, converting a caught panic
into `GitError::PushRejected("<non-UTF-8 rejection reason from remote>"
.to_string())` or a new dedicated error variant — so a hostile/broken
remote can only ever produce a `Result::Err` from this public API, never an
unrecovered panic. (Requires `AssertUnwindSafe` around the closure capturing
`&rejection`/`&mut on_progress`, since neither `RefCell` nor `&mut
impl FnMut` is `UnwindSafe` by default — reasonable here since a poisoned
`RefCell`/interrupted callback state is discarded immediately after, not
reused.) This is `ide-core`'s responsibility regardless of what `ui`/`tui`
do downstream, since a public library function silently panicking on
attacker-influenced network input is exactly the class of bug `rev`'s own
"Errors returned to callers... don't expose internal state" checklist item
is guarding against, just manifesting as "doesn't return an error at all"
rather than "leaks something in the error."

### 2. `[security: Medium]` `DoS` — `fetch` has no cap on the number of refs a remote can advertise

**Location:** `crates/core/src/git/mod.rs`, `GitRepo::fetch`
(`remote.fetch(&[] as &[&str], Some(&mut fetch_options), None)`, ~line 1088).

**Attack scenario:** the same `origin` remote (malicious, compromised, or
just misbehaving) advertises an enormous number of refs during the
git-upload-pack ref-advertisement phase, before any pack data transfer even
begins. `fetch` has no client-side limit on how many refs it will accept
into its ref-advertisement parse — unlike every other unbounded-input path
this same module already caps (`MAX_DIFF_FILES`, `MAX_COMMITS_SCANNED`,
`MAX_BLAME_LINES`), there is no `MAX_*` constant governing this one.

**Verified live:** test #3's fake `git-upload-pack` server, run twice:

| Refs advertised | Wall time (`GitRepo::fetch` call) | Peak RSS |
|---|---|---|
| 300,000 | 5.57s | ~65 MB |
| 2,000,000 | 37.13s | ~397 MB |

(the server then drops the connection, so both runs end in a `Broken pipe`
`Err` — the cost is entirely in *parsing/storing the advertisement*, not in
transferring any pack data, and scales roughly linearly with ref count in
both time and memory.) Extrapolating linearly, an advertisement in the tens
of millions of refs — trivial for a malicious server to generate, since it
never has to correspond to real objects — would tie up the calling thread
for several minutes and consume multiple GB of memory, with **no
cancellation mechanism** anywhere in `GitRepo::fetch`'s signature or this
doc's `RemoteOpState` design (§2.2/§2.3) for a user to abort an in-flight
fetch once started.

**Impact:** bounded to the same background thread `RemoteOpState::start`
spawns (not the whole process, given the doc's threading design), and
recoverable by killing the app — but a user who points the IDE at (or is
redirected/MITM'd/typo-squatted to) a malicious host has no way to abort a
multi-minute, multi-GB fetch attempt from within the IDE itself once
`fetch()` has been called. This is exactly the "cheaply exhaust memory/CPU
with malformed/flood input" scenario the skill's own DoS checklist item
describes, and precisely what the feature doc's own §6 anticipated
("a fetch against a malicious local server that advertises an enormous or
malformed ref list") — confirming the concern was warranted, not just
theoretical.

**Suggested fix direction:** the more targeted option is a ref-count cap
enforced via `RemoteCallbacks::update_tips` (called once per advertised/
updated ref during `fetch`'s negotiation) — counting refs seen and
returning `false` (abort) past some `MAX_ADVERTISED_REFS`-style constant,
the same "bound the axis nothing else bounds" pattern `MAX_DIFF_FILES`/
`MAX_COMMITS_SCANNED` already establish in this file. A coarser
alternative — an overall wall-clock timeout wrapping the `remote.fetch(...)`
call — would also contain the worst case but wouldn't distinguish "slow
network, legitimate large repo" from "malicious ref-list flood" as cleanly.

## Verdict

Findings, highest severity Medium. Both are real, live-reproduced, and
within this diff's own declared scope (fetch/push's handling of
remote-supplied data) — neither is a memory-safety or RCE issue, and both
are contained to the single background thread this doc's own threading
design already isolates, but both leave the calling code with no clean way
to recover (a silent panic instead of an `Err`; an uncancellable multi-
minute/GB-scale parse) rather than the graceful, bounded failure this
module's own established conventions (`MAX_DIFF_FILES` etc., and every
other `Result`-returning method in this file) otherwise provide throughout.

## Fix-round update (2026-09-04, same day)

**Finding 1 — fully fixed.** `GitRepo::push` now wraps the `remote.push(...)`
call in `std::panic::catch_unwind`; a caught panic converts to
`Err(GitError::PushRejected("remote sent a non-UTF-8 or otherwise malformed
rejection reason".to_string()))`. Regression test
`push_with_a_non_utf8_rejection_reason_from_the_remote_is_an_err_not_a_panic`
reproduces the exact live attack from this doc (a from-scratch, minimal
git-wire-protocol server on a local `TcpListener`, not real `git`) and
asserts `Err(PushRejected(_))` instead of an unwind. Verified: `cargo test`
passes this test; before the fix, the same scenario produced a real
`panic!` (this doc's own live-test transcript above).

**Finding 2 — only partially fixable; the originally-proposed fix direction
was empirically wrong and has been corrected.** This doc's own "suggested
fix direction" above proposed capping via `RemoteCallbacks::update_tips`.
While implementing it, a direct probe (`update_tips` registered, counting
invocations, against the same 300k-ref adversarial server used above)
showed **zero `update_tips` invocations** despite the same ~5s parsing cost
being paid before the connection failed. `update_tips` fires only while
*applying already-negotiated updates to local tracking refs* — a step that
happens **after** the ref advertisement has already been fully received and
parsed, not during it. Re-verified with the single-`sendall()` version of
the attack server (ruling out "the Python test server's own per-line-send
overhead was the real cost," not "libgit2's parsing"): 2,000,000 refs sent
in one 141MB write (server-side send time: 1.03s) still cost `fetch` ~26s
wall-clock and ~397MB peak RSS client-side, confirming the cost is genuinely
in `git2`/libgit2's own advertisement handling, not measurement artifact.

No hook in `git2` 0.21.0's public API fires during ref-advertisement
parsing itself (confirmed by grepping `git2-rs`'s `RemoteCallbacks`/
`Remote`/`FetchOptions` surface for anything ref-count/depth/limit-shaped —
`depth` only controls shallow-clone history depth, unrelated). Closing this
gap would require patching libgit2 or implementing a custom smart
subtransport — out of scope for this fix round. `MAX_ADVERTISED_REFS` +
`update_tips` were still implemented and kept (see `GitRepo::fetch_capped`)
because they *do* genuinely protect a real, narrower case — a fetch that
clears the advertisement phase and negotiates a pack transfer, then tries
to apply an unreasonable number of local ref updates — verified via
`fetch_capped_aborts_once_more_refs_update_than_the_cap_allows`, which
exercises the real `update_tips` abort wiring (not a mock) against branches
that must be genuinely re-fetched. The advertisement-parsing cost itself
remains **an open, upstream-level limitation**, documented in
`MAX_ADVERTISED_REFS`'s own doc comment and in
`docs/features/git-fetch-pull-push.md`'s Revision notes rather than silently
left for a later hacker pass to rediscover.

This residual gap is flagged to the user directly rather than presented as
resolved — the dev-chain fix-loop was applied for what's actually fixable,
and the honest limit of what this dependency's public API allows is
recorded here instead of overclaiming a fix.

## Follow-up investigation (same day, at the user's request)

Asked to investigate a custom mitigation for finding 2 further rather than
accept the residual gap outright. Found and verified a genuine, narrower
improvement — not a fix for the original flood scenario, but a real fix for
a *related, previously completely unmitigated* attack this pass hadn't
originally tested: a remote that accepts the connection and then simply
never sends anything.

**Live-tested:** `git2` 0.21.0 exposes `git2::opts::
set_server_connect_timeout_in_milliseconds`/`set_server_timeout_in_
milliseconds`, wrapping libgit2's `GIT_OPT_SET_SERVER_TIMEOUT`/
`GIT_OPT_SET_SERVER_CONNECT_TIMEOUT` (process-global, not thread-safe to
change concurrently with other in-flight `git2` calls, per libgit2's own
docs). Before any timeout is configured, `GitRepo::fetch` against a
stalling server (a `TcpListener` that accepts and then never writes)
**blocks indefinitely** — confirmed by letting it run well past the point
any legitimate operation would have completed, with no way to recover
short of killing the process. With a 3-second timeout configured, the
identical scenario returned `Err("could not read from socket: timed out")`
at **exactly** 3.00s elapsed.

**Also confirmed the obvious follow-up question rather than assuming**:
does this same timeout help against the original fast-flood attack
(finding 2's own 2,000,000-fake-ref scenario)? No — re-run with the
identical 3-second timeout configured, the flood attack still took its
full ~26s/~400MB, unaffected. This makes sense in hindsight: the flood
server delivers its entire payload in one continuous burst (already sitting
in the OS's TCP receive buffer), so no individual `read()` call libgit2
makes ever blocks long enough to trip a per-read timeout — the cost there
is CPU-bound parsing between reads, not I/O wait, and a read/write timeout
mechanism structurally cannot address a cost that isn't I/O-wait-shaped.

**Implemented:** `ide_core::git::configure_network_timeouts(connect_ms,
io_ms)` (`unsafe fn`, process-wide, call-once-at-startup by design — see
its own doc comment for the full safety contract). This does not change
`fetch`/`pull`/`push`'s own signatures or behavior; it's a new required
startup integration point documented in `docs/features/
git-fetch-pull-push.md` for whichever role wires up `ide-ui`/`ide-tui`'s
`main.rs` next. Regression test
`configure_network_timeouts_bounds_a_stalling_remote` reproduces the
stalling-server scenario and asserts the call returns within the
configured window rather than hanging.

**Updated verdict on finding 2**: the fast-flood/advertisement-parsing cost
remains open and unfixable within `git2` 0.21.0's public API (unchanged
from the fix-round update above) — this was investigated further and
confirmed, not newly discovered to be fixable. What changed is that a
*different*, previously fully-unmitigated attack in the same neighborhood
(indefinite stall) now has a real, verified fix. Recommending the user
accept the flood-cost gap as documented (open, upstream-level, tracked in
`MAX_ADVERTISED_REFS`'s doc comment) while taking credit for the stall fix
as a genuine improvement from this follow-up.

## Round 2 — re-verification pass (2026-09-04, same day), commit `925f3cb`

Re-ran the fix round's own claims live rather than trusting the unit tests
at face value, and probed the fix round's own new surface (`update_tips`,
`configure_network_timeouts`) for anything the narrower scope might have
missed.

**Re-confirmed live, unchanged:**
- Finding 1's fix: `cargo test -p ide-core --lib git::` (150/150 green)
  re-triggers `push_with_a_non_utf8_rejection_reason_from_the_remote_is_an_
  err_not_a_panic`'s real TCP server; the panic still occurs deep inside
  `git2`-rs's FFI trampoline exactly as before, and is still caught and
  converted to `Err(PushRejected(_))` rather than propagating — confirmed
  by the test passing.
- Task 5 (flood-cost characterization): not re-run live this round (no
  code in the advertisement-parsing path changed since the original
  measurement); reasoning-only reconfirmation that the ~26s/~400MB/
  2,000,000-ref characterization still applies.
- Task 4 (`update_tips` abort mid-fetch, partial ref-state risk): traced
  `git_remote_update_tips`'s ref-application loop in libgit2 1.9.6's
  `remote.c` (`update_tips_for_spec`, calling `update_ref` per ref). Each
  `update_ref` call **fully writes and commits** its own ref via
  `git_reference_create`/`git_reference_create_matching` *before* invoking
  the `update_tips` callback for that ref (`remote.c:1779-1794`) — so when
  `fetch_capped`'s counter trips and returns `false` past
  `MAX_ADVERTISED_REFS`, every ref processed *before* the trip point is
  already fully, individually committed to the local repo; the abort only
  stops *further* refs from being processed, exactly the same partial-
  progress shape an ordinary network-interrupted fetch already has with no
  cap involved. **Not a new finding** — this is inherent, pre-existing
  libgit2 behavior this fix round didn't change, and no ref is left in a
  half-written/corrupt state (each `git_reference_create` call is
  independently atomic).

**New finding 3.** `[security: Medium]` `InputValidation`/`DoS` —
`update_tips_cb`'s refname parameter shares push's exact unguarded-unwrap
pattern, and is now reachable via `fetch`/`pull` with no `catch_unwind`
protection.

*Location:* `git2-0.21.0/src/remote_callbacks.rs`, `update_tips_cb`
(~line 401): `str::from_utf8(CStr::from_ptr(refname).to_bytes()).unwrap()`
— byte-for-byte the same pattern as the already-fixed
`push_update_reference_cb` (finding 1). `fetch_capped`
(`crates/core/src/git/mod.rs`) registers `callbacks.update_tips(...)` to
implement `MAX_ADVERTISED_REFS`, but `fetch`'s call to `remote.fetch(...)`
has no `catch_unwind` around it — only `push`'s call site was wrapped by
the fix round, since `update_tips` didn't exist as a registered callback
before that same fix round introduced it.

*Attack scenario:* a malicious/misbehaving remote whose ref advertisement
includes a ref name containing invalid-UTF-8 bytes. Traced the exact
call path in libgit2 1.9.6's `remote.c:update_ref` (line 1755-1799): it
calls `git_reference_create`/`git_reference_create_matching` to write the
local tracking ref to disk **first**, and only invokes the `update_tips`
callback (line 1792-1794) once that write already succeeded — so
whether the vulnerable callback is ever reached depends on whether the
local filesystem accepts a ref file name containing that exact byte
sequence.

*Verified live, with a platform caveat honestly disclosed:* on this
machine (macOS/APFS), attempting to create a ref with an invalid-UTF-8
byte in its name — both as a loose-ref filename and via a `packed-refs`
line whose destination lock-file APFS also validates — fails at the OS
level with `Illegal byte sequence` (`EILSEQ`), which `git_reference_
create` surfaces as a normal libgit2 error, returned as `Err`, **before**
`update_tips_cb` is ever invoked. This is real, live-tested behavior, but
it is a macOS/APFS-specific filename-encoding rule, not a libgit2 or
`ide-core` protection — Linux's ext4 (a target platform per this
project's `CLAUDE.md`) treats filenames as opaque byte strings and would
not reject the same write, meaning `git_reference_create` would succeed
and `update_tips_cb`'s `.unwrap()` would then run on the attacker-supplied
bytes, panicking exactly like finding 1 did before its fix. This finding
is therefore **confirmed by code-path tracing plus partial live evidence**
(per the skill's own allowance for reasoning where a live repro needs a
platform this pass doesn't have), not a full live repro on this host.

*Suggested fix direction:* wrap `fetch_capped`'s `remote.fetch(...)` call
in `std::panic::catch_unwind` (with `AssertUnwindSafe`, same shape as
`push`'s fix), converting a caught panic into a `GitError` variant instead
of letting it propagate. Mirrors finding 1's fix exactly.

**New finding 4.** `[quality]` (surfaced by this pass, not itself an
attacker-facing vulnerability) — the committed regression test
`configure_network_timeouts_bounds_a_stalling_remote` does not actually
exercise `configure_network_timeouts`'s effect; it passes for an unrelated
reason regardless of whether the timeout fix works at all.

*Location:* `crates/core/src/git/mod.rs`,
`configure_network_timeouts_bounds_a_stalling_remote`.

*What was found:* the test's fake server reads only 1 byte
(`let mut sink = [0u8; 1]; conn.read(&mut sink)`) from the client's
git-upload-pack request line (which is longer than 1 byte), then lets the
`TcpStream` drop — closing a socket with unread data still sitting in the
kernel receive buffer sends a TCP RST rather than a clean FIN. `GitRepo::
fetch` then fails almost instantly (~1ms, observed) with `"error
receiving data from socket: Connection reset by peer"` — an error that has
nothing to do with the configured timeout.

*Verified live:* built a standalone harness reproducing the exact test
scenario byte-for-byte, once *with* `configure_network_timeouts` called
and once *without*. Both failed in under 1ms with the identical
connection-reset error — proving the test would pass identically whether
the fix is present, broken, or entirely deleted. Then built a corrected
version of the same harness whose fake server fully drains the client's
request (a 64KB read) before stalling: with the timeout configured, fetch
now genuinely fails at `2.00122425s` elapsed with `"could not read from
socket: timed out"`; without it, fetch was confirmed still blocked past
6 seconds (killed manually rather than waited out) — this is what the
existing test *should* be asserting, and it confirms the shipped
`configure_network_timeouts` fix itself is genuinely correct. Only the
regression test's own fake server is flawed.

*Impact:* the production fix works (independently re-verified above); the
risk is purely that a future regression to `configure_network_timeouts`
or to how `git2`/libgit2 surfaces read timeouts would go completely
undetected by this test suite, since the test would keep passing for the
wrong reason.

*Suggested fix direction:* change the test's fake server to fully drain
the client's request (a generously-sized single read, e.g. 64KB) before
holding the connection open and sending nothing further — exactly the
corrected harness shape verified live above.

## Round 2 verdict

Findings, highest severity Medium (new finding 3). Task 1/2/5
re-confirmations found no regression. Task 4's question (partial
ref-state on `update_tips` abort) is answered and is not a new finding —
it's inherent, unchanged libgit2 behavior. Two new items: finding 3 (a
plausible, code-path-confirmed but not fully live-reproducible-on-this-
host panic surface introduced by this very fix round's own `update_tips`
registration) and finding 4 (a test-validity gap that doesn't affect
shipped behavior but should still be fixed so the regression suite
actually proves what it claims). Recommend a third fix round: `catch_unwind`
around `fetch_capped`'s `remote.fetch(...)` call (mirroring finding 1's
fix) and correcting the stalling-server test's fake server to drain the
request fully before stalling.
