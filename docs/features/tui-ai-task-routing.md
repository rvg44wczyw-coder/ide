# T55: TUI AI Task Routing

## 1. Purpose

[T49 "TUI AI Hybrid"](tui-ai-hybrid-fallback.md) gives every chat request
the same provider chain regardless of what the request actually asks for
— a one-line clarifying question and a "plan this multi-file refactor"
prompt hit the exact same `provider_order`, in the exact same order,
using each provider's single fixed `default_model`.

This feature adds an optional **role**-based layer on top of that: a
request can be routed to a *different* provider chain, and optionally a
*different model* on the serving provider, depending on what kind of task
it is. Two ways to pick a role are supported, both off by default:

1. **Manual, config-only**: `.ide/ai.json` declares per-role
   `provider_order`/`model_override` pairs (`role_routes`). No UI in v1 —
   same "config exists, no settings screen yet" cut T49 already made for
   its own settings.
2. **Automatic ("auto-route")**: a new `auto_route: bool` config flag
   (default `false`, fully opt-in per direct user request — origin quoted
   in full in §6). When enabled, a small/fast/cheap model classifies the
   outgoing message's complexity before the real request is dispatched,
   and the resulting `TaskRole` selects which `role_routes` entry serves
   the actual reply.

**Origin**: direct user request, following a discussion of a locally
built AI orchestrator (role-based provider assignment — a "planner" model
for task breakdown, a fast model for generation, etc.) and a specific
follow-up: *"I want [it] optional to use small model for detecting
complexity and selecting needed AI as a config option."* Both halves of
that sentence are load-bearing: **optional** (default off, zero behavior
change for existing configs) and **config option** (no new UI surface).

**Scope cuts (v1)**:
- No UI to pick a role manually mid-conversation — role is either always
  `General` (`auto_route: false`) or decided by the classifier
  (`auto_route: true`). A manual role-picker in the AI panel is a
  plausible follow-up, not this doc's scope.
- No per-provider model override *within* a single role's chain — a
  role's `model_override`, when set, applies uniformly to whichever
  provider in that role's `provider_order` ends up serving. If a role's
  chain spans providers that need different override model names, split
  it into more specific roles instead (documented as a known v1
  simplification, not silently swallowed).
- `TaskRole::Review`/`Coding`/`Planning` are fixed, hardcoded variants —
  no user-defined role names in v1. Four roles cover the concrete request
  ("planner"/"coder"/general chat/review); an open-ended role registry is
  more machinery than the ask justifies right now.
- **Named directly, not left implicit**: a role that only reorders
  `provider_order` among the same 4 fixed providers, without also setting
  `model_override`, buys little over just curating one good default order
  — this feature's real payoff is concentrated in `model_override` (e.g.
  Gemini Flash vs. Pro), not in the role concept by itself. A leaner
  alternative — letting `AiConfig` override `default_model` per provider
  directly, with no roles or classifier at all — would deliver most of
  "use the strong model for hard stuff" for a fraction of this doc's new
  surface (new config shape, a new `Router::chat` parameter, a second
  cloud-egress path to review). This doc still specifies the fuller
  role-based design because both the manual routing and the
  complexity-classifier were explicitly requested together; the
  simplification is recorded here as the honest tradeoff, not adopted
  unilaterally.

## 2. Interface / API

### 2.1 `ide-ai` (`crates/ai`, `rust-tui-dev`)

```rust
/// Which kind of task a chat request represents, used to pick a
/// (possibly different) provider chain and model. `General` is the
/// default and the always-safe fallback -- every code path that can't
/// determine a more specific role (classifier disabled, classifier
/// failed, an unrecognized classifier label) lands here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TaskRole {
    General,
    Planning,
    Coding,
    Review,
}

/// One role's routing override. Both fields are optional in effect:
/// an empty `provider_order` falls back to `AiConfig::provider_order`;
/// a `None` `model_override` falls back to `default_model(id)` for
/// whichever provider ends up serving.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RoleRoute {
    pub provider_order: Vec<ProviderId>,
    pub model_override: Option<String>,
}

// AiConfig (existing struct, `crates/ai/src/project.rs`) gains three
// fields, all `#[serde(default)]` so an old `.ide/ai.json` written before
// this feature still loads unchanged:
pub struct AiConfig {
    // ...existing provider_order / sanitize_local / *_sanitize_threshold...

