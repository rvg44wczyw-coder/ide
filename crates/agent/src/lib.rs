//! `ide-agent`: local agentic IDE assistant tool-calling engine
//! (`docs/features/tui-local-agent.md`, T56). Consumed only by `ide-tui`;
//! touches zero lines of `ide-core`/`ide-ai`/`ide-dap`, only their
//! existing public APIs.

mod agent_loop;
mod executor;
mod protocol;
mod tool;

pub use agent_loop::{
    AgentEvent, AgentHandle, AgentLoop, AgentResume, DoneReason, MAX_AGENT_STEPS,
    MAX_TOOL_RESULT_CHARS,
};
pub use executor::ToolExecutor;
pub use tool::{AgentTool, DebugAction, PermissionMode, ToolError, ToolResult};
