# TUI: AI Hybrid — Local LLM chat + FIM autocomplete, cloud fallback (T49)

## 1. Purpose

The IDE has a Claude assistant panel (`claude_panel.rs`, T7/T8) shelling out
to the external `claude` CLI, but no way to use a local LLM or a build-free
cloud model for in-IDE chat (selected code → explanation / transformation)
or fill-in-the-middle autocompletion. The user asked for a local AI provider
first (Ollama), with a hybrid fallback chain to cloud free-tiers
(Gemini/Groq/GitHub Models) when the local model is unavailable, under
per-provider capability + rate-limit conditions.

**Scope decisions (recorded for reviewability):**

1. **TUI-first.** This run lands `ide-ai` (`crates/ai`), `ide-sanitizer`
   (`crates/sanitizer`), and the `ide-tui` panel + FIM wiring. The GUI
   (`ide-ui`) integration is a separate later run — `ide-ai` is built as a
   frontend-independent library expressly so that run is thin.
2. **Two new crates.** `ide-ai` (provider layer, HTTP, router) and
   `ide-sanitizer` (zero-trust masking) rather than growing `ide-core` —
   `ide-core` stays free of network code (its own rule), and both new
   crates are independently testable. `rust-tui-dev` owns them (fresh
   crates created by that role, the way `rust-dap-dev` created
   `crates/dap`).
3. **No `reqwest`, no `axum`. `tokio` + hand-rolled `hyper` client,
   in-process, no daemon.** Locked 2026-09-06; `hyper` companions
   `http-body-util` + `bytes` approved by the user as part of the client
   (recorded in `CLAUDE.md`). `async_trait` is **not** used — native
   `async fn` in traits (Rust 1.97 / edition 2021).
4. **Credentials env-only, credential-driven enablement.** `GEMINI_API_KEY`,
   `GROQ_API_KEY`, `GITHUB_MODELS_TOKEN`. A cloud provider joins the
   fallback chain **only** if its env var is present; otherwise silently
   skipped. Never persisted, never logged, never echoed in errors. Ollama
   needs no key (localhost).
5. **Config via a new `ProjectSettingsFile::Ai` slot** (`.ide/ai.json`,
   content-named, shared with the GUI later) — decided by the user at
   2026-09-06: provider order + per-provider sanitize flags persist now
   (settings UI deferred). Bounded deserialization per the repo's DoS
   guard convention (T42 `MAX_CUSTOM_ACTIONS` precedent).
6. **SSE streaming in v1** — decided by the user 2026-09-06: replies stream
   token-by-token into the panel, so the panel has a delta channel rather
   than the ClaudePanel's whole-reply-at-once contract.
7. **Sanitizer: cloud routes always masked; local default on, configurable**
   via the `.ide/ai.json` slot. Roundtrip restores originals into the
   reply. One-shot, per-request map, never persisted.

**Security posture, stated up front:** this feature sends project source to
external (cloud) LLM endpoints over TLS. Per the `CLAUDE.md`
security-sensitive-paths list (network I/O, config-driven program names),
**`hacker` pass is mandatory** before merge. The sanitizer's whole job is
that no secret (API key, token, internal address, high-entropy literal)
reaches a cloud provider, and no generated text is rendered into the UI
unhandled. Nothing is dispatching to any provider until the sanitizer
guarantee is provable in tests.

## 2. Interface / API

### 2.1 `ide-core` (first role, `rust-core-dev`)

One new variant on `ProjectSettingsFile` (`crates/core/src/project_settings.rs`):

```rust
pub enum ProjectSettingsFile {
    // ...
    ///  AI provider config: order + per-provider sanitize flags
    ///  (`docs/features/tui-ai-hybrid-fallback.md`, T49). Content-named,
    ///  not frontend-named, like `Navigation`/`CustomActions` — `ide-tui`
    ///  is the first user; nothing ties the file to it.
    Ai,
}
```

