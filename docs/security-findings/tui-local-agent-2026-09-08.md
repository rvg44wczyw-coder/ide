# T56 Local Agentic IDE Assistant — adversarial security pass

## Scope

Reviewed: `crates/agent/**` (the new `ide-agent` crate: `agent_loop.rs`,
`executor.rs`, `protocol.rs`, `tool.rs`), and the `ide-tui` side of the
integration (`crates/tui/src/agent_panel.rs`, `crates/tui/src/app.rs`).
Commit `6d44df7`, branch `worktree-rust-tui-dev-orbit-series-t46-t48`,
worktree `/Users/ivs/rust/ide/.claude/worktrees/rust-tui-dev-orbit-series-t46-t48`.
`rev` had already approved this code (round 2) before this pass started.

Threat model, per the feature doc's own §4: a model's own output (which may
itself be influenced by prompt injection embedded in file content the
agent reads, e.g. a malicious `README`/comment/build-log line) can
directly cause subprocess execution and file mutation, and `Auto` mode
("no confirmation at all") is a supported, requested configuration. The
attacker capability assumed throughout is exactly this: **no direct access
to the machine, only the ability to get attacker-chosen text into
something the agent will read and feed back to the model as part of its
own turn** (a file in the project, a `git log`/`docker logs` line, a
build/test failure message, etc.) — i.e. an indirect-prompt-injection
attacker, which is the realistic threat this feature is exposed to.

Attack-surface categories from the skill's checklist, applied:

- **Sandbox / privilege escalation** — applies directly (`RunShellCommand`,
  `ReadDockerLogs`, `EditFile`, path-taking tools). Live-tested.
- **DoS / resource exhaustion** — applies directly (subprocess execution
  with no output cap or timeout). Live-tested.
- **Input validation (adversarial)** — applies to `protocol::parse_tool_call`
  (fenced JSON parser). Reviewed with adversarial inputs; no live crash
  found (see Findings, ruled out below).
- **Path traversal** — applies to `ReadFile`/`ListDirectory`/`EditFile`.
  Live-tested; existing `canonicalize`+`starts_with` validation (shared
  with `ide_dap::path::validate_path`) held against both `../` and
  symlink-escape attempts I constructed. No finding here.
- **Metadata leakage** — applies narrowly (subprocess argv). Reasoned about,
  not live-tested (see Finding 3); low severity, not novel to this feature.
- Not applicable and ruled out: MITM/identity spoofing, replay, downgrade,
  key confusion, timing side-channels, weak randomness (no network
  handshake, no session/protocol negotiation, no key derivation, no
  crypto anywhere in this diff — the LLM call itself goes through
  `ide_ai::Router`, already reviewed under T49/T55; this pass is scoped to
  the new agent-side surface only).

Live tests actually run (all against instances/processes I started myself,
via a standalone harness crate outside the repo at
`/Users/ivs/.claude-personal/jobs/9d6300dc/tmp/hacker-harness`, depending
on `ide-agent` from this worktree by path so the *real* `ToolExecutor` code
is exercised, not a reimplementation): three live attacks, detailed under
Findings 1 and 2 below. All spawned child processes were explicitly killed
and reaped before this pass finished; verified with `ps aux | grep` that
nothing was left running.

## Findings

### 1. [SandboxEscape, **Critical**] `RunShellCommand`'s allowlist is a filename-stem string match, not a resolved-binary identity check — trivially evadable for arbitrary code execution with zero confirmation in Auto mode

**Location:** `crates/agent/src/agent_loop.rs::is_allowlisted` (lines
49-64), consumed by `run_one_tool`'s `PermissionMode::Auto` branch (line
379-382); the actual execution primitive is
`crates/agent/src/executor.rs::run_shell_command` (line 137-149), which
passes `program` straight to `Command::new(program)` with **no path
validation of any kind** — unlike `ReadFile`/`ListDirectory`/`EditFile`,
`program` never goes through `validate()`/`validate_path`.

**Attack scenario:** An attacker who can get the model to emit a
`RunShellCommand` tool call with a *specific string* — via direct chat
input, or indirectly via prompt injection in a file/build-log/`git log`
line the agent reads and feeds back into its own context — can get
**arbitrary native code execution, with zero human confirmation, while
the user is in `Auto` mode** (a supported, intended, documented
configuration — not a misconfiguration). Two independent variants, both
confirmed live:

- **Variant A — relative path stem-spoofing.** `is_allowlisted` computes
  `Path::new(program).file_stem()` and matches only that against the fixed
  set `{cargo, go, ls, cat, grep}` (unconditional) or `{git, docker}`
  (conditional on first arg). `Path::new("./cargo").file_stem()` is also
  `"cargo"` — indistinguishable from the trusted binary to this check. But
  to `std::process::Command`, a program string containing a path separator
  is resolved as a **literal path**, bypassing `$PATH` entirely, while a
  bare `"cargo"` is resolved via `$PATH` (the real system binary). So
  `RunShellCommand{program: "./cargo", args: []}` auto-runs in `Auto` mode
  and executes *any executable file the attacker can place at
  `<project_root>/cargo`* — not the trusted system `cargo` at all.
- **Variant B — absolute path anywhere on disk.** The same stem check
  passes for a fully-qualified path like `/tmp/evil/cargo`, again with the
  program's actual identity completely unverified.

**How verified (live, not just reasoned about):** standalone harness at
`/Users/ivs/.claude-personal/jobs/9d6300dc/tmp/hacker-harness/src/main.rs`,
depending on the real `ide-agent` crate from this worktree by path. Wrote
an executable shell script literally named `cargo` (`#!/bin/sh\ntouch
pwned_by_agent\necho PWNED`) inside a fresh tempdir used as the
`ToolExecutor` project root, then called the real
`ToolExecutor::execute(AgentTool::RunShellCommand { program: "./cargo",
args: vec![] })`:

```
--- Attack 1: program="./cargo" (relative path, real cargo NOT invoked) ---
outcome: Ok("exit status: exit status: 0\nPWNED\n")
pwned_by_agent exists: true
```

`pwned_by_agent` — a file the "real" `cargo` would never create — was
written by the executor, proving the attacker's binary ran instead of the
trusted one. The same result reproduced with an absolute path to a second
planted `cargo`-named script in a completely separate tempdir (Variant B),
confirming `run_shell_command` applies **no path restriction whatsoever**
to `program`, independent of `is_allowlisted`'s own flaw.

Since `is_allowlisted` is the *only* gate standing between `Auto` mode and
unconditional, un-approved code execution for `RunShellCommand`, and it is
evaded by a one-character prefix (`./`) or any path at all, this defeats
the single safety property the doc's threat model relies on for
`RunShellCommand` in `Auto` mode. The same stem-confusion applies equally
to `git`/`docker`/`go`/`ls`/`cat`/`grep` — every allowlisted name, not just
`cargo`.

