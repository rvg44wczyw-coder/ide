//! Tool-call protocol: the system prompt `AgentLoop` prepends to every run,
//! and the parser that turns a model's fenced ```` ```tool_call ```` block
//! back into an `AgentTool` (`docs/features/tui-local-agent.md` §2.1).

use serde_json::Value;

use crate::tool::{AgentTool, DebugAction};

const FENCE_OPEN: &str = "```tool_call";
const FENCE_CLOSE: &str = "```";

/// How much of a malformed fence to surface in `AgentEvent::
/// ToolCallParseFailed` -- a debugging aid, not a full dump.
const MAX_SNIPPET_CHARS: usize = 400;

pub enum ParseOutcome {
    /// No `tool_call` fence at all -- an ordinary final answer.
    None,
    /// A fence was present but failed to parse.
    Malformed(String),
    Tool(AgentTool),
}

/// The system prompt prepended at `history[0]` on every `Router::chat`
/// call, describing every `AgentTool` variant's exact JSON shape.
pub fn system_prompt() -> String {
    r#"You are an agentic IDE assistant. To take an action, reply with
exactly one fenced block tagged ```tool_call containing a single JSON
object, and nothing else in that block. Otherwise reply with plain text --
that ends the task.

Available tools:
```tool_call
{"tool": "ReadFile", "args": {"path": "relative/path.rs"}}
```
```tool_call
{"tool": "SearchCode", "args": {"query": "needle"}}
```
```tool_call
{"tool": "ListDirectory", "args": {"path": "relative/dir"}}
```
```tool_call
{"tool": "ReadDockerLogs", "args": {"container_id": "abc123"}}
```
```tool_call
{"tool": "EditFile", "args": {"path": "relative/path.rs", "new_text": "...full file contents..."}}
```
```tool_call
{"tool": "RunShellCommand", "args": {"program": "cargo", "args": ["test"]}}
```
```tool_call
{"tool": "DebugControl", "args": {"action": "Resume"}}
```
```tool_call
{"tool": "DebugControl", "args": {"action": "ToggleBreakpoint", "path": "relative/path.rs", "line": 42}}
```

`DebugControl`'s `action` is one of: Resume, StepOver, StepInto, StepOut,
Pause, Stop, ToggleBreakpoint. Only one tool call per reply. `EditFile`
always sends the complete new file content, never a patch."#
        .to_string()
}

/// The step-limit-reached final call's system prompt: tools are never
/// mentioned, so a well-behaved model has no protocol to follow for this
/// one turn.
pub fn final_system_prompt() -> String {
    "You are an agentic IDE assistant. You have reached the maximum number \
     of tool calls for this task. Summarize what you accomplished and what \
     remains, in plain text. No tools are available for this reply."
        .to_string()
}

/// Looks for a ```` ```tool_call ```` fence in `turn_text`; only the first
/// one is ever considered (§2.1). Anything that isn't a well-formed match
/// -- no fence, malformed JSON, an unknown tool name, missing/wrong-typed
/// args -- when a fence IS present is `Malformed`; no fence at all is
/// `None`.
pub fn parse_tool_call(turn_text: &str) -> ParseOutcome {
    let Some(open_idx) = turn_text.find(FENCE_OPEN) else {
        return ParseOutcome::None;
    };
    let after_open = open_idx + FENCE_OPEN.len();
    let Some(close_offset) = turn_text[after_open..].find(FENCE_CLOSE) else {
        return ParseOutcome::Malformed(snippet(&turn_text[open_idx..]));
    };
    let inner = turn_text[after_open..after_open + close_offset].trim();
    let snippet_source = &turn_text[open_idx..after_open + close_offset + FENCE_CLOSE.len()];

    let value: Value = match serde_json::from_str(inner) {
        Ok(v) => v,
        Err(_) => return ParseOutcome::Malformed(snippet(snippet_source)),
    };
    match tool_from_value(&value) {
        Some(tool) => ParseOutcome::Tool(tool),
        None => ParseOutcome::Malformed(snippet(snippet_source)),
    }
}

fn snippet(s: &str) -> String {
    if s.chars().count() > MAX_SNIPPET_CHARS {
        s.chars().take(MAX_SNIPPET_CHARS).collect()
    } else {
        s.to_string()
    }
}

fn tool_from_value(value: &Value) -> Option<AgentTool> {
    let tool_name = value.get("tool")?.as_str()?;
    let args = value.get("args")?;
    match tool_name {
        "ReadFile" => Some(AgentTool::ReadFile {
            path: str_field(args, "path")?,
        }),
        "SearchCode" => Some(AgentTool::SearchCode {
            query: str_field(args, "query")?,
        }),
        "ListDirectory" => Some(AgentTool::ListDirectory {
            path: str_field(args, "path")?,
        }),
        "ReadDockerLogs" => Some(AgentTool::ReadDockerLogs {
            container_id: str_field(args, "container_id")?,
        }),
        "EditFile" => Some(AgentTool::EditFile {
            path: str_field(args, "path")?,
            new_text: str_field(args, "new_text")?,
        }),
        "RunShellCommand" => {
            let program = str_field(args, "program")?;
            let args_list: Vec<String> = args
                .get("args")?
                .as_array()?
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?;
            Some(AgentTool::RunShellCommand {
                program,
                args: args_list,
            })
        }
        "DebugControl" => debug_action_from_value(args).map(AgentTool::DebugControl),
        _ => None,
    }
}

fn debug_action_from_value(args: &Value) -> Option<DebugAction> {
    let action = args.get("action")?.as_str()?;
    match action {
        "Resume" => Some(DebugAction::Resume),
        "StepOver" => Some(DebugAction::StepOver),
        "StepInto" => Some(DebugAction::StepInto),
        "StepOut" => Some(DebugAction::StepOut),
        "Pause" => Some(DebugAction::Pause),
        "Stop" => Some(DebugAction::Stop),
        "ToggleBreakpoint" => Some(DebugAction::ToggleBreakpoint {
            path: str_field(args, "path")?,
            line: args.get("line")?.as_u64()? as u32,
        }),
        _ => None,
    }
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_fence_is_none() {
        assert!(matches!(
            parse_tool_call("just a plain answer"),
            ParseOutcome::None
        ));
    }

    #[test]
    fn unterminated_fence_is_malformed() {
        let text = "prose\n```tool_call\n{\"tool\": \"ReadFile\"";
        assert!(matches!(parse_tool_call(text), ParseOutcome::Malformed(_)));
    }

    #[test]
    fn invalid_json_is_malformed() {
        let text = "```tool_call\nnot json at all\n```";
        assert!(matches!(parse_tool_call(text), ParseOutcome::Malformed(_)));
    }

    #[test]
    fn unknown_tool_name_is_malformed() {
        let text = "```tool_call\n{\"tool\": \"DeleteEverything\", \"args\": {}}\n```";
        assert!(matches!(parse_tool_call(text), ParseOutcome::Malformed(_)));
    }

    #[test]
    fn missing_required_arg_is_malformed() {
        let text = "```tool_call\n{\"tool\": \"ReadFile\", \"args\": {}}\n```";
        assert!(matches!(parse_tool_call(text), ParseOutcome::Malformed(_)));
    }

    #[test]
    fn read_file_parses() {
        let text = "```tool_call\n{\"tool\": \"ReadFile\", \"args\": {\"path\": \"a.rs\"}}\n```";
        match parse_tool_call(text) {
            ParseOutcome::Tool(AgentTool::ReadFile { path }) => assert_eq!(path, "a.rs"),
            _ => panic!("expected ReadFile"),
        }
    }

    #[test]
    fn run_shell_command_parses_program_and_args() {
        let text = r#"```tool_call
{"tool": "RunShellCommand", "args": {"program": "cargo", "args": ["test", "-p", "ide-agent"]}}
```"#;
        match parse_tool_call(text) {
            ParseOutcome::Tool(AgentTool::RunShellCommand { program, args }) => {
                assert_eq!(program, "cargo");
                assert_eq!(args, vec!["test", "-p", "ide-agent"]);
            }
            _ => panic!("expected RunShellCommand"),
        }
    }

    #[test]
    fn debug_control_toggle_breakpoint_parses() {
        let text = r#"```tool_call
{"tool": "DebugControl", "args": {"action": "ToggleBreakpoint", "path": "a.rs", "line": 42}}
```"#;
        match parse_tool_call(text) {
            ParseOutcome::Tool(AgentTool::DebugControl(DebugAction::ToggleBreakpoint {
                path,
                line,
            })) => {
                assert_eq!(path, "a.rs");
                assert_eq!(line, 42);
            }
            _ => panic!("expected DebugControl(ToggleBreakpoint)"),
        }
    }

    #[test]
    fn debug_control_resume_parses() {
        let text =
            "```tool_call\n{\"tool\": \"DebugControl\", \"args\": {\"action\": \"Resume\"}}\n```";
        assert!(matches!(
            parse_tool_call(text),
            ParseOutcome::Tool(AgentTool::DebugControl(DebugAction::Resume))
        ));
    }

    #[test]
    fn debug_control_unknown_action_is_malformed() {
        let text =
            "```tool_call\n{\"tool\": \"DebugControl\", \"args\": {\"action\": \"Nope\"}}\n```";
        assert!(matches!(parse_tool_call(text), ParseOutcome::Malformed(_)));
    }

    #[test]
    fn only_the_first_fenced_block_is_considered() {
        let text = r#"```tool_call
{"tool": "ReadFile", "args": {"path": "first.rs"}}
```
some prose
```tool_call
{"tool": "ReadFile", "args": {"path": "second.rs"}}
```"#;
        match parse_tool_call(text) {
            ParseOutcome::Tool(AgentTool::ReadFile { path }) => assert_eq!(path, "first.rs"),
            _ => panic!("expected ReadFile(first.rs)"),
        }
    }

    #[test]
    fn prose_alongside_a_tool_call_is_still_parsed() {
        let text = "Sure, let me check.\n```tool_call\n{\"tool\": \"ReadFile\", \"args\": {\"path\": \"a.rs\"}}\n```";
        assert!(matches!(
            parse_tool_call(text),
            ParseOutcome::Tool(AgentTool::ReadFile { .. })
        ));
    }

    #[test]
    fn snippet_is_truncated_for_a_very_long_malformed_fence() {
        let long_garbage = "x".repeat(MAX_SNIPPET_CHARS * 2);
        let text = format!("```tool_call\n{long_garbage}\n```");
        match parse_tool_call(&text) {
            ParseOutcome::Malformed(snippet) => {
                assert!(snippet.chars().count() <= MAX_SNIPPET_CHARS)
            }
            _ => panic!("expected Malformed"),
        }
    }
}