`file_name(self)` gains `ProjectSettingsFile::Ai => "ai.json"`. No other
change to this module — `read`/`write`/`settings_dir`/`ensure_gitignored`
are already generic over the payload (verified `crates/core/src/
project_settings.rs:89-130`) and already carry the `.ide/` symlink-escape
and atomic-write guarantees.

### 2.2 `ide-sanitizer` (`crates/sanitizer`, second role, `rust-tui-dev`)

Standalone crate; no runtime deps beyond already-approved ones. Feature
`rust-ast` (default on) gates the `syn` path:

```toml
[dependencies]
regex = "1"
syn = { version = "2", features = ["full", "visit"], optional = true }

[features]
default = ["rust-ast"]
rust-ast = ["dep:syn"]
```

Public surface (`pub use` of three masked sub-mods):

```rust
/// A single masked token: placeholder ↔ original.
pub struct MaskEntry { pub placeholder: String, pub original: String }

/// In-progress masking state; one per request/response pair, never persisted.
pub struct Sanitizer {
    map: Vec<MaskEntry>,
    next_id: usize,
}

impl Sanitizer {
    pub fn new() -> Self;
    /// Register a placeholder for `original`; returns `__IDE_SAN_<n>__`.
    pub fn register(&mut self, original: &str) -> String;
    /// Full masking pipeline over `input` (regex passes + entropy, plus
    /// Rust-string-literal masking when the `rust-ast` feature is on),
    /// using the default 2.0 entropy gate.
    pub fn mask(&mut self, input: &str) -> Sanitized;
    /// [`Self::mask`] with an explicit entropy gate for the opaque-token
    /// sweep — the local (`local_sanitize_threshold`) vs cloud
    /// (`cloud_sanitize_threshold`) routes pass different thresholds here;
    /// known-shape patterns (JWTs, token prefixes, private IPs) are masked
    /// unconditionally regardless of the gate.
    pub fn mask_with_threshold(&mut self, input: &str, threshold: f64) -> Sanitized;
    /// True if nothing masked yet; count of distinct masked values.
    pub fn is_empty(&self) -> bool;
    pub fn len(&self) -> usize;
}

pub struct Sanitized { pub masked: String, pub count: usize }

/// Roundtrip helpers: map all entries, restore placeholders back.
pub fn as_map(s: &Sanitizer) -> std::collections::HashMap<String, String>;
pub fn restore_originals(masked: &str, map: &HashMap<String, String>) -> String;
pub fn reset_placeholders(s: &mut Sanitizer);

/// Regex passes: API-key patterns, JWT (`eyJ...`), internal IPv4
/// (RFC1919 + private ranges). Bounded token/input size. Delegates to
/// [`mask_secrets_with_threshold`] with a 2.0 gate.
pub fn mask_secrets(s: &mut Sanitizer, input: &str) -> String;
pub fn mask_secrets_with_threshold(s: &mut Sanitizer, input: &str, threshold: f64) -> String;

/// Shannon entropy over candidate tokens; threshold configurable per
/// provider (cloud tighter than local).
pub fn shannon_entropy(s: &str) -> f64;
pub fn entropy_above(s: &str, threshold: f64) -> bool;

/// syn AST walk collecting `LitStr`, masking string literals to
/// placeholders (feature `rust-ast` only).
#[cfg(feature = "rust-ast")]
pub fn mask_rust_strings(s: &mut Sanitizer, input: &str) -> String;
```

### 2.3 `ide-ai` (`crates/ai`, second role, `rust-tui-dev`)

Depends on `ide-core` (for `ProjectSettingsFile::Ai` read) and optionally
`ide-sanitizer` (feature `sanitizer`, default on; cloud dispatch enforces
it at runtime):

