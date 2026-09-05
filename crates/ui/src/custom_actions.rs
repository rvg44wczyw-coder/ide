//! User-declared named external commands, run from the Custom Actions dock
//! tab (`docs/features/custom-actions.md`, `G8`). Shares
//! `.ide/custom_actions.json` with `ide-tui`'s own `crates/tui/src/
//! custom_actions.rs` (`T42`) -- the struct shapes here are field-identical
//! to that module's on purpose, so the file round-trips between frontends.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

use ide_core::project_settings::{self, ProjectSettingsFile};
use serde::{Deserialize, Serialize};

/// Hard cap on how many actions `load` will hand back -- mirrors
/// `crates/tui/src/custom_actions.rs`'s own `MAX_CUSTOM_ACTIONS`, applied at
/// *load* time since `.ide/custom_actions.json` can be populated by
/// something other than this feature's own write path (a cloned
/// repository, or `ide-tui`'s own Manage popup).
const MAX_CUSTOM_ACTIONS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CustomAction {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CustomActionsFile {
    pub(crate) actions: Vec<CustomAction>,
}

/// Best-effort load: missing file, malformed JSON, or a `.ide/` symlink
/// escape all collapse to an empty list.
pub(crate) fn load(project_root: &Path) -> Vec<CustomAction> {
    let mut actions = project_settings::read::<CustomActionsFile>(
        project_root,
        ProjectSettingsFile::CustomActions,
    )
    .ok()
    .flatten()
    .unwrap_or_default()
    .actions;
    actions.truncate(MAX_CUSTOM_ACTIONS);
    actions
}

/// Best-effort save -- swallows every failure, same fail-open posture as
/// `load`.
pub(crate) fn save(project_root: &Path, actions: &[CustomAction]) {
    let _ = project_settings::write(
        project_root,
        ProjectSettingsFile::CustomActions,
        &CustomActionsFile {
            actions: actions.to_vec(),
        },
    );
}

pub(crate) enum StreamEvent {
    Line(String),
    Done,
}

#[derive(Default)]
pub(crate) struct CustomActionsPanel {
    pub(crate) actions: Vec<CustomAction>,
    pub(crate) selected: usize,
    /// A **clone** of the action currently running, not an index into
    /// `actions` -- deleting the definition mid-run (via the Manage popup)
    /// must never invalidate or panic the in-flight run.
    pub(crate) running: Option<CustomAction>,
    pub(crate) output: Vec<String>,
    pub(crate) rx: Option<Receiver<StreamEvent>>,
}

impl CustomActionsPanel {
    /// No-op if `running.is_some()` (v1 scope: at most one in flight, same
    /// as `CargoPanel::run`) or if `actions` is empty or `selected` is out
    /// of range.
    pub(crate) fn run_selected(&mut self, project_root: &Path) {
        if self.running.is_some() {
            return;
        }
        let Some(action) = self.actions.get(self.selected).cloned() else {
            return;
        };
        self.output.clear();
        self.rx = Some(spawn_streaming(&action.command, &action.args, project_root));
        self.running = Some(action);
    }

    /// Returns `true` if `output`/`running` changed this call -- same
    /// signature as `CargoPanel::poll`, used by the caller to decide
    /// whether to `ctx.request_repaint()`.
    pub(crate) fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        let mut changed = false;
        loop {
            match rx.try_recv() {
                Ok(StreamEvent::Line(line)) => {
                    self.output.push(line);
                    changed = true;
                }
                Ok(StreamEvent::Done) => {
                    self.running = None;
                    self.rx = None;
                    changed = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.running = None;
                    self.rx = None;
                    changed = true;
                    break;
                }
            }
        }
        changed
    }
}

fn spawn_streaming(program: &str, args: &[String], project_root: &Path) -> Receiver<StreamEvent> {
    let (tx, rx) = mpsc::channel();
    let program = program.to_string();
    let args = args.to_vec();
    let project_root = project_root.to_path_buf();
    thread::spawn(move || run_and_stream(&program, &args, &project_root, &tx));
    rx
}

/// `args` reaches the child as an explicit argv via `Command::args` --
/// never formatted into a shell command string -- so shell metacharacters
/// typed into an action's Args field can't be interpreted.
fn run_and_stream(program: &str, args: &[String], project_root: &Path, tx: &Sender<StreamEvent>) {
    let mut child = match Command::new(program)
        .args(args)
        .current_dir(project_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = tx.send(StreamEvent::Line(format!("{program} not found on PATH")));
            let _ = tx.send(StreamEvent::Done);
            return;
        }
        Err(e) => {
            let _ = tx.send(StreamEvent::Line(format!("failed to run {program}: {e}")));
            let _ = tx.send(StreamEvent::Done);
            return;
        }
    };

    // stdout/stderr are always Some: Stdio::piped() was just requested for
    // both above.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let tx_out = tx.clone();
    let stdout_thread = thread::spawn(move || stream_lines(stdout, &tx_out));
    let tx_err = tx.clone();
    let stderr_thread = thread::spawn(move || stream_lines(stderr, &tx_err));

    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let _ = child.wait();
    let _ = tx.send(StreamEvent::Done);
}

