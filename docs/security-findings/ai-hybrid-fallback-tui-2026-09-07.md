# Security/quality review — AI hybrid-fallback (T49)

**Scope**: `crates/ai/**`, `crates/sanitizer/**`,
`crates/tui/src/ai_panel.rs`, and their wiring into `crates/tui/src/app.rs`
(`handle_ai_panel_key`). Reviewed at the commit T49 had already merged to
`main` at (`a988b31` + two follow-up fixup commits, `4b8bc03` and
`8962308`), before this pass's own fixes. This feature was originally
implemented by a different tool (OpenCode, evidenced by a stray
`opencode.json`/worktree in `/opencode/`) and merged directly to `main`
without a persisted `docs/security-findings/` artifact, despite
`docs/roadmap.md`'s T49 row claiming a completed 2-round `hacker` pass with
findings F1/F2 — that claim could not be verified against any file on disk
and is treated here as unsubstantiated, not as prior work to build on.

Attack-surface categories considered:

- **Network protocol parsing** (applies) — hand-rolled SSE stream parsing
  from cloud/local LLM endpoints, untrusted response bodies.
- **Subprocess execution** (does not apply) — this feature spawns no
  subprocesses.
- **Credential handling** (applies) — API keys from `GEMINI_API_KEY`/
  `GROQ_API_KEY`/`GITHUB_MODELS_TOKEN`.
- **DoS / resource exhaustion** (applies) — untrusted/malformed streaming
  responses, unbounded retries/buffers.
- **File-system access** (does not apply beyond the pre-existing
  `.ide/ai.json` config read, unchanged by this pass).
- **Multi-tenant/shared state, cryptography, sandbox escape** — not
  applicable; single-user local process, no crypto primitives beyond TLS
  (delegated to `hyper-rustls`/`rustls`, not hand-rolled).

Live testing performed: yes, against `127.0.0.1` mock HTTP/SSE servers
started by this pass's own test additions (`crates/ai/src/tests.rs`) —
`TcpListener::bind("127.0.0.1:0")`, no external network access. Verified:
a stalled connection (headers sent, then silence) actually times out
rather than hanging; a stream that never terminates an SSE event actually
gets rejected once its buffered size crosses the new cap, rather than
growing unbounded; a cloud request with `sanitized: false` is refused
before any socket is opened. Everything else below is code-analysis-only,
noted per finding.

## Findings

### 1. [InputValidation/DoS, Medium] Permanent request wedge with no recovery path

**Location**: `crates/ai/src/lib.rs` — `HttpTransport::new()` (was
`.expect("load native TLS roots")`), `HttpTransport::post_json`'s body-read
`await` (was unwrapped, no timeout), `Provider::dispatch`'s SSE read loop
(`while let Some(chunk) = stream.text().await`, was unwrapped, no timeout).

**Scenario**: A local Ollama instance (or a compromised/misbehaving
network path to a cloud provider) accepts the TCP connection and sends
response headers, then never sends another byte and never closes the
connection. Before this fix, `dispatch`'s `stream.text().await` blocks
forever — the background thread that owns this future never returns, `tx`
is never sent a terminal message, `AiPanel::poll` never sees
`is_in_flight() == false` again, and the only user-facing recovery was
restarting the whole `ide` process. Separately, `HttpTransport::new()`'s
`.expect()` on `with_native_roots()` would panic the background thread on
a system with a broken/absent CA trust store (a minimal container image,
e.g.), again with the request silently wedged rather than an error
reaching the panel.

**Verified**: live — `dispatch_times_out_on_a_stalled_stream` in
`crates/ai/src/tests.rs` starts a mock server that sends SSE headers then
sleeps 5s before closing; against the (test-only, short) chunk timeout the
call returns `AiError::Timeout` rather than hanging.

**Fix applied**: `HttpTransport::new()` returns `Result<Self, AiError>`
instead of panicking. `post_json`'s body-read `await` and `dispatch`'s
per-chunk stream read are each wrapped in `tokio::time::timeout` (60s,
`STREAM_CHUNK_TIMEOUT` constant). `AiPanel::cancel()` (new method) drops
the receiver so `is_in_flight()` goes false immediately and a fresh
request can be submitted without waiting out the transport timeout; wired
to `Esc` in `handle_ai_panel_key` as a manual backstop.

### 2. [DoS, Medium] Unbounded SSE parser buffer

**Location**: `crates/ai/src/lib.rs` — `SseParser::buf` (`String`, no size
cap), pushed into on every chunk inside `dispatch`'s read loop.

**Scenario**: A malformed or hostile server sends an unbounded stream of
bytes that never contains the SSE event terminator (a blank line). Every
chunk read appends to `SseParser::buf` with no size check; `find_event_boundary`
never finds a boundary, so the buffer grows for as long as the server keeps
sending and the connection stays open, exhausting memory. `MAX_REPLY_CHARS`
(the existing cap) does not help here since it only bounds *extracted*
delta text, checked after an event is already fully parsed out of the
buffer — a buffer that never completes an event never reaches that check.

**Verified**: live — `dispatch_rejects_an_sse_stream_that_never_terminates_an_event`
sends `MAX_SSE_BUFFER_BYTES + 1` bytes of non-terminated filler and
confirms `dispatch` returns an error (rather than continuing to buffer)
once the cap is crossed.

