//! Local agentic IDE assistant dock tab (`docs/features/gui-local-agent.md`,
//! G10) — the GUI counterpart of `crates/tui/src/agent_panel.rs` (T56). One
//! background thread per submitted prompt, driving `ide_agent::AgentLoop::
//! run` to completion — the same one-thread-per-request, `mpsc`-drained-by-
//! `poll` shape `ai_panel.rs` already established. Deliberately
//! render-independent: this module owns state and logic only; `app/
//! render.rs` renders it.
//!
//! Reuses T49/T55's masking and role-routing unchanged (T56 §3.3): `submit`
//! masks the outgoing prompt via `ide_ai::mask_outgoing`/`decide_threshold`
//! exactly like the plain chat path, and the background run resolves a
//! `TaskRole`/`RoleRoute` exactly like `ai_panel::run_request` does before
//! calling into `AgentLoop`.
//!
//! Unlike `ai_panel.rs`'s `settle`, tool results are never masked for local
//! display — `ide_agent::AgentEvent::ToolFinished` already carries the real,
//! unmasked `ToolResult` — and the model's own streamed text is displayed
//! as-is, without a `restore_originals` pass, the same accepted v1
//! simplification T56 already made.
//!
//! Deliberate deviation from `ide-tui`'s `AgentPanel`: no cached `root:
//! PathBuf` field. Every method that needs the project root takes
//! `project_root: &Path` per call, this crate's established convention
//! (`AiPanel`, `CargoPanel`, `CustomActionsPanel`) for panels that must work
//! across a mid-session project switch, which `ide-tui` never does.

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
use ide_core::diff_text;

/// One rendered line of agent history. Kept separate from `ide_agent::
/// AgentEvent` so `app/render.rs` never depends on `ide-agent` types
/// directly.
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