```rust
// Provider identity + enablement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderId { OllamaLocal, Gemini, Groq, GitHubModels }

impl ProviderId {
    /// Env var that credentials this provider (`None` for Ollama).
    pub fn credential_env(self) -> Option<&'static str>;
}

// Persisted config (`ProjectSettingsFile::Ai` payload), bounded serde.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub provider_order: Vec<ProviderId>,
    pub sanitize_local: bool,          // default true
    pub local_sanitize_threshold: f64, // entropy threshold, default 4.0
    pub cloud_sanitize_threshold: f64, // default 3.5 (tighter)
}
impl Default for AiConfig { /* provider_order = [OllamaLocal, Gemini, Groq, GitHubModels] ... */ }
impl AiConfig {
    /// Credential-driven: drops cloud providers whose env var is absent.
    pub fn enabled_providers(&self) -> Vec<ProviderId>;
    /// Bounded read (max 4 providers; truncates; malformed → default).
    pub fn load(project_root: &std::path::Path) -> Self;
}

// Chat: one prompt/context pair through the chain.
pub struct ChatMessage { pub role: ChatRole, pub text: String }
pub enum ChatRole { User, Assistant }

/// JSON request shape as sent on the wire (OpenAI-compatible for
/// Ollama/Groq/GitHub Models; Gemini's own `contents`/`parts` shape).
/// Unit-testable without network.
pub struct ChatRequest { pub messages: Vec<ChatMessage>, pub model: String }

// Result of a streamed chat completion.
pub struct ChatDelta { pub text: String }

```

**Note ("native async, no async_trait, no futures/stream dep"):** the crate
does **not** use `async_trait` and adds **no** `futures`/`tokio-stream`
dependency (only the approved table above). Streaming is exposed as a
**synchronous, channel-bound reader**, not an async stream — the caller
hands in a `tokio`-agnostic `std::sync::mpsc::Sender<Result<ChatDelta,
AiError>>` and the provider's async task pushes into it. This keeps the
panel's "background thread + std mpsc" model (`claude_panel.rs`) and
`ide-ai` free of a stream-ecosystem dependency. See §3.2 for the exact
thread choreography.

```rust
/// Full provider surface; implemented per provider, sharing one hyper
/// client. No trait object, no async-trait: each provider is a struct
/// (one variant per `ProviderId`) behind `enum Provider` dispatch.
pub enum Provider {
    Ollama { transport: HttpTransport },
    Gemini { transport: HttpTransport },
    Groq { transport: HttpTransport },
    GitHubModels { transport: HttpTransport },
}
```

**Provider endpoints (locked in the plan, fixing them here):**

| Provider | Endpoint | Auth |
|---|---|---|
| `OllamaLocal` | `http://localhost:11434/v1/chat/completions` | none |
| `Gemini` | `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key=…` | `GEMINI_API_KEY` as query `key` |
| `Groq` | `https://api.groq.com/openai/v1/chat/completions` | `GROQ_API_KEY` as `Bearer` header |
| `GitHubModels` | `https://models.inference.ai.azure.com/chat/completions` | `GITHUB_MODELS_TOKEN` as `Bearer` header |

Gemini's payload is its own `contents: [{ role, parts: [{ text }] }]`
shape; every other provider gets the OpenAI-compatible
`messages`/`ChatMessage` shape. The `ChatMessage::Assistant` role maps to
Gemini's `role: "model"`; `ChatMessage::User` maps to `role: "user"`
(Gemini rejects `assistant`).

```rust
impl Provider {
    /// URL + auth header source for this provider (`key=this credential`).
    pub fn endpoint(&self) -> String;
    /// POST `request`, stream SSE `data:` deltas into `tx`. Resolves when
    /// the stream closes or errors. Non-blocking to the caller.
    pub async fn stream_chat(
        &self, request: &ChatRequest,
        tx: std::sync::mpsc::Sender<Result<ChatDelta, AiError>>,
    ) -> Result<(), AiError>;
    /// FIM (fill-in-the-middle): prefix/suffix around a caret →
    /// completion text. Ollama-only — every other provider returns
    /// `AiError::Unsupported` (cloud free-tiers have no FIM contract).
    pub async fn complete_fim(&self, prefix: &str, suffix: &str) -> Result<String, AiError>;
}

// Error taxonomy — every variant has a status-line display string.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("connection refused (is the local model running?)")] ConnectionRefused,
    #[error("provider timed out")] Timeout,
    #[error("rate limited (HTTP 429/408)")] RateLimited,
    #[error("provider error: HTTP {0}")] Http(u16),
    #[error("stream ended without completion")] StreamEnded,
    #[error("this provider does not support FIM")] Unsupported,
    #[error("{0}")] Message(String),
}
```