**Fix applied**: `MAX_SSE_BUFFER_BYTES` (1 MiB) constant; checked in
`dispatch`'s loop immediately after each `parser.push(&chunk)`, returning
`AiError::Message` if exceeded.

### 3. [InputValidation, Low] `HttpTransport::new()` panic path (subsumed by #1)

Listed separately since it's a distinct code location from the timeout
fixes, but the fix and verification are the same as #1's `HttpTransport::new()`
half. No separate test beyond the type-level guarantee that
`HttpTransport::new()` can no longer panic (all 15 call sites across
`lib.rs`/`tests.rs` were updated to handle the `Result`).

### 4. [InputValidation/Authorization-adjacent, Medium] Cloud-dispatch sanitizer gate was compile-time only

**Location**: `crates/ai/src/lib.rs` — `Provider::stream_chat`'s gate,
originally `if self.id().is_cloud() && cfg!(not(feature = "sanitizer")) { ... }`.

**Scenario**: The gate only checked whether the crate was *compiled* with
the `sanitizer` feature (a default feature, so effectively always on in
practice), never whether the specific request being dispatched had
actually been through masking. Any future caller of `Provider::stream_chat`
(or a bug in `ai_panel.rs`'s own masking decision) that built a
`ChatRequest` without running it through `Sanitizer` first would have had
its raw, unmasked text sent straight to a cloud provider with no
compile-time or run-time signal catching the mistake — the check simply
couldn't see whether masking had happened for *this* call.

**Verified**: live — `stream_chat_refuses_a_cloud_request_that_is_not_sanitized`
constructs a `ChatRequest { sanitized: false, .. }` targeting `ProviderId::Gemini`
with no mock server running at all; the call returns
`AiError::Message` containing "sanitized" rather than attempting any
network I/O (which would have surfaced as a connection-refused error
instead, confirming the rejection happens before dispatch).

**Fix applied**: `ChatRequest` gained a `sanitized: bool` field.
`Router::chat`/`DefaultRouter::chat` gained a `sanitized: bool` parameter,
forwarded into each per-attempt `ChatRequest`. `ai_panel.rs`'s `run_request`
passes `map.is_some()` (whether `mask_outgoing` actually produced a
roundtrip map) as this argument. `stream_chat`'s gate now requires
**both** the compile-time feature **and** `request.sanitized`.

### 5. [InputValidation, Low] Sanitizer coverage gaps + overstated doc claim

**Location**: `crates/sanitizer/src/lib.rs` — `mask_secrets_with_threshold`'s
`known` pattern list; the crate's top-of-file doc comment ("Zero-trust
masking of sensitive substrings").

**Scenario**: The regex/entropy pipeline covered JWTs, common token
prefixes, and private IPv4s unconditionally, plus a generic high-entropy
opaque-token sweep gated by a configurable threshold. It had no pattern
for common low-entropy `NAME=value`-shaped secrets (e.g. `PASSWORD=hunter2`
— a dictionary word has low Shannon entropy and sails past any reasonable
threshold) or PEM private-key blocks (whose base64 body can occasionally
dip below the entropy gate on short keys). Separately, the crate's doc
comment asserted "Zero-trust masking," which overstates what a
regex+entropy heuristic can actually guarantee — a secret in a shape/entropy
range none of the patterns cover still reaches the cloud provider unmasked,
and a reader relying on the doc comment's literal claim could
under-estimate that residual risk.

**Verified**: code-analysis + new unit tests, not a live network test (no
network surface to attack here — this is a pure string-transform
function). New tests: `mask_secrets_hides_a_low_entropy_password_assignment`,
`mask_secrets_hides_a_quoted_api_key_assignment`,
`mask_secrets_does_not_touch_an_unrelated_key_value_pair` (regression
against over-matching plain `key=value` pairs), `mask_secrets_hides_a_pem_private_key_block`,
and `mask_secrets_restores_a_jwt_nested_inside_a_secret_assignment_unambiguously`
(regression for a double-masking hazard introduced and caught during this
same fix — see below).

**Fix applied**: two new patterns added to `known`: a case-insensitive
`NAME=value`/`NAME: value` matcher for `secret`/`password`/`passwd`/`token`/
`api_key`/`access_key`-shaped names, and a PEM private-key block matcher.
Both are ordered **before** the JWT/token-prefix/IP patterns rather than
after — during development, adding them *after* caused a double-masking
bug: an input like `token=eyJ...` would first get its JWT portion masked
into a placeholder by the JWT pattern, and then the new generic pattern
would match `token=__IDE_SAN_0__` (the placeholder now looks like an
opaque value) and wrap it in a *second* placeholder — whose restore
correctness then depended on `HashMap` iteration order in
`restore_originals`, which Rust does not specify, meaning the restored
reply could non-deterministically still contain a literal
`__IDE_SAN_0__` placeholder string depending on unlucky hash iteration.
Reordering (coarsest shape first) avoids the nesting entirely; the
regression test above locks this in. The crate doc comment was reworded
from "Zero-trust masking" to "best-effort, heuristic masking," explicit
that a gap is possible and this is defense-in-depth alongside the
local-vs-cloud routing choice, not a standalone guarantee.

## Verdict

**Findings, highest severity Medium** — three Medium (permanent wedge, SSE
buffer growth, compile-time-only sanitizer gate) and two Low (panic path
subsumed by the wedge fix; sanitizer coverage/doc-claim gap). All five are
fixed in this pass, with regression tests added for each; no findings were
deferred or left open. No Critical/High findings.