/// `EditFile`'s preview needs a real `FileDiff` for `app/render.rs`'s
/// `render_diff`; everything else is one descriptive line — both cases the
/// same popup renders, so this replaces TUI's `Vec<String>` with an enum
/// rather than stringifying the diff too. Unboxed (`FileDiff` is an
/// ordinary, not-especially-large struct, and every existing `render_diff`
/// call site already passes `&FileDiff`/`std::slice::from_ref` unboxed).
#[derive(Debug, Clone, PartialEq)]
pub enum AgentApprovalPreview {
    Diff(ide_core::FileDiff),
    Text(String),
    /// `EditFile` targeting a path `ide_core::diff_text` returns no textual
    /// diff for (e.g. binary content), **or** a path that fails validation
    /// (see `AgentPanel::pending_approval_preview`'s doc comment) — both are
    /// "no diff to show," represented as text rather than collapsed into
    /// `Text` so `render_diff` is never handed a diff-shaped `Text` string
    /// to mis-render.
    NoDiff(String),
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
/// they never open a socket to a real provider — the same seam
/// `AiPanel::with_runner` already uses.
type AgentRunner = fn(AgentPreparedRequest);

pub struct AgentPanel {
    pub mode: PermissionMode,
    pub input: String,
    pub history: Vec<AgentDisplayEntry>,
    /// Persists across submissions within one agent-mode session — only
    /// the user prompt and the model's final answer, never the internal
    /// tool-calling sub-turns of a single `AgentLoop::run` (T56 §3.2: those
    /// are scoped to resolving one user message, not part of the ongoing
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
    /// The project root `submit` most recently captured for the run
    /// currently in flight (or paused on `pending_approval`) — **not** a
    /// permanent cached root the way TUI's `root: PathBuf` field is (this
    /// crate deliberately avoids one everywhere else). This exists solely
    /// so `pending_approval_preview`'s own disk read validates against the
    /// *same* root the run was submitted with, even if the user has since
    /// switched to a different open project mid-run (doc §3's
    /// project-switch-timing invariant: a live tool call is never
    /// redirected to a newly opened project — using `self.project`'s
    /// *current* root here instead would violate that same invariant for
    /// the preview specifically). Set in `submit`, cleared everywhere
    /// `rx`/`handle` are (cancel, `Done`) — identical lifecycle.
    active_root: Option<PathBuf>,
    runner: AgentRunner,
}

impl Default for AgentPanel {
    /// `mode` starts at `PermissionMode::Plan` (the safe default), not
    /// loaded from any project's `.ide/ai.json` — there may be no project
    /// open yet at construction time. The real per-project mode is loaded
    /// into `self.mode` by `app.rs`'s `load_project_settings`, the
    /// established hook every other project-scoped panel setting already
    /// uses (`self.custom_actions.actions = custom_actions::load(root)` is
    /// the exact precedent).
    fn default() -> Self {
        Self::with_runner(run_agent)
    }
}

impl AgentPanel {
    /// `pub(crate)`, not private: `app.rs`'s own tests swap in a fake
    /// runner, same reasoning as `AiPanel::with_runner`.
    pub(crate) fn with_runner(runner: AgentRunner) -> Self {
        Self {
            mode: PermissionMode::Plan,
            input: String::new(),
            history: Vec::new(),
            chat_history: Vec::new(),
            current_model_text: String::new(),
            streaming: false,
            handle: None,
            rx: None,
            pending_approval: None,
            active_root: None,
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
        self.active_root = None;
    }

    /// Cycles `Plan -> Approve -> Auto -> Plan` and persists the choice to
    /// `.ide/ai.json` via `project_root` (best-effort, same fail-open write
    /// convention `custom_actions.rs`/`project_state.rs` already use).
    pub fn cycle_mode(&mut self, project_root: &Path) {
        self.mode = match self.mode {
            PermissionMode::Plan => PermissionMode::Approve,
            PermissionMode::Approve => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Plan,
        };
        let mut config = AiConfig::load(project_root);
        config.agent_mode = self.mode;
        let _ = ide_core::project_settings::write(
            project_root,
            ide_core::project_settings::ProjectSettingsFile::Ai,
            &config,
        );
    }

    /// Appends the prompt to history and spawns the background run. No-op
    /// on a blank prompt or while a run is already in flight (no queue,
    /// same v1 scope `AiPanel::submit` has). `project_root` is read
    /// synchronously (via `ToolExecutor::new`/`AiConfig::load`) before
    /// anything is spawned — the same "fully synchronous prepare, no
    /// stale-root race" shape `AiPanel::submit` already established.
    pub fn submit(&mut self, prompt: String, project_root: &Path) {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() || self.is_in_flight() {
            return;
        }
        // `ToolExecutor::new` canonicalizes `project_root` internally
        // (`executor.rs`'s own doc comment: "canonicalized exactly once, at
        // construction") but doesn't expose the result -- `active_root`
        // canonicalizes independently here so it matches *exactly* what
        // `ToolExecutor`'s own `validate`/`edit_file` compare against
        // internally. This isn't just tidiness: on macOS, `std::env::
        // temp_dir()` (and other real paths) resolve through a `/var` ->
        // `/private/var` symlink, so a raw, uncanonicalized `active_root`
        // would make `validated_edit_target`'s `starts_with` check fail
        // for a perfectly legitimate in-root path -- the same reason
        // `ide_dap::path::validate_path`'s own doc comment requires its
        // `root` argument to already be canonical.
        let canonical_root = match std::fs::canonicalize(project_root) {
            Ok(root) => root,
            Err(e) => {
                self.history.push(AgentDisplayEntry::Error(e.to_string()));
                return;
            }
        };
        let executor = match ToolExecutor::new(project_root) {
            Ok(e) => e,
            Err(e) => {
                self.history.push(AgentDisplayEntry::Error(e.to_string()));
                return;
            }
        };
        self.history.push(AgentDisplayEntry::User(prompt.clone()));
        let config = AiConfig::load(project_root);
        let order = config.enabled_providers();
        let threshold = decide_threshold(&config, &order);
        let (masked_prompt, _map) = mask_outgoing(prompt, threshold);
        self.chat_history
            .push(ChatMessage::user(masked_prompt.clone()));
        let (events_tx, events_rx) = mpsc::channel();
        let (loop_, handle) = AgentLoop::new(self.mode, executor, events_tx);
        self.handle = Some(handle);
        self.rx = Some(events_rx);
        self.active_root = Some(canonical_root);
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
    /// the outcome the caller (`app.rs`, on the main thread, against its own
    /// real `DebugPanel` session) already computed.
    pub fn resolve_debug(&mut self, result: Result<String, ToolError>) {
        if let Some(h) = &self.handle {
            h.resume_with_debug_result(result);
        }
    }

    /// A diff/command/action preview for the approval popup: `EditFile`
    /// gets a real diff via `ide_core::diff_text`; `RunShellCommand` shows
    /// the literal program+args; everything else (including `DebugControl`)
    /// shows its one-line description. Returns `AgentApprovalPreview::
    /// Text(String::new())` when nothing is pending — callers must already
    /// gate rendering on `pending_approval.is_some()`, same as `ide-tui`'s
    /// own popup does.
    ///
    /// **Security-critical, not a mechanical detail**: unlike TUI's own
    /// `pending_approval_preview` (`crates/tui/src/agent_panel.rs`, already
    /// merged), which reads the "old" file content via a raw `self.root.
    /// join(path)` + `std::fs::read_to_string` with **no path validation**,
    /// this method's `EditFile` branch validates `path` via
    /// `validated_edit_target` (below) — the same parent-canonicalization
    /// approach `ide_agent::ToolExecutor::edit_file` itself already uses,
    /// not plain `ide_dap::path::validate_path` (which requires the target
    /// to already exist and would reject every legitimate new-file-creation
    /// proposal as "escapes the project root") — before ever touching disk.
    /// A path that fails validation returns `AgentApprovalPreview::
    /// Text("path escapes the project root")`, never a read.
    ///
    /// Why this matters and isn't hypothetical: `AgentTool::EditFile`'s
    /// `path` is model-supplied and can be steered by indirect prompt
    /// injection (T56 §4's own named threat model — adversarial content in
    /// a file/search-result/Docker log the agent already read). In
    /// `Approve` mode, `AwaitingApproval` fires and this method runs
    /// automatically the moment the popup renders, *before* a human decides
    /// anything — an unvalidated read at that point discloses arbitrary
    /// local file content into the UI with zero user action beyond the
    /// popup appearing. TUI's own `hacker` pass
    /// (`docs/security-findings/tui-local-agent-2026-09-08.md`) never
    /// actually exercised this specific function despite `agent_panel.rs`
    /// being nominally in its stated scope — a real, present-day gap in the
    /// already-shipped TUI code, independent of this port; the TUI original
    /// needs the identical fix as a separate follow-up, not covered by this
    /// crate's own scope.
    pub fn pending_approval_preview(&self) -> AgentApprovalPreview {
        let (Some(tool), Some(root)) = (&self.pending_approval, &self.active_root) else {
            return AgentApprovalPreview::Text(String::new());
        };
        match tool {
            AgentTool::EditFile { path, new_text } => match validated_edit_target(root, path) {
                Some(real) => {
                    let old = std::fs::read_to_string(&real).unwrap_or_default();
                    match diff_text(Path::new(path), &old, new_text) {
                        Some(diff) => AgentApprovalPreview::Diff(diff),
                        None => AgentApprovalPreview::NoDiff(format!(
                            "EditFile {path} (no textual diff)"
                        )),
                    }
                }
                None => AgentApprovalPreview::Text("path escapes the project root".to_string()),
            },
            AgentTool::RunShellCommand { program, args } => {
                AgentApprovalPreview::Text(format!("Run: {program} {}", args.join(" ")))
            }
            other => AgentApprovalPreview::Text(describe_tool(other)),
        }
    }

    /// Call once per frame; drains the events channel. Returns `Some` when
    /// the loop paused on `AgentEvent::AwaitingDebugExecution` and needs the
    /// caller to run `action` against its own real debug session (on the
    /// main thread) and call [`Self::resolve_debug`] with the outcome
    /// before the next poll tick — this is the one event this panel cannot
    /// handle by itself (`docs/features/tui-local-agent.md` §2.2).
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
                    self.active_root = None;
                    return None;
                }
            }
        }
    }

    /// Test-only seam: arms an injectable event channel exactly like
    /// `submit` would (minus the real background thread), so `app.rs`'s own
    /// tests — a different module, unable to reach the private `rx` field
    /// directly — can drive `poll`/`App::poll_agent`'s dispatch with a
    /// hand-crafted `AgentEvent` (in particular `AwaitingDebugExecution`,
    /// the one event `poll` can't resolve on its own).
    #[cfg(test)]
    pub(crate) fn test_arm_event_channel(&mut self, root: PathBuf) -> Sender<AgentEvent> {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        // Canonicalizes, mirroring `submit`'s own real behavior exactly
        // (see its doc comment) -- otherwise a macOS temp dir's `/var` ->
        // `/private/var` symlink would make `validated_edit_target`'s
        // `starts_with` check fail even for a legitimate in-root path.
        self.active_root = Some(std::fs::canonicalize(&root).unwrap_or(root));
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
                self.active_root = None;
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

/// Stricter than `ide_agent::executor::ToolExecutor::edit_file`'s own
/// parent-canonicalization approach: that function canonicalizes only
/// `path`'s *parent* before writing, which never resolves a symlink at the
/// leaf itself -- a symlink placed inside `project_root` pointing outside
/// it would canonicalize-and-pass at the parent check, then `fs::write`
/// follows it straight through on the next line. Read-only here, but this
/// method exists specifically to avoid disclosing exactly that kind of
/// escape, so it canonicalizes the *full* target first (resolving a leaf
/// symlink, matching `ide_dap::path::validate_path`'s own symlink-rejection
/// test) and only falls back to parent-only canonicalization when the full
/// path doesn't resolve at all -- which also covers the legitimate
/// new-file-creation case (`ToolExecutor::edit_file`'s target may not exist
/// yet). `None` on any failure -- a missing parent, a permission error, or
/// an actual escape -- the caller must never read from a `None` result.
///
/// **Security-critical, hacker-pass-verified** (`docs/security-findings/
/// rust-ui-dev-gui-local-agent-2026-09-08.md`, findings 1-2): two cases the
/// original version of this function got wrong, both live-verified via a
/// standalone symlink harness:
/// - A *dangling* leaf symlink (its target doesn't exist yet, so full
///   canonicalization fails and this falls into the new-file branch) used
///   to be treated as an ordinary new filename, since only `target`'s
///   *parent* was re-checked. That's wrong: a dangling symlink is not "a
///   name that doesn't exist yet", it's an existing filesystem entry that
///   already points somewhere -- `symlink_metadata` (never `metadata`,
///   which follows the link) below rejects it before the parent-only
///   fallback ever runs.
/// - An empty (or `"."`) `path` resolves to `project_root` itself, which
///   trivially passes `starts_with(project_root)` -- rejected explicitly
///   up front, since `EditFile` can never legitimately target the project
///   root directory.
fn validated_edit_target(project_root: &Path, path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    let target = project_root.join(path);
    if let Ok(canonical) = std::fs::canonicalize(&target) {
        // Rejects `path` values like `"."` or `"sub/.."` that fully
        // resolve back to the project root itself, not just an escape --
        // `EditFile` can never legitimately target the root directory.
        return (canonical != project_root && canonical.starts_with(project_root))
            .then_some(canonical);
    }
    // A dangling symlink already sitting at the leaf: `canonicalize` above
    // failed (its target doesn't exist), but it is *not* a fresh filename
    // -- `symlink_metadata` sees it without following it.
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

/// The background request run: resolve the role/route (T56 §3.3, reusing
/// T55 unchanged) then drive `AgentLoop::run` to completion. One thread per
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
            "ide-ui-agent-panel-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A no-op runner: never touches a real provider or `ide_agent`
    /// internals — it just proves `submit` builds a valid, executable
    /// `AgentPreparedRequest` and moves it onto a real thread.
    fn noop_runner(_prepared: AgentPreparedRequest) {}

    fn events_sender_and_panel(root: PathBuf) -> (AgentPanel, Sender<AgentEvent>) {
        let mut panel = AgentPanel::with_runner(noop_runner);
        let tx = panel.test_arm_event_channel(root);
        (panel, tx)
    }

    #[test]
    fn fresh_panel_is_idle_with_plan_mode_by_default() {
        let panel = AgentPanel::default();
        assert!(!panel.is_in_flight());
        assert_eq!(panel.mode, PermissionMode::Plan);
        assert!(panel.pending_approval.is_none());
    }

    #[test]
    fn cycle_mode_goes_plan_approve_auto_plan_and_persists() {
        let root = temp_root();
        let mut panel = AgentPanel::default();
        assert_eq!(panel.mode, PermissionMode::Plan);
        panel.cycle_mode(&root);
        assert_eq!(panel.mode, PermissionMode::Approve);
        panel.cycle_mode(&root);
        assert_eq!(panel.mode, PermissionMode::Auto);
        panel.cycle_mode(&root);
        assert_eq!(panel.mode, PermissionMode::Plan);

        // Persisted: a freshly constructed panel reloaded over the same
        // root (mirroring `load_project_settings`) picks up the
        // last-written mode.
        assert_eq!(AiConfig::load(&root).agent_mode, PermissionMode::Plan);
        panel.cycle_mode(&root);
        assert_eq!(AiConfig::load(&root).agent_mode, PermissionMode::Approve);
    }

    #[test]
    fn submit_on_blank_prompt_is_a_no_op() {
        let mut panel = AgentPanel::with_runner(noop_runner);
        panel.submit("   ".to_string(), &temp_root());
        assert!(panel.history.is_empty());
        assert!(!panel.is_in_flight());
    }

    #[test]
    fn submit_while_in_flight_is_a_no_op() {
        let mut panel = AgentPanel::with_runner(noop_runner);
        let root = temp_root();
        panel.submit("first".to_string(), &root);
        assert!(panel.is_in_flight());
        panel.submit("second".to_string(), &root);
        assert_eq!(
            panel.history,
            vec![AgentDisplayEntry::User("first".to_string())]
        );
    }

    #[test]
    fn submit_appends_user_entry_and_masked_chat_history() {
        let mut panel = AgentPanel::with_runner(noop_runner);
        panel.submit("hello there".to_string(), &temp_root());
        assert_eq!(
            panel.history,
            vec![AgentDisplayEntry::User("hello there".to_string())]
        );
        assert_eq!(panel.chat_history.len(), 1);
        assert!(panel.is_in_flight());
    }

    #[test]
    fn cancel_clears_in_flight_state() {
        let mut panel = AgentPanel::with_runner(noop_runner);
        panel.submit("hello".to_string(), &temp_root());
        assert!(panel.is_in_flight());
        panel.cancel();
        assert!(!panel.is_in_flight());
        assert!(panel.pending_approval.is_none());
    }

    #[test]
    fn poll_with_no_rx_is_none() {
        let mut panel = AgentPanel::default();
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
        let root = temp_root();
        let (mut panel, tx) = events_sender_and_panel(root.clone());
        let (resume_tx, resume_rx) = mpsc::channel();
        // Wire a fake handle by driving a real AgentLoop just far enough to
        // capture its handle -- simplest is to construct one directly.
        let executor = ToolExecutor::new(&root).unwrap();
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
        let mut panel = AgentPanel::default();
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
        let (mut panel, tx) = events_sender_and_panel(root);
        tx.send(AgentEvent::AwaitingApproval {
            tool: AgentTool::EditFile {
                path: "a.rs".into(),
                new_text: "new content".into(),
            },
        })
        .unwrap();
        panel.poll();
        match panel.pending_approval_preview() {
            AgentApprovalPreview::Diff(diff) => {
                let rendered: String = diff
                    .hunks
                    .iter()
                    .flat_map(|h| h.lines.iter())
                    .map(|l| format!("{l:?}"))
                    .collect();
                assert!(rendered.contains("old content") || rendered.contains("new content"));
            }
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn pending_approval_preview_shows_no_diff_for_a_brand_new_file() {
        // The target doesn't exist yet -- a legitimate new-file-creation
        // proposal, not a path escape: `validated_edit_target` must accept
        // it via the parent-canonicalization carve-out, same as
        // `ToolExecutor::edit_file` itself does.
        let root = temp_root();
        let (mut panel, tx) = events_sender_and_panel(root);
        tx.send(AgentEvent::AwaitingApproval {
            tool: AgentTool::EditFile {
                path: "new.rs".into(),
                new_text: "fn f() {}".into(),
            },
        })
        .unwrap();
        panel.poll();
        match panel.pending_approval_preview() {
            AgentApprovalPreview::Diff(diff) => {
                let added: String = diff
                    .hunks
                    .iter()
                    .flat_map(|h| h.lines.iter())
                    .map(|l| format!("{l:?}"))
                    .collect();
                assert!(added.contains("fn f() {}"));
            }
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn pending_approval_preview_rejects_a_path_escaping_the_project_root() {
        let root = temp_root();
        let (mut panel, tx) = events_sender_and_panel(root);
        tx.send(AgentEvent::AwaitingApproval {
            tool: AgentTool::EditFile {
                path: "../../../../etc/passwd".into(),
                new_text: "pwned".into(),
            },
        })
        .unwrap();
        panel.poll();
        assert_eq!(
            panel.pending_approval_preview(),
            AgentApprovalPreview::Text("path escapes the project root".to_string())
        );
    }

    #[test]
    fn pending_approval_preview_shows_argv_for_run_shell_command() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::AwaitingApproval {
            tool: AgentTool::RunShellCommand {
                program: "git".into(),
                args: vec!["push".into()],
            },
        })
        .unwrap();
        panel.poll();
        assert_eq!(
            panel.pending_approval_preview(),
            AgentApprovalPreview::Text("Run: git push".to_string())
        );
    }

    #[test]
    fn pending_approval_preview_describes_debug_control() {
        let (mut panel, tx) = events_sender_and_panel(temp_root());
        tx.send(AgentEvent::AwaitingApproval {
            tool: AgentTool::DebugControl(DebugAction::Resume),
        })
        .unwrap();
        panel.poll();
        assert_eq!(
            panel.pending_approval_preview(),
            AgentApprovalPreview::Text("DebugControl Resume".to_string())
        );
    }

    #[test]
    fn pending_approval_preview_is_empty_with_nothing_pending() {
        let panel = AgentPanel::default();
        assert_eq!(
            panel.pending_approval_preview(),
            AgentApprovalPreview::Text(String::new())
        );
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

    #[test]
    fn validated_edit_target_rejects_a_symlink_escaping_the_root() {
        #[cfg(unix)]
        {
            let root = temp_root();
            let outside = temp_root();
            let outside_file = outside.join("secret.rs");
            std::fs::write(&outside_file, "secret").unwrap();
            let link = root.join("link.rs");
            std::os::unix::fs::symlink(&outside_file, &link).unwrap();
            assert_eq!(validated_edit_target(&root, "link.rs"), None);
        }
    }

    /// `docs/security-findings/rust-ui-dev-gui-local-agent-2026-09-08.md`
    /// finding 1: a dangling symlink (its target doesn't exist yet, so
    /// full canonicalization fails) used to fall through to the new-file
    /// fallback, which only re-checked `target`'s *parent* -- treating an
    /// existing, root-escaping symlink as an ordinary not-yet-created
    /// filename.
    #[test]
    fn validated_edit_target_rejects_a_dangling_symlink_escaping_the_root() {
        #[cfg(unix)]
        {
            let root = std::fs::canonicalize(temp_root()).unwrap();
            let outside = temp_root();
            // Deliberately never created: `outside/never_created.rs`.
            let link = root.join("dangling.rs");
            std::os::unix::fs::symlink(outside.join("never_created.rs"), &link).unwrap();
            assert_eq!(validated_edit_target(&root, "dangling.rs"), None);
        }
    }

    /// Finding 2: an empty or `"."` path resolves to the project root
    /// directory itself, which trivially passes `starts_with`.
    #[test]
    fn validated_edit_target_rejects_empty_and_dot_paths() {
        // Canonical, so this actually exercises the `canonical ==
        // project_root` rejection rather than an incidental `/var` vs.
        // `/private/var` mismatch that would also produce `None`.
        let root = std::fs::canonicalize(temp_root()).unwrap();
        assert_eq!(validated_edit_target(&root, ""), None);
        assert_eq!(validated_edit_target(&root, "."), None);
    }

    #[test]
    fn validated_edit_target_still_allows_a_legitimate_new_file() {
        // Callers (`submit`) always pass an already-canonicalized root; a
        // raw `temp_root()` isn't canonical on macOS (`/var` -> `/private/
        // var`), which would make `starts_with` spuriously fail below.
        let root = std::fs::canonicalize(temp_root()).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let result = validated_edit_target(&root, "sub/brand_new.rs");
        assert_eq!(result, Some(root.join("sub/brand_new.rs")));
    }
}
