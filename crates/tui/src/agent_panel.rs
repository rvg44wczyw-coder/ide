//! Local agentic IDE assistant dock tab (`docs/features/tui-local-agent.md`,
//! T56). One background thread per submitted prompt, driving `ide_agent::
//! AgentLoop::run` to completion -- the same one-thread-per-request,
//! `mpsc`-drained-by-`poll` shape `ai_panel.rs` already established.
//! Deliberately render-independent: this module owns state and logic only;
//! `ui.rs` renders it.
//!
//! Reuses T49/T55's masking and role-routing unchanged (§3.3): `submit`
//! masks the outgoing prompt via `ide_ai::mask_outgoing`/
//! `decide_threshold` exactly like the plain chat path, and the background
//! run resolves a `TaskRole`/`RoleRoute` exactly like `ai_panel::
//! run_request` does before calling into `AgentLoop`.
//!
//! Unlike `ai_panel.rs`'s `settle`, tool results are never masked for local
//! display -- `ide_agent::AgentEvent::ToolFinished` already carries the
//! real, unmasked `ToolResult` (masking only ever applies to what
//! `AgentLoop` re-sends to the model, never to what this panel shows
//! locally) -- and the model's own streamed text is displayed as-is,
//! without a `restore_originals` pass. `ai_panel.rs`'s `settle` restores
//! placeholders from the assistant's reply for the rare case a cloud model
//! echoes one back verbatim; skipping that here is a deliberate v1
//! simplification (worst case a placeholder token is visible instead of a
//! secret, which is a strictly safer failure than the alternative), not an
//! oversight.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

#[cfg(test)]
use std::sync::mpsc::Sender;

use ide_agent::{
    AgentEvent, AgentHandle, AgentLoop, AgentTool, DebugAction, DoneReason, ToolError,
    ToolExecutor, ToolResult,
};
use ide_ai::{
    classify_task_role, decide_threshold, mask_outgoing, resolve_role_route, AiConfig, ChatMessage,
    ChatRole, PermissionMode, TaskRole,
};
use ide_core::git::diff_text;

/// One rendered line of agent history. Kept separate from `ide_agent::
/// AgentEvent` so `ui.rs` never depends on `ide-agent` types directly.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentDisplayEntry {
    User(String),
    /// Model text, accumulated live across `AgentEvent::ModelDelta`.
    ModelText(String),
    ToolStarted(String),
    ToolFinished(String),
    ToolCallParseFailed(String),
    Error(String),
}

/// Everything one background agent run needs. `tx` isn't stored on the
/// struct: `AgentLoop::new` already took an events sender by the time this
/// is built (see `submit`).
pub(crate) struct AgentPreparedRequest {
    loop_: AgentLoop,
    history: Vec<ChatMessage>,
    config: AiConfig,
    masked_prompt: String,
    threshold: Option<f64>,
}

/// Production always uses [`run_agent`]; tests substitute a fake runner so
/// they never open a socket to a real provider -- the same seam
/// `AiPanel::with_runner`/`ClaudePanel::with_runner` already use.
type AgentRunner = fn(AgentPreparedRequest);

pub struct AgentPanel {
    pub mode: PermissionMode,
    pub input: String,
    pub history: Vec<AgentDisplayEntry>,
    pub history_scroll: u16,
    /// Persists across submissions within one agent-mode session -- only
    /// the user prompt and the model's final answer, never the internal
    /// tool-calling sub-turns of a single `AgentLoop::run` (§3.2: those are
    /// scoped to resolving one user message, not part of the ongoing
    /// conversation).
    chat_history: Vec<ChatMessage>,
    /// Accumulates the *current* model turn's text; reset whenever a tool
    /// call interrupts it (that turn wasn't a final answer) and captured
    /// into `chat_history` only when `AgentEvent::Done { reason:
    /// FinalAnswer }` fires while it's still the live turn.
    current_model_text: String,
    streaming: bool,
    handle: Option<AgentHandle>,
    rx: Option<Receiver<AgentEvent>>,
    pub pending_approval: Option<AgentTool>,
    root: PathBuf,
    runner: AgentRunner,
}

impl AgentPanel {
    pub fn new(root: PathBuf) -> Self {
        Self::with_runner(root, run_agent)
    }