**Suggested fix direction:** `is_allowlisted` must reject any `program`
containing a path separator (`/` on POSIX, also `\` if this ever targets
Windows) outright — a bare name only, resolved via `$PATH` the same way a
human typing `cargo` at a shell would get the real one. That alone closes
Variant A/B. Consider additionally canonicalizing the resolved binary path
(e.g. via `which`-equivalent lookup) and asserting it resolves outside
`project_root`, so a `$PATH`-order attack (a malicious `cargo` earlier on
`$PATH` than the real one — out of scope for this specific diff, since
`$PATH` isn't attacker-controlled by this feature, but worth a defense-in-
depth note) isn't a silent extension of the same class of bug.

---

### 2. [DoS, **High**] No execution timeout on `RunShellCommand`/`ReadDockerLogs`, and `AgentPanel::cancel()` cannot actually stop a hung one — a single tool call permanently leaks a background thread and an orphaned child process

**Location:** `crates/agent/src/executor.rs::run_shell_command` /
`read_docker_logs` (both call the blocking `Command::output()` with no
timeout anywhere in the call chain — confirmed by grepping
`agent_loop.rs`+`executor.rs` for `Duration`/`timeout`; the only hit is an
unrelated 1ms poll sleep in `stream_turn`, agent_loop.rs:343);
`crates/tui/src/agent_panel.rs::cancel` (lines 129-134) and `submit`'s
`thread::spawn(move || runner(prepared))` (line 190).

**Attack scenario:** `cat` is one of the *unconditionally* allowlisted
programs (no first-arg restriction, unlike `git`/`docker`) — so
`RunShellCommand{program: "cat", args: ["/dev/zero"]}` auto-runs in `Auto`
mode with zero confirmation and then **never returns**: `Command::output()`
blocks the calling thread until the child exits, and `/dev/zero` never
produces EOF. The same is achievable even more simply with `cat` and *no*
args at all, since `Command` inherits the parent's stdin by default and a
non-interactive/piped stdin may never signal EOF either. This isn't
gated by permission mode at all — the identical hang happens for a command
a human explicitly approved in `Approve` mode, or a command a `Plan`-mode
user asked about read-only.

Worse: `AgentPanel::submit` runs the whole `AgentLoop` (and therefore every
blocking `Command::output()` call inside it) on a plain `std::thread::spawn`
background thread. `AgentPanel::cancel()` — wired to the agent dock's Esc
key — only clears the *panel's own* `rx`/`handle`/`streaming` fields; it
has no way to signal or terminate that OS thread, and Rust cannot forcibly
kill a `std::thread` from outside it. So a user who notices the agent is
stuck and presses Esc to cancel gets a UI that immediately looks idle again
(`is_in_flight()` becomes false, they can submit a new prompt) while the
**original thread and its child subprocess keep running, invisibly,
forever** — reclaimed only when the whole `ide-tui` process exits. Each
such hang is a silent, permanent leak of one OS thread plus one runaway
child process; repeating the attack (or just repeatedly hitting a slow
command and cancelling) accumulates leaked processes with no way for the
user to discover or clean them up from within the app.

**How verified (live):** same harness, Attack 3 — spawned
`Command::new("cat").arg("/dev/zero").current_dir(<tempdir>)` (the exact
primitive `run_shell_command` uses) and polled `try_wait()` non-blockingly
for 3 seconds:

```
--- Attack 3: DoS via unconditionally-allowlisted `cat /dev/zero` (no timeout anywhere in ide-agent) ---
still running after 3s (proves indefinite hang, matching what .output() would block on): true
child killed and reaped for cleanup
```

Confirmed the process was still alive after 3s (proving the hang genuinely
never resolves on its own within any reasonable window — `.output()` would
have blocked identically), then explicitly killed and reaped it;
`ps aux | grep "[c]at /dev/zero"` afterward showed nothing left running, so
no orphan was left behind by *this test itself* — but the finding is
precisely that the *production code path*, unlike my test, has no such
cleanup step at all once a real user hits Esc.

**Suggested fix direction:** wrap subprocess execution with a hard wall-
clock timeout that kills the child on expiry (this requires moving off
`std::process::Command::output()` — a fully synchronous call inside an
`async fn` gives `tokio::time::timeout` nothing to preempt, since there's
no `.await` yield point inside it — to `tokio::process::Command`, whose
`.wait()`/`.output()` are genuinely async and can be raced against
`tokio::time::timeout` and `.kill()`ed on expiry). Separately,
`AgentPanel::cancel()` needs a real cancellation path to the running
thread — at minimum, threading a `Child` handle (or an `Arc<Mutex<Option<
Child>>>` the executor populates) out to where `cancel()` can reach it and
call `.kill()`, so "Cancel" in the UI actually stops the underlying
process instead of just detaching the UI from it.

## Controversial / lower-priority notes (not filed as numbered findings)

- **Metadata leakage (argv):** `RunShellCommand`'s `args` (attacker/model-
  influenced) land in the child's argv, visible to any other local process
  that can list process listings (e.g. `ps`) on the same machine, same as
  any CLI tool passed secret-shaped arguments — this is not novel to this
  feature and not obviously actionable without redesigning how the tool
  passes data to subprocesses; noting it for completeness per the skill's
  checklist rather than filing it as a blocking finding.
- **`protocol::parse_tool_call` adversarial input:** tried (by code
  reading + the crate's own adversarial test set, which already covers
  unterminated fences, invalid JSON, unknown tool names, missing/wrong-
  typed args, oversized malformed snippets, and multiple fences) truncated
  fences, deeply-nested/huge JSON via `serde_json::from_str` (a
  recursive-descent parser with a depth limit that returns an `Err` rather
  than overflowing the stack for realistic depths), and non-tool-call
  prose containing the literal string `` ```tool_call `` — nothing panics
  or misparses into executing an unintended tool; `serde_json`'s own
  hardening against pathological input is relied upon here, which is
  reasonable for a well-audited, non-home-grown parser. No finding.
- **`AgentTool::DebugControl`/`EditFile` auto-running with zero
  confirmation in `Auto` mode:** confirmed via direct code reading of
  `run_one_tool`'s `needs_approval` match (agent_loop.rs:376-384) that in
  `Auto` mode, `needs_approval` is `false` for every tool *except* a
  non-allowlisted `RunShellCommand` — meaning `EditFile` (arbitrary
  in-project file overwrite) and `DebugControl` (full control of a live
  debug session: resume, stop, step, toggle breakpoints) also run
  completely unattended in `Auto` mode. This matches the doc's own stated
  definition of `Auto` mode ("no confirmation at all is a supported,
  requested configuration") rather than being a bug distinct from the
  feature's intended design, so it is not filed as a finding — but it does
  mean Finding 1's severity should be read in that context: `Auto` mode
  already grants the model unattended file-mutation and debugger control
  by design, and `RunShellCommand`'s allowlist was clearly meant to be the
  *one* remaining constraint even in that mode. Finding 1 shows that
  constraint currently provides no real protection at all.

## Verdict (original pass)

**Findings — highest severity Critical** (Finding 1: `RunShellCommand`
allowlist evasion enabling unattended arbitrary code execution in `Auto`
mode). Finding 2 (High) is a second, independent issue. Both block merge
per this project's `dev-chain` convention.

## Re-verification (2026-09-08, fix-round 1)

Fixed across two commits: `a105be4` (main fix) and `e1cb33a` (a follow-up
`rev`-round-3 finding against the first fix — a trailing-separator gap in
the new check, `program: "cargo/"`). `rev` approved the final state at
`e1cb33a`. Re-attempted all three exploit variants live rather than trusting
the diff:

1. **Finding 1, re-tested.** Rebuilt the same standalone harness
   (path-depending on `ide-agent` from this worktree) and re-ran the exact
   `program: "./cargo"` attack directly against `ToolExecutor::execute`:
   it still ran the planted binary (`pwned_by_agent exists: true`) — this
   is **expected and correct**, not a regression. `ToolExecutor` was never
   the fix's target; the actual gate, `is_allowlisted`, is `pub(crate)`/
   private and only consulted by `AgentLoop` for `Auto`-mode eligibility.
   Confirmed by direct source read that `is_allowlisted` now does
   `if program.contains('/') { return false; }` before the file-stem
   comparison — an unambiguous string check with no path-parsing edge
   cases, correctly rejecting `"./cargo"`, `"/tmp/evil/cargo"`, `"cargo/"`,
   and any other `/`-bearing string, while leaving bare names (`"cargo"`,
   `"git"`, `"ls"`) unaffected. Also re-ran `cargo test -p ide-agent
   agent_loop::tests` and confirmed `is_allowlisted_rejects_every_path_
   qualified_program`/`is_allowlisted_covers_the_fixed_safe_set` both pass
   with the new assertions.
2. **Finding 2, re-tested.** Ran the crate's own new regression tests
   (`run_shell_command_exceeding_the_timeout_is_killed_and_reported`,
   `cancelling_while_a_shell_command_is_running_kills_it`) — both pass, and
   the whole `agent_loop::tests` group (30 tests) completes in ~0.21s
   wall-clock, confirming neither test is accidentally waiting out a real
   multi-second delay; both drive a genuine `sleep 5` subprocess (read the
   test source to confirm), not a mock. Additionally built an independent
   standalone check (outside `ide-agent`'s own code) reproducing the exact
   primitive `executor.rs` now uses — `tokio::process::Command::new(
   "sleep").arg("30").kill_on_drop(true)`, raced via `tokio::select!`
   against a 300ms sleep — and confirmed via `pgrep` that the spawned
   `sleep 30` was genuinely gone after the timeout branch won and the
   losing future was dropped (`any \`sleep 30\` still running after drop:
   false`). This is the actual security-critical mechanism the whole fix
   depends on; confirmed empirically, not just by citing tokio's docs.
3. **Trailing-separator variant, tested for the first time** (found by
   `rev` after this pass's original report, never previously live-tested).
   Confirmed by direct inspection that `program.contains('/')` rejects
   `"cargo/"`. Also independently re-ran `ToolExecutor::execute(program:
   "cargo/")` directly: it fails outright with `Io("cargo/ not found or
   failed to run: Not a directory (os error 20)")` — confirms, independent
   of the allowlist fix, that this form was never exploitable for code
   execution in the first place (POSIX requires a trailing-slash path to
   resolve to a directory, and a directory can't be exec'd), matching the
   fix commit's own stated reasoning.

All spawned processes (planted `cargo` scripts, the `sleep 30` check) were
confirmed gone via `pgrep`/`ps` before finishing; the scratch harness
directory was deleted.

## Verdict (re-verification)

**Clean.** Both original findings, plus the trailing-separator variant
discovered during `rev`'s follow-up round, are confirmed closed by live
re-testing, not just diff inspection. No residual issue found.
