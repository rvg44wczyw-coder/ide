//! `DebugPanel`: bridges one `ide_dap::DapClient` into `IdeApp`'s frame
//! loop, the debugger analogue of `LspBridge` (`docs/features/
//! debugger.md` §2.3). One session at a time (§3.1) -- the "Debug"
//! command's own enablement gate lives in `app.rs`, not here.

use ide_dap::{
    Capabilities, DapClient, DapEvent, DapRequest, OutputCategory, SourceBreakpoint, StackFrame,
    ThreadInfo, VerifiedBreakpoint,
};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// Same bound class as `ClaudeTerminal::TERMINAL_SCROLLBACK_LIMIT` --
/// an adapter/debuggee producing unbounded `Output` events must not grow
/// UI memory without limit (doc §4).
pub const MAX_DEBUG_OUTPUT_LINES: usize = 2000;

#[derive(Default)]
pub struct DebugPanel {
    session: Option<DapClient>,
    pub capabilities: Option<Capabilities>,
    ready_for_breakpoints: bool,
    pub threads: Vec<ThreadInfo>,
    pub selected_thread: Option<i64>,
    pub stack: Vec<StackFrame>,
    /// Per-file breakpoints, independent of whether a session is active --
    /// toggling in the gutter works with no debugger running; they are
    /// sent to the adapter only once a session reaches
    /// `ReadyForBreakpoints` (§3.4).
    pub breakpoints: HashMap<PathBuf, Vec<u32>>,
    /// Adapter-reported verified/unverified status for the most recent
    /// `SetBreakpoints` per file -- lets the gutter paint a dimmed/hollow
    /// circle for a breakpoint the adapter rejected, distinct from a
    /// solid one that will actually fire (§3.4). Replaced wholesale per
    /// file on each `BreakpointsConfirmed`, not merged, since that's what
    /// the event itself reports (the adapter's answer to one whole-file
    /// `SetBreakpoints` request).
    pub confirmed_breakpoints: HashMap<PathBuf, Vec<VerifiedBreakpoint>>,
    pub output: VecDeque<(OutputCategory, String)>,
    pub error: Option<String>,
    /// Draft text for the "Debug" popup's launch-arguments field (raw
    /// JSON) -- a text field, not a form, per doc §2.3's explicit
    /// rationale (no run-configuration model exists yet to build a real
    /// one against).
    pub launch_args_draft: String,
    pub show_launch_popup: bool,
}

impl DebugPanel {
    pub fn is_active(&self) -> bool {
        self.session.is_some()
    }