### 2.4 `ide-tui` (second role, `rust-tui-dev`)

Follows the crate's established panel pattern exactly (`claude_panel.rs` /
`cargo_panel.rs`): a background thread + `mpsc`, polled unconditionally
each frame.

**New file `crates/tui/src/ai_panel.rs`:**

```rust
/// Provider answer, one message per reply. Walls the panel from
/// `AiError`/delta machinery.
pub enum AiDisplayMessage {
    User(String),
    Assistant(String),                    // complete reply text
    StreamingDelta(String),               // appended to the active reply
    ProviderServing(String),              // e.g. "local (ollama)" / "cloud (gemini)"
    Error(String),
}

pub struct AiPanel {
    pub input: String,
    pub history: Vec<AiDisplayMessage>,
    pub sanitized: bool,                    // was the outgoing payload masked?
    idle: bool,
    rx: Option<Receiver<AiDisplayMessage>>,
    ctx: Option<AiContext>,                 // what the user selected / asked about
}

pub(crate) enum AiContext {
    Selection(String),   // selected text from the editor
    WholeFile(String),
    None,
}

/// Appends input→history, sanitizes the outgoing payload, spawns a
/// background thread driving the router; returns immediately.
pub fn submit(&mut self, prompt: String, context: AiContext);
/// Returns true if a reply is still streaming.
pub fn is_in_flight(&self) -> bool;
/// Call once per frame; drains the channel; returns true if `history`
/// changed (caller requests a repaint).
pub fn poll(&mut self) -> bool;
```

**`app.rs` wiring** (every one of these is a real existing integration
point, verified in the tree):

- `BottomDockTab` gains `Ai` (inserted between `Docker` and `Kubernetes`
  in the `next`/`prev` cycles — verify exact enum position at
  `app.rs:782-811`); `show_bottom_dock_tab(BottomDockTab::Ai)` /
  `close_all_overlays` behave like existing tabs.
- `App` gains `ai: AiPanel` + `ai_panel_open: bool` (`app.rs:1004-1007`).
  Unlike `claude`/`claude_panel_open` (a standalone GUI overlaid view), the
  AI assistant is a **normal bottom-dock tab**: `ai_panel_open` is a
  *derived dock-tab visibility* mirroring on/off — `toggle_ai_panel()`
  drives the same `show_bottom_dock_tab(BottomDockTab::Ai)` path
  `toggle_custom_actions_panel` uses (`app.rs:2403-2417`) — kept so the
  key dispatcher and palette gate on one flag.
- `handle_bottom_dock_key` gets an `BottomDockTab::Ai` arm delegating to a
  new `handle_ai_panel_key`. Keys mirror the Claude chat view's actual
  contract (verified against `handle_claude_chat_key`, `app.rs:2276`):
  `Esc` closes the panel, `Enter` submits, `Backspace`/ordinary chars edit
  `input`. **No scroll-back** — tail-only render of `history`, exactly the
  `tui-claude-panel.md` §1.1 / `render_cargo_panel` precedent
  (`ui.rs:1417`).
  Nothing new is invented here: no `Up`/`Down` history scrolling, no
  `Ctrl+L` clear — the Claude panel has neither, and this panel inherits
  the same v1 cuts.
- `poll_ai()` added to `lib.rs::run()`'s unconditional poll section
  (verified location: `lib.rs:228-285`, after `poll_remote_op`) — polling
  unconditional so a reply keeps streaming while the panel is closed,
  exactly the `poll_cargo` / `poll_custom_actions` precedence.
