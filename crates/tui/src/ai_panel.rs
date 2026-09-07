//! Hybrid local/cloud AI assistant dock tab (`docs/features/
//! tui-ai-hybrid-fallback.md` §2.4, T49). One background thread per
//! request, that thread's own `tokio` current-thread runtime drives the
//! two-attempt router while the same thread concurrently drains the
//! delta channel -- so `StreamingDelta`s arrive live, never in a burst
//! when the reply completes (§3.2). The outgoing payload is passed
//! through `ide_sanitizer` inside that thread on every route that
//! masks (§3.3): the masking decision (local vs cloud threshold) is made
//! synchronously in `prepare` so `sanitized` is truthful for the status
//! line, but the sanitizing pass itself, the roundtrip map's lifetime,
//! and the restore are all thread-local.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use ide_ai::{
    classify_task_role, resolve_role_route, AiConfig, AiError, ChatMessage, DefaultRouter,
    ProviderId, Router, TaskRole,
};
use ide_sanitizer::{as_map, restore_originals, Sanitizer};

/// Provider answer, one message per reply. Walls the panel (and anything
/// reading `history`) from `AiError`/delta machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiDisplayMessage {
    User(String),
    /// Complete reply text (restored originals where the outgoing payload
    /// was masked) -- replaces an open streaming reply when it arrives.
    Assistant(String),
    /// Appended to the active reply while the model streams.
    StreamingDelta(String),
    /// e.g. "local (ollama)" / "cloud (gemini)" -- status line, not a
    /// history line.
    ProviderServing(String),
    Error(String),
}

/// What the user selected / asked about. Copied into the payload at submit
/// time (§3.3: never read live from the editor mid-request).
#[derive(Debug, Clone)]
pub(crate) enum AiContext {
    Selection(String),
    WholeFile(String),
    None,
}

/// Everything a background request run needs, produced by
/// `AiPanel::prepare` and consumed (spawned) by `submit`. `tx` lets tests
/// feed the panel's channel directly without spawning a thread.
pub struct PreparedRequest {
    pub(crate) payload: String,
    order: Vec<ProviderId>,
    threshold: Option<f64>,
    /// Carried through so `run_request` can resolve a `TaskRole`/
    /// `RoleRoute` on the background thread (`docs/features/
    /// tui-ai-task-routing.md`, T55 §2.2) -- role resolution (the
    /// classifier call, when `auto_route` is set) is async and must not
    /// run in `prepare`'s synchronous, frame-blocking path.
    config: AiConfig,
    pub(crate) tx: Sender<AiDisplayMessage>,
}

/// How a prepared request actually runs. Production always uses
/// [`run_request`]; tests substitute a fake runner so they never open a
/// socket to a real provider (the exact seam `ClaudePanel::with_runner`
/// is in `claude_panel.rs`).
type AiRunner = fn(PreparedRequest);

pub struct AiPanel {
    pub input: String,
    pub history: Vec<AiDisplayMessage>,
    /// Whether the outgoing payload of the current/last request was masked
    /// (for the status line) -- `docs/features/tui-ai-hybrid-fallback.md`
    /// §3.3.
    pub sanitized: bool,
    /// Lines scrolled back from the live tail (`docs/features/
    /// tui-panel-history-scroll.md` §2.4/§3.1, T52) -- same shape as
    /// `ClaudePanel::history_scroll`.
    pub history_scroll: u16,
    /// The last provider that served a reply, for the status line.
    pub(crate) provider: Option<String>,
    /// True while an assistant reply is still accumulating over the
    /// channel (drives `ingest`'s append-vs-new-reply decision).
    streaming: bool,
    rx: Option<Receiver<AiDisplayMessage>>,
    root: PathBuf,
    runner: AiRunner,
}

impl Default for AiPanel {
    fn default() -> Self {
        Self::new(PathBuf::new())
    }
}

impl AiPanel {
    pub fn new(root: PathBuf) -> Self {
        Self::with_runner(root, run_request)
    }