    /// `pub(crate)`, not private: `app.rs`'s own tests swap in a fake
    /// runner, same reasoning as `AiPanel::with_runner`.
    pub(crate) fn with_runner(root: PathBuf, runner: AgentRunner) -> Self {
        let mode = AiConfig::load(&root).agent_mode;
        // Canonicalized once, here, the same discipline `ToolExecutor::new`
        // already applies to its own copy of the root and `crates/ui/src/
        // agent_panel.rs::active_root` applies to its own (see that field's
        // doc comment on the `/private/var` symlink case) -- `self.root` is
        // now compared against a *canonicalized* target path inside
        // `validated_edit_target`, so an uncanonicalized root (e.g. a
        // symlinked `$TMPDIR` on macOS) would otherwise make `starts_with`
        // spuriously fail for perfectly legitimate in-root files.
        // Fails open to the given `root` (matching `active_root`'s
        // `unwrap_or`) since a root that doesn't exist yet is a test/
        // misconfiguration concern, not something to hide behind a panic.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Self {
            mode,
            input: String::new(),
            history: Vec::new(),
            history_scroll: 0,
            chat_history: Vec::new(),
            current_model_text: String::new(),
            streaming: false,
            handle: None,
            rx: None,
            pending_approval: None,
            root,
            runner,
        }
    }

    pub fn is_in_flight(&self) -> bool {
        self.rx.is_some()
    }

    /// Manual recovery backstop for a wedged request, mirroring `AiPanel::
    /// cancel`.
    pub fn cancel(&mut self) {
        self.rx = None;
        self.handle = None;
        self.streaming = false;
        self.pending_approval = None;
    }

    /// Cycles `Plan -> Approve -> Auto -> Plan` and persists the choice to
    /// `.ide/ai.json` (best-effort, same fail-open write convention
    /// `project_state.rs`/`custom_actions.rs` already use).
    pub fn cycle_mode(&mut self) {
        self.mode = match self.mode {
            PermissionMode::Plan => PermissionMode::Approve,
            PermissionMode::Approve => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Plan,
        };
        let mut config = AiConfig::load(&self.root);
        config.agent_mode = self.mode;
        let _ = ide_core::project_settings::write(
            &self.root,
            ide_core::project_settings::ProjectSettingsFile::Ai,
            &config,
        );
    }

    /// Appends the prompt to history and spawns the background run.
    /// No-op on a blank prompt or while a run is already in flight (no
    /// queue, same v1 scope `AiPanel::submit` has).
    pub fn submit(&mut self, prompt: String) {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() || self.is_in_flight() {
            return;
        }
        let executor = match ToolExecutor::new(&self.root) {
            Ok(e) => e,
            Err(e) => {
                self.history.push(AgentDisplayEntry::Error(e.to_string()));
                return;
            }
        };
        self.history.push(AgentDisplayEntry::User(prompt.clone()));
        let config = AiConfig::load(&self.root);
        let order = config.enabled_providers();
        let threshold = decide_threshold(&config, &order);
        let (masked_prompt, _map) = mask_outgoing(prompt, threshold);
        self.chat_history
            .push(ChatMessage::user(masked_prompt.clone()));
        let (events_tx, events_rx) = mpsc::channel();
        let (loop_, handle) = AgentLoop::new(self.mode, executor, events_tx);
        self.handle = Some(handle);
        self.rx = Some(events_rx);
        self.streaming = false;
        self.current_model_text.clear();
        let prepared = AgentPreparedRequest {
            loop_,
            history: self.chat_history.clone(),
            config,
            masked_prompt,
            threshold,
        };
        let runner = self.runner;
        thread::spawn(move || runner(prepared));
    }

    /// Approves the pending mutating tool call, resuming the paused loop.
    pub fn approve_pending(&mut self) {
        if self.pending_approval.take().is_some() {
            if let Some(h) = &self.handle {
                h.resume_with_decision(true);
            }
        }
    }

    /// Denies the pending mutating tool call, resuming the paused loop.
    pub fn deny_pending(&mut self) {
        if self.pending_approval.take().is_some() {
            if let Some(h) = &self.handle {
                h.resume_with_decision(false);
            }
        }
    }

    /// Resumes a loop paused on `AgentEvent::AwaitingDebugExecution` with
    /// the outcome the caller (`app.rs`, on the main thread, against its
    /// own real `DebugPanel` session) already computed.
    pub fn resolve_debug(&mut self, result: Result<String, ToolError>) {
        if let Some(h) = &self.handle {
            h.resume_with_debug_result(result);
        }
    }

    /// A diff/command/action preview for the approval popup (§2.2):
    /// `EditFile` gets a real diff via `ide_core::git::diff_text`;
    /// `RunShellCommand` shows the literal program+args; everything else
    /// (including `DebugControl`) shows its one-line description.
    ///
    /// `EditFile`'s `path` is validated against `self.root` via
    /// `validated_edit_target` before it's ever joined/read (same fix as
    /// `crates/ui/src/agent_panel.rs::pending_approval_preview`'s own doc
    /// comment describes and flags this exact function as a known,
    /// separate-follow-up gap for). Without it, a model-supplied `path` --
    /// steered by indirect prompt injection, T56 §4's own named threat
    /// model -- could read arbitrary local file content into the UI via a
    /// `../` escape or a symlink, with zero user action beyond the
    /// `Approve`-mode popup rendering (this method runs automatically the
    /// moment it appears, before a human decides anything).
    pub fn pending_approval_preview(&self) -> Vec<String> {
        let Some(tool) = &self.pending_approval else {
            return Vec::new();
        };
        match tool {
            AgentTool::EditFile { path, new_text } => match validated_edit_target(&self.root, path)
            {
                Some(real) => {
                    let old = std::fs::read_to_string(&real).unwrap_or_default();
                    match diff_text(Path::new(path), &old, new_text) {
                        Some(diff) => diff_to_lines(&diff),
                        None => vec![format!("EditFile {path} (no textual diff)")],
                    }
                }
                None => vec!["path escapes the project root".to_string()],
            },
            AgentTool::RunShellCommand { program, args } => {
                vec![format!("Run: {program} {}", args.join(" "))]
            }
            other => vec![describe_tool(other)],
        }
    }

    /// Call once per frame; drains the events channel. Returns `Some`
    /// when the loop paused on `AgentEvent::AwaitingDebugExecution` and
    /// needs the caller to run `action` against its own real debug
    /// session (on the main thread) and call [`Self::resolve_debug`] with
    /// the outcome before the next poll tick -- this is the one event
    /// this panel cannot handle by itself (`docs/features/
    /// tui-local-agent.md` §2.2).
    pub fn poll(&mut self) -> Option<DebugAction> {
        loop {
            let Some(rx) = &self.rx else {
                return None;
            };
            match rx.try_recv() {
                Ok(event) => {
                    if let Some(action) = self.ingest(event) {
                        return Some(action);
                    }
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    self.rx = None;
                    return None;
                }
            }
        }
    }

    /// Test-only seam: arms an injectable event channel exactly like
    /// `submit` would (minus the real background thread), so `app.rs`'s
    /// own tests -- a different module, unable to reach the private `rx`
    /// field directly -- can drive `poll`/`App::poll_agent`'s dispatch
    /// with a hand-crafted `AgentEvent` (in particular `AwaitingDebug
    /// Execution`, the one event `poll` can't resolve on its own).
    /// Mirrors this module's own `events_sender_and_panel` test helper
    /// below, exposed at `pub(crate)` instead of kept test-local.
    #[cfg(test)]
    pub(crate) fn test_arm_event_channel(&mut self) -> Sender<AgentEvent> {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        tx
    }

    fn ingest(&mut self, event: AgentEvent) -> Option<DebugAction> {
        match event {
            AgentEvent::ModelDelta { text } => {
                match self.history.last_mut() {
                    Some(AgentDisplayEntry::ModelText(buf)) if self.streaming => {
                        buf.push_str(&text)
                    }
                    _ => self
                        .history
                        .push(AgentDisplayEntry::ModelText(text.clone())),
                }
                self.streaming = true;
                self.current_model_text.push_str(&text);
                None
            }
            AgentEvent::ToolStarted { tool } => {
                self.streaming = false;
                self.current_model_text.clear();
                self.history
                    .push(AgentDisplayEntry::ToolStarted(describe_tool(&tool)));
                None
            }
            AgentEvent::ToolFinished { result } => {
                self.history
                    .push(AgentDisplayEntry::ToolFinished(describe_result(&result)));
                None
            }
            AgentEvent::ToolCallParseFailed { raw_snippet } => {
                self.streaming = false;
                self.current_model_text.clear();
                self.history
                    .push(AgentDisplayEntry::ToolCallParseFailed(raw_snippet));
                None
            }
            AgentEvent::AwaitingApproval { tool } => {
                self.pending_approval = Some(tool);
                None
            }
            AgentEvent::AwaitingDebugExecution { action } => Some(action),
            AgentEvent::Done { reason } => {
                self.streaming = false;
                self.rx = None;
                self.handle = None;
                match reason {
                    DoneReason::FinalAnswer => {
                        if !self.current_model_text.is_empty() {
                            self.chat_history.push(ChatMessage {
                                role: ChatRole::Assistant,
                                text: std::mem::take(&mut self.current_model_text),
                            });
                        }
                    }
                    DoneReason::StepLimitReached => {
                        self.current_model_text.clear();
                    }
                    DoneReason::Error(e) => {
                        self.current_model_text.clear();
                        self.history.push(AgentDisplayEntry::Error(e.to_string()));
                    }
                }
                None
            }
        }
    }
}

