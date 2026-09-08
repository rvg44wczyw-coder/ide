//! `AgentLoop`/`AgentHandle`/`AgentEvent` — drives one user message through
//! the tool-calling loop (`docs/features/tui-local-agent.md` §2.1, §3).

use std::sync::mpsc::Sender;
use std::time::Duration;

use ide_ai::{AiError, ChatMessage, ChatRole, DefaultRouter, ProviderId, Router};
use ide_sanitizer::Sanitizer;

use crate::executor::ToolExecutor;
use crate::protocol::{final_system_prompt, parse_tool_call, system_prompt, ParseOutcome};
use crate::tool::{AgentTool, DebugAction, PermissionMode, ToolError, ToolResult};

/// Bounded round-trip cap: an agent run can call at most this many tools
/// before being forced to stop and answer with whatever it has. Sized off
/// this doc's own worked examples: a realistic multi-step task (read,
/// edit, run tests, read the failure, fix, re-run) lands around 6-10 tool
/// calls, so 15 leaves headroom for one extra retry cycle while still
/// bounding a runaway/adversarial loop to a human-noticeable number of
/// steps.
pub const MAX_AGENT_STEPS: usize = 15;

/// Each tool result fed back to the model is truncated to this many chars
/// before being added to the conversation -- a `RunShellCommand`/
/// `ReadDockerLogs`/`ReadFile` result can otherwise be arbitrarily large
/// and blow the context window in one step. ~2,000 tokens, matching T55's
/// per-message budget reasoning.
pub const MAX_TOOL_RESULT_CHARS: usize = 8_000;

/// Wall-clock cap on one `RunShellCommand`/`ReadDockerLogs` subprocess.
/// Neither has any other bound on how long it can run -- `Command::output`
/// blocks until the child exits, and an unconditionally-allowlisted
/// command like `cat /dev/zero` never does (`hacker` finding 2,
/// 2026-09-08: live-tested, confirmed still running after 3s with nothing
/// in the codebase to stop it). 30s comfortably covers a `cargo build`/
/// `cargo test`-shaped command while still bounding a hang to a
/// human-noticeable, not-infinite wait. `run_one_tool` races the tool
/// execution against this timeout (and, separately, against the loop's
/// own resume channel closing/firing, which is what actually detects a
/// user cancellation) via `tokio::select!`; the losing future is dropped,
/// and `Command::kill_on_drop(true)` (set on every subprocess this crate
/// spawns) kills the child at that point instead of leaving it to run
/// detached.
pub const TOOL_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Small, char-boundary-safe duplicate of `ide_ai`'s crate-private
/// `truncate` -- that one has no `pub` modifier, so `ide-agent` cannot
/// depend on it.
fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    } else {
        s.to_string()
    }
}

/// §3.4: even in `Auto` mode, `RunShellCommand` only runs unattended when
/// it matches this fixed, non-configurable allowlist. `program` must be a
/// single bare-name path component -- any directory separator at all
/// (`./cargo`, `../x`, `sub/cargo`, `/usr/bin/git`) is rejected outright,
/// *before* the file-stem comparison below. A path-qualified `program`
/// bypasses `$PATH` resolution entirely and runs whatever file sits at
/// that literal location instead of the trusted system binary -- and
/// `file_stem()` alone can't tell `"./cargo"` apart from `"cargo"`, since
/// both stem to `"cargo"` (`hacker` finding 1, 2026-09-08: live-tested
/// planting an executable file literally named `cargo` in the project
/// root and running it, unattended, via `program: "./cargo"`). Rejecting
/// every path-qualified form -- including a full path to what really is
/// the legitimate system binary -- is a deliberate, stricter tradeoff:
/// there is no way from a string alone to distinguish "the user's real
/// `/usr/bin/git`" from "an attacker's file that happens to live at
/// `/usr/bin/git`"; only a bare name, resolved via `$PATH` the same way a
/// human typing it at a shell would get, is trustworthy here.
fn is_allowlisted(program: &str, args: &[String]) -> bool {
    if std::path::Path::new(program).components().count() != 1 {
        return false;
    }
    let stem = std::path::Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    let first_arg = args.first().map(String::as_str);
    match stem.as_str() {
        "cargo" | "go" | "ls" | "cat" | "grep" => true,
        "git" => matches!(
            first_arg,
            Some("status" | "diff" | "log" | "show" | "branch" | "blame" | "remote" | "fetch")
        ),
        "docker" => matches!(first_arg, Some("ps" | "inspect" | "images")),
        _ => false,
    }
}

fn mask(text: &str, threshold: Option<f64>) -> String {
    match threshold {
        Some(t) => Sanitizer::new().mask_with_threshold(text, t).masked,
        None => text.to_string(),
    }
}

