//! `AgentTool`/`DebugAction`/`ToolResult`/`ToolError` — the fixed set of
//! actions a model can call (`docs/features/tui-local-agent.md` §2.1).

/// The three modes the user asked for, by name. Defined in `ide_ai`
/// (alongside `AiConfig`, which persists it as `agent_mode`) rather than
/// here: `ide-ai` cannot depend on `ide-agent` without a cycle, so a type
/// `AiConfig` stores can't be defined on this side of that dependency
/// edge. Re-exported here so `ide_agent::PermissionMode` still works.
pub use ide_ai::PermissionMode;

/// One callable action. Read-only variants never require approval in
/// `PermissionMode::Approve` (§3.1).
#[derive(Debug, Clone, PartialEq)]
pub enum AgentTool {
    /// Read one file's full text. Path is relative to the project root.
    ReadFile { path: String },
    /// `ide_core::search_in_path::search_tree_advanced` over the project.
    SearchCode { query: String },
    /// Non-recursive directory listing (file/dir names only, no content).
    ListDirectory { path: String },
    /// `docker logs <container_id>` (read-only; container must already be
    /// running/exist -- this tool never starts one).
    ReadDockerLogs { container_id: String },

    /// Replace one file's full text with `new_text`, creating the file if
    /// it doesn't exist yet (its parent directory must already exist --
    /// no directory-creation tool in v1).
    EditFile { path: String, new_text: String },
    /// `program` + `args` only -- never a shell string (§4). `cwd` is
    /// always the project root; not configurable per-call.
    RunShellCommand { program: String, args: Vec<String> },
    /// One `ide_dap` session-control action.
    DebugControl(DebugAction),
}

impl AgentTool {
    /// Whether this tool needs the user's explicit per-call approval
    /// before it runs, independent of `PermissionMode` (`PermissionMode`
    /// decides *whether this flag is consulted at all* -- see §3.1).
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            AgentTool::ReadFile { .. }
                | AgentTool::SearchCode { .. }
                | AgentTool::ListDirectory { .. }
                | AgentTool::ReadDockerLogs { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugAction {
    Resume,
    StepOver,
    StepInto,
    StepOut,
    Pause,
    Stop,
    /// Toggles one line on/off. `ide_dap::DapRequest::SetBreakpoints` has
    /// no "toggle one" primitive -- it replaces a file's entire breakpoint
    /// set -- so the real per-file breakpoint list stays owned by
    /// `ide-tui`'s `DebugPanel`, which already tracks it.
    ToggleBreakpoint {
        path: String,
        line: u32,
    },
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub tool: AgentTool,
    pub outcome: Result<String, ToolError>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ToolError {
    #[error("path escapes the project root")]
    PathEscape,
    #[error("denied by permission mode")]
    Denied,
    #[error("denied by the user")]
    UserDenied,
    #[error("{0}")]
    Io(String),
    #[error("no active debug session")]
    NoDebugSession,
    /// The subprocess exceeded `AgentLoop::TOOL_EXECUTION_TIMEOUT` and was
    /// killed (`hacker` finding 2, 2026-09-08 -- previously an
    /// unconditionally-allowlisted command like `cat /dev/zero` could hang
    /// the executing thread forever with no recovery).
    #[error("command timed out and was killed")]
    Timeout,
    /// The user cancelled the agent request while this tool was running;
    /// the subprocess (if any) was killed rather than left to run
    /// detached in the background.
    #[error("cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_tools_are_not_mutating() {
        assert!(!AgentTool::ReadFile { path: "x".into() }.is_mutating());
        assert!(!AgentTool::SearchCode { query: "x".into() }.is_mutating());
        assert!(!AgentTool::ListDirectory { path: "x".into() }.is_mutating());
        assert!(!AgentTool::ReadDockerLogs {
            container_id: "x".into()
        }
        .is_mutating());
    }

    #[test]
    fn mutating_tools_are_mutating() {
        assert!(AgentTool::EditFile {
            path: "x".into(),
            new_text: "y".into()
        }
        .is_mutating());
        assert!(AgentTool::RunShellCommand {
            program: "cargo".into(),
            args: vec![]
        }
        .is_mutating());
        assert!(AgentTool::DebugControl(DebugAction::Resume).is_mutating());
        assert!(AgentTool::DebugControl(DebugAction::ToggleBreakpoint {
            path: "x".into(),
            line: 1
        })
        .is_mutating());
    }
}