    /// Per-role overrides. A role absent from this map, or present with
    /// an empty `provider_order`, uses the top-level `provider_order` and
    /// `default_model` exactly as if this feature didn't exist.
    pub role_routes: std::collections::HashMap<TaskRole, RoleRoute>,

    /// Opt-in (default `false`): classify each outgoing message via
    /// `classifier_provider` before dispatch, and route using the
    /// resulting role's `RoleRoute` instead of always `General`.
    pub auto_route: bool,

    /// Which provider runs the classification call when `auto_route` is
    /// `true`. Ignored otherwise. Default `ProviderId::OllamaLocal` --
    /// deliberately *not* one of the cloud providers already in
    /// `provider_order`: classification runs on every message, so a cloud
    /// default would add a second cloud round-trip per turn that competes
    /// for the exact free-tier rate-limit budget the r7 fix exists to
    /// conserve (worst case, `classifier_provider` and the main chain's
    /// first entry share a quota, so `auto_route` could make 429-exhaustion
    /// happen *faster*, undercutting its own purpose). A local default
    /// sidesteps that entirely: it costs no rate-limit budget, and if
    /// Ollama isn't running the call fails fast (`ConnectionRefused`) and
    /// degrades to `TaskRole::General` per §3.2's existing contract -- the
    /// same safe default state as `auto_route: false`. A user who wants
    /// genuinely smarter (not just free) classification can still point
    /// `classifier_provider` at a cloud provider explicitly; §5's example
    /// does exactly that.
    pub classifier_provider: ProviderId,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            // ...existing provider_order / sanitize_local / *_sanitize_threshold...
            role_routes: std::collections::HashMap::new(),
            auto_route: false,
            classifier_provider: ProviderId::OllamaLocal,
        }
    }
}

/// Resolves which `RoleRoute` actually serves a request for `role`,
/// applying the fallback chain: an explicit, non-empty `provider_order`
/// in `config.role_routes[role]` wins outright; otherwise (the role is
/// absent, its `provider_order` is empty, *or* every provider it lists is
/// disabled at runtime per `ProviderId::enabled()` -- e.g. a configured
/// `planning: { provider_order: ["Gemini"] }` with no `GEMINI_API_KEY`
/// set) falls back to `config.provider_order`/`default_model` unchanged.
/// The third case is deliberate: a role that's *configured but currently
/// unreachable* must degrade exactly like a role that was *never
/// configured*, not surface a harder failure than simply not using this
/// feature would have. (`Router::chat`'s own `enabled()` filter would
/// otherwise see an empty post-filter list for that role and return its
/// existing "no enabled providers" error -- this function's job is to
/// never hand it that empty list in the first place.)
pub fn resolve_role_route(config: &AiConfig, role: TaskRole) -> RoleRoute {
    config
        .role_routes
        .get(&role)
        .filter(|r| r.provider_order.iter().any(|id| id.enabled()))
        .cloned()
        .unwrap_or(RoleRoute {
            provider_order: config.provider_order.clone(),
            model_override: None,
        })
}