#[derive(Debug)]
pub enum AgentEvent {
    /// A tool is about to run and needs approval. The loop is paused until
    /// `AgentHandle::resume_with_decision` is called.
    AwaitingApproval {
        tool: AgentTool,
    },
    /// An `AgentTool::DebugControl(action)` call that survived
    /// permission-mode gating -- `ToolExecutor` cannot run it, so the
    /// caller must execute `action` against its own real debug session and
    /// call `AgentHandle::resume_with_debug_result`.
    AwaitingDebugExecution {
        action: DebugAction,
    },
    ToolStarted {
        tool: AgentTool,
    },
    ToolFinished {
        result: ToolResult,
    },
    /// A ```` ```tool_call ```` fence was present in the model's turn but
    /// failed to parse -- distinct from a turn with no fence at all, which
    /// is an ordinary final answer and never fires this.
    ToolCallParseFailed {
        raw_snippet: String,
    },
    /// Streaming text delta from the model's current turn.
    ModelDelta {
        text: String,
    },
    /// The model produced a final plain-text answer (no further tool
    /// call) or the step cap was hit.
    Done {
        reason: DoneReason,
    },
}

#[derive(Debug)]
pub enum DoneReason {
    FinalAnswer,
    StepLimitReached,
    Error(AiError),
}

/// Zero-sized, `Clone`-able wrapper around `ide_ai::DefaultRouter` --
/// `DefaultRouter` itself has no `Clone` impl, but `stream_turn` needs to
/// move an owned router into a spawned task on every call (mirroring
/// `ai_panel.rs`'s `run_request`, which spawns a fresh `DefaultRouter.chat`
/// task per request). Defined here rather than adding `#[derive(Clone)]`
/// to `ide_ai::DefaultRouter` itself, which is out of this crate's scope.
#[derive(Clone)]
struct DefaultRouterHandle;

impl Router for DefaultRouterHandle {
    fn chat(
        &self,
        messages: Vec<ChatMessage>,
        order: &[ProviderId],
        model_override: Option<&str>,
        sanitized: bool,
        tx: Sender<Result<ide_ai::ChatDelta, AiError>>,
    ) -> impl std::future::Future<Output = Result<ProviderId, AiError>> + Send {
        DefaultRouter.chat(messages, order, model_override, sanitized, tx)
    }
}

/// What the caller sends back into a paused loop.
#[derive(Debug)]
pub enum AgentResume {
    Approval(bool),
    Debug(Result<String, ToolError>),
}

/// Drives one user message through the tool-calling loop.
pub struct AgentLoop {
    mode: PermissionMode,
    executor: ToolExecutor,
    events: Sender<AgentEvent>,
    resume_rx: tokio::sync::mpsc::UnboundedReceiver<AgentResume>,
    /// `TOOL_EXECUTION_TIMEOUT` in normal operation; shortened by
    /// `#[cfg(test)] with_tool_timeout` so timeout-handling tests don't
    /// need to wait 30 real seconds.
    tool_timeout: Duration,
}

/// The only way the caller interacts with a running `AgentLoop` after
/// spawning it -- a cheap, `Clone`-able sender into `AgentLoop`'s internal
/// resume channel. Deliberately plain (synchronous) methods, not `async
/// fn`: `ide-tui`'s main thread that calls these has no `tokio` runtime of
/// its own (only the background thread `AgentLoop::run` is moved onto
/// does), and `tokio::sync::mpsc::UnboundedSender::send` is already
/// non-blocking, so there is nothing an `async` signature would add here
/// except an executor `ide-tui`'s main loop doesn't otherwise need.
#[derive(Clone)]
pub struct AgentHandle {
    resume_tx: tokio::sync::mpsc::UnboundedSender<AgentResume>,
}

impl AgentHandle {
    /// Resumes a loop paused on `AgentEvent::AwaitingApproval`.
    pub fn resume_with_decision(&self, approved: bool) {
        let _ = self.resume_tx.send(AgentResume::Approval(approved));
    }

    /// Resumes a loop paused on `AgentEvent::AwaitingDebugExecution` with
    /// the outcome of running that action against the caller's own real
    /// debug session.
    pub fn resume_with_debug_result(&self, result: Result<String, ToolError>) {
        let _ = self.resume_tx.send(AgentResume::Debug(result));
    }
}