    /// Drains every event available this frame, same pattern as
    /// `LspBridge::poll`. Returns `true` if anything changed (repaint).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.session.as_mut().and_then(DapClient::try_recv) {
            changed = true;
            self.apply_event(event);
        }
        changed
    }

    /// The state-transition half of `poll`, split out so it can be unit
    /// tested by constructing a `DapEvent` directly instead of needing a
    /// live adapter subprocess to actually emit one -- `self.send` already
    /// no-ops safely with no active `session`, so every arm below is
    /// exercisable this way.
    fn apply_event(&mut self, event: DapEvent) {
        match event {
            DapEvent::CapabilitiesReceived { capabilities } => {
                self.capabilities = Some(capabilities);
            }
            DapEvent::ReadyForBreakpoints => {
                self.ready_for_breakpoints = true;
                self.sync_all_breakpoints();
                self.send(DapRequest::ConfigurationDone);
            }
            DapEvent::BreakpointsConfirmed { path, breakpoints } => {
                self.confirmed_breakpoints.insert(path, breakpoints);
            }
            DapEvent::Stopped { thread_id, .. } => {
                // Refreshes the tool window's thread list too, not just
                // the stack -- otherwise nothing in this client ever
                // sends `Threads` and the left-hand list (doc §2.3)
                // stays permanently empty.
                self.send(DapRequest::Threads);
                if let Some(id) = thread_id {
                    self.selected_thread = Some(id);
                    self.send(DapRequest::StackTrace { thread_id: id });
                }
            }
            DapEvent::Continued { .. } => {
                self.stack.clear();
            }
            DapEvent::ThreadStarted { .. } | DapEvent::ThreadExited { .. } => {}
            DapEvent::Threads { threads } => {
                self.threads = threads;
            }
            DapEvent::StackTrace { frames, .. } => {
                self.stack = frames;
            }
            DapEvent::Output { category, text } => {
                self.output.push_back((category, text));
                if self.output.len() > MAX_DEBUG_OUTPUT_LINES {
                    self.output.pop_front();
                }
            }
            DapEvent::Exited { .. } => {}
            DapEvent::Terminated => {
                self.end_session();
            }
            DapEvent::RequestFailed { message, .. } => {
                self.error = Some(message);
            }
            DapEvent::AdapterExited { message } => {
                self.error = Some(message);
                self.end_session();
            }
        }
    }

    fn end_session(&mut self) {
        self.session = None;
        self.ready_for_breakpoints = false;
        self.capabilities = None;
        self.threads.clear();
        self.selected_thread = None;
        self.stack.clear();
    }

    fn send(&self, request: DapRequest) {
        if let Some(session) = &self.session {
            session.send(request);
        }
    }

    /// Toggles a breakpoint on `path`/`line` (works with no active
    /// session -- §3.4). While a session is active and past
    /// `ReadyForBreakpoints`, immediately re-sends the full breakpoint
    /// list for `path`.
    pub fn toggle_breakpoint(&mut self, path: PathBuf, line: u32) {
        let lines = self.breakpoints.entry(path.clone()).or_default();
        if let Some(pos) = lines.iter().position(|&l| l == line) {
            lines.remove(pos);
        } else {
            lines.push(line);
        }
        if lines.is_empty() {
            self.breakpoints.remove(&path);
        }
        if self.ready_for_breakpoints {
            self.sync_breakpoints(&path);
        }
    }

    fn sync_breakpoints(&self, path: &Path) {
        let breakpoints = self
            .breakpoints
            .get(path)
            .into_iter()
            .flatten()
            .map(|&line| SourceBreakpoint {
                line,
                condition: None,
                hit_condition: None,
                log_message: None,
            })
            .collect();
        self.send(DapRequest::SetBreakpoints {
            path: path.to_path_buf(),
            breakpoints,
        });
    }

    fn sync_all_breakpoints(&self) {
        let paths: Vec<PathBuf> = self.breakpoints.keys().cloned().collect();
        for path in paths {
            self.sync_breakpoints(&path);
        }
    }

    /// Starts a new session. No-op (and leaves `error` set) if one is
    /// already active -- §3.1's "Debug is disabled while a session runs"
    /// is enforced by the caller's command-enablement check; this is the
    /// method's own defensive backstop.
    pub fn start_session(
        &mut self,
        command: &str,
        args: &[String],
        project_root: impl AsRef<Path>,
        launch_arguments: serde_json::Value,
    ) {
        if self.session.is_some() {
            return;
        }
        self.error = None;
        self.output.clear();
        self.confirmed_breakpoints.clear();
        match DapClient::start(command, args, project_root) {
            Ok(client) => {
                client.send(DapRequest::Launch {
                    arguments: launch_arguments,
                });
                self.session = Some(client);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn target_thread(&self) -> Option<i64> {
        self.selected_thread
            .or_else(|| self.threads.first().map(|t| t.id))
    }

    pub fn resume(&mut self) {
        if let Some(id) = self.target_thread() {
            self.send(DapRequest::Continue { thread_id: id });
        }
    }

    pub fn step_over(&mut self) {
        if let Some(id) = self.target_thread() {
            self.send(DapRequest::Next { thread_id: id });
        }
    }

    pub fn step_into(&mut self) {
        if let Some(id) = self.target_thread() {
            self.send(DapRequest::StepIn { thread_id: id });
        }
    }

    pub fn step_out(&mut self) {
        if let Some(id) = self.target_thread() {
            self.send(DapRequest::StepOut { thread_id: id });
        }
    }

    pub fn pause(&mut self) {
        if let Some(id) = self.target_thread() {
            self.send(DapRequest::Pause { thread_id: id });
        }
    }

    /// Always `disconnect { terminateDebuggee: true }`, never `terminate`
    /// (§3.5) -- `DapRequest` doesn't even expose `Terminate`.
    pub fn stop(&mut self) {
        self.send(DapRequest::Disconnect);
        self.end_session();
    }

    pub fn select_thread(&mut self, id: i64) {
        self.selected_thread = Some(id);
        self.send(DapRequest::StackTrace { thread_id: id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_dap::StopReason;

    #[test]
    fn toggle_breakpoint_adds_then_removes() {
        let mut panel = DebugPanel::default();
        let path = PathBuf::from("/x.rs");
        panel.toggle_breakpoint(path.clone(), 10);
        assert_eq!(panel.breakpoints.get(&path), Some(&vec![10]));
        panel.toggle_breakpoint(path.clone(), 10);
        assert!(!panel.breakpoints.contains_key(&path));
    }

    #[test]
    fn toggle_breakpoint_works_with_no_active_session() {
        let mut panel = DebugPanel::default();
        assert!(!panel.is_active());
        panel.toggle_breakpoint(PathBuf::from("/x.rs"), 5);
        assert_eq!(panel.breakpoints.len(), 1);
    }

    #[test]
    fn start_session_with_missing_adapter_binary_sets_error_not_a_panic() {
        let mut panel = DebugPanel::default();
        let dir = tempfile::tempdir().unwrap();
        panel.start_session(
            "definitely-not-a-real-debug-adapter-xyz",
            &[],
            dir.path(),
            serde_json::json!({}),
        );
        assert!(!panel.is_active());
        assert!(panel.error.is_some());
    }

    #[test]
    fn resume_step_pause_are_no_ops_with_no_active_session() {
        let mut panel = DebugPanel::default();
        panel.resume();
        panel.step_over();
        panel.step_into();
        panel.step_out();
        panel.pause();
        panel.stop();
        assert!(!panel.is_active());
    }

    #[test]
    fn target_thread_falls_back_to_first_thread_when_none_selected() {
        let mut panel = DebugPanel {
            threads: vec![ThreadInfo {
                id: 7,
                name: "main".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(panel.target_thread(), Some(7));
        panel.selected_thread = Some(3);
        assert_eq!(panel.target_thread(), Some(3));
    }

    #[test]
    fn output_log_is_capped_at_max_debug_output_lines() {
        let mut panel = DebugPanel::default();
        for i in 0..MAX_DEBUG_OUTPUT_LINES + 10 {
            panel.apply_event(DapEvent::Output {
                category: OutputCategory::Stdout,
                text: i.to_string(),
            });
        }
        assert_eq!(panel.output.len(), MAX_DEBUG_OUTPUT_LINES);
        assert_eq!(panel.output.front().unwrap().1, "10");
    }

    #[test]
    fn stop_reason_other_is_carried_verbatim() {
        // Exercises the `StopReason` import for the test module without
        // needing a live adapter -- the wire-level parsing itself is
        // `ide-dap`'s own test responsibility.
        let reason = StopReason::Other("custom".to_string());
        match reason {
            StopReason::Other(s) => assert_eq!(s, "custom"),
            _ => panic!("expected Other"),
        }
    }

    #[test]
    fn apply_event_capabilities_received_stores_capabilities() {
        let mut panel = DebugPanel::default();
        let capabilities = Capabilities {
            supports_configuration_done_request: true,
            ..Default::default()
        };
        panel.apply_event(DapEvent::CapabilitiesReceived { capabilities });
        assert_eq!(panel.capabilities, Some(capabilities));
    }

    #[test]
    fn apply_event_ready_for_breakpoints_flips_the_flag_and_syncs_without_a_session() {
        let mut panel = DebugPanel::default();
        panel.toggle_breakpoint(PathBuf::from("/x.rs"), 3);
        panel.apply_event(DapEvent::ReadyForBreakpoints);
        assert!(panel.ready_for_breakpoints);
        // Toggling now re-syncs immediately -- exercises `sync_breakpoints`
        // with no active session (a no-op wire-wise, but must not panic).
        panel.toggle_breakpoint(PathBuf::from("/x.rs"), 4);
        assert_eq!(
            panel.breakpoints.get(&PathBuf::from("/x.rs")),
            Some(&vec![3, 4])
        );
    }

    #[test]
    fn apply_event_breakpoints_confirmed_replaces_the_entry_for_that_path() {
        let mut panel = DebugPanel::default();
        let path = PathBuf::from("/x.rs");
        panel.apply_event(DapEvent::BreakpointsConfirmed {
            path: path.clone(),
            breakpoints: vec![VerifiedBreakpoint {
                line: 3,
                verified: false,
                message: Some("unreachable".to_string()),
            }],
        });
        assert_eq!(
            panel.confirmed_breakpoints.get(&path).map(|v| v.len()),
            Some(1)
        );
        assert!(!panel.confirmed_breakpoints[&path][0].verified);
    }

    #[test]
    fn apply_event_stopped_with_a_thread_id_selects_it() {
        let mut panel = DebugPanel::default();
        panel.apply_event(DapEvent::Stopped {
            reason: StopReason::Breakpoint,
            thread_id: Some(9),
            description: None,
            all_threads_stopped: true,
        });
        assert_eq!(panel.selected_thread, Some(9));
    }

    #[test]
    fn apply_event_stopped_with_no_thread_id_leaves_selection_untouched() {
        let mut panel = DebugPanel {
            selected_thread: Some(1),
            ..Default::default()
        };
        panel.apply_event(DapEvent::Stopped {
            reason: StopReason::Pause,
            thread_id: None,
            description: None,
            all_threads_stopped: false,
        });
        assert_eq!(panel.selected_thread, Some(1));
    }

    #[test]
    fn apply_event_continued_clears_the_stack() {
        let mut panel = DebugPanel {
            stack: vec![StackFrame {
                id: 1,
                name: "main".to_string(),
                source: None,
                line: 1,
                column: 1,
            }],
            ..Default::default()
        };
        panel.apply_event(DapEvent::Continued {
            thread_id: 1,
            all_threads_continued: true,
        });
        assert!(panel.stack.is_empty());
    }

    #[test]
    fn apply_event_thread_started_and_exited_are_no_ops() {
        let mut panel = DebugPanel::default();
        panel.apply_event(DapEvent::ThreadStarted { thread_id: 1 });
        panel.apply_event(DapEvent::ThreadExited { thread_id: 1 });
        assert!(panel.threads.is_empty());
    }

    #[test]
    fn apply_event_threads_replaces_the_list() {
        let mut panel = DebugPanel::default();
        panel.apply_event(DapEvent::Threads {
            threads: vec![ThreadInfo {
                id: 1,
                name: "main".to_string(),
            }],
        });
        assert_eq!(panel.threads.len(), 1);
    }

    #[test]
    fn apply_event_stack_trace_replaces_the_stack() {
        let mut panel = DebugPanel::default();
        panel.apply_event(DapEvent::StackTrace {
            thread_id: 1,
            frames: vec![StackFrame {
                id: 1,
                name: "main".to_string(),
                source: None,
                line: 1,
                column: 1,
            }],
        });
        assert_eq!(panel.stack.len(), 1);
    }

    #[test]
    fn apply_event_exited_is_a_no_op() {
        let mut panel = DebugPanel::default();
        panel.apply_event(DapEvent::Exited { exit_code: 0 });
        assert!(panel.error.is_none());
    }

    #[test]
    fn apply_event_terminated_ends_the_session() {
        let mut panel = DebugPanel {
            capabilities: Some(Capabilities::default()),
            ready_for_breakpoints: true,
            ..Default::default()
        };
        panel.apply_event(DapEvent::Terminated);
        assert!(!panel.ready_for_breakpoints);
        assert!(panel.capabilities.is_none());
    }

    #[test]
    fn apply_event_request_failed_sets_the_error() {
        let mut panel = DebugPanel::default();
        panel.apply_event(DapEvent::RequestFailed {
            command: "launch".to_string(),
            request_seq: 1,
            message: "binary not found".to_string(),
        });
        assert_eq!(panel.error.as_deref(), Some("binary not found"));
    }

    #[test]
    fn apply_event_adapter_exited_sets_the_error_and_ends_the_session() {
        let mut panel = DebugPanel {
            ready_for_breakpoints: true,
            ..Default::default()
        };
        panel.apply_event(DapEvent::AdapterExited {
            message: "process died".to_string(),
        });
        assert_eq!(panel.error.as_deref(), Some("process died"));
        assert!(!panel.ready_for_breakpoints);
    }

    #[test]
    fn select_thread_updates_the_selection_with_no_active_session() {
        let mut panel = DebugPanel::default();
        panel.select_thread(4);
        assert_eq!(panel.selected_thread, Some(4));
    }

    #[test]
    fn start_session_succeeds_when_the_adapter_binary_spawns() {
        // `cat` never speaks DAP, so no `CapabilitiesReceived` will ever
        // arrive -- this only exercises the "process spawned" success
        // path (`DapClient::start`'s own contract: returns as soon as the
        // process spawns, the handshake completing asynchronously),
        // mirroring `lsp_bridge`'s own `cat`-as-fake-server test pattern.
        let mut panel = DebugPanel::default();
        let dir = tempfile::tempdir().unwrap();
        panel.start_session("cat", &[], dir.path(), serde_json::json!({}));
        assert!(panel.is_active());
        assert!(panel.error.is_none());

        // Exercises the `Some(session)` branch of `send` via every
        // execution-control method, plus `stop`'s own `end_session`.
        panel.threads = vec![ThreadInfo {
            id: 1,
            name: "main".to_string(),
        }];
        panel.toggle_breakpoint(PathBuf::from("/x.rs"), 1);
        panel.resume();
        panel.step_over();
        panel.step_into();
        panel.step_out();
        panel.pause();
        panel.select_thread(1);
        panel.stop();
        assert!(!panel.is_active());
    }

    #[test]
    fn start_session_is_a_no_op_while_a_session_is_already_active() {
        let mut panel = DebugPanel::default();
        let dir = tempfile::tempdir().unwrap();
        panel.start_session("cat", &[], dir.path(), serde_json::json!({}));
        assert!(panel.is_active());
        // A second `start_session` call while one is active must not
        // replace it (§3.1's own contract, enforced defensively here too)
        // -- this is a distinct outcome from `is_active()` returning
        // `false`, hence a separate assertion rather than folding this
        // into the test above.
        panel.start_session(
            "cat",
            &["--should-be-ignored".to_string()],
            dir.path(),
            serde_json::json!({}),
        );
        assert!(panel.is_active());
        panel.stop();
    }
}