    /// `pub(crate)`, not private: `app.rs`'s own tests swap in a fake
    /// runner before exercising `handle_ai_panel_key`'s `Enter` path, so
    /// `App`'s test suite never opens a socket to a real provider.
    pub(crate) fn with_runner(root: PathBuf, runner: AiRunner) -> Self {
        Self {
            input: String::new(),
            history: Vec::new(),
            sanitized: false,
            history_scroll: 0,
            provider: None,
            streaming: false,
            rx: None,
            root,
            runner,
        }
    }

    /// Returns true if a reply is still streaming.
    pub fn is_in_flight(&self) -> bool {
        self.rx.is_some()
    }

    /// Manual recovery backstop for a wedged request (`hacker` fix round):
    /// drops the receiver so `poll` stops waiting on it and `is_in_flight`
    /// goes false immediately, letting the user submit a fresh request
    /// without waiting out the transport's own timeout. The orphaned
    /// background thread keeps running until its own read times out or its
    /// channel send fails (`run_request`'s loop already checks
    /// `tx.send(..).is_err()` and returns), but it can no longer touch
    /// anything this panel reads. A no-op when nothing is in flight.
    pub fn cancel(&mut self) {
        self.rx = None;
        self.streaming = false;
    }

    /// Compose the single line that actually leaves for the provider:
    /// prompt + any selection/whole-file context folded in. `None` for a
    /// blank/whitespace-only prompt (no request).
    fn compose(&self, prompt: String, context: AiContext) -> Option<String> {
        if prompt.trim().is_empty() {
            return None;
        }
        Some(match context {
            AiContext::None => prompt,
            AiContext::Selection(sel) => format!("{prompt}\n\nSelected code:\n```\n{sel}\n```"),
            AiContext::WholeFile(file) => format!("{prompt}\n\nFull file:\n```\n{file}\n```"),
        })
    }

    /// Records the user turn and prepares the request run. Pure apart from
    /// the tiny `.ide/ai.json` config read, so tests exercise it directly
    /// and `submit` is just the spawn.
    fn prepare(&mut self, prompt: String, context: AiContext) -> Option<PreparedRequest> {
        let payload = self.compose(prompt, context)?;
        if self.is_in_flight() {
            return None;
        }
        self.history.push(AiDisplayMessage::User(payload.clone()));
        let config = AiConfig::load(&self.root);
        let order = config.enabled_providers();
        let threshold = decide_threshold(&config, &order);
        self.sanitized = threshold.is_some();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.streaming = false;
        Some(PreparedRequest {
            payload,
            order,
            threshold,
            config,
            tx,
        })
    }

    /// Appends input→history, prepares the masking decision + threshold,
    /// and spawns the one background thread that sanitizes, routes, streams
    /// and restores. Returns immediately; a second submit while in flight
    /// is a no-op with a status-line notice (the caller's `.notify`), no
    /// queue -- v1 scope (§3.2).
    pub fn submit(&mut self, prompt: String, context: AiContext) {
        let Some(prepared) = self.prepare(prompt, context) else {
            return;
        };
        let runner = self.runner;
        thread::spawn(move || runner(prepared));
    }