impl AgentLoop {
    /// Constructs a loop and a cheap, cloneable `AgentHandle` for resuming
    /// it later. `run` takes `self` by value: the loop is meant to be
    /// moved onto its own background thread and run to completion there.
    pub fn new(
        mode: PermissionMode,
        executor: ToolExecutor,
        events: Sender<AgentEvent>,
    ) -> (Self, AgentHandle) {
        let (resume_tx, resume_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                mode,
                executor,
                events,
                resume_rx,
                tool_timeout: TOOL_EXECUTION_TIMEOUT,
            },
            AgentHandle { resume_tx },
        )
    }

    /// Test-only seam: shortens `tool_timeout` so a timeout-handling test
    /// doesn't need to wait `TOOL_EXECUTION_TIMEOUT` (30s) of real time.
    #[cfg(test)]
    pub(crate) fn with_tool_timeout(mut self, timeout: Duration) -> Self {
        self.tool_timeout = timeout;
        self
    }

    /// Runs until `AgentEvent::Done` or the `events` channel closes.
    ///
    /// Deviates from the doc's originally specified `sanitized: bool`
    /// parameter: masking each tool result before it re-enters the
    /// conversation (§3.3) requires the actual threshold value, not just
    /// whether one applies -- a `bool` alone can't reproduce
    /// `mask_outgoing`'s masking, only whether it happened. `sanitized` for
    /// each `Router::chat` call is derived as `threshold.is_some()`, the
    /// same rule `decide_threshold`'s caller already applies.
    pub async fn run(
        self,
        history: Vec<ChatMessage>,
        order: &[ProviderId],
        model_override: Option<&str>,
        threshold: Option<f64>,
    ) {
        self.run_with_router(
            DefaultRouterHandle,
            history,
            order,
            model_override,
            threshold,
        )
        .await
    }

    /// The real loop logic, generic over `Router` so tests can substitute
    /// a fake one instead of dispatching to a real provider. `run` is the
    /// only public entry point; this stays crate-private.
    pub(crate) async fn run_with_router<R: Router + Clone + Send + Sync + 'static>(
        mut self,
        router: R,
        history: Vec<ChatMessage>,
        order: &[ProviderId],
        model_override: Option<&str>,
        threshold: Option<f64>,
    ) {
        let sanitized = threshold.is_some();
        let mut turns = history;

        for _ in 0..MAX_AGENT_STEPS {
            let mut messages = Vec::with_capacity(turns.len() + 1);
            messages.push(ChatMessage::user(system_prompt()));
            messages.extend(turns.iter().cloned());

            let turn_text = match self
                .stream_turn(&router, messages, order, model_override, sanitized)
                .await
            {
                Ok(text) => text,
                Err(e) => {
                    let _ = self.events.send(AgentEvent::Done {
                        reason: DoneReason::Error(e),
                    });
                    return;
                }
            };
            turns.push(ChatMessage {
                role: ChatRole::Assistant,
                text: turn_text.clone(),
            });

            match parse_tool_call(&turn_text) {
                ParseOutcome::None => {
                    let _ = self.events.send(AgentEvent::Done {
                        reason: DoneReason::FinalAnswer,
                    });
                    return;
                }
                ParseOutcome::Malformed(raw_snippet) => {
                    let _ = self
                        .events
                        .send(AgentEvent::ToolCallParseFailed { raw_snippet });
                    let _ = self.events.send(AgentEvent::Done {
                        reason: DoneReason::FinalAnswer,
                    });
                    return;
                }
                ParseOutcome::Tool(tool) => {
                    let feedback = self.run_one_tool(tool, threshold).await;
                    turns.push(ChatMessage::user(feedback));
                }
            }
        }

        // Step limit reached: one final call, tools omitted entirely --
        // even if the model emits a fence anyway, its output is never
        // parsed as a tool call for this turn (§3.2).
        let mut messages = Vec::with_capacity(turns.len() + 1);
        messages.push(ChatMessage::user(final_system_prompt()));
        messages.extend(turns);
        let _ = self
            .stream_turn(&router, messages, order, model_override, sanitized)
            .await;
        let _ = self.events.send(AgentEvent::Done {
            reason: DoneReason::StepLimitReached,
        });
    }

    /// Streams one `Router::chat` call to completion, forwarding every
    /// delta as `AgentEvent::ModelDelta` as it arrives -- the same
    /// spawn-a-task-and-drain-concurrently shape `ai_panel.rs`'s
    /// `run_request` already uses, so deltas stream live instead of
    /// arriving in one burst when the reply completes.
    async fn stream_turn<R: Router + Clone + Send + Sync + 'static>(
        &self,
        router: &R,
        messages: Vec<ChatMessage>,
        order: &[ProviderId],
        model_override: Option<&str>,
        sanitized: bool,
    ) -> Result<String, AiError> {
        let (delta_tx, delta_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let router = router.clone();
        let order = order.to_vec();
        let model_override = model_override.map(str::to_string);
        tokio::spawn(async move {
            let res = router
                .chat(
                    messages,
                    &order,
                    model_override.as_deref(),
                    sanitized,
                    delta_tx,
                )
                .await;
            let _ = result_tx.send(res);
        });
        let mut accumulated = String::new();
        let mut result = None;
        while result.is_none() {
            while let Ok(Ok(delta)) = delta_rx.try_recv() {
                accumulated.push_str(&delta.text);
                let _ = self
                    .events
                    .send(AgentEvent::ModelDelta { text: delta.text });
            }
            if let Ok(res) = result_rx.try_recv() {
                result = Some(res);
            } else {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        while let Ok(Ok(delta)) = delta_rx.try_recv() {
            accumulated.push_str(&delta.text);
            let _ = self
                .events
                .send(AgentEvent::ModelDelta { text: delta.text });
        }
        result
            .expect("set by the loop above")
            .map(|_provider| accumulated)
    }

    /// Handles one tool call end to end: permission-mode gating, the
    /// approval pause, `DebugControl` interception, real execution, and
    /// masking the result before it re-enters the conversation. Returns
    /// the (masked, truncated) text to append as the next user turn.
    async fn run_one_tool(&mut self, tool: AgentTool, threshold: Option<f64>) -> String {
        let _ = self
            .events
            .send(AgentEvent::ToolStarted { tool: tool.clone() });

        if self.mode == PermissionMode::Plan && tool.is_mutating() {
            return self.finish(
                ToolResult {
                    tool,
                    outcome: Err(ToolError::Denied),
                },
                threshold,
            );
        }

        let needs_approval = tool.is_mutating()
            && match self.mode {
                PermissionMode::Approve => true,
                PermissionMode::Auto => matches!(
                    &tool,
                    AgentTool::RunShellCommand { program, args } if !is_allowlisted(program, args)
                ),
                PermissionMode::Plan => false,
            };

        if needs_approval {
            let _ = self
                .events
                .send(AgentEvent::AwaitingApproval { tool: tool.clone() });
            let approved = matches!(
                self.resume_rx.recv().await,
                Some(AgentResume::Approval(true))
            );
            if !approved {
                return self.finish(
                    ToolResult {
                        tool,
                        outcome: Err(ToolError::UserDenied),
                    },
                    threshold,
                );
            }
        }

        if let AgentTool::DebugControl(action) = &tool {
            let _ = self.events.send(AgentEvent::AwaitingDebugExecution {
                action: action.clone(),
            });
            let outcome = match self.resume_rx.recv().await {
                Some(AgentResume::Debug(result)) => result,
                _ => Err(ToolError::NoDebugSession),
            };
            return self.finish(ToolResult { tool, outcome }, threshold);
        }

        // Races real execution against `tool_timeout` and against this
        // loop's own resume channel closing (dropping `AgentHandle`, which
        // `AgentPanel::cancel` already does) or firing unexpectedly --
        // either is treated as "stop this tool now". Whichever future
        // loses is dropped; for `RunShellCommand`/`ReadDockerLogs` that
        // drops the in-flight `tokio::process::Child` too, and
        // `Command::kill_on_drop(true)` (set on every subprocess this
        // crate spawns) kills it instead of leaving it running detached
        // (`hacker` finding 2, 2026-09-08). The other tool variants have
        // no internal `.await` point, so this timeout/cancel race can't
        // actually interrupt them mid-flight -- harmless, since they're
        // bounded by local disk speed rather than an external process
        // that can hang forever, which is what this race exists to bound.
        let fallback_tool = tool.clone();
        let result = tokio::select! {
            r = self.executor.execute(tool) => r,
            _ = tokio::time::sleep(self.tool_timeout) => ToolResult {
                tool: fallback_tool.clone(),
                outcome: Err(ToolError::Timeout),
            },
            _ = self.resume_rx.recv() => ToolResult {
                tool: fallback_tool,
                outcome: Err(ToolError::Cancelled),
            },
        };
        self.finish(result, threshold)
    }

    /// Truncates *before* masking, not after: `mask` runs a regex-based
    /// scan (`Sanitizer::mask_with_threshold`) whose cost scales with input
    /// size, and a `RunShellCommand`/`ReadDockerLogs`/`ReadFile` result can
    /// be arbitrarily large (a big file, a verbose build log) -- scanning
    /// the whole thing before capping it would let a single large tool
    /// result cost far more than the `MAX_TOOL_RESULT_CHARS` budget this
    /// truncation exists to enforce implies. Content past the cutoff is
    /// discarded either way (it never reaches the model in either
    /// ordering), so this doesn't weaken masking for anything that
    /// actually gets sent -- a secret straddling the cutoff exactly is no
    /// worse off than before, since it was never going to be transmitted
    /// past that point regardless of order (`rev` fix round 1). Masking a
    /// pre-truncated slice can grow the final feedback slightly past
    /// `MAX_TOOL_RESULT_CHARS` (a short match replaced by the fixed-length
    /// `__IDE_SAN_<n>__` placeholder) -- an acceptable, tightly bounded
    /// trade against no longer scanning unbounded input.
    fn finish(&mut self, result: ToolResult, threshold: Option<f64>) -> String {
        let feedback = match &result.outcome {
            Ok(text) => mask(&truncate(text, MAX_TOOL_RESULT_CHARS), threshold),
            Err(e) => e.to_string(),
        };
        let _ = self.events.send(AgentEvent::ToolFinished { result });
        feedback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FakeRouter {
        turns: Arc<Mutex<VecDeque<String>>>,
        seen_messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    }

    impl FakeRouter {
        fn new(turns: Vec<&str>) -> Self {
            Self {
                turns: Arc::new(Mutex::new(turns.into_iter().map(String::from).collect())),
                seen_messages: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<Vec<ChatMessage>> {
            self.seen_messages.lock().unwrap().clone()
        }
    }

    impl Router for FakeRouter {
        async fn chat(
            &self,
            messages: Vec<ChatMessage>,
            _order: &[ProviderId],
            _model_override: Option<&str>,
            _sanitized: bool,
            tx: Sender<Result<ide_ai::ChatDelta, AiError>>,
        ) -> Result<ProviderId, AiError> {
            self.seen_messages.lock().unwrap().push(messages);
            let text = self.turns.lock().unwrap().pop_front().unwrap_or_default();
            let _ = tx.send(Ok(ide_ai::ChatDelta { text }));
            Ok(ProviderId::OllamaLocal)
        }
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    fn tool_call(tool: &str, args: &str) -> String {
        format!("```tool_call\n{{\"tool\": \"{tool}\", \"args\": {args}}}\n```")
    }

    struct Harness {
        events: std::sync::mpsc::Receiver<AgentEvent>,
        router: FakeRouter,
    }

    impl Harness {
        fn collect(self) -> (Vec<AgentEvent>, FakeRouter) {
            (self.events.try_iter().collect(), self.router)
        }
    }

    /// Runs `run_with_router` to completion against a temp project root,
    /// pre-arming `resumes` in the internal channel before starting (safe
    /// because the channel is unbounded and FIFO -- the loop consumes them
    /// in the same order it pauses in, so this never needs true thread
    /// concurrency to drive a paused loop forward).
    fn run(
        root: &std::path::Path,
        mode: PermissionMode,
        turns: Vec<&str>,
        resumes: Vec<AgentResume>,
        threshold: Option<f64>,
    ) -> Harness {
        let executor = ToolExecutor::new(root).unwrap();
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let (loop_, handle) = AgentLoop::new(mode, executor, events_tx);
        for resume in resumes {
            match resume {
                AgentResume::Approval(a) => handle.resume_with_decision(a),
                AgentResume::Debug(r) => handle.resume_with_debug_result(r),
            }
        }
        let router = FakeRouter::new(turns);
        let router_clone = router.clone();
        block_on(loop_.run_with_router(
            router_clone,
            Vec::new(),
            &[ProviderId::OllamaLocal],
            None,
            threshold,
        ));
        Harness {
            events: events_rx,
            router,
        }
    }

    #[test]
    fn plan_mode_denies_mutating_tool_and_feeds_denied_back() {
        let dir = tempfile::tempdir().unwrap();
        let (events, router) = run(
            dir.path(),
            PermissionMode::Plan,
            vec![
                &tool_call("EditFile", r#"{"path": "a.rs", "new_text": "x"}"#),
                "done",
            ],
            vec![],
            None,
        )
        .collect();
        assert!(matches!(
            events
                .iter()
                .find(|e| matches!(e, AgentEvent::ToolFinished { .. })),
            Some(AgentEvent::ToolFinished {
                result: ToolResult {
                    outcome: Err(ToolError::Denied),
                    ..
                }
            })
        ));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::FinalAnswer
            })
        ));
        assert!(!dir.path().join("a.rs").exists());
        let calls = router.calls();
        let feedback_text = &calls[1].last().unwrap().text;
        assert_eq!(feedback_text, &ToolError::Denied.to_string());
    }

    #[test]
    fn approve_mode_pauses_then_runs_on_approval() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Approve,
            vec![
                &tool_call("EditFile", r#"{"path": "new.rs", "new_text": "fn f() {}"}"#),
                "done",
            ],
            vec![AgentResume::Approval(true)],
            None,
        )
        .collect();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingApproval { .. })));
        assert!(matches!(
            events
                .iter()
                .find(|e| matches!(e, AgentEvent::ToolFinished { .. })),
            Some(AgentEvent::ToolFinished {
                result: ToolResult { outcome: Ok(_), .. }
            })
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.rs")).unwrap(),
            "fn f() {}"
        );
    }

    #[test]
    fn approve_mode_denies_on_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Approve,
            vec![
                &tool_call("EditFile", r#"{"path": "new.rs", "new_text": "x"}"#),
                "done",
            ],
            vec![AgentResume::Approval(false)],
            None,
        )
        .collect();
        assert!(matches!(
            events
                .iter()
                .find(|e| matches!(e, AgentEvent::ToolFinished { .. })),
            Some(AgentEvent::ToolFinished {
                result: ToolResult {
                    outcome: Err(ToolError::UserDenied),
                    ..
                }
            })
        ));
        assert!(!dir.path().join("new.rs").exists());
    }

    #[test]
    fn auto_mode_allowlisted_command_runs_without_pause() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Auto,
            vec![
                &tool_call("RunShellCommand", r#"{"program": "ls", "args": []}"#),
                "done",
            ],
            vec![],
            None,
        )
        .collect();
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingApproval { .. })));
        assert!(matches!(
            events
                .iter()
                .find(|e| matches!(e, AgentEvent::ToolFinished { .. })),
            Some(AgentEvent::ToolFinished {
                result: ToolResult { outcome: Ok(_), .. }
            })
        ));
    }

    #[test]
    fn auto_mode_edit_file_runs_without_pause() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Auto,
            vec![
                &tool_call("EditFile", r#"{"path": "new.rs", "new_text": "x"}"#),
                "done",
            ],
            vec![],
            None,
        )
        .collect();
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingApproval { .. })));
        assert!(dir.path().join("new.rs").exists());
    }

    #[test]
    fn auto_mode_non_allowlisted_command_pauses_like_approve() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Auto,
            vec![
                &tool_call(
                    "RunShellCommand",
                    r#"{"program": "git", "args": ["reset", "--hard"]}"#,
                ),
                "done",
            ],
            vec![AgentResume::Approval(false)],
            None,
        )
        .collect();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingApproval { .. })));
        assert!(matches!(
            events
                .iter()
                .find(|e| matches!(e, AgentEvent::ToolFinished { .. })),
            Some(AgentEvent::ToolFinished {
                result: ToolResult {
                    outcome: Err(ToolError::UserDenied),
                    ..
                }
            })
        ));
    }

    #[test]
    fn debug_control_in_approve_mode_requires_approval_then_execution() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Approve,
            vec![
                &tool_call("DebugControl", r#"{"action": "Resume"}"#),
                "done",
            ],
            vec![
                AgentResume::Approval(true),
                AgentResume::Debug(Ok("resumed".to_string())),
            ],
            None,
        )
        .collect();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingApproval { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingDebugExecution { .. })));
        assert!(matches!(
            events.iter().find(|e| matches!(e, AgentEvent::ToolFinished { .. })),
            Some(AgentEvent::ToolFinished {
                result: ToolResult { outcome: Ok(text), .. }
            }) if text == "resumed"
        ));
    }

    #[test]
    fn debug_control_in_auto_mode_skips_approval() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Auto,
            vec![
                &tool_call(
                    "DebugControl",
                    r#"{"action": "ToggleBreakpoint", "path": "a.rs", "line": 3}"#,
                ),
                "done",
            ],
            vec![AgentResume::Debug(Ok("toggled".to_string()))],
            None,
        )
        .collect();
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingApproval { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingDebugExecution { .. })));
    }

    #[test]
    fn debug_control_in_plan_mode_is_denied_before_debug_execution() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Plan,
            vec![&tool_call("DebugControl", r#"{"action": "Stop"}"#), "done"],
            vec![],
            None,
        )
        .collect();
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingApproval { .. })));
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingDebugExecution { .. })));
        assert!(matches!(
            events
                .iter()
                .find(|e| matches!(e, AgentEvent::ToolFinished { .. })),
            Some(AgentEvent::ToolFinished {
                result: ToolResult {
                    outcome: Err(ToolError::Denied),
                    ..
                }
            })
        ));
    }

    #[test]
    fn no_debug_session_reported_by_caller_is_fed_back_as_error() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Auto,
            vec![
                &tool_call("DebugControl", r#"{"action": "Resume"}"#),
                "done",
            ],
            vec![AgentResume::Debug(Err(ToolError::NoDebugSession))],
            None,
        )
        .collect();
        assert!(matches!(
            events
                .iter()
                .find(|e| matches!(e, AgentEvent::ToolFinished { .. })),
            Some(AgentEvent::ToolFinished {
                result: ToolResult {
                    outcome: Err(ToolError::NoDebugSession),
                    ..
                }
            })
        ));
    }

    #[test]
    fn malformed_tool_call_fence_emits_parse_failed_then_done() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Plan,
            vec!["```tool_call\nnot json\n```"],
            vec![],
            None,
        )
        .collect();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallParseFailed { .. })));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::FinalAnswer
            })
        ));
    }

    #[test]
    fn no_fence_is_final_answer_immediately_with_no_tool_events() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _router) = run(
            dir.path(),
            PermissionMode::Plan,
            vec!["just an answer"],
            vec![],
            None,
        )
        .collect();
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStarted { .. })));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::FinalAnswer
            })
        ));
    }

    #[test]
    fn step_limit_reached_makes_one_final_tools_omitted_call() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "content").unwrap();
        let read_call = tool_call("ReadFile", r#"{"path": "a.rs"}"#);
        let mut turns: Vec<&str> =
            std::iter::repeat_n(read_call.as_str(), MAX_AGENT_STEPS).collect();
        turns.push("closing remarks");
        let (events, router) = run(dir.path(), PermissionMode::Plan, turns, vec![], None).collect();
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::StepLimitReached
            })
        ));
        let calls = router.calls();
        assert_eq!(calls.len(), MAX_AGENT_STEPS + 1);
        assert_eq!(calls.last().unwrap()[0].text, final_system_prompt());
    }

    #[test]
    fn tool_result_is_fed_back_unmasked_when_no_threshold() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "plain content").unwrap();
        let (_events, router) = run(
            dir.path(),
            PermissionMode::Plan,
            vec![&tool_call("ReadFile", r#"{"path": "a.rs"}"#), "done"],
            vec![],
            None,
        )
        .collect();
        let calls = router.calls();
        assert_eq!(calls[1].last().unwrap().text, "plain content");
    }

    #[test]
    fn tool_result_is_masked_before_being_fed_back_when_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "AKIAABCDEFGHIJKLMNOP1234567890";
        std::fs::write(dir.path().join("a.rs"), secret).unwrap();
        let (_events, router) = run(
            dir.path(),
            PermissionMode::Plan,
            vec![&tool_call("ReadFile", r#"{"path": "a.rs"}"#), "done"],
            vec![],
            Some(0.0),
        )
        .collect();
        let calls = router.calls();
        assert_ne!(calls[1].last().unwrap().text, secret);
    }

    #[test]
    fn tool_result_is_truncated_before_masking_not_after() {
        // Regression for `rev` fix round 1: `finish` used to mask the full
        // result then truncate -- a secret placed *after*
        // `MAX_TOOL_RESULT_CHARS` would still get scanned (wasting the
        // work truncation exists to bound) but, more importantly, this is
        // the only way to observe the ordering from outside `finish`: with
        // truncate-first, a secret past the cutoff is sliced away before
        // `mask` ever sees it, so it can't appear (masked or not) in the
        // fed-back text at all.
        let dir = tempfile::tempdir().unwrap();
        let secret = "AKIAABCDEFGHIJKLMNOP1234567890";
        let padding = "x".repeat(MAX_TOOL_RESULT_CHARS + 100);
        let content = format!("{padding}{secret}");
        std::fs::write(dir.path().join("a.rs"), &content).unwrap();
        let (_events, router) = run(
            dir.path(),
            PermissionMode::Plan,
            vec![&tool_call("ReadFile", r#"{"path": "a.rs"}"#), "done"],
            vec![],
            Some(0.0),
        )
        .collect();
        let calls = router.calls();
        let fed_back = &calls[1].last().unwrap().text;
        assert!(!fed_back.contains(secret));
        assert!(fed_back.chars().count() <= MAX_TOOL_RESULT_CHARS + 32);
    }

    #[test]
    fn is_allowlisted_covers_the_fixed_safe_set() {
        assert!(is_allowlisted("cargo", &["build".into()]));
        assert!(is_allowlisted("go", &["build".into()]));
        assert!(is_allowlisted("ls", &[]));
        assert!(is_allowlisted("cat", &["a.rs".into()]));
        assert!(is_allowlisted("grep", &["x".into()]));
        assert!(is_allowlisted("git", &["diff".into()]));
        assert!(!is_allowlisted("git", &["reset".into(), "--hard".into()]));
        assert!(!is_allowlisted("git", &[]));
        assert!(is_allowlisted("docker", &["ps".into()]));
        assert!(!is_allowlisted("docker", &["run".into()]));
        assert!(!is_allowlisted("sh", &["-c".into(), "rm -rf ~".into()]));
        assert!(!is_allowlisted("bash", &[]));
        assert!(!is_allowlisted("python", &[]));
        assert!(!is_allowlisted("definitely-not-safe", &[]));
    }

    /// Regression for `hacker` finding 1 (2026-09-08,
    /// `docs/security-findings/tui-local-agent-2026-09-08.md`): a
    /// path-qualified `program` must never be allowlisted, even when its
    /// file stem matches a trusted name -- live-tested against the real
    /// `ToolExecutor` that `program: "./cargo"` runs an attacker-planted
    /// file instead of the real `cargo`. This also covers what used to be
    /// an *intentionally accepted* case (`/usr/bin/git`, a full path to
    /// the presumably-real binary) -- accepting any path-qualified form at
    /// all is exactly the hole the finding exploited, since a string alone
    /// can't distinguish a real system path from an attacker's file that
    /// happens to sit at that same path.
    #[test]
    fn is_allowlisted_rejects_every_path_qualified_program() {
        assert!(!is_allowlisted("./cargo", &[]));
        assert!(!is_allowlisted("../cargo", &[]));
        assert!(!is_allowlisted("sub/cargo", &[]));
        assert!(!is_allowlisted("/tmp/evil/cargo", &[]));
        assert!(!is_allowlisted("/usr/bin/git", &["status".into()]));
        assert!(!is_allowlisted("./git", &["status".into()]));
        assert!(!is_allowlisted("./docker", &["ps".into()]));
        // A bare name -- no separator at all -- is still eligible and
        // still resolves via `$PATH`, same as before.
        assert!(is_allowlisted("cargo", &[]));
    }

    #[test]
    fn truncate_respects_char_boundaries_and_max_len() {
        assert_eq!(truncate("short", 100), "short");
        let long = "a".repeat(20);
        assert_eq!(truncate(&long, 5), "aaaaa");
        // Multi-byte boundary: each 'é' is 2 bytes -- truncating at byte 5
        // must not split one in half.
        let multibyte = "éééééé";
        let truncated = truncate(multibyte, 5);
        assert!(truncated.len() <= 5);
        assert!(multibyte.starts_with(&truncated));
    }

    #[test]
    fn mask_passes_through_unmasked_with_no_threshold() {
        assert_eq!(mask("hello", None), "hello");
    }

    /// Regression for `hacker` finding 2 (2026-09-08,
    /// `docs/security-findings/tui-local-agent-2026-09-08.md`): a
    /// `RunShellCommand` that outlives `tool_timeout` is killed, not left
    /// to hang forever, and reported back as `ToolError::Timeout`.
    #[test]
    fn run_shell_command_exceeding_the_timeout_is_killed_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path()).unwrap();
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let (loop_, handle) = AgentLoop::new(PermissionMode::Approve, executor, events_tx);
        let loop_ = loop_.with_tool_timeout(Duration::from_millis(200));
        // A `sleep` far longer than the 200ms override -- if the timeout
        // didn't actually kill it, this test would hang for 5 real
        // seconds instead of failing fast.
        handle.resume_with_decision(true);
        let router = FakeRouter::new(vec![
            &tool_call("RunShellCommand", r#"{"program": "sleep", "args": ["5"]}"#),
            "done",
        ]);
        block_on(loop_.run_with_router(router, Vec::new(), &[ProviderId::OllamaLocal], None, None));
        let events: Vec<AgentEvent> = events_rx.try_iter().collect();
        let finished = events.iter().find_map(|e| match e {
            AgentEvent::ToolFinished { result } => Some(result),
            _ => None,
        });
        assert!(
            matches!(
                finished,
                Some(ToolResult {
                    outcome: Err(ToolError::Timeout),
                    ..
                })
            ),
            "{finished:?}"
        );
    }

    /// Regression for `hacker` finding 2: cancelling mid-run (modeled the
    /// same way `AgentPanel::cancel` does it -- dropping the `AgentHandle`,
    /// which closes `resume_rx`) kills a running `RunShellCommand` rather
    /// than leaving it detached in the background. Asserted by wall-clock
    /// time: a 5s `sleep` that isn't actually interrupted would make this
    /// test take ~5s; a correctly-cancelled one returns almost instantly.
    #[test]
    fn cancelling_while_a_shell_command_is_running_kills_it() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path()).unwrap();
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let (loop_, handle) = AgentLoop::new(PermissionMode::Approve, executor, events_tx);
        let loop_ = loop_.with_tool_timeout(Duration::from_secs(30));
        handle.resume_with_decision(true);
        // Mirrors `AgentPanel::cancel`'s `self.handle = None` -- the only
        // `AgentHandle`/`resume_tx` this loop's `resume_rx` will ever see
        // is gone from here on.
        drop(handle);
        let router = FakeRouter::new(vec![
            &tool_call("RunShellCommand", r#"{"program": "sleep", "args": ["5"]}"#),
            "done",
        ]);
        let start = std::time::Instant::now();
        block_on(loop_.run_with_router(router, Vec::new(), &[ProviderId::OllamaLocal], None, None));
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(4),
            "cancellation should interrupt the 5s sleep almost immediately, took {elapsed:?}"
        );
        let events: Vec<AgentEvent> = events_rx.try_iter().collect();
        let finished = events.iter().find_map(|e| match e {
            AgentEvent::ToolFinished { result } => Some(result),
            _ => None,
        });
        assert!(
            matches!(
                finished,
                Some(ToolResult {
                    outcome: Err(ToolError::Cancelled),
                    ..
                })
            ),
            "{finished:?}"
        );
    }
}