fn stream_lines(reader: impl Read, tx: &Sender<StreamEvent>) {
    for line in BufReader::new(reader).lines() {
        match line {
            Ok(line) => {
                if tx.send(StreamEvent::Line(line)).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn wait_until<F: FnMut() -> bool>(mut condition: F) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "condition did not become true in time"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn sample_action(name: &str) -> CustomAction {
        CustomAction {
            name: name.to_string(),
            command: "cargo".to_string(),
            args: vec!["test".to_string()],
        }
    }

    #[test]
    fn load_from_a_fresh_project_returns_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), Vec::new());
    }

    #[test]
    fn save_then_load_round_trips_the_actions() {
        let dir = tempfile::tempdir().unwrap();
        let actions = vec![sample_action("Run tests"), sample_action("Run clippy")];
        save(dir.path(), &actions);
        assert_eq!(load(dir.path()), actions);
    }

    #[test]
    fn load_truncates_a_maliciously_oversized_file_to_max_custom_actions() {
        let dir = tempfile::tempdir().unwrap();
        let ide_dir = dir.path().join(".ide");
        std::fs::create_dir_all(&ide_dir).unwrap();
        let oversized = CustomActionsFile {
            actions: (0..MAX_CUSTOM_ACTIONS + 50)
                .map(|i| sample_action(&format!("action-{i}")))
                .collect(),
        };
        std::fs::write(
            ide_dir.join("custom_actions.json"),
            serde_json::to_vec(&oversized).unwrap(),
        )
        .unwrap();

        let loaded = load(dir.path());
        assert_eq!(loaded.len(), MAX_CUSTOM_ACTIONS);
        assert_eq!(loaded[0].name, "action-0");
    }

    #[test]
    fn load_on_malformed_json_returns_an_empty_list_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let ide_dir = dir.path().join(".ide");
        std::fs::create_dir_all(&ide_dir).unwrap();
        std::fs::write(ide_dir.join("custom_actions.json"), "{ not json").unwrap();
        assert_eq!(load(dir.path()), Vec::new());
    }

    #[test]
    fn run_selected_on_an_empty_list_is_a_noop() {
        let mut panel = CustomActionsPanel::default();
        panel.run_selected(Path::new("."));
        assert!(panel.running.is_none());
    }

    #[test]
    fn run_selected_with_selected_out_of_range_is_a_noop() {
        let mut panel = CustomActionsPanel {
            actions: vec![sample_action("a")],
            selected: 5,
            ..Default::default()
        };
        panel.run_selected(Path::new("."));
        assert!(panel.running.is_none());
    }

    #[test]
    fn run_selected_while_already_running_is_a_noop() {
        let mut panel = CustomActionsPanel {
            actions: vec![sample_action("a"), sample_action("b")],
            selected: 1,
            running: Some(sample_action("a")),
            output: vec!["existing".to_string()],
            rx: None,
        };
        panel.run_selected(Path::new("."));
        assert_eq!(panel.running, Some(sample_action("a")));
        assert_eq!(panel.output, vec!["existing".to_string()]);
    }

    #[test]
    fn poll_with_nothing_running_returns_false() {
        let mut panel = CustomActionsPanel::default();
        assert!(!panel.poll());
    }

    #[test]
    fn spawn_streaming_reports_missing_binary() {
        let rx = spawn_streaming("definitely-not-a-real-binary-xyz", &[], Path::new("."));
        let mut lines = Vec::new();
        while let StreamEvent::Line(line) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            lines.push(line);
        }
        assert_eq!(
            lines,
            vec!["definitely-not-a-real-binary-xyz not found on PATH".to_string()]
        );
    }

    #[test]
    fn run_and_poll_streams_stdout_and_stderr_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = CustomActionsPanel {
            actions: vec![CustomAction {
                name: "Stream".to_string(),
                command: fixture("streaming_output.sh"),
                args: Vec::new(),
            }],
            selected: 0,
            ..Default::default()
        };
        panel.run_selected(dir.path());
        assert!(panel.running.is_some());

        wait_until(|| {
            panel.poll();
            panel.running.is_none()
        });

        assert!(panel.output.contains(&"line1".to_string()));
        assert!(panel.output.contains(&"err1".to_string()));
        assert!(panel.output.contains(&"line2".to_string()));
    }

    #[test]
    fn args_reach_the_child_as_argv_not_shell_interpreted() {
        let dir = tempfile::tempdir().unwrap();
        let payload = "build; $(whoami) `id` && rm -rf /";
        let rx = spawn_streaming(&fixture("argv_echo.sh"), &[payload.to_string()], dir.path());
        let mut lines = Vec::new();
        while let StreamEvent::Line(line) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            lines.push(line);
        }
        assert_eq!(lines, vec![format!("argv: {payload}")]);
    }

    #[test]
    fn deleting_the_running_actions_definition_does_not_affect_the_in_flight_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = CustomActionsPanel {
            actions: vec![CustomAction {
                name: "Stream".to_string(),
                command: fixture("streaming_output.sh"),
                args: Vec::new(),
            }],
            selected: 0,
            ..Default::default()
        };
        panel.run_selected(dir.path());
        let running_before = panel.running.clone();
        assert!(running_before.is_some());

        // Simulates the Manage popup deleting the only defined action
        // while it's running -- `running` is a clone, so this must not
        // disturb it.
        panel.actions.clear();

        wait_until(|| {
            panel.poll();
            panel.running.is_none()
        });

        assert!(panel.output.contains(&"line1".to_string()));
        assert!(panel.actions.is_empty());
    }
}