- Sanitize-before-send and restore-after-receive are wired inside the
  background thread around every router call — **there is no code path
  that dispatches a payload that skipped the sanitizer**.

**`commands.rs`** — `Action::ToggleAiPanel`, palette-only with no default
binding (same reasoning as `ToggleClaudePanel` — no reference-IDE keymap
lists an assistant panel; `commands.rs:895-901`). No invented binding.

**`ui.rs`** — `render_ai_panel` (titles/history/input, status line showing
`ProviderServing` + `sanitized`), plus the dock tab label. Follows
`render_claude_panel`'s rendering conventions.

## 3. Behaviour

### 3.1 Router

> See `diagrams/tui-ai-hybrid-fallback-sequence.png` — the full
> sanitize → route → stream → restore choreography in one picture.

- Provider order comes from `AiConfig::provider_order`, defaulted to
  `[OllamaLocal, Gemini, Groq, GitHubModels]`, filtered by
  `enabled_providers()` (credential-driven).
- Local first. `complete_fim` requires `OllamaLocal` (only local model is
  FIM-capable in the defaults); if local is unavailable, FIM reports a
  status-line error rather than falling to cloud (cloud free-tiers have no
  FIM contract).
- Fallback on: `ConnectionRefused`, `Timeout`, `RateLimited`, `Http(5xx)`,
  `StreamEnded`. **No** fallback on `Http(4xx)` other than 408/429 — a
  4xx is a request/credential problem, retrying a different provider
  masks it.
- Bounded retry: max 1 retry per provider, max total 2 attempts across
  the chain, fixed 500 ms backoff between attempts (no unbounded waits).
- A reply records which provider served it (`ProviderServing`) for the
  status line. Switching providers mid-reply is *not* attempted — the
  fallback decision happens at request start, not at first bad byte.

### 3.2 Threading / streaming

- `AiPanel::submit` spawns one background thread. That thread owns a
  `tokio` `Runtime` (created on the thread) which drives the selected
  provider's `stream_chat`. Each SSE `data:` delta is pushed into the
  `mpsc` as `StreamingDelta`; the terminal frame receives it via
  `poll_ai()` and appends to the active assistant message.
- No async in the frame loop. One in-flight request at a time (v1), same
  as `ClaudePanel`; a second `submit` while in flight is a no-op with a
  status-line notice (no queue — v1 scope).

### 3.3 Sanitizer wiring (dispatch boundary)

- Outgoing payload is **always** passed through the sanitizer on cloud
  routes (enforced in `ide-ai`'s dispatch: a cloud route with the
  `sanitizer` feature disabled refuses to send — `AiError::Message`).
- Local route: sanitize if `AiConfig::sanitize_local` (default true) with
  `local_sanitize_threshold`; cloud always uses `cloud_sanitize_threshold`.
  The route picks the threshold and calls `Sanitizer::mask_with_threshold`
  inside `AiPanel::prepare` (via the extracted, unit-tested
  `mask_outgoing` helper — see §3.3 below).
- The placeholder map from the sanitizing pass is held only for the
  lifetime of that request; on reply completion the roundtrip restores
  originals into the accumulated assistant text, then the map is dropped.
  If the request fails, the map is dropped without restore.
- `AiPanel::sanitized` records (for the status line) whether the outgoing
  payload was masked this turn, set synchronously in `prepare`; the
  background thread masks via `mask_with_threshold` + `as_map` and
  restores via `settle`'s `restore_originals` (`mask_outgoing`/`settle` are
  extracted helpers, both unit-tested).
- Selection/whole-file context (`AiContext`) is copied at submit time,
  not read live from the editor — a mid-stream buffer change can't make
  the displayed context diverge from what was actually sent.

### 3.4 FIM inline autocomplete

