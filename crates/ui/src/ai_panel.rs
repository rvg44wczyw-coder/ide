//! Hybrid local/cloud AI assistant dock tab (`docs/features/
//! gui-ai-orchestration.md` §2.2, G9) — the GUI counterpart of
//! `crates/tui/src/ai_panel.rs` (T49/T55). One background thread per
//! request, that thread's own `tokio` current-thread runtime drives the
//! two-attempt router while the same thread concurrently drains the delta
//! channel -- so `StreamingDelta`s arrive live, never in a burst when the
//! reply completes (§3.2). The outgoing payload is passed through
//! `ide_sanitizer` inside that thread on every route that masks (§3.3): the
//! masking decision (local vs cloud threshold) is made synchronously in
//! `prepare` so `sanitized` is truthful for the status line, but the
//! sanitizing pass itself, the roundtrip map's lifetime, and the restore
//! are all thread-local.
//!
//! Deliberate deviation from `ide-tui`'s `AiPanel`: `prepare`/`submit` take
//! `project_root: &Path` per call instead of caching a `root: PathBuf`
//! field, matching this crate's own established convention
//! (`CargoPanel::run`, `CustomActionsPanel::run_selected`) for panels that
//! must work across a mid-session project switch, which `ide-tui` never
//! does.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use ide_ai::{
    classify_task_role, decide_threshold, mask_outgoing, resolve_role_route, AiConfig, AiError,
    ChatMessage, DefaultRouter, ProviderId, Router, TaskRole,
};
use ide_sanitizer::restore_originals;

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

/// Cap on selection/whole-file context chars folded into one outgoing
/// payload (`compose`, below) -- same order of magnitude as `ide_ai::
/// MAX_REPLY_CHARS`, the closest existing precedent for "how much text is
/// a reasonable single AI turn" on the inbound side.
const MAX_CONTEXT_CHARS: usize = 100_000;

/// Truncates `text` to at most `MAX_CONTEXT_CHARS` chars (never splitting
/// a multi-byte char), returning the possibly-shortened text and a marker
/// string to append (empty when nothing was cut).
fn truncate_context(text: &str) -> (String, &'static str) {
    if text.chars().count() <= MAX_CONTEXT_CHARS {
        (text.to_string(), "")
    } else {
        (
            text.chars().take(MAX_CONTEXT_CHARS).collect(),
            "\n...(truncated)",
        )
    }
}

/// Everything a background request run needs, produced by
/// `AiPanel::prepare` and consumed (spawned) by `submit`. `tx` lets tests
/// feed the panel's channel directly without spawning a thread.
pub struct PreparedRequest {
    pub(crate) payload: String,
    order: Vec<ProviderId>,
    threshold: Option<f64>,
    /// Carried through so `run_request` can resolve a `TaskRole`/
    /// `RoleRoute` on the background thread (T55 §2.2) -- role resolution
    /// (the classifier call, when `auto_route` is set) is async and must
    /// not run in `prepare`'s synchronous, frame-blocking path.
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
    /// (for the status line) -- §3.3.
    pub sanitized: bool,
    /// The last provider that served a reply, for the status line.
    pub(crate) provider: Option<String>,
    /// True while an assistant reply is still accumulating over the
    /// channel (drives `ingest`'s append-vs-new-reply decision).
    streaming: bool,
    rx: Option<Receiver<AiDisplayMessage>>,
    runner: AiRunner,
}

impl Default for AiPanel {
    fn default() -> Self {
        Self::with_runner(run_request)
    }
}

impl AiPanel {
    /// `pub(crate)`, not private: `app.rs`'s own tests swap in a fake
    /// runner before exercising the AI dock tab's `Enter` path, so `App`'s
    /// test suite never opens a socket to a real provider.
    pub(crate) fn with_runner(runner: AiRunner) -> Self {
        Self {
            input: String::new(),
            history: Vec::new(),
            sanitized: false,
            provider: None,
            streaming: false,
            rx: None,
            runner,
        }
    }

    /// Returns true if a reply is still streaming.
    pub fn is_in_flight(&self) -> bool {
        self.rx.is_some()
    }