    /// Call once per frame; drains the channel. Returns `true` if
    /// `history`/status changed (the caller should request a repaint).
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        let msgs: Vec<AiDisplayMessage> = rx.try_iter().collect();
        if msgs.is_empty() {
            return false;
        }
        let finished = matches!(
            msgs.last(),
            Some(AiDisplayMessage::Assistant(_)) | Some(AiDisplayMessage::Error(_))
        );
        if finished {
            self.rx = None;
        }
        let mut changed = false;
        for m in msgs {
            changed |= self.ingest(m);
        }
        changed
    }

    /// Apply one message from the channel to `history`/status.
    fn ingest(&mut self, msg: AiDisplayMessage) -> bool {
        match msg {
            AiDisplayMessage::User(t) => {
                self.history.push(AiDisplayMessage::User(t));
                true
            }
            AiDisplayMessage::ProviderServing(p) => {
                self.provider = Some(p);
                false
            }
            AiDisplayMessage::StreamingDelta(d) => {
                match self.history.last_mut() {
                    Some(AiDisplayMessage::Assistant(buf)) if self.streaming => buf.push_str(&d),
                    _ => {
                        self.history.push(AiDisplayMessage::Assistant(d));
                    }
                }
                self.streaming = true;
                true
            }
            AiDisplayMessage::Assistant(full) => {
                // Settle the accumulated streaming entry: the thread sent
                // the restored full text only after the stream closed.
                if self.streaming {
                    self.history.pop();
                }
                self.streaming = false;
                self.history.push(AiDisplayMessage::Assistant(full));
                true
            }
            AiDisplayMessage::Error(e) => {
                self.streaming = false;
                self.history.push(AiDisplayMessage::Error(e));
                true
            }
        }
    }
}

/// §3.3's masking rule: any cloud provider in the enabled chain forces the
/// (tighter) cloud threshold; a local-only chain masks at the local
/// threshold only when `sanitize_local` is set; local with sanitizing off
/// sends raw. Returns `None` for the only unmasked case.
fn decide_threshold(config: &AiConfig, order: &[ProviderId]) -> Option<f64> {
    let has_cloud = order
        .iter()
        .any(|id| !matches!(id, ProviderId::OllamaLocal));
    if has_cloud {
        Some(config.cloud_sanitize_threshold)
    } else if config.sanitize_local {
        Some(config.local_sanitize_threshold)
    } else {
        None
    }
}

/// The outbound half of the background run: sanitize `payload` when a
/// threshold applies, keeping the roundtrip map (thread-local) so the
/// reply can be restored; pass through untouched when unmasked.
fn mask_outgoing(
    payload: String,
    threshold: Option<f64>,
) -> (String, Option<HashMap<String, String>>) {
    let mut sanitizer = Sanitizer::new();
    match threshold {
        Some(t) => {
            let out = sanitizer.mask_with_threshold(&payload, t);
            (out.masked, Some(as_map(&sanitizer)))
        }
        None => (payload, None),
    }
}

/// The settle messages for one completed request run: the serving
/// provider (status line) and the restored reply, or the error.
fn settle(
    accumulated: String,
    map: &Option<HashMap<String, String>>,
    outcome: Result<ProviderId, AiError>,
    role_label: Option<&str>,
) -> Vec<AiDisplayMessage> {
    match outcome {
        Ok(provider) => {
            let label = match role_label {
                Some(role) => format!("{} ({role})", provider.label()),
                None => provider.label().to_string(),
            };
            vec![
                AiDisplayMessage::ProviderServing(label),
                AiDisplayMessage::Assistant(match map {
                    Some(map) => restore_originals(&accumulated, map),
                    None => accumulated,
                }),
            ]
        }
        Err(e) => vec![AiDisplayMessage::Error(e.to_string())],
    }
}