- Triggered by a user-invoked `Action::TriggerFimAutocomplete` command
  (palette-only, no default binding — reference IDE keymaps bind code
  completion to `⌃Space` but this is a *model-driven* autocomplete distinct
  from the LSP's; no mapping exists, so no invented combining chord).
- Prefix/suffix = buffer text before/after the caret (up to a
  `MAX_FIM_CONTEXT_CHARS = 4096` cap each), sent to `complete_fim`.
- Insertion is one normal buffer `Transaction` (single undo step),
  reusing the editor machinery `tui-code-actions-and-rename.md` and
  `refactor-this.md` (T43) already use.
- Non-blocking: request on background thread; insertion applied when the
  result arrives, replacing nothing (FIM output is appended at caret).

## 4. Constraints and invariants

- **`hacker` pass mandatory** (see Purpose). The sanitizer guarantee — no
  secret in any cloud payload — is proven by tests, not asserted.
- **Credentials:** env-only; never written to any project file, never
  logged, never included in error strings, never rendered in the UI. An
  env-var value is never a network *path* component (only a query
  parameter / header).
- **Rounding bounds:** `AiConfig::provider_order` capped at 4 entries on
  load (truncate, mirroring `MAX_CUSTOM_ACTIONS`); no unbounded vectors
  from provider data. Streaming deltas capped at `MAX_REPLY_CHARS =
  200_000` per reply (truncate + close); a hostile/looping model can't
  balloon memory.
- **Untrusted model output:** generated text is rendered as plain
  `Span` text only — never interpreted as input, never appended to a
  buffer except via the FIM transaction, never executed. `AiError` /
  status messages are `&'static` or bounded display strings.
- **No TLS weakening.** Cloud calls use default hyper TLS verification —
  never disabled "to make a fetch succeed".
- **Localhost-only local provider:** the Ollama URL is fixed
  `http://localhost:11434`; never prompt-injected or config-driven (no
  SSRF-from-config surface).
- **One-shot roundtrip map:** created per request, dropped per request,
  never persisted. `restore_originals` matches placeholders only, never
  writes arbitrary strings into files — the restored text goes into
  `history` display and the FIM transaction only.
- **Config file is untrusted** (a cloned repo could carry `.ide/ai.json`):
  bounded deserialization + fall back to defaults on malformed/no-file.
- **Cut:** no settings UI this run (reads/writes the `.ide/ai.json` slot
  via defaults; the UI to edit it is a follow-up). Settings absent →
  defaults, exactly like `Preferences`'s existing behavior.

## 5. Examples

```rust
// Router + chat (library perspective; `hacker`-gated behavior).
let config = AiConfig::load(project_root);          // cred-driven order
assert!(config.enabled_providers().contains(&ProviderId::OllamaLocal));

// Panel perspective (what the TUI really drives).
let mut panel = AiPanel::default();
panel.submit("explain this".into(), AiContext::Selection(sel.clone()));
while !app.ai.is_in_flight() { /* first frame */ }
app.poll_ai();  // drains StreamingDelta / Assistant / Error into history
assert!(app.ai.history.iter().any(|m| matches!(m, AiDisplayMessage::Assistant(_))));
```

## 6. Dependencies & integration points

**New workspace members** (root `Cargo.toml`): `crates/ai`, `crates/sanitizer`.

**Approved deps** (all recorded in `CLAUDE.md` 2026-09-06):

| Crate | For |
|---|---|
| `tokio` (full features) | async HTTP + background thread runtime |
| `hyper` | hand-rolled HTTP/1.1 client |
| `http-body-util` + `bytes` | hyper response-body reading (approved as hyper companions, 2026-09-06) |
| `syn` | Rust string-literal AST masking (feature-gated `rust-ast`) |
| `regex` (already approved) | sanitizer regex passes |

**No `reqwest`, no `axum`, no `async_trait`, no new keymap bindings.**

**Role merge order:**

1. `rust-core-dev` — `ProjectSettingsFile::Ai` variant + `file_name`. Small
   diff; `ide-tui` builds against it.
2. `rust-tui-dev` — `crates/sanitizer`, `crates/ai`, `crates/tui`
   (`ai_panel.rs`, app/commands/ui/lib wiring). Depends on step 1.
3. `hacker` — mandatory (cloud dispatch, credentials, untrusted model
   output). Review the sanitizer guarantee + streaming/roundtrip bounds.

## Revision notes

- **r1 (2026-09-06, initial scaffold review):** this doc was drafted after
  hand-written scaffolding in `crates/ai`/`crates/sanitizer` existed
  (pre-doc, process violation acknowledged). **The entire pre-doc scaffold
  is superseded by this doc** — the doc's Cargo.toml snippets and function
  signatures are authoritative; `rust-tui-dev` reuses only the *ideas* it
  needs and writes everything else fresh against this doc. In particular
  the scaffold's `sanitizer/src/strings.rs` (referenced by the scaffold but
  never written) is dropped: Rust-string masking via `syn` is re-specced
  as `mask_rust_strings` here, implemented by `rust-tui-dev`, not patched
  onto the scaffold.
