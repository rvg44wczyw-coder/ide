//! Project-scoped AI config (`.ide/ai.json`; `docs/features/
//! tui-ai-hybrid-fallback.md` §2.3, T49). Read via `ide-core`'s bounded
//! `project_settings::read`, so a malformed or hand-edited file can never
//! crash the panel -- `load` falls back to defaults exactly as if the file
//! were absent, the same fail-open contract `custom_actions.rs` already
//! uses for its own settings slot.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ProviderId;

/// Maximum number of providers `load` will ever materialize; the file may
/// list more, but a `provider_order` beyond this is truncated (both a
/// sanity bound and a tiny DoS guard against an adversarial `.ide/ai.json`
/// -- the same `MAX_*` bounded-read idea `custom_actions.rs`'s
/// `MAX_CUSTOM_ACTIONS` already encodes).
const MAX_PROVIDERS: usize = 4;

/// Which kind of task a chat request represents, used to pick a
/// (possibly different) provider chain and model (`docs/features/
/// tui-ai-task-routing.md`, T55). `General` is the default and the
/// always-safe fallback -- every code path that can't determine a more
/// specific role (classifier disabled, classifier failed, an
/// unrecognized classifier label) lands here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskRole {
    General,
    Planning,
    Coding,
    Review,
}

/// One role's routing override (T55 §2.1). Both fields are optional in
/// effect: an empty `provider_order` falls back to
/// [`AiConfig::provider_order`]; a `None` `model_override` falls back to
/// `default_model(id)` for whichever provider ends up serving.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleRoute {
    pub provider_order: Vec<ProviderId>,
    pub model_override: Option<String>,
}

/// Persisted AI provider config. All fields default on `serde(default)`,
/// so a partial file (hand-edited, older version) still loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Fallback order the router walks in full (§3.1): cloud/free-tier
    /// providers first, `OllamaLocal` last, so a local fallback only
    /// happens once every configured cloud provider has returned a
    /// fallback-eligible error (429/5xx/timeout/etc. -- see
    /// `fallback_eligible`).
    pub provider_order: Vec<ProviderId>,
    /// Sanitize the outgoing payload on the *local* (Ollama) route too.
    pub sanitize_local: bool,
    /// Entropy threshold for the local route. Higher = more conservative
    /// marking; the local model never leaves the machine.
    pub local_sanitize_threshold: f64,
    /// Entropy threshold for cloud routes; tighter than local because the
    /// payload leaves the machine.
    pub cloud_sanitize_threshold: f64,
    /// Per-role overrides (T55). A role absent from this map, or present
    /// with an empty `provider_order` or one whose every listed provider
    /// is disabled at runtime, uses `provider_order`/`default_model`
    /// exactly as if this feature didn't exist -- see
    /// [`resolve_role_route`].
    pub role_routes: HashMap<TaskRole, RoleRoute>,
    /// Opt-in (default `false`): classify each outgoing message via
    /// `classifier_provider` before dispatch, and route using the
    /// resulting role's `RoleRoute` instead of always `General`.
    pub auto_route: bool,
    /// Which provider runs the classification call when `auto_route` is
    /// `true`. Ignored otherwise. Defaults to `OllamaLocal` -- not one of
    /// the cloud providers already in `provider_order` -- because
    /// classification runs on every message, and a cloud default would
    /// add a second cloud round-trip per turn competing for the same
    /// free-tier rate-limit budget the r7 router fix exists to conserve.
    pub classifier_provider: ProviderId,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider_order: vec![
                ProviderId::Gemini,
                ProviderId::Groq,
                ProviderId::GitHubModels,
                ProviderId::OllamaLocal,
            ],
            sanitize_local: true,
            local_sanitize_threshold: 4.0,
            cloud_sanitize_threshold: 3.5,
            role_routes: HashMap::new(),
            auto_route: false,
            classifier_provider: ProviderId::OllamaLocal,
        }
    }
}

/// Resolves which [`RoleRoute`] actually serves a request for `role`
/// (T55 §3.1). An explicit, non-empty `provider_order` in
/// `config.role_routes[role]` with at least one currently-*enabled*
/// provider wins outright; otherwise (the role is absent, its
/// `provider_order` is empty, or every provider it lists is disabled at
/// runtime -- e.g. a configured `Planning: { provider_order: ["Gemini"] }`
/// with no `GEMINI_API_KEY` set) falls back to `config.provider_order`/
/// `default_model`, identically to an unconfigured role. This keeps a
/// role that's *configured but currently unreachable* from surfacing a
/// harder failure than simply not using this feature would have --
/// `Router::chat`'s own `enabled()` filter would otherwise see an empty
/// post-filter list for that role and return its "no enabled providers"
/// error.
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

impl AiConfig {
    /// Credential-driven: drops cloud providers whose env var is absent,
    /// dedupes, and caps at [`MAX_PROVIDERS`]. `OllamaLocal` is always
    /// present (it needs no credential).
    pub fn enabled_providers(&self) -> Vec<ProviderId> {
        let mut seen = Vec::with_capacity(MAX_PROVIDERS);
        for id in &self.provider_order {
            if seen.len() == MAX_PROVIDERS {
                break;
            }
            if id.enabled() && !seen.contains(id) {
                seen.push(*id);
            }
        }
        seen
    }

    /// Bounded read of `.ide/ai.json`. Malformed or unreadable → `Default`
    /// (the panel must never fail to open because of bad config).
    pub fn load(project_root: &Path) -> Self {
        let mut config = ide_core::project_settings::read::<AiConfig>(
            project_root,
            ide_core::project_settings::ProjectSettingsFile::Ai,
        )
        .unwrap_or_default()
        .unwrap_or_default();
        config.provider_order.truncate(MAX_PROVIDERS);
        for route in config.role_routes.values_mut() {
            route.provider_order.truncate(MAX_PROVIDERS);
        }
        config
    }
}
