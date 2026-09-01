//! Shells out to the local `claude` CLI for the integrated assistant
//! panel. Never blocks the UI thread — each prompt runs on a background
//! thread and reports back over a channel polled once per frame. Never
//! reads, stores, or logs any Claude credential itself; that's the local
//! `claude` CLI's own responsibility (see `docs/features/editor-shell.md`
//! §2.2).

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeMessage {
    User(String),
    Assistant(String),
    Error(String),
}

/// How to actually run a prompt. Production code always uses
/// [`run_claude_cli`]; tests substitute a fake runner so they never spawn
/// the real `claude` binary (which won't be installed in CI) and stay
/// fast and deterministic.
type Runner = fn(&str) -> ClaudeOutcomeResult;
type ClaudeOutcomeResult = Result<String, String>;

pub struct ClaudePanel {
    pub input: String,
    pub history: Vec<ClaudeMessage>,
    queue: Vec<String>,
    rx: Option<Receiver<ClaudeOutcomeResult>>,
    runner: Runner,
}

impl Default for ClaudePanel {
    fn default() -> Self {
        Self::with_runner(run_claude_cli)
    }
}

impl ClaudePanel {
    fn with_runner(runner: Runner) -> Self {
        Self {
            input: String::new(),
            history: Vec::new(),
            queue: Vec::new(),
            rx: None,
            runner,
        }
    }

    /// Appends `prompt` to `history` as `ClaudeMessage::User`, then either
    /// starts it immediately on a background thread or, if a prompt is
    /// already in flight, queues it to run once the current one finishes
    /// (v1 processes one at a time — see doc §3). Non-blocking: returns
    /// immediately. No-op for a blank/whitespace-only prompt.
    pub fn submit(&mut self, prompt: String) {
        if prompt.trim().is_empty() {
            return;
        }
        self.history.push(ClaudeMessage::User(prompt.clone()));
        if self.is_in_flight() {
            self.queue.push(prompt);
        } else {
            self.start(prompt);
        }
    }

    fn start(&mut self, prompt: String) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let runner = self.runner;
        thread::spawn(move || {
            let _ = tx.send(runner(&prompt));
        });
    }

    pub fn is_in_flight(&self) -> bool {
        self.rx.is_some()
    }

    /// Call once per frame. Returns `true` if `history` changed (the
    /// caller should request a repaint).
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        match rx.try_recv() {
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                self.history.push(ClaudeMessage::Error(
                    "claude process ended unexpectedly".into(),
                ));
                self.start_next_queued();
                true
            }
            Ok(Ok(reply)) => {
                self.rx = None;
                self.history.push(ClaudeMessage::Assistant(reply));
                self.start_next_queued();
                true
            }
            Ok(Err(err)) => {
                self.rx = None;
                self.history.push(ClaudeMessage::Error(err));
                self.start_next_queued();
                true
            }
        }
    }

    fn start_next_queued(&mut self) {
        if !self.queue.is_empty() {
            let next = self.queue.remove(0);
            self.start(next);
        }
    }
}

fn run_claude_cli(prompt: &str) -> ClaudeOutcomeResult {
    run_command("claude", prompt)
}

