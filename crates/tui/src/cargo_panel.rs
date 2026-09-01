//! Shells out to `cargo` for Build/Run/Test/Check/Clippy/Fmt, streaming
//! stdout+stderr line by line as the process runs (`docs/features/
//! tui-cargo-panel.md`, `T10`). A near-verbatim port of `ide-ui`'s
//! `cargo_panel.rs` -- same background-thread + `mpsc` poll-once-per-frame
//! shape -- duplicated rather than shared, since `ide-tui` has no
//! dependency on `ide-ui`.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CargoCommand {
    Build,
    Run,
    Test,
    Check,
    Clippy,
    Fmt,
}

impl CargoCommand {
    pub(crate) fn subcommand(self) -> &'static str {
        match self {
            CargoCommand::Build => "build",
            CargoCommand::Run => "run",
            CargoCommand::Test => "test",
            CargoCommand::Check => "check",
            CargoCommand::Clippy => "clippy",
            CargoCommand::Fmt => "fmt",
        }
    }
}

enum StreamEvent {
    Line(String),
    Done,
}

#[derive(Default)]
pub(crate) struct CargoPanel {
    pub(crate) output: Vec<String>,
    pub(crate) running: Option<CargoCommand>,
    rx: Option<Receiver<StreamEvent>>,
}

impl CargoPanel {
    /// Spawns `cargo <subcommand>` with `current_dir(project_root)` and no
    /// other arguments, off the main loop thread. No-op if a command is
    /// already in flight -- v1 runs at most one at a time.
    pub(crate) fn run(&mut self, project_root: &Path, command: CargoCommand) {
        if self.running.is_some() {
            return;
        }
        self.output.clear();
        self.running = Some(command);
        self.rx = Some(spawn_streaming("cargo", command.subcommand(), project_root));
    }

    /// Call once per loop iteration, regardless of whether the panel is
    /// visible -- a running command keeps streaming into `output` in the
    /// background even while the panel is closed (`docs/features/
    /// tui-cargo-panel.md` §3/§4).
    pub(crate) fn poll(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(StreamEvent::Line(line)) => self.output.push(line),
                Ok(StreamEvent::Done) => {
                    self.running = None;
                    self.rx = None;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.running = None;
                    self.rx = None;
                    break;
                }
            }
        }
    }
}

fn spawn_streaming(program: &str, subcommand: &str, project_root: &Path) -> Receiver<StreamEvent> {
    let (tx, rx) = mpsc::channel();
    let program = program.to_string();
    let subcommand = subcommand.to_string();
    let project_root = project_root.to_path_buf();
    thread::spawn(move || run_and_stream(&program, &subcommand, &project_root, &tx));
    rx
}

/// `subcommand` is always passed as a single, explicit argv element via
/// `Command::arg` -- never formatted into a shell command string -- so
/// shell metacharacters in it (which never occur in practice, since `run`
/// only ever passes a fixed literal from `CargoCommand::subcommand`) can't
/// be interpreted. No shell is invoked at all.
fn run_and_stream(program: &str, subcommand: &str, project_root: &Path, tx: &Sender<StreamEvent>) {
    let mut child = match Command::new(program)
        .arg(subcommand)
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

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn run_while_already_running_is_a_noop() {
        let mut panel = CargoPanel {
            output: vec!["existing".to_string()],
            running: Some(CargoCommand::Build),
            rx: None,
        };
        panel.run(Path::new("."), CargoCommand::Test);
        assert_eq!(panel.running, Some(CargoCommand::Build));
        assert_eq!(panel.output, vec!["existing".to_string()]);
    }

    #[test]
    fn poll_with_nothing_running_is_a_noop() {
        let mut panel = CargoPanel::default();
        panel.poll();
        assert!(panel.output.is_empty());
        assert!(panel.running.is_none());
    }

    #[test]
    fn spawn_streaming_reports_missing_binary() {
        let rx = spawn_streaming("definitely-not-a-real-cargo-xyz", "build", Path::new("."));
        let mut lines = Vec::new();
        while let StreamEvent::Line(line) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            lines.push(line);
        }
        assert_eq!(
            lines,
            vec!["definitely-not-a-real-cargo-xyz not found on PATH".to_string()]
        );
    }

    #[test]
    fn run_and_poll_streams_stdout_and_stderr_lines() {
        let dir = tempfile::tempdir().unwrap();
        let rx = spawn_streaming(&fixture("streaming_output.sh"), "", dir.path());
        let mut panel = CargoPanel {
            output: Vec::new(),
            running: Some(CargoCommand::Build),
            rx: Some(rx),
        };

        wait_until(|| {
            panel.poll();
            panel.running.is_none()
        });

        assert!(panel.output.contains(&"line1".to_string()));
        assert!(panel.output.contains(&"err1".to_string()));
        assert!(panel.output.contains(&"line2".to_string()));
    }

    #[test]
    fn output_is_streamed_as_it_arrives_not_buffered_until_exit() {
        let dir = tempfile::tempdir().unwrap();
        let rx = spawn_streaming(&fixture("slow_output.sh"), "", dir.path());
        let mut panel = CargoPanel {
            output: Vec::new(),
            running: Some(CargoCommand::Build),
            rx: Some(rx),
        };

        wait_until(|| {
            panel.poll();
            panel.output.contains(&"before-sleep".to_string())
        });
        // The fixture sleeps for a while before its second line -- if this
        // process were buffering until exit, `running` would already be
        // `None` and `after-sleep` already present by the time the first
        // line shows up.
        assert!(panel.running.is_some());
        assert!(!panel.output.contains(&"after-sleep".to_string()));

        wait_until(|| {
            panel.poll();
            panel.running.is_none()
        });
        assert!(panel.output.contains(&"after-sleep".to_string()));
    }

    #[test]
    fn subcommand_reaches_child_as_a_single_argv_element_not_shell_interpreted() {
        let dir = tempfile::tempdir().unwrap();
        let payload = "build; $(whoami) `id` && rm -rf /";
        let rx = spawn_streaming(&fixture("argv_echo.sh"), payload, dir.path());
        let mut lines = Vec::new();
        while let StreamEvent::Line(line) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            lines.push(line);
        }
        assert_eq!(lines, vec![format!("argv: {payload}")]);
    }

    #[test]
    fn run_while_already_running_does_not_spawn_a_second_process() {
        // Regression guard for the "already running" no-op: if `run` ever
        // stopped checking `self.running` first, this would leave two
        // `rx`s alive and `poll` would silently drop one process's output.
        let mut panel = CargoPanel::default();
        let dir = tempfile::tempdir().unwrap();
        panel.run(dir.path(), CargoCommand::Build);
        assert!(panel.running.is_some());
        let rx_ptr_before = panel.rx.as_ref().map(|rx| rx as *const _);
        panel.run(dir.path(), CargoCommand::Test);
        let rx_ptr_after = panel.rx.as_ref().map(|rx| rx as *const _);
        assert_eq!(rx_ptr_before, rx_ptr_after);
        assert_eq!(panel.running, Some(CargoCommand::Build));
    }

    #[test]
    fn cargo_command_maps_to_the_expected_subcommand() {
        assert_eq!(CargoCommand::Build.subcommand(), "build");
        assert_eq!(CargoCommand::Run.subcommand(), "run");
        assert_eq!(CargoCommand::Test.subcommand(), "test");
        assert_eq!(CargoCommand::Check.subcommand(), "check");
        assert_eq!(CargoCommand::Clippy.subcommand(), "clippy");
        assert_eq!(CargoCommand::Fmt.subcommand(), "fmt");
    }
}