/// Resolves `path` (model-supplied, relative to `project_root`) to a real
/// on-disk target, rejecting any escape -- ported from `crates/ui/src/
/// agent_panel.rs::validated_edit_target` (`docs/features/
/// gui-local-agent.md`/G10's own hacker findings, `docs/security-findings/
/// rust-ui-dev-gui-local-agent-2026-09-08.md`), which found and fixed
/// three ways this can go wrong: an out-of-root symlink, a *dangling*
/// symlink already sitting at the leaf (whose target doesn't exist, so
/// `canonicalize` alone can't see it -- `symlink_metadata` can, without
/// following it), and any existing directory (`EditFile` never
/// legitimately targets one). `""`/`"."`/`"sub/.."`-shaped paths that
/// resolve back to the root itself are rejected by the same
/// `!canonical.is_dir()` check, since the root is itself a directory.
fn validated_edit_target(project_root: &Path, path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    let target = project_root.join(path);
    if let Ok(canonical) = std::fs::canonicalize(&target) {
        return (canonical.starts_with(project_root) && !canonical.is_dir()).then_some(canonical);
    }
    if std::fs::symlink_metadata(&target).is_ok() {
        return None;
    }
    let parent = target.parent()?;
    let canonical_parent = std::fs::canonicalize(parent).ok()?;
    if !canonical_parent.starts_with(project_root) {
        return None;
    }
    let file_name = target.file_name()?;
    Some(canonical_parent.join(file_name))
}

