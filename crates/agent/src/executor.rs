//! `ToolExecutor` — runs one `AgentTool` for real (`docs/features/
//! tui-local-agent.md` §2.1). Never touches `AgentTool::DebugControl`;
//! `AgentLoop` intercepts that variant before it reaches here (see the
//! doc's "Why `DebugControl` can't be just another `ToolExecutor` case").

use std::path::{Path, PathBuf};

use tokio::process::Command;

use ide_core::buffer_search::SearchOptions;
use ide_core::project::Project;
use ide_core::search_in_path::{search_tree_advanced, PathSearchOptions};

use crate::tool::{AgentTool, ToolError, ToolResult};

/// Owns the canonicalized project root (canonicalized exactly once, at
/// construction). Executes one `AgentTool` at a time; never spawns
/// concurrent tool calls.
pub struct ToolExecutor {
    project_root: PathBuf,
}

impl ToolExecutor {
    pub fn new(project_root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            project_root: std::fs::canonicalize(project_root)?,
        })
    }

    /// Runs `tool` for real (permission-mode/approval gating already
    /// happened in `AgentLoop` -- this method always executes). Never
    /// called with `AgentTool::DebugControl`.
    pub async fn execute(&mut self, tool: AgentTool) -> ToolResult {
        let outcome = match &tool {
            AgentTool::ReadFile { path } => self.read_file(path),
            AgentTool::ListDirectory { path } => self.list_directory(path),
            AgentTool::SearchCode { query } => self.search_code(query),
            AgentTool::ReadDockerLogs { container_id } => self.read_docker_logs(container_id).await,
            AgentTool::EditFile { path, new_text } => self.edit_file(path, new_text),
            AgentTool::RunShellCommand { program, args } => {
                self.run_shell_command(program, args).await
            }
            AgentTool::DebugControl(_) => Err(ToolError::Io(
                "DebugControl must be executed by the caller, not ToolExecutor".to_string(),
            )),
        };
        ToolResult { tool, outcome }
    }

    fn validate(&self, path: &str) -> Result<PathBuf, ToolError> {
        let candidate = self.project_root.join(path);
        ide_dap::path::validate_path(&self.project_root, &candidate).ok_or(ToolError::PathEscape)
    }

    fn read_file(&self, path: &str) -> Result<String, ToolError> {
        let real = self.validate(path)?;
        std::fs::read_to_string(&real).map_err(|e| ToolError::Io(e.to_string()))
    }

    fn list_directory(&self, path: &str) -> Result<String, ToolError> {
        let real = self.validate(path)?;
        let mut entries: Vec<String> = std::fs::read_dir(&real)
            .map_err(|e| ToolError::Io(e.to_string()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                match entry.file_type() {
                    Ok(ft) if ft.is_dir() => format!("{name}/"),
                    _ => name,
                }
            })
            .collect();
        entries.sort();
        Ok(entries.join("\n"))
    }

    fn search_code(&self, query: &str) -> Result<String, ToolError> {
        let project =
            Project::open(&self.project_root).map_err(|e| ToolError::Io(e.to_string()))?;
        let tree = project.scan_tree();
        let options = PathSearchOptions {
            search: SearchOptions {
                case_sensitive: false,
                whole_word: false,
                regex: false,
            },
            include: Vec::new(),
            exclude: Vec::new(),
            respect_gitignore: true,
        };
        let results = search_tree_advanced(&tree, query, &options)
            .map_err(|e| ToolError::Io(e.to_string()))?;
        let mut lines: Vec<String> = results
            .matches
            .iter()
            .map(|m| format!("{}:{}: {}", m.path.display(), m.line + 1, m.line_text))
            .collect();
        if results.truncated {
            lines.push("... (results truncated)".to_string());
        }
        Ok(lines.join("\n"))
    }

    async fn read_docker_logs(&self, container_id: &str) -> Result<String, ToolError> {
        // `kill_on_drop(true)`: if the caller (`AgentLoop::run_one_tool`)
        // drops this future because it lost a `tokio::select!` race
        // against its timeout or a cancellation signal, the spawned
        // `docker` process is killed rather than left running detached
        // (`hacker` finding 2, 2026-09-08).
        let output = Command::new("docker")
            .args(["logs", container_id])
            .current_dir(&self.project_root)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| ToolError::Io(format!("docker not found or failed to run: {e}")))?;
        Ok(format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }

    /// `EditFile` cannot use `validate` unmodified: its target may not
    /// exist yet (creating a new file), and `validate_path` canonicalizes
    /// the path itself, which requires the target to already exist.
    ///
    /// This used to canonicalize only the *parent* directory and join the
    /// file name back on unchecked -- which meant an existing symlink
    /// already sitting at the leaf position (dangling or not) was never
    /// itself resolved or rejected, and `std::fs::write` follows symlinks:
    /// a project-tree symlink pointing outside `project_root` let `path`
    /// resolve inside the check but write anywhere on disk the symlink's
    /// target could reach. Exactly the class of bug `crates/ui/src/
    /// agent_panel.rs::validated_edit_target` was written to fix for the
    /// read-only preview path (G10's own `hacker` findings,
    /// `docs/security-findings/rust-ui-dev-gui-local-agent-2026-09-08.md`)
    /// -- flagged there as a known, unported gap in this exact function
    /// (`docs/roadmap.md`'s G10 row) since this one is the actual write
    /// path, not just a preview. Ported the identical fix: canonicalize
    /// the full target first; an existing leaf must resolve inside
    /// `project_root` and must not be a directory (`!canonical.is_dir()`
    /// also subsumes the case where `path` resolves back to the root
    /// itself, e.g. `""`/`"."`); a *dangling* symlink at the leaf (whose
    /// target doesn't exist, so `canonicalize` can't see it --
    /// `symlink_metadata` can, without following it) is rejected too. Only
    /// once neither the leaf nor anything symlink-shaped exists there does
    /// this fall back to the original parent-only canonicalization, for a
    /// genuinely new file. A parent that fails to canonicalize at all
    /// (doesn't exist, permission denied) is `ToolError::Io`, not
    /// `PathEscape` -- that variant is reserved for an actual escape.
    /// There is no directory-creation tool in v1, so a missing parent is a
    /// real, narrow scope cut, not a silent gap.
    fn validated_edit_target(&self, path: &str) -> Result<PathBuf, ToolError> {
        if path.is_empty() {
            return Err(ToolError::PathEscape);
        }
        let target = self.project_root.join(path);
        if let Ok(canonical) = std::fs::canonicalize(&target) {
            return if canonical.starts_with(&self.project_root) && !canonical.is_dir() {
                Ok(canonical)
            } else {
                Err(ToolError::PathEscape)
            };
        }
        if std::fs::symlink_metadata(&target).is_ok() {
            return Err(ToolError::PathEscape);
        }
        let parent = target.parent().ok_or(ToolError::PathEscape)?;
        let canonical_parent =
            std::fs::canonicalize(parent).map_err(|e| ToolError::Io(e.to_string()))?;
        if !canonical_parent.starts_with(&self.project_root) {
            return Err(ToolError::PathEscape);
        }
        let file_name = target.file_name().ok_or(ToolError::PathEscape)?;
        Ok(canonical_parent.join(file_name))
    }

    fn edit_file(&self, path: &str, new_text: &str) -> Result<String, ToolError> {
        let real_target = self.validated_edit_target(path)?;
        std::fs::write(&real_target, new_text).map_err(|e| ToolError::Io(e.to_string()))?;
        Ok(format!("wrote {} bytes to {path}", new_text.len()))
    }

    async fn run_shell_command(&self, program: &str, args: &[String]) -> Result<String, ToolError> {
        // See `read_docker_logs`'s comment on `kill_on_drop(true)`.
        let output = Command::new(program)
            .args(args)
            .current_dir(&self.project_root)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| ToolError::Io(format!("{program} not found or failed to run: {e}")))?;
        Ok(format!(
            "exit status: {}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::AgentTool;

    fn temp_project() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn read_file_returns_contents() {
        let dir = temp_project();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::ReadFile {
            path: "a.txt".into(),
        }));
        assert_eq!(result.outcome.unwrap(), "hello");
    }

    #[test]
    fn read_file_rejects_path_outside_root() {
        let dir = temp_project();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "nope").unwrap();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let escape = format!(
            "../{}/secret.txt",
            outside.path().file_name().unwrap().to_string_lossy()
        );
        let result = block_on(exec.execute(AgentTool::ReadFile { path: escape }));
        assert_eq!(result.outcome.unwrap_err(), ToolError::PathEscape);
    }

    #[test]
    fn read_file_missing_is_path_escape_not_io() {
        // `validate_path` canonicalizes its target, which requires the
        // target to already exist -- a missing file is indistinguishable
        // from an escape at that layer, same as `ide_dap`'s own
        // `rejects_nonexistent_path` test establishes. Only `EditFile` gets
        // the parent-dir workaround that can tell the two apart.
        let dir = temp_project();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::ReadFile {
            path: "missing.txt".into(),
        }));
        assert_eq!(result.outcome.unwrap_err(), ToolError::PathEscape);
    }

    #[test]
    fn list_directory_lists_files_and_dirs() {
        let dir = temp_project();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::ListDirectory { path: ".".into() }));
        let text = result.outcome.unwrap();
        assert!(text.contains("a.txt"));
        assert!(text.contains("sub/"));
    }

    #[test]
    fn edit_file_creates_a_new_file_without_path_escape() {
        let dir = temp_project();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::EditFile {
            path: "new_file.rs".into(),
            new_text: "fn main() {}".into(),
        }));
        assert!(result.outcome.is_ok(), "{:?}", result.outcome);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new_file.rs")).unwrap(),
            "fn main() {}"
        );
    }

    #[test]
    fn edit_file_overwrites_an_existing_file() {
        let dir = temp_project();
        std::fs::write(dir.path().join("a.rs"), "old").unwrap();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::EditFile {
            path: "a.rs".into(),
            new_text: "new".into(),
        }));
        assert!(result.outcome.is_ok());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "new"
        );
    }

    #[test]
    fn edit_file_rejects_parent_directory_that_does_not_exist() {
        let dir = temp_project();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::EditFile {
            path: "missing_dir/new_file.rs".into(),
            new_text: "x".into(),
        }));
        assert!(matches!(result.outcome, Err(ToolError::Io(_))));
    }

    #[test]
    fn edit_file_rejects_parent_outside_root() {
        let dir = temp_project();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::EditFile {
            path: "../new_file.rs".into(),
            new_text: "x".into(),
        }));
        assert_eq!(result.outcome.unwrap_err(), ToolError::PathEscape);
    }

    #[test]
    fn edit_file_rejects_a_symlink_at_the_leaf_that_escapes_the_root() {
        // The bug this guards against: a project-tree symlink pointing
        // outside `project_root`, with the join-then-canonicalize-parent-
        // only approach never resolving/rejecting the leaf itself --
        // `std::fs::write` follows symlinks, so the old code would have
        // written straight through it to `outside_file`.
        #[cfg(unix)]
        {
            let dir = temp_project();
            let outside = temp_project();
            let outside_file = outside.path().join("secret.rs");
            std::fs::write(&outside_file, "secret").unwrap();
            let link = dir.path().join("link.rs");
            std::os::unix::fs::symlink(&outside_file, &link).unwrap();
            let mut exec = ToolExecutor::new(dir.path()).unwrap();
            let result = block_on(exec.execute(AgentTool::EditFile {
                path: "link.rs".into(),
                new_text: "pwned".into(),
            }));
            assert_eq!(result.outcome.unwrap_err(), ToolError::PathEscape);
            assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "secret");
        }
    }

    #[test]
    fn edit_file_rejects_a_dangling_symlink_at_the_leaf() {
        // `canonicalize` can't see this (the symlink's target doesn't
        // exist), but it must not be treated as a fresh filename either --
        // `symlink_metadata` sees the symlink itself without following it.
        #[cfg(unix)]
        {
            let dir = temp_project();
            let outside = temp_project();
            let nonexistent_target = outside.path().join("does_not_exist.rs");
            let link = dir.path().join("dangling.rs");
            std::os::unix::fs::symlink(&nonexistent_target, &link).unwrap();
            let mut exec = ToolExecutor::new(dir.path()).unwrap();
            let result = block_on(exec.execute(AgentTool::EditFile {
                path: "dangling.rs".into(),
                new_text: "pwned".into(),
            }));
            assert_eq!(result.outcome.unwrap_err(), ToolError::PathEscape);
            assert!(!nonexistent_target.exists());
        }
    }

    #[test]
    fn edit_file_rejects_an_existing_directory() {
        let dir = temp_project();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::EditFile {
            path: "sub".into(),
            new_text: "x".into(),
        }));
        assert_eq!(result.outcome.unwrap_err(), ToolError::PathEscape);
    }

    #[test]
    fn edit_file_rejects_an_empty_path() {
        let dir = temp_project();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::EditFile {
            path: String::new(),
            new_text: "x".into(),
        }));
        assert_eq!(result.outcome.unwrap_err(), ToolError::PathEscape);
    }

    #[test]
    fn run_shell_command_never_uses_a_shell() {
        let dir = temp_project();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        // A literal `;` in an argv element must be passed through
        // untouched, never interpreted -- proof there is no shell
        // involved.
        let result = block_on(exec.execute(AgentTool::RunShellCommand {
            program: "echo".into(),
            args: vec!["a; rm -rf /".into()],
        }));
        let text = result.outcome.unwrap();
        assert!(text.contains("a; rm -rf /"));
        assert!(!dir.path().join("rm").exists());
    }

    /// `ToolExecutor` itself is deliberately unopinionated about which
    /// programs `RunShellCommand` may name -- that gate is
    /// `agent_loop::is_allowlisted`'s job, consulted only for *unattended*
    /// `Auto`-mode execution. In `Approve`/`Plan` mode a human explicitly
    /// approves the exact `program`/`args` shown, including a path-
    /// qualified one pointing at a project-local script -- a legitimate
    /// use `is_allowlisted`'s post-`hacker`-finding-1 path-separator
    /// rejection must not also block here. Confirms the executor runs a
    /// same-named-as-a-trusted-tool, path-qualified, non-`$PATH` binary
    /// exactly as instructed, with no path check of its own.
    #[test]
    fn run_shell_command_runs_whatever_program_it_is_given_no_matter_the_path() {
        let dir = temp_project();
        let planted = dir.path().join("cargo");
        std::fs::write(&planted, "#!/bin/sh\necho not-the-real-cargo\n").unwrap();
        let mut perms = std::fs::metadata(&planted).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        std::fs::set_permissions(&planted, perms).unwrap();

        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::RunShellCommand {
            program: "./cargo".into(),
            args: vec![],
        }));
        assert!(result.outcome.unwrap().contains("not-the-real-cargo"));
    }

    #[test]
    fn run_shell_command_missing_program_is_io_error() {
        let dir = temp_project();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::RunShellCommand {
            program: "definitely-not-a-real-binary-xyz".into(),
            args: vec![],
        }));
        assert!(matches!(result.outcome, Err(ToolError::Io(_))));
    }

    #[test]
    fn search_code_finds_a_match() {
        let dir = temp_project();
        std::fs::write(dir.path().join("a.rs"), "fn needle() {}").unwrap();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result = block_on(exec.execute(AgentTool::SearchCode {
            query: "needle".into(),
        }));
        assert!(result.outcome.unwrap().contains("needle"));
    }

    #[test]
    fn debug_control_is_never_executed_by_tool_executor() {
        let dir = temp_project();
        let mut exec = ToolExecutor::new(dir.path()).unwrap();
        let result =
            block_on(exec.execute(AgentTool::DebugControl(crate::tool::DebugAction::Resume)));
        assert!(matches!(result.outcome, Err(ToolError::Io(_))));
    }
}