/// The background request run: mask (roundtrip map held only here) →
/// route via the two-attempt router → forward deltas live → restore
/// originals into the accumulated reply → settle with `ProviderServing` +
/// `Assistant`. One thread per request, owning its own tokio runtime
/// (§3.2).
fn run_request(prepared: PreparedRequest) {
    let PreparedRequest {
        payload,
        order,
        threshold,
        config,
        tx,
    } = prepared;
    if order.is_empty() {
        let _ = tx.send(AiDisplayMessage::Error(
            "no enabled AI providers".to_string(),
        ));
        return;
    }
    let (outgoing, map) = mask_outgoing(payload, threshold);
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = tx.send(AiDisplayMessage::Error(format!(
                "failed to build runtime: {e}"
            )));
            return;
        }
    };
    rt.block_on(async move {
        let sanitized = map.is_some();
        // Role resolution (§3.1) runs here, not in `prepare`: the
        // classifier call is async and would block the frame loop if it
        // ran synchronously on the UI thread.
        let role = if config.auto_route {
            classify_task_role(config.classifier_provider, &outgoing, sanitized).await
        } else {
            TaskRole::General
        };
        let route = resolve_role_route(&config, role);
        let role_label = config
            .auto_route
            .then(|| format!("{role:?}").to_lowercase());
        let (delta_tx, delta_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let messages = vec![ChatMessage::user(outgoing)];
        tokio::spawn(async move {
            let res = DefaultRouter
                .chat(
                    messages,
                    &route.provider_order,
                    route.model_override.as_deref(),
                    sanitized,
                    delta_tx,
                )
                .await;
            let _ = result_tx.send(res);
        });
        // Drive the router task forward while draining deltas as they
        // arrive: both run on this same thread's runtime.
        let mut accumulated = String::new();
        let mut result = None;
        while result.is_none() {
            while let Ok(Ok(delta)) = delta_rx.try_recv() {
                accumulated.push_str(&delta.text);
                if tx
                    .send(AiDisplayMessage::StreamingDelta(delta.text))
                    .is_err()
                {
                    return;
                }
            }
            if let Ok(res) = result_rx.try_recv() {
                result = Some(res);
            } else {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        // The result is sent after every delta, so anything left queued
        // belongs to this request.
        while let Ok(Ok(delta)) = delta_rx.try_recv() {
            accumulated.push_str(&delta.text);
            let _ = tx.send(AiDisplayMessage::StreamingDelta(delta.text));
        }
        let outcome = result.expect("set by the loop above");
        for msg in settle(accumulated, &map, outcome, role_label.as_deref()) {
            let _ = tx.send(msg);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_root() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ide-ai-panel-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn fresh_panel_is_idle_and_poll_is_false() {
        let mut panel = AiPanel::new(temp_root());
        assert!(!panel.is_in_flight());
        assert!(!panel.poll());
        assert!(!panel.sanitized);
    }

    #[test]
    fn compose_folds_context_into_the_prompt() {
        let panel = AiPanel::new(temp_root());
        assert_eq!(
            panel.compose("explain".into(), AiContext::None),
            Some("explain".to_string())
        );
        let sel = panel.compose(
            "explain".into(),
            AiContext::Selection("fn main() {}".into()),
        );
        assert_eq!(
            sel.unwrap(),
            "explain\n\nSelected code:\n```\nfn main() {}\n```"
        );
        let whole = panel.compose(
            "review".into(),
            AiContext::WholeFile("pub struct A;".into()),
        );
        assert_eq!(
            whole.unwrap(),
            "review\n\nFull file:\n```\npub struct A;\n```"
        );
    }

    #[test]
    fn compose_rejects_blank_prompt() {
        let panel = AiPanel::new(temp_root());
        assert_eq!(panel.compose("   ".into(), AiContext::None), None);
        assert_eq!(panel.compose(String::new(), AiContext::None), None);
    }

    #[test]
    fn prepare_pushes_user_message_and_chooses_config_driven_threshold() {
        // No `.ide/ai.json`: defaults → [OllamaLocal, ...clouds filtered by
        // creds absent] → local-only chain → local threshold 4.0, masked.
        let mut panel = AiPanel::new(temp_root());
        let prepared = panel.prepare("hi".into(), AiContext::None).unwrap();
        assert_eq!(
            panel.history,
            vec![AiDisplayMessage::User("hi".to_string())]
        );
        assert!(panel.is_in_flight());
        assert!(panel.sanitized);
        assert_eq!(prepared.threshold, Some(4.0));
        assert_eq!(prepared.payload, "hi");
    }

    #[test]
    fn prepare_is_noop_while_in_flight() {
        let mut panel = AiPanel::new(temp_root());
        assert!(panel.prepare("first".into(), AiContext::None).is_some());
        let second = panel.prepare("second".into(), AiContext::None);
        assert!(second.is_none());
        assert_eq!(
            panel.history.len(),
            1,
            "in-flight submit pushes nothing new"
        );
    }

    #[test]
    fn cancel_clears_in_flight_and_lets_a_new_request_start() {
        let mut panel = AiPanel::new(temp_root());
        panel.prepare("first".into(), AiContext::None).unwrap();
        assert!(panel.is_in_flight());

        panel.cancel();
        assert!(!panel.is_in_flight());

        // A fresh submit is no longer blocked by the (now-detached)
        // in-flight state.
        let second = panel.prepare("second".into(), AiContext::None);
        assert!(second.is_some());
        assert_eq!(
            panel.history,
            vec![
                AiDisplayMessage::User("first".to_string()),
                AiDisplayMessage::User("second".to_string()),
            ]
        );
    }

    #[test]
    fn cancel_is_a_noop_when_nothing_is_in_flight() {
        let mut panel = AiPanel::new(temp_root());
        panel.cancel();
        assert!(!panel.is_in_flight());
        assert!(panel.history.is_empty());
    }

    #[test]
    fn cancel_orphans_the_old_channel_so_late_sends_are_silently_dropped() {
        // A message the (now-orphaned) background thread tries to send
        // after `cancel` must not resurrect the old request in `history`
        // via a later `poll` -- there is no receiver left to drain.
        let mut panel = AiPanel::new(temp_root());
        let prepared = panel.prepare("first".into(), AiContext::None).unwrap();
        panel.cancel();
        assert!(prepared
            .tx
            .send(AiDisplayMessage::Assistant("late reply".to_string()))
            .is_err());
        assert!(!panel.poll());
        assert_eq!(
            panel.history,
            vec![AiDisplayMessage::User("first".to_string())]
        );
    }

    #[test]
    fn prepare_rejects_blank_prompt_even_when_idle() {
        let mut panel = AiPanel::new(temp_root());
        assert!(panel.prepare("   ".into(), AiContext::None).is_none());
        assert!(panel.history.is_empty());
        assert!(!panel.is_in_flight());
    }

    #[test]
    fn poll_drains_deltas_and_settles_on_assistant() {
        let mut panel = AiPanel::new(temp_root());
        let prepared = panel.prepare("hi".into(), AiContext::None).unwrap();
        prepared
            .tx
            .send(AiDisplayMessage::StreamingDelta("hel".to_string()))
            .unwrap();
        prepared
            .tx
            .send(AiDisplayMessage::StreamingDelta("lo".to_string()))
            .unwrap();
        prepared
            .tx
            .send(AiDisplayMessage::ProviderServing(
                "local (ollama)".to_string(),
            ))
            .unwrap();
        prepared
            .tx
            .send(AiDisplayMessage::Assistant("hello restored".to_string()))
            .unwrap();

        assert!(panel.poll());
        assert!(!panel.is_in_flight());

        let expected = vec![
            AiDisplayMessage::User("hi".to_string()),
            AiDisplayMessage::Assistant("hello restored".to_string()),
        ];
        assert_eq!(panel.history, expected);
        assert_eq!(panel.provider.as_deref(), Some("local (ollama)"));
    }

    #[test]
    fn poll_settles_error_keeping_partial_reply() {
        let mut panel = AiPanel::new(temp_root());
        let prepared = panel.prepare("hi".into(), AiContext::None).unwrap();
        prepared
            .tx
            .send(AiDisplayMessage::StreamingDelta("partial".to_string()))
            .unwrap();
        prepared
            .tx
            .send(AiDisplayMessage::Error("boom".to_string()))
            .unwrap();

        assert!(panel.poll());
        assert!(!panel.is_in_flight());
        assert_eq!(
            panel.history,
            vec![
                AiDisplayMessage::User("hi".to_string()),
                AiDisplayMessage::Assistant("partial".to_string()),
                AiDisplayMessage::Error("boom".to_string()),
            ]
        );
    }

    #[test]
    fn poll_with_only_deltas_stays_in_flight() {
        let mut panel = AiPanel::new(temp_root());
        let prepared = panel.prepare("hi".into(), AiContext::None).unwrap();
        prepared
            .tx
            .send(AiDisplayMessage::StreamingDelta("a".to_string()))
            .unwrap();
        assert!(panel.poll());
        assert!(panel.is_in_flight(), "no terminal message yet");
    }

    #[test]
    fn decide_threshold_follows_route() {
        let default_cloud = AiConfig {
            provider_order: vec![ProviderId::OllamaLocal, ProviderId::Groq],
            ..AiConfig::default()
        };
        assert_eq!(
            decide_threshold(&default_cloud, &default_cloud.provider_order),
            Some(3.5)
        );
        let local_on = AiConfig {
            provider_order: vec![ProviderId::OllamaLocal],
            sanitize_local: true,
            ..AiConfig::default()
        };
        assert_eq!(
            decide_threshold(&local_on, &local_on.provider_order),
            Some(4.0)
        );
        let local_off = AiConfig {
            provider_order: vec![ProviderId::OllamaLocal],
            sanitize_local: false,
            ..AiConfig::default()
        };
        assert_eq!(
            decide_threshold(&local_off, &local_off.provider_order),
            None
        );
    }

    #[test]
    fn run_request_with_no_provider_pushes_error() {
        let (tx, rx) = mpsc::channel();
        run_request(PreparedRequest {
            payload: "hi".into(),
            order: Vec::new(),
            threshold: None,
            config: AiConfig::default(),
            tx,
        });
        let msg = rx.recv().unwrap();
        assert!(matches!(
            msg,
            AiDisplayMessage::Error(e) if e == "no enabled AI providers"
        ));
    }

    fn secret_token() -> String {
        "cRx7kL9pQw2vN4mBxZdF6gHj8sT1uYw0eDrTaCb".to_string()
    }

    #[test]
    fn mask_outgoing_blanks_secrets_and_passes_through_unmasked() {
        let payload = format!("password is {}", secret_token());
        let (masked, map) = mask_outgoing(payload, Some(4.0));
        assert!(
            !masked.contains(&secret_token()),
            "high-entropy token is masked"
        );
        assert!(map.is_some());
        assert!(masked.contains("password is"));

        let (passed, map2) = mask_outgoing("hi".to_string(), None);
        assert_eq!(passed, "hi");
        assert!(map2.is_none());
    }

    #[test]
    fn settle_restores_masked_reply_against_the_roundtrip_map() {
        let payload = format!("password is {}", secret_token());
        let (masked, map) = mask_outgoing(payload.clone(), Some(4.0));
        assert_ne!(masked, payload, "masking actually changed the payload");
        let echoed = ChatMessage::user(masked.clone()).text; // the model "echoes" the masked payload
        let msgs = settle(echoed, &map, Ok(ProviderId::OllamaLocal), None);
        assert!(matches!(
            &msgs[0],
            AiDisplayMessage::ProviderServing(p) if !p.is_empty()
        ));
        match &msgs[1] {
            AiDisplayMessage::Assistant(restored) => {
                assert!(
                    restored.contains(&secret_token()),
                    "secret is restored into the reply"
                );
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn settle_passes_through_unmasked_reply() {
        let msgs = settle(
            "plain reply".to_string(),
            &None,
            Ok(ProviderId::OllamaLocal),
            None,
        );
        assert!(matches!(
            &msgs[1],
            AiDisplayMessage::Assistant(a) if a == "plain reply"
        ));
    }

    #[test]
    fn settle_builds_error_message() {
        let msgs = settle(String::new(), &None, Err(AiError::Unsupported), None);
        assert!(matches!(&msgs[0], AiDisplayMessage::Error(e) if e.contains("does not support")));
    }

    #[test]
    fn settle_appends_role_label_to_the_provider_status_line_when_present() {
        let msgs = settle(
            "reply".to_string(),
            &None,
            Ok(ProviderId::OllamaLocal),
            Some("planning"),
        );
        assert!(matches!(
            &msgs[0],
            AiDisplayMessage::ProviderServing(p) if p.ends_with(" (planning)")
        ));
    }
}