/// Classifies `latest_user_message` into a `TaskRole` via one short,
/// non-streaming completion call to `classifier_provider`. Never returns
/// an error -- a timeout, a transport error, or a response that doesn't
/// parse to a known label all classify as `TaskRole::General`, so a
/// broken or slow classifier can only ever cost one bounded extra round
/// trip, never block or fail the real request.
///
/// `sanitized` is passed straight through to the classifier's own
/// internal `ChatRequest.sanitized` field -- exactly `ChatRequest`'s
/// existing contract (§2.3 of `tui-ai-hybrid-fallback.md`): the caller
/// asserts it already ran `mask_outgoing` on `latest_user_message` when
/// `classifier_provider.is_cloud()`, and `Provider::stream_chat` enforces
/// that assertion (refuses cloud dispatch when `sanitized` is `false`).
/// This function does **not** mask on the caller's behalf and does
/// **not** default `sanitized` to `true` -- an implementation that
/// hardcodes `true` here regardless of the actual argument would silently
/// defeat the gate for this call site specifically.
///
/// `latest_user_message` is truncated to [`MAX_CLASSIFY_INPUT_CHARS`]
/// via this crate's existing char-boundary-safe `truncate()` helper
/// (already used elsewhere in `lib.rs`) before being sent -- never a raw
/// byte slice, which would panic on non-ASCII UTF-8 input (e.g. a
/// non-English comment or string literal in the classified message).
pub async fn classify_task_role(
    classifier_provider: ProviderId,
    latest_user_message: &str,
    sanitized: bool,
) -> TaskRole;