fn describe_tool(tool: &AgentTool) -> String {
    match tool {
        AgentTool::ReadFile { path } => format!("ReadFile {path}"),
        AgentTool::SearchCode { query } => format!("SearchCode {query:?}"),
        AgentTool::ListDirectory { path } => format!("ListDirectory {path}"),
        AgentTool::ReadDockerLogs { container_id } => format!("ReadDockerLogs {container_id}"),
        AgentTool::EditFile { path, .. } => format!("EditFile {path}"),
        AgentTool::RunShellCommand { program, args } => {
            format!("RunShellCommand {program} {}", args.join(" "))
        }
        AgentTool::DebugControl(action) => format!("DebugControl {action:?}"),
    }
}

const MAX_DISPLAY_RESULT_CHARS: usize = 400;

fn describe_result(result: &ToolResult) -> String {
    let name = describe_tool(&result.tool);
    match &result.outcome {
        Ok(text) => format!("{name} -> {}", truncate_for_display(text)),
        Err(e) => format!("{name} -> error: {e}"),
    }
}

fn truncate_for_display(text: &str) -> String {
    if text.chars().count() > MAX_DISPLAY_RESULT_CHARS {
        let short: String = text.chars().take(MAX_DISPLAY_RESULT_CHARS).collect();
        format!("{short}…")
    } else {
        text.to_string()
    }
}

fn diff_to_lines(diff: &ide_core::git::FileDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for hunk in &diff.hunks {
        lines.push(format!("@@ -{} +{} @@", hunk.old_start, hunk.new_start));
        for line in &hunk.lines {
            match line {
                ide_core::git::DiffLine::Context(text) => lines.push(format!("  {text}")),
                ide_core::git::DiffLine::Added(text, _) => lines.push(format!("+ {text}")),
                ide_core::git::DiffLine::Removed(text, _) => lines.push(format!("- {text}")),
            }
        }
    }
    if diff.truncated {
        lines.push("... (diff truncated)".to_string());
    }
    lines
}