    /// Manual recovery backstop for a wedged request, identical contract to
    /// `ide-tui`'s own `cancel`: drops the receiver so `poll` stops waiting
    /// on it and a fresh `submit` can start immediately. The orphaned
    /// background thread keeps running until its own transport timeout or
    /// a failed `tx.send`. A no-op when nothing is in flight.
    pub fn cancel(&mut self) {
        self.rx = None;
        self.streaming = false;
    }

    /// Compose the single line that actually leaves for the provider:
    /// prompt + any selection/whole-file context folded in. `None` for a
    /// blank/whitespace-only prompt (no request). Selection/whole-file text
    /// is capped at `MAX_CONTEXT_CHARS` (`hacker` finding #1,
    /// `docs/security-findings/rust-ui-dev-gui-ai-orchestration-2026-09-08.md`)
    /// -- `ide-ai` bounds the FIM/classifier/reply paths but has no
    /// equivalent cap for an ordinary chat body, so an unbounded whole-file
    /// send (e.g. a large file in an untrusted cloned repo) is a
    /// memory/bandwidth/cost-DoS surface if left uncapped here.
    fn compose(&self, prompt: String, context: AiContext) -> Option<String> {
        if prompt.trim().is_empty() {
            return None;
        }
        Some(match context {
            AiContext::None => prompt,
            AiContext::Selection(sel) => {
                let (sel, marker) = truncate_context(&sel);
                format!("{prompt}\n\nSelected code:\n```\n{sel}{marker}\n```")
            }
            AiContext::WholeFile(file) => {
                let (file, marker) = truncate_context(&file);
                format!("{prompt}\n\nFull file:\n```\n{file}{marker}\n```")
            }
        })
    }