/// Passes `prompt` on the child's stdin rather than as a CLI argument, so
/// it never appears in the process's argv (visible to any co-resident
/// local process via `ps`/`/proc/<pid>/cmdline` for the subprocess's
/// lifetime — see `docs/security-findings/editor-shell-ui-claude-panel-*.md`).
/// `-p` with no following argument reads the prompt from stdin.
///
/// The stdin write happens on its own thread, concurrently with
/// `wait_with_output()` draining stdout/stderr on the caller's thread: a
/// prompt/response pair large enough to fill the OS pipe buffer (~64KB)
/// before the child has consumed all of stdin would otherwise deadlock a
/// synchronous write against an unread stdout pipe.
fn run_command(program: &str, prompt: &str) -> ClaudeOutcomeResult {
    let mut child = match Command::new(program)
        .arg("-p")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("claude CLI not found on PATH".to_string());
        }
        Err(e) => return Err(format!("failed to run claude: {e}")),
    };

    // stdin is always Some: we just requested Stdio::piped() above.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let prompt_bytes = prompt.as_bytes().to_vec();
    let writer = thread::spawn(move || stdin.write_all(&prompt_bytes));

    let output = match child.wait_with_output() {
        Ok(out) => out,
        Err(e) => return Err(format!("failed to run claude: {e}")),
    };
    // A write failure (e.g. the child exited early and closed its end of
    // the pipe) doesn't need separate reporting: the exit status/stderr
    // checked below already reflects whatever went wrong.
    let _ = writer.join();

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(if stderr.trim().is_empty() {
            format!("claude exited with {}", output.status)
        } else {
            stderr.trim().to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn ok_echo(prompt: &str) -> ClaudeOutcomeResult {
        Ok(format!("echo: {prompt}"))
    }

    fn always_error(_prompt: &str) -> ClaudeOutcomeResult {
        Err("boom".to_string())
    }

    fn wait_until<F: FnMut() -> bool>(mut condition: F) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "condition did not become true in time"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn submit_appends_user_message_immediately() {
        let mut panel = ClaudePanel::with_runner(ok_echo);
        panel.submit("hi".into());
        assert_eq!(panel.history, vec![ClaudeMessage::User("hi".into())]);
        assert!(panel.is_in_flight());
    }

    #[test]
    fn submit_blank_prompt_is_noop() {
        let mut panel = ClaudePanel::with_runner(ok_echo);
        panel.submit("   ".into());
        assert!(panel.history.is_empty());
        assert!(!panel.is_in_flight());
    }

    #[test]
    fn poll_appends_assistant_reply_on_success() {
        let mut panel = ClaudePanel::with_runner(ok_echo);
        panel.submit("hi".into());
        wait_until(|| panel.poll());
        assert_eq!(
            panel.history,
            vec![
                ClaudeMessage::User("hi".into()),
                ClaudeMessage::Assistant("echo: hi".into()),
            ]
        );
        assert!(!panel.is_in_flight());
    }

    #[test]
    fn poll_appends_error_message_on_failure() {
        let mut panel = ClaudePanel::with_runner(always_error);
        panel.submit("hi".into());
        wait_until(|| panel.poll());
        assert_eq!(
            panel.history,
            vec![
                ClaudeMessage::User("hi".into()),
                ClaudeMessage::Error("boom".into()),
            ]
        );
    }

    #[test]
    fn poll_with_nothing_in_flight_returns_false() {
        let mut panel = ClaudePanel::with_runner(ok_echo);
        assert!(!panel.poll());
    }

    #[test]
    fn second_submit_while_in_flight_queues_and_input_stays_enabled() {
        let mut panel = ClaudePanel::with_runner(ok_echo);
        panel.submit("first".into());
        assert!(panel.is_in_flight());
        panel.submit("second".into());
        // both user messages appear immediately, even though only the
        // first has actually started running
        assert_eq!(
            panel.history,
            vec![
                ClaudeMessage::User("first".into()),
                ClaudeMessage::User("second".into()),
            ]
        );

        wait_until(|| panel.poll()); // first completes, second auto-starts
        assert!(panel.is_in_flight());
        wait_until(|| panel.poll()); // second completes

        assert_eq!(
            panel.history,
            vec![
                ClaudeMessage::User("first".into()),
                ClaudeMessage::User("second".into()),
                ClaudeMessage::Assistant("echo: first".into()),
                ClaudeMessage::Assistant("echo: second".into()),
            ]
        );
        assert!(!panel.is_in_flight());
    }

    #[test]
    fn run_command_reports_not_found_with_doc_specified_message() {
        // Exercises the real (non-fake) subprocess path end-to-end against
        // a binary name guaranteed not to exist, without depending on
        // whether the actual `claude` CLI happens to be installed in this
        // environment.
        let result = run_command("definitely-not-a-real-claude-binary-xyz", "hi");
        assert_eq!(result, Err("claude CLI not found on PATH".to_string()));
    }

    #[test]
    fn run_command_sends_prompt_on_stdin_not_argv_and_never_shell_interprets_it() {
        // A prompt containing shell metacharacters must reach the child
        // only via stdin, verbatim, never interpreted by a shell and never
        // visible as a second argv element — using a fixture script that
        // ignores its argv (including the "-p" flag `run_command` always
        // passes) and copies stdin to stdout, so we can observe exactly
        // what was written. If this were shell-interpreted, `$(whoami)`
        // would be substituted; it comes back byte-for-byte instead.
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stdin_echo.sh");
        let payload = "-n; $(whoami) `id` && rm -rf /";
        let result = run_command(fixture, payload);
        assert_eq!(result, Ok(payload.to_string()));
    }

    #[test]
    fn run_command_does_not_deadlock_on_large_prompt_and_output() {
        // Regression test: a synchronous write to the child's stdin before
        // anything reads its stdout would deadlock once the payload
        // exceeds the OS pipe buffer (commonly 64KB) and the child echoes
        // it back before finishing reading. Runs on its own thread with a
        // bounded wait so a regression fails the test instead of hanging
        // the whole suite.
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stdin_echo.sh");
        let payload = "x".repeat(500_000);
        let payload_for_thread = payload.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(run_command(fixture, &payload_for_thread));
        });
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("run_command did not complete in time (possible pipe deadlock)");
        assert_eq!(result, Ok(payload));
    }
}