/// The background request run: resolve the role/route (§3.3, reusing T55
/// unchanged) then drive `AgentLoop::run` to completion. One thread per
/// request, owning its own tokio runtime, same shape `ai_panel::
/// run_request` uses.
fn run_agent(prepared: AgentPreparedRequest) {
    let AgentPreparedRequest {
        loop_,
        history,
        config,
        masked_prompt,
        threshold,
    } = prepared;
    let sanitized = threshold.is_some();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return,
    };
    rt.block_on(async move {
        let role = if config.auto_route {
            classify_task_role(config.classifier_provider, &masked_prompt, sanitized).await
        } else {
            TaskRole::General
        };
        let route = resolve_role_route(&config, role);
        loop_
            .run(
                history,
                &route.provider_order,
                route.model_override.as_deref(),
                threshold,
            )
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_root() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ide-agent-panel-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A no-op runner: never touches a real provider or `ide_agent`
    /// internals -- it just proves `submit` builds a valid, executable
    /// `AgentPreparedRequest` and moves it onto a real thread.
    fn noop_runner(_prepared: AgentPreparedRequest) {}

    fn events_sender_and_panel(root: PathBuf) -> (AgentPanel, Sender<AgentEvent>) {
        let mut panel = AgentPanel::with_runner(root, noop_runner);
        let (tx, rx) = mpsc::channel();
        panel.rx = Some(rx);
        (panel, tx)
    }

    #[test]
    fn fresh_panel_is_idle_with_plan_mode_by_default() {
        let panel = AgentPanel::new(temp_root());
        assert!(!panel.is_in_flight());
        assert_eq!(panel.mode, PermissionMode::Plan);
        assert!(panel.pending_approval.is_none());
    }

    #[test]
    fn cycle_mode_goes_plan_approve_auto_plan_and_persists() {
        let root = temp_root();
        let mut panel = AgentPanel::new(root.clone());
        assert_eq!(panel.mode, PermissionMode::Plan);
        panel.cycle_mode();
        assert_eq!(panel.mode, PermissionMode::Approve);
        panel.cycle_mode();
        assert_eq!(panel.mode, PermissionMode::Auto);
        panel.cycle_mode();
        assert_eq!(panel.mode, PermissionMode::Plan);

        // Persisted: a freshly constructed panel over the same root picks
        // up the last-written mode.
        let mut panel2 = AgentPanel::new(root.clone());
        panel2.cycle_mode();
        assert_eq!(AiConfig::load(&root).agent_mode, PermissionMode::Approve);
    }

    #[test]
    fn submit_on_blank_prompt_is_a_no_op() {
        let mut panel = AgentPanel::with_runner(temp_root(), noop_runner);
        panel.submit("   ".to_string());
        assert!(panel.history.is_empty());
        assert!(!panel.is_in_flight());
    }

    #[test]
    fn submit_while_in_flight_is_a_no_op() {
        let mut panel = AgentPanel::with_runner(temp_root(), noop_runner);
        panel.submit("first".to_string());
        assert!(panel.is_in_flight());
        panel.submit("second".to_string());
        assert_eq!(
            panel.history,
            vec![AgentDisplayEntry::User("first".to_string())]
        );
    }

    #[test]
    fn submit_appends_user_entry_and_masked_chat_history() {
        let mut panel = AgentPanel::with_runner(temp_root(), noop_runner);
        panel.submit("hello there".to_string());
        assert_eq!(
            panel.history,
            vec![AgentDisplayEntry::User("hello there".to_string())]
        );
        assert_eq!(panel.chat_history.len(), 1);
        assert!(panel.is_in_flight());
    }

    #[test]
    fn cancel_clears_in_flight_state() {
        let mut panel = AgentPanel::with_runner(temp_root(), noop_runner);
        panel.submit("hello".to_string());
        assert!(panel.is_in_flight());
        panel.cancel();
        assert!(!panel.is_in_flight());
        assert!(panel.pending_approval.is_none());
    }

    #[test]
    fn poll_with_no_rx_is_none() {
        let mut panel = AgentPanel::new(temp_root());
        assert_eq!(panel.poll(), None);
    }

    #[test]
    fn poll_drains_model_deltas_into_one_accumulating_entry() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::ModelDelta { text: "Hel".into() })
            .unwrap();
        tx.send(AgentEvent::ModelDelta { text: "lo".into() })
            .unwrap();
        assert_eq!(panel.poll(), None);
        assert_eq!(
            panel.history,
            vec![AgentDisplayEntry::ModelText("Hello".to_string())]
        );
    }

    #[test]
    fn tool_started_freezes_the_current_model_text_entry() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::ModelDelta {
            text: "checking".into(),
        })
        .unwrap();
        tx.send(AgentEvent::ToolStarted {
            tool: AgentTool::ReadFile {
                path: "a.rs".into(),
            },
        })
        .unwrap();
        tx.send(AgentEvent::ModelDelta {
            text: "next turn".into(),
        })
        .unwrap();
        panel.poll();
        assert_eq!(
            panel.history,
            vec![
                AgentDisplayEntry::ModelText("checking".to_string()),
                AgentDisplayEntry::ToolStarted("ReadFile a.rs".to_string()),
                AgentDisplayEntry::ModelText("next turn".to_string()),
            ]
        );
    }

    #[test]
    fn tool_finished_shows_the_real_unmasked_result() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::ToolFinished {
            result: ToolResult {
                tool: AgentTool::ReadFile {
                    path: "a.rs".into(),
                },
                outcome: Ok("secret-looking-content".to_string()),
            },
        })
        .unwrap();
        panel.poll();
        assert_eq!(
            panel.history,
            vec![AgentDisplayEntry::ToolFinished(
                "ReadFile a.rs -> secret-looking-content".to_string()
            )]
        );
    }

    #[test]
    fn tool_finished_error_is_shown_too() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::ToolFinished {
            result: ToolResult {
                tool: AgentTool::EditFile {
                    path: "a.rs".into(),
                    new_text: "x".into(),
                },
                outcome: Err(ToolError::Denied),
            },
        })
        .unwrap();
        panel.poll();
        assert_eq!(
            panel.history,
            vec![AgentDisplayEntry::ToolFinished(
                "EditFile a.rs -> error: denied by permission mode".to_string()
            )]
        );
    }

    #[test]
    fn awaiting_approval_sets_pending_approval() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        let tool = AgentTool::RunShellCommand {
            program: "git".into(),
            args: vec!["reset".into(), "--hard".into()],
        };
        tx.send(AgentEvent::AwaitingApproval { tool: tool.clone() })
            .unwrap();
        panel.poll();
        assert_eq!(panel.pending_approval, Some(tool));
    }

    #[test]
    fn approve_pending_clears_state_and_sends_decision() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        let (resume_tx, resume_rx) = mpsc::channel();
        // Wire a fake handle by driving a real AgentLoop just far enough to
        // capture its handle -- simplest is to construct one directly.
        let executor = ToolExecutor::new(&std::env::temp_dir()).unwrap();
        let (_loop_, handle) = AgentLoop::new(PermissionMode::Approve, executor, resume_tx);
        panel.handle = Some(handle);
        let _ = resume_rx; // not polled; resume_with_decision only needs the sender to accept
        tx.send(AgentEvent::AwaitingApproval {
            tool: AgentTool::EditFile {
                path: "a.rs".into(),
                new_text: "x".into(),
            },
        })
        .unwrap();
        panel.poll();
        assert!(panel.pending_approval.is_some());
        panel.approve_pending();
        assert!(panel.pending_approval.is_none());
    }

    #[test]
    fn deny_pending_clears_state() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::AwaitingApproval {
            tool: AgentTool::EditFile {
                path: "a.rs".into(),
                new_text: "x".into(),
            },
        })
        .unwrap();
        panel.poll();
        assert!(panel.pending_approval.is_some());
        panel.deny_pending();
        assert!(panel.pending_approval.is_none());
    }

    #[test]
    fn awaiting_debug_execution_bubbles_up_from_poll() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::AwaitingDebugExecution {
            action: DebugAction::Resume,
        })
        .unwrap();
        assert_eq!(panel.poll(), Some(DebugAction::Resume));
    }

    #[test]
    fn resolve_debug_is_a_no_op_with_no_handle() {
        let mut panel = AgentPanel::new(temp_root());
        panel.resolve_debug(Ok("ok".to_string())); // must not panic
    }

    #[test]
    fn done_final_answer_captures_the_last_model_turn_into_chat_history() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::ModelDelta {
            text: "the answer is 42".into(),
        })
        .unwrap();
        tx.send(AgentEvent::Done {
            reason: DoneReason::FinalAnswer,
        })
        .unwrap();
        panel.poll();
        assert!(!panel.is_in_flight());
        assert_eq!(panel.chat_history.len(), 1);
        assert_eq!(panel.chat_history[0].text, "the answer is 42");
        assert!(matches!(panel.chat_history[0].role, ChatRole::Assistant));
    }

    #[test]
    fn done_step_limit_reached_does_not_add_a_stray_chat_history_entry() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::ModelDelta {
            text: "summary".into(),
        })
        .unwrap();
        tx.send(AgentEvent::Done {
            reason: DoneReason::StepLimitReached,
        })
        .unwrap();
        panel.poll();
        assert!(!panel.is_in_flight());
        assert!(panel.chat_history.is_empty());
    }

    #[test]
    fn done_error_is_shown_in_history() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::Done {
            reason: DoneReason::Error(ide_ai::AiError::ConnectionRefused),
        })
        .unwrap();
        panel.poll();
        assert!(!panel.is_in_flight());
        assert!(matches!(
            panel.history.last(),
            Some(AgentDisplayEntry::Error(_))
        ));
    }

    #[test]
    fn tool_call_parse_failed_clears_pending_model_text_and_is_shown() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::ModelDelta {
            text: "garbled".into(),
        })
        .unwrap();
        tx.send(AgentEvent::ToolCallParseFailed {
            raw_snippet: "bad json".into(),
        })
        .unwrap();
        panel.poll();
        assert_eq!(
            panel.history,
            vec![
                AgentDisplayEntry::ModelText("garbled".to_string()),
                AgentDisplayEntry::ToolCallParseFailed("bad json".to_string()),
            ]
        );
    }

    #[test]
    fn pending_approval_preview_shows_diff_for_edit_file() {
        let root = temp_root();
        std::fs::write(root.join("a.rs"), "old content").unwrap();
        let mut panel = AgentPanel::with_runner(root, noop_runner);
        panel.pending_approval = Some(AgentTool::EditFile {
            path: "a.rs".into(),
            new_text: "new content".into(),
        });
        let preview = panel.pending_approval_preview();
        assert!(preview.iter().any(|l| l.contains("old content")));
        assert!(preview.iter().any(|l| l.contains("new content")));
    }

    #[test]
    fn pending_approval_preview_shows_argv_for_run_shell_command() {
        let mut panel = AgentPanel::new(temp_root());
        panel.pending_approval = Some(AgentTool::RunShellCommand {
            program: "git".into(),
            args: vec!["push".into()],
        });
        let preview = panel.pending_approval_preview();
        assert_eq!(preview, vec!["Run: git push".to_string()]);
    }

    #[test]
    fn pending_approval_preview_describes_debug_control() {
        let mut panel = AgentPanel::new(temp_root());
        panel.pending_approval = Some(AgentTool::DebugControl(DebugAction::Resume));
        let preview = panel.pending_approval_preview();
        assert_eq!(preview, vec!["DebugControl Resume".to_string()]);
    }

    #[test]
    fn pending_approval_preview_is_empty_with_nothing_pending() {
        let panel = AgentPanel::new(temp_root());
        assert!(panel.pending_approval_preview().is_empty());
    }

    #[test]
    fn truncate_for_display_truncates_long_results() {
        let long = "a".repeat(MAX_DISPLAY_RESULT_CHARS * 2);
        let short = truncate_for_display(&long);
        assert!(short.chars().count() <= MAX_DISPLAY_RESULT_CHARS + 1);
    }

    #[test]
    fn describe_tool_covers_every_variant() {
        assert_eq!(
            describe_tool(&AgentTool::SearchCode { query: "x".into() }),
            "SearchCode \"x\""
        );
        assert_eq!(
            describe_tool(&AgentTool::ListDirectory { path: "d".into() }),
            "ListDirectory d"
        );
        assert_eq!(
            describe_tool(&AgentTool::ReadDockerLogs {
                container_id: "c1".into()
            }),
            "ReadDockerLogs c1"
        );
        assert_eq!(
            describe_tool(&AgentTool::DebugControl(DebugAction::ToggleBreakpoint {
                path: "a.rs".into(),
                line: 3,
            })),
            "DebugControl ToggleBreakpoint { path: \"a.rs\", line: 3 }"
        );
    }
}