/// Bounded timeout for `classify_task_role`'s single completion call --
/// deliberately much shorter than `STREAM_CHUNK_TIMEOUT` (60s) since a
/// slow classifier must not meaningfully delay ordinary chat latency.
pub const CLASSIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Hard cap on how much of the outgoing message the classifier ever
/// sees -- classification needs a gist, not the full payload, and this
/// keeps the extra cloud call cheap and fast regardless of how long the
/// user's actual message is. Applied via `truncate()`, so it never
/// splits a multi-byte character.
pub const MAX_CLASSIFY_INPUT_CHARS: usize = 500;
```

`Router::chat` (§3.1 of `tui-ai-hybrid-fallback.md`) gains one parameter,
threaded through from the resolved `RoleRoute`:

```rust
pub trait Router {
    fn chat(
        &self,
        messages: Vec<ChatMessage>,
        order: &[ProviderId],
        model_override: Option<&str>,
        sanitized: bool,
        tx: Sender<Result<ChatDelta, AiError>>,
    ) -> impl std::future::Future<Output = Result<ProviderId, AiError>> + Send;
}
```

`DefaultRouter::chat`'s implementation is otherwise unchanged from the r7
fix (`try_in_order` still walks the whole filtered `order`) — the only
new line is using `model_override.unwrap_or_else(|| default_model(id))`
in place of the unconditional `default_model(id)` call when building each
attempt's `ChatRequest`. Every existing caller (the r7 tests, `ai_panel.rs`
in "no auto-route" mode) passes `None` and observes byte-identical
behavior to before this feature.

### 2.2 `ide-tui` (`crates/tui/src/ai_panel.rs`, `app.rs`)

- `AiPanel::submit` resolves a `RoleRoute` **before** spawning the
  background thread's dispatch, not inside it — role resolution (the
  classifier call, when enabled) happens on the same background thread
  the existing dispatch already runs on, so the frame loop still never
  blocks:
  1. `role = if config.auto_route { classify_task_role(config.classifier_provider, &classify_input, classify_input_was_masked).await } else { TaskRole::General }`, where `classify_input` is the (already-truncated) latest user message and `classify_input_was_masked` is `true` exactly when `AiPanel::submit` ran `mask_outgoing` on it first (mirrors the main request's own `sanitized` bookkeeping one-for-one — see §3.3).
  2. `route = resolve_role_route(&config, role)` (§3.1's fallback rule, including the disabled-at-runtime case).
  3. `Router::chat(messages, &route.provider_order, route.model_override.as_deref(), sanitized, tx)`.
- `AiDisplayMessage`'s existing `ProviderServing` status line gains the
  resolved role only when `auto_route` was on for that request (e.g.
  `"Gemini (planning)"` vs. plain `"Gemini"` when role is always
  `General`) — a`General` role from `auto_route: false` renders exactly
  as today, no visual change for anyone who hasn't opted in.
- No new keybinding, command, or popup. No `close_all_overlays`/
  `any_popup_open` wiring — there's no new overlay, this is a pre-dispatch
  decision entirely inside the existing submit path.

## 3. Behaviour

### 3.1 Role resolution

> See `diagrams/tui-ai-task-routing-sequence.png` — the full
> classify → resolve → route flow, layered on top of T49's existing
> sanitize → route → stream → restore choreography.

- `auto_route: false` (the default): role is always `TaskRole::General`.
  Byte-for-byte the same request/response path as before this feature —
  this is what makes the feature safe to ship default-off.
- `auto_route: true`: `classify_task_role` runs once per `submit` call,
  on the classifier provider, with the outgoing message truncated to
  `MAX_CLASSIFY_INPUT_CHARS`. Its prompt asks for exactly one of the four
  role names; the response is lowercased and trimmed, then matched
  case-insensitively against `general`/`planning`/`coding`/`review` — any
  other text (extra words, an apologetic preamble, empty output) is
  `General`.
- `resolve_role_route` (§2.1) then decides the actual chain: a missing
  role, a role with an empty `provider_order`, *or* a role whose every
  listed provider is disabled at runtime (missing credentials) — all
  three fall straight back to the top-level `provider_order`/
  `default_model`, identically. There is no way to configure `.ide/
  ai.json` into a state where a chat request has *zero* providers to try,
  beyond the existing "no enabled providers" case T49 already handles.
- **`role_routes` applies independent of `auto_route`.** Role resolution
  (§3.1's first bullet) always produces *some* `TaskRole` — `General` when
  `auto_route` is `false`, a classified role when it's `true` — and
  `resolve_role_route` is always consulted for whatever role that was.
  This means a `role_routes.General` entry takes effect even with
  `auto_route: false`: it's a legitimate way to override just the default
  route without ever enabling classification (§1's "manual, config-only"
  option). §5 shows both usages.

### 3.2 Classifier failure handling

- Any `AiError` from the classifier's own transport/dispatch, and the
  classifier's own 5s timeout firing, both resolve to `TaskRole::General`
  — never surfaced to the user as an error, never retried, never
  fallen back across providers (the classifier call is not routed through
  `Router`/`try_in_order` at all; it is one direct `Provider::stream_chat`
  call to exactly `classifier_provider`, drained to its first complete
  response or timeout).
- A classifier failure therefore costs at most `CLASSIFY_TIMEOUT` (5s) of
  added latency before the real request proceeds exactly as if
  `auto_route` were `false` for that one message.

### 3.3 Sanitizer wiring

- When `classifier_provider.is_cloud()`, the text handed to
  `classify_task_role` must already be sanitized — `AiPanel::submit`
  reuses the exact same `mask_outgoing` call it already makes for the
  main request's payload (§3.3 of `tui-ai-hybrid-fallback.md`), applied
  to the truncated classify input, before the classifier call, and passes
  `classify_task_role`'s `sanitized` argument (§2.1) as `true` only when
  that masking actually ran. This is not optional: without threading a
  real `sanitized` value through (as opposed to hardcoding `true`),
  `auto_route: true` would open a **second**, previously-nonexistent cloud
  egress path for unmasked project text, independent of whichever
  provider ends up serving the real reply — this is the one place in this
  feature where a naive implementation could silently reopen a hole T49/r6
  already closed for the main request.
- With the default `classifier_provider: OllamaLocal` (§2.1), this
  section doesn't apply in the out-of-the-box configuration at all — cloud
  sanitization only becomes relevant once a user explicitly points
  `classifier_provider` at a cloud provider. `sanitize_local` is still
  respected for the local case exactly like the main request — no special
  case.
- **Discoverability note**: if a user enables `auto_route: true` but has
  no local Ollama running and never overrides `classifier_provider`, every
  classification call fails fast (`ConnectionRefused`) and every message
  is silently treated as `General` (§3.2) — `auto_route` will appear to
  "do nothing." This is the correct safe degradation, not a bug, but it's
  worth a status-line hint or log line in the implementation so a user in
  this state isn't left wondering why role-based routing never seems to
  trigger.

## 4. Constraints and invariants

- **Default-off, zero behavior change**: `auto_route: false` and an empty
  `role_routes` (the loaded-default state for every `.ide/ai.json` written
  before this feature, via `#[serde(default)]`) reproduce T49/r7's
  behavior exactly. This is the feature's core safety property, not
  incidental — `rev`/`hacker` should specifically verify no code path can
  reach the classifier or a role-specific chain when `auto_route` is
  unset.
- **`hacker` pass mandatory**: this touches the same cloud-dispatch
  surface `tui-ai-hybrid-fallback.md` already declared sensitive, and adds
  a genuinely new one (the classifier call) that must not become an
  un-sanitized cloud egress path.
- **Classifier call is bounded and non-blocking-on-failure**:
  `CLASSIFY_TIMEOUT` (5s) hard cap; any failure mode degrades to `General`,
  never to an error the user sees, never a retry loop of its own.
- **`model_override` is a plain string, not validated against the
  provider's actual model catalog** — an invalid override surfaces as
  whatever HTTP error the provider returns for an unknown model (already
  a handled `AiError` variant); this feature does not add model-name
  validation, matching `default_model`'s own unvalidated-constant
  precedent.
- **No new persisted secrets or credentials** — `classifier_provider` is
  a `ProviderId`, gated by the same `credential_env`/`enabled()` check as
  every other provider use; nothing new to leak.
- **`role_routes` is bounded implicitly** — at most 4 keys exist
  (`TaskRole`'s 4 variants), and each entry's `provider_order` is subject
  to the same `MAX_PROVIDERS = 4` truncation `AiConfig::load` already
  applies to the top-level `provider_order` (extend `load`'s truncation
  loop to also truncate every `RoleRoute.provider_order` it finds).

## 5. Examples

**Example A** — `.ide/ai.json` opting into both manual per-role routing
and automatic classification. Note the JSON keys under `role_routes` are
`TaskRole`'s actual serde output — PascalCase, no `rename_all`, matching
`provider_order`'s own `ProviderId` string convention in this same file
(`"OllamaLocal"`, not `"ollama_local"`):

```json
{
  "provider_order": ["Gemini", "Groq", "GitHubModels", "OllamaLocal"],
  "sanitize_local": true,
  "local_sanitize_threshold": 4.0,
  "cloud_sanitize_threshold": 3.5,
  "auto_route": true,
  "classifier_provider": "Groq",
  "role_routes": {
    "Planning": {
      "provider_order": ["Gemini"],
      "model_override": "gemini-1.5-pro"
    },
    "Coding": {
      "provider_order": ["Groq", "OllamaLocal"]
    },
    "Review": {
      "provider_order": ["GitHubModels", "Gemini"]
    }
  }
}
```

This example deliberately overrides `classifier_provider` to `Groq`
(a cloud provider, not the `OllamaLocal` default) to get faster/better
classification than a local model might manage — accepting the cloud
round-trip and rate-limit-competition tradeoff described in §2.1's
`classifier_provider` doc comment as an explicit, informed choice, not the
out-of-the-box behavior.

With this config: a message classified as "Planning" always goes to
Gemini using `gemini-1.5-pro` instead of the default `gemini-2.0-flash`;
"Coding" tries Groq's fast model first, falling back to local; "Review"
tries GitHub Models then Gemini (at Gemini's default model, since
`Review` has no `model_override`); anything classified `General` uses the
top-level `provider_order` unchanged from T49.

```rust
// Programmatic equivalent of resolving "Coding" with the config above:
let route = resolve_role_route(&config, TaskRole::Coding);
assert_eq!(route.provider_order, vec![ProviderId::Groq, ProviderId::OllamaLocal]);
assert_eq!(route.model_override, None);
```

**Example B** — manual override with `auto_route` left `false` (§3.1's
"applies independent of `auto_route`" point): every request still uses
`General`, but `General` itself is now pinned to a specific model, with
no classifier call ever made and no `classifier_provider` needed:

```json
{
  "provider_order": ["Gemini", "Groq", "GitHubModels", "OllamaLocal"],
  "role_routes": {
    "General": {
      "provider_order": ["Gemini", "Groq", "GitHubModels", "OllamaLocal"],
      "model_override": "gemini-1.5-pro"
    }
  }
}
```

## 6. Dependencies & integration points

- Builds directly on [T49 "TUI AI Hybrid"](tui-ai-hybrid-fallback.md) and
  its r7 router fix (this same worktree, commit `478d7d7`) — `try_in_order`
  is reused unchanged; `Router::chat`'s signature gains one parameter
  (§2.1) that every existing call site must update.
- `ide-core::project_settings` — unchanged; `AiConfig` keeps using the
  existing `ProjectSettingsFile::Ai` slot, no new settings file.
- No new external crate dependencies — `classify_task_role` reuses
  `Provider::stream_chat`/`dispatch` exactly as they exist today.
- Origin: user request following a review of a Gemini-assisted design
  discussion about local multi-provider orchestration (role-based model
  assignment: a strong/large-context model for planning, a fast model for
  generation, etc.), plus the specific, quoted follow-up requirement that
  drove §2.1's `auto_route`/`classifier_provider` design: *"optional to
  use small model for detecting complexity and selecting needed AI as a
  config option."*

## Revision notes

- **r1 (2026-09-07, self-review before implementation):** a `rev`
  documentation-review pass found 5 concrete gaps, all fixed in place:
  (1) `classify_task_role` had no way to receive whether its input was
  already sanitized, risking a hardcoded `true` that would defeat T49/r6's
  cloud-sanitize gate for this new call path — added an explicit
  `sanitized: bool` parameter; (2) §5's example JSON used lowercase
  `role_routes` keys (`"planning"`) that don't match `TaskRole`'s actual
  PascalCase serde output — fixed to `"Planning"` etc., matching
  `ProviderId`'s existing convention in this file; (3) the classify-input
  truncation mechanism was unspecified, risking a non-UTF-8-boundary-safe
  slice — specified reuse of the crate's existing `truncate()` helper;
  (4) a role configured with a non-empty `provider_order` that's entirely
  disabled at runtime (missing credentials) would hard-error instead of
  falling back like an unconfigured role — added `resolve_role_route`
  with an explicit `enabled()`-aware fallback; (5) `role_routes` applying
  independent of `auto_route` (a legitimate manual-override path) wasn't
  stated — added to §3.1 plus a second example. The same pass raised 3
  controversial findings, all addressed rather than left as commentary:
  the classifier's default provider was changed from `Groq` to
  `OllamaLocal` specifically to remove the free-tier-rate-limit
  competition the original design would have created (the controversial
  finding that motivated it is now spelled out in §2.1's doc comment
  rather than hidden); the "role indirection's value is mostly in
  `model_override`" tradeoff is now named explicitly in §1's scope cuts
  instead of left implicit; the unverified "Groq is fastest" claim was
  dropped along with the provider it was justifying. This review-and-fix
  cycle also produced a standing addition to the `rev` skill itself
  (`~/.claude-personal/skills/rev/SKILL.md`): controversial findings are
  now expected to be *addressed* (resolved, or explicitly named as a
  tradeoff, or escalated to the user) rather than only logged, in every
  future review.
