//! Project-scoped AI config (`.ide/ai.json`; `docs/features/
//! tui-ai-hybrid-fallback.md` §2.3, T49). Read via `ide-core`'s bounded
//! `project_settings::read`, so a malformed or hand-edited file can never
//! crash the panel -- `load` falls back to defaults exactly as if the file
//! were absent, the same fail-open contract `custom_actions.rs` already
//! uses for its own settings slot.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ProviderId;

/// Maximum number of providers `load` will ever materialize; the file may
/// list more, but a `provider_order` beyond this is truncated (both a
/// sanity bound and a tiny DoS guard against an adversarial `.ide/ai.json`
/// -- the same `MAX_*` bounded-read idea `custom_actions.rs`'s
/// `MAX_CUSTOM_ACTIONS` already encodes).
const MAX_PROVIDERS: usize = 4;

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
        }
    }
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
        config
    }
}