    /// Records the user turn and prepares the request run. Pure apart from
    /// the tiny `.ide/ai.json` config read, so tests exercise it directly
    /// and `submit` is just the spawn.
    fn prepare(
        &mut self,
        prompt: String,
        context: AiContext,
        project_root: &Path,
    ) -> Option<PreparedRequest> {
        let payload = self.compose(prompt, context)?;
        if self.is_in_flight() {
            return None;
        }
        self.history.push(AiDisplayMessage::User(payload.clone()));
        let config = AiConfig::load(project_root);
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

    /// Appends input->history, prepares the masking decision + threshold,
    /// and spawns the one background thread that sanitizes, routes,
    /// streams and restores. Returns immediately; a second submit while in
    /// flight is a silent no-op, no queue -- same v1 scope as
    /// `ClaudePanel`/`CargoPanel`/`CustomActionsPanel` (§3.2).
    pub fn submit(&mut self, prompt: String, context: AiContext, project_root: &Path) {
        let Some(prepared) = self.prepare(prompt, context, project_root) else {
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

/// The background request run: mask (roundtrip map held only here) ->
/// route via the two-attempt router -> forward deltas live -> restore
/// originals into the accumulated reply -> settle with `ProviderServing` +
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_root() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ide-ui-ai-panel-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn fresh_panel_is_idle_and_poll_is_false() {
        let mut panel = AiPanel::default();
        assert!(!panel.is_in_flight());
        assert!(!panel.poll());
        assert!(!panel.sanitized);
    }

    #[test]
    fn compose_folds_context_into_the_prompt() {
        let panel = AiPanel::default();
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
        let panel = AiPanel::default();
        assert_eq!(panel.compose("   ".into(), AiContext::None), None);
        assert_eq!(panel.compose(String::new(), AiContext::None), None);
    }

    #[test]
    fn compose_truncates_an_oversized_whole_file_context() {
        let panel = AiPanel::default();
        let huge = "x".repeat(MAX_CONTEXT_CHARS + 10);
        let composed = panel
            .compose("review".into(), AiContext::WholeFile(huge))
            .unwrap();
        assert!(composed.contains("...(truncated)"));
        // prompt + fences + marker overhead, but the embedded file body
        // itself must not exceed the cap.
        let body_len = composed
            .split("```\n")
            .nth(1)
            .unwrap()
            .trim_end_matches("\n```")
            .trim_end_matches("\n...(truncated)")
            .chars()
            .count();
        assert_eq!(body_len, MAX_CONTEXT_CHARS);
    }

    #[test]
    fn compose_truncates_an_oversized_selection_context() {
        let panel = AiPanel::default();
        let huge = "y".repeat(MAX_CONTEXT_CHARS + 1);
        let composed = panel
            .compose("explain".into(), AiContext::Selection(huge))
            .unwrap();
        assert!(composed.contains("...(truncated)"));
    }

    #[test]
    fn compose_does_not_truncate_context_at_or_under_the_cap() {
        let panel = AiPanel::default();
        let exactly_at_cap = "z".repeat(MAX_CONTEXT_CHARS);
        let composed = panel
            .compose("review".into(), AiContext::WholeFile(exactly_at_cap))
            .unwrap();
        assert!(!composed.contains("truncated"));
    }

    #[test]
    fn truncate_context_never_splits_a_multi_byte_char() {
        let text = "é".repeat(MAX_CONTEXT_CHARS + 5);
        let (truncated, marker) = truncate_context(&text);
        assert_eq!(truncated.chars().count(), MAX_CONTEXT_CHARS);
        assert_eq!(marker, "\n...(truncated)");
        // Every char in the result must still be valid UTF-8 -- this would
        // already be guaranteed by `String`'s own invariants, but the
        // assertion documents the intent explicitly.
        assert!(truncated.chars().all(|c| c == 'é'));
    }

    #[test]
    fn prepare_pushes_user_message_and_chooses_config_driven_threshold() {
        // No `.ide/ai.json`: defaults -> [OllamaLocal, ...clouds filtered
        // by creds absent] -> local-only chain -> local threshold 4.0,
        // masked.
        let mut panel = AiPanel::default();
        let root = temp_root();
        let prepared = panel.prepare("hi".into(), AiContext::None, &root).unwrap();
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
        let mut panel = AiPanel::default();
        let root = temp_root();
        assert!(panel
            .prepare("first".into(), AiContext::None, &root)
            .is_some());
        let second = panel.prepare("second".into(), AiContext::None, &root);
        assert!(second.is_none());
        assert_eq!(
            panel.history.len(),
            1,
            "in-flight submit pushes nothing new"
        );
    }

    #[test]
    fn cancel_clears_in_flight_and_lets_a_new_request_start() {
        let mut panel = AiPanel::default();
        let root = temp_root();
        panel
            .prepare("first".into(), AiContext::None, &root)
            .unwrap();
        assert!(panel.is_in_flight());

        panel.cancel();
        assert!(!panel.is_in_flight());

        // A fresh submit is no longer blocked by the (now-detached)
        // in-flight state.
        let second = panel.prepare("second".into(), AiContext::None, &root);
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
        let mut panel = AiPanel::default();
        panel.cancel();
        assert!(!panel.is_in_flight());
        assert!(panel.history.is_empty());
    }

    #[test]
    fn cancel_orphans_the_old_channel_so_late_sends_are_silently_dropped() {
        // A message the (now-orphaned) background thread tries to send
        // after `cancel` must not resurrect the old request in `history`
        // via a later `poll` -- there is no receiver left to drain.
        let mut panel = AiPanel::default();
        let root = temp_root();
        let prepared = panel
            .prepare("first".into(), AiContext::None, &root)
            .unwrap();
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
        let mut panel = AiPanel::default();
        let root = temp_root();
        assert!(panel
            .prepare("   ".into(), AiContext::None, &root)
            .is_none());
        assert!(panel.history.is_empty());
        assert!(!panel.is_in_flight());
    }

    #[test]
    fn poll_drains_deltas_and_settles_on_assistant() {
        let mut panel = AiPanel::default();
        let root = temp_root();
        let prepared = panel.prepare("hi".into(), AiContext::None, &root).unwrap();
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
        let mut panel = AiPanel::default();
        let root = temp_root();
        let prepared = panel.prepare("hi".into(), AiContext::None, &root).unwrap();
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
        let mut panel = AiPanel::default();
        let root = temp_root();
        let prepared = panel.prepare("hi".into(), AiContext::None, &root).unwrap();
        prepared
            .tx
            .send(AiDisplayMessage::StreamingDelta("a".to_string()))
            .unwrap();
        assert!(panel.poll());
        assert!(panel.is_in_flight(), "no terminal message yet");
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