- **r2 (2026-09-06, doc-review):** `#tui-ai-hybrid-fallback`'s key-handling
  claim originally described `Up`/`Down` history scrolling and a `Ctrl+L`
  clear-input binding — neither exists in `handle_claude_chat_key`
  (`app.rs:2276`), the contract this panel mirrors; both were invented.
  Corrected to the real chat-view keys (`Esc`/`Enter`/`Backspace`/chars)
  and the tail-only no-scroll-back precedent (`ui.rs:1417`). Line
  citations corrected (`handle_claude_chat_key` at `app.rs:2276`, not
  2287; scroll-back note at `ui.rs:1417`). The §2.3 streaming signature
  was drafted with `futures_sink::Stream` + an `#[async_trait]` marker,
  i.e. two unapproved dependencies in a doc that explicitly bans both —
  re-specced to a channel-bound `stream_chat(&self, req, tx)` with a
  `std::sync::mpsc::Sender`, no futures crate, matching the panel's
  existing kanal-independent threading model (fix before first rev pass,
  recorded here).
- **Decisions locked with the user at doc time:** SSE streaming in v1
  (delta channel, not ClaudePanel's whole-reply contract); `Ai` settings
  slot now (persisted config, no settings UI); `http-body-util`+`bytes`
  approved as hyper companions.
- **r3 (2026-09-06, impl vs doc alignment):** `complete_fim` is
  **Ollama-only** — the doc's claim that "Gemini supports FIM" was wrong;
  Gemini returns `AiError::Unsupported` like the other cloud providers, so
  §2.3 now says exactly that and adds the `Unsupported` variant to the
  error taxonomy. §2.2's Cargo.toml listed a `thiserror` dep the sanitizer
  never needed (dropped before merge); the threshold API names are
  corrected to the real `mask_with_threshold` /
  `mask_secrets_with_threshold` (not the never-existing `mask_secrets`
  `_with_threshold` family), and the §2.4 wiring's "mirrors
  `claude_panel_open`" wording is fixed: the AI assistant is a **normal
  bottom-dock tab**, `ai_panel_open` is a *derived* dock-tab visibility kept
  for the key dispatcher, `toggle_ai_panel` drives
  `show_bottom_dock_tab(BottomDockTab::Ai)` (`app.rs:2403-2417`) rather
  than mirroring the standalone Claude overlay. §3.3 adds the
  `prepare`/`mask_outgoing`/`settle` helper names (all unit-tested) and the
  threshold-selection detail. Coverage: `crates/ai/src/lib.rs` regions
  raised to 81.12% / lines 86.45% (was 78.82% / 84.73%) by adding transport
  tests that exercise the bearer-header branch of `post_json`/`post_stream`
  and `dispatch`'s streaming error/reader-gone/partial-without-done/reply-cap
  paths — all ≥ the rust-tui-dev 80% line gate for non-rendering touched
  files.