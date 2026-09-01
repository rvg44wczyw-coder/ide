//! Shared subprocess-spawning helpers for `docker_panel.rs`/`k8s_panel.rs`
//! (`docs/features/tui-docker-and-kubernetes.md` §2.1) -- a generalization
//! of `cargo_panel.rs`'s `spawn_streaming`/`run_and_stream`/`stream_lines`
//! (which only ever passes one fixed subcommand string) to a real
//! `args: &[String]` argv, shared between two panels rather than
//! duplicated a third time. `cargo_panel.rs` itself is untouched -- it
//! already works and is already tested; this is new code for new
//! callers, not a refactor of working code.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

/// Hard backstop on raw bytes captured from a single stream (stdout or
/// stderr), independent of the panels' own `MAX_DOCKER_LIST_ITEMS`/
/// `MAX_K8S_LIST_ITEMS` (500 *items*, a display concern applied only
/// after the full response is already in memory). A Docker host or
/// Kubernetes cluster with a very large object count -- ordinary
/// accumulation on a long-lived shared host/cluster, or a lower-trust
/// co-tenant in a multi-tenant environment -- can otherwise make a single
/// refresh consume unbounded memory before that item cap ever runs
/// (`docs/security-findings/tui-docker-and-kubernetes-2026-08-31.md`,
/// finding 1). 4 MiB comfortably covers thousands of typical container/
/// pod listing lines -- far more than the 500-item display cap ever
/// needs -- so hitting it in practice means "an extreme amount of real
/// data", not "the command legitimately needed more room".
const MAX_CAPTURED_BYTES_PER_STREAM: u64 = 4 * 1024 * 1024;

pub(crate) enum StreamEvent {
    Line(String),
    Done,
}

/// Spawns `program` with `args` (an explicit argv, never a shell string)
/// and `current_dir` if given, off the calling thread. Streams
/// stdout+stderr line-by-line via the returned channel, `Done` once the
/// process exits. "Failed to spawn" / "not found on PATH" is reported as
/// one `Line` immediately followed by `Done` -- same convention
/// `cargo_panel.rs::run_and_stream` already established, so a missing
/// `docker`/`kubectl` binary is a normal, recoverable error state rather
/// than something the caller has to special-case.
pub(crate) fn spawn_streaming(
    program: &str,
    args: &[String],
    current_dir: Option<&Path>,
) -> Receiver<StreamEvent> {
    let (tx, rx) = mpsc::channel();
    let program = program.to_string();
    let args = args.to_vec();
    let current_dir = current_dir.map(|p| p.to_path_buf());
    thread::spawn(move || run_and_stream(&program, &args, current_dir.as_deref(), &tx));
    rx
}

/// Spawns `program`/`args` off the calling thread the same way
/// [`spawn_streaming`] does, but the returned channel yields **exactly
/// one** message once the process exits: its combined stdout+stderr
/// lines, in order, plus whether it exited successfully. For callers that
/// want a single collected result rather than incremental lines (list
/// refresh, lifecycle/destructive actions, describe).
pub(crate) fn spawn_to_completion(
    program: &str,
    args: &[String],
    current_dir: Option<&Path>,
) -> Receiver<(Vec<String>, bool)> {
    let (tx, rx) = mpsc::channel();
    let program = program.to_string();
    let args = args.to_vec();
    let current_dir = current_dir.map(|p| p.to_path_buf());
    thread::spawn(move || {
        let (line_tx, line_rx) = mpsc::channel();
        let success = run_and_stream(&program, &args, current_dir.as_deref(), &line_tx);
        let mut lines = Vec::new();
        while let Ok(event) = line_rx.recv() {
            match event {
                StreamEvent::Line(line) => lines.push(line),
                StreamEvent::Done => break,
            }
        }
        let _ = tx.send((lines, success));
    });
    rx
}

/// `args` reaches the child as a real argv, one element per entry, via
/// `Command::args` -- never concatenated into a shell string, so shell
/// metacharacters in any element (a container/pod name, typed
/// confirmation text, a replica count) can't be interpreted regardless of
/// where that string originated. Returns whether the process both spawned
/// and exited successfully -- `spawn_to_completion`'s only source of
/// truth for its `success` bool, so a command that spawns fine but exits
/// nonzero (bad container id, `kubectl` RBAC denial, wrong context, ...)
/// is correctly reported as a failure rather than silently treated as
/// success just because nothing crashed on the way there.
fn run_and_stream(
    program: &str,
    args: &[String],
    current_dir: Option<&Path>,
    tx: &Sender<StreamEvent>,
) -> bool {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = tx.send(StreamEvent::Line(format!("{program} not found on PATH")));
            let _ = tx.send(StreamEvent::Done);
            return false;
        }
        Err(e) => {
            let _ = tx.send(StreamEvent::Line(format!("failed to run {program}: {e}")));
            let _ = tx.send(StreamEvent::Done);
            return false;
        }
    };

    // stdout/stderr are always Some: Stdio::piped() was just requested
    // for both above.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Shared so either reader thread can kill the child the instant it
    // hits its own cap -- necessary, not just tidy: once a thread stops
    // pulling bytes from its pipe, the child's next write to that pipe
    // blocks forever once the OS pipe buffer fills, so `child.wait()`
    // below would otherwise hang rather than ever observing an exit.
    let child = Arc::new(Mutex::new(child));
    // Set by either reader thread on a cap trip -- overrides the exit
    // status `run_and_stream` returns as `success`, since a kill this
    // crate itself issued to stop reading is a truncation of otherwise-
    // valid data, not a real command failure (`apply_refresh_result`
    // must still parse and display what was captured, not treat it as an
    // error just because this cap fired for a legitimately large
    // listing).
    let capped = Arc::new(AtomicBool::new(false));

    let tx_out = tx.clone();
    let child_out = Arc::clone(&child);
    let capped_out = Arc::clone(&capped);
    let stdout_thread =
        thread::spawn(move || stream_lines(stdout, &tx_out, &child_out, &capped_out));
    let tx_err = tx.clone();
    let child_err = Arc::clone(&child);
    let capped_err = Arc::clone(&capped);
    let stderr_thread =
        thread::spawn(move || stream_lines(stderr, &tx_err, &child_err, &capped_err));

    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let status = child.lock().unwrap().wait();
    let _ = tx.send(StreamEvent::Done);
    if capped.load(Ordering::SeqCst) {
        true
    } else {
        status.map(|s| s.success()).unwrap_or(false)
    }
}

fn stream_lines(
    reader: impl Read,
    tx: &Sender<StreamEvent>,
    child: &Arc<Mutex<Child>>,
    capped: &Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(reader).take(MAX_CAPTURED_BYTES_PER_STREAM);
    loop {
        let mut buf = Vec::new();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return,
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                if tx
                    .send(StreamEvent::Line(
                        String::from_utf8_lossy(&buf).into_owned(),
                    ))
                    .is_err()
                {
                    return;
                }
            }
            Err(_) => return,
        }
        if reader.limit() == 0 {
            // The cap and the stream's real end can coincide by chance (the
            // command's total output happens to be exactly
            // MAX_CAPTURED_BYTES_PER_STREAM). Peek at the underlying reader
            // before declaring truncation: a real, already-finished stream
            // reports Ok(&[]) here (an ordinary EOF check, not a hang risk --
            // the writer either still has buffered bytes waiting, in which
            // case this returns immediately, or has already exited/closed
            // its end, which also surfaces as immediate EOF; a writer that
            // pauses indefinitely without closing this stream was already
            // going to block a plain read_until at any byte count, cap or
            // not, so this adds no new hang mode).
            let mut inner = reader.into_inner();
            let genuinely_truncated =
                !matches!(inner.fill_buf(), Ok(remaining) if remaining.is_empty());
            if !genuinely_truncated {
                return;
            }
            capped.store(true, Ordering::SeqCst);
            let _ = tx.send(StreamEvent::Line(
                "... output truncated (exceeded this panel's internal capture limit) ..."
                    .to_string(),
            ));
            let _ = child.lock().unwrap().kill();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn spawn_streaming_reports_missing_binary() {
        let rx = spawn_streaming("definitely-not-a-real-docker-xyz", &[], None);
        let mut lines = Vec::new();
        while let StreamEvent::Line(line) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            lines.push(line);
        }
        assert_eq!(
            lines,
            vec!["definitely-not-a-real-docker-xyz not found on PATH".to_string()]
        );
    }

    #[test]
    fn spawn_streaming_streams_stdout_and_stderr_lines() {
        let dir = tempfile::tempdir().unwrap();
        let rx = spawn_streaming(&fixture("streaming_output.sh"), &[], Some(dir.path()));
        let mut lines = Vec::new();
        while let StreamEvent::Line(line) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            lines.push(line);
        }
        assert!(lines.contains(&"line1".to_string()));
        assert!(lines.contains(&"err1".to_string()));
        assert!(lines.contains(&"line2".to_string()));
    }

    #[test]
    fn args_reach_the_child_as_argv_elements_not_shell_interpreted() {
        let dir = tempfile::tempdir().unwrap();
        let payload = "arg1; $(whoami) `id` && rm -rf /".to_string();
        let rx = spawn_streaming(
            &fixture("argv_echo.sh"),
            std::slice::from_ref(&payload),
            Some(dir.path()),
        );
        let mut lines = Vec::new();
        while let StreamEvent::Line(line) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            lines.push(line);
        }
        assert_eq!(lines, vec![format!("argv: {payload}")]);
    }

    #[test]
    fn multiple_args_each_reach_the_child_as_a_separate_argv_element() {
        let dir = tempfile::tempdir().unwrap();
        let rx = spawn_streaming(
            &fixture("argv_count.sh"),
            &["one".to_string(), "two".to_string(), "three".to_string()],
            Some(dir.path()),
        );
        let mut lines = Vec::new();
        while let StreamEvent::Line(line) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            lines.push(line);
        }
        assert_eq!(lines, vec!["argc: 3".to_string()]);
    }

    #[test]
    fn spawn_to_completion_collects_every_line_and_reports_success() {
        let dir = tempfile::tempdir().unwrap();
        let rx = spawn_to_completion(&fixture("streaming_output.sh"), &[], Some(dir.path()));
        let (lines, success) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(success);
        assert!(lines.contains(&"line1".to_string()));
        assert!(lines.contains(&"line2".to_string()));
    }

    #[test]
    fn spawn_to_completion_reports_failure_for_a_missing_binary() {
        let rx = spawn_to_completion("definitely-not-a-real-kubectl-xyz", &[], None);
        let (lines, success) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!success);
        assert_eq!(
            lines,
            vec!["definitely-not-a-real-kubectl-xyz not found on PATH".to_string()]
        );
    }

    #[test]
    fn spawn_to_completion_reports_failure_for_a_nonzero_exit_status() {
        // Regression guard: a command that spawns fine but exits nonzero
        // (a real `docker`/`kubectl` failure -- bad id, RBAC denial, wrong
        // context) must be reported as `success: false`, not just the two
        // spawn-failure cases above -- `run_and_stream`'s return value is
        // the real exit status, not a string-sniff over stdout/stderr.
        let rx = spawn_to_completion("false", &[], None);
        let (_, success) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!success);
    }

    #[test]
    fn spawn_to_completion_yields_exactly_one_message() {
        let dir = tempfile::tempdir().unwrap();
        let rx = spawn_to_completion(&fixture("streaming_output.sh"), &[], Some(dir.path()));
        let _ = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // A second recv on a completed, single-shot channel must report
        // the sender disconnected, not yield a second result.
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn current_dir_none_does_not_set_a_working_directory_override() {
        // Regression guard: `run_and_stream` must tolerate `current_dir:
        // None` without panicking (used by context/namespace-less
        // `kubectl` calls, which have no natural project-relative cwd).
        let rx = spawn_streaming(&fixture("streaming_output.sh"), &[], None);
        let saw_done;
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                StreamEvent::Line(_) => {}
                StreamEvent::Done => {
                    saw_done = true;
                    break;
                }
            }
        }
        assert!(saw_done);
    }

    #[test]
    fn many_lines_past_the_cap_are_truncated_and_reported_as_success() {
        // Regression guard for docs/security-findings/
        // tui-docker-and-kubernetes-2026-08-31.md finding 1: far more
        // lines than `MAX_CAPTURED_BYTES_PER_STREAM` must not grow memory
        // unboundedly, must still complete (not hang on a child blocked
        // writing to an undrained pipe), and must be reported as
        // `success: true` -- a cap trip is a truncation of otherwise-valid
        // data, not a command failure, so `apply_refresh_result` still
        // parses and displays what was captured.
        let dir = tempfile::tempdir().unwrap();
        let line = "container-name-with-a-realistic-length abcdef123456 Up 2 hours";
        let total_bytes = (MAX_CAPTURED_BYTES_PER_STREAM * 3) as usize;
        let script = format!("yes '{line}' | head -c {total_bytes}");
        let rx = spawn_to_completion("sh", &["-c".to_string(), script], Some(dir.path()));
        let (lines, success) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(success);
        // Bounded well below what `total_bytes` would have produced
        // (millions of lines) -- the exact count depends on where a line
        // boundary lands relative to the cap, so assert an order-of-
        // magnitude bound rather than an exact count.
        assert!(
            lines.len() < 100_000,
            "expected a bounded line count, got {}",
            lines.len()
        );
        assert!(lines.iter().any(|l| l.contains("truncated")));
    }

    #[test]
    fn a_single_line_past_the_cap_is_truncated_and_reported_as_success() {
        // Same regression guard, for the other flavor of the finding: one
        // pathologically long line with no newline (a process that never
        // terminates a line) must not grow memory unboundedly either.
        let dir = tempfile::tempdir().unwrap();
        let total_bytes = (MAX_CAPTURED_BYTES_PER_STREAM * 3) as usize;
        let script = format!("yes x | head -c {total_bytes} | tr -d '\\n'");
        let rx = spawn_to_completion("sh", &["-c".to_string(), script], Some(dir.path()));
        let (lines, success) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(success);
        // The one pathological line plus the truncation marker.
        assert_eq!(lines.len(), 2);
        assert!(lines[0].len() <= MAX_CAPTURED_BYTES_PER_STREAM as usize);
        assert!(lines[1].contains("truncated"));
    }

    #[test]
    fn output_landing_exactly_on_the_cap_boundary_is_not_reported_as_truncated() {
        // Regression guard for the rev pass's controversial-findings note:
        // a stream whose real output happens to end exactly at
        // MAX_CAPTURED_BYTES_PER_STREAM must not spuriously report a
        // truncation marker (or the misleading redundant kill of an
        // already-exited child that came with it) just because the byte
        // count coincided with the cap.
        let dir = tempfile::tempdir().unwrap();
        let total_bytes = MAX_CAPTURED_BYTES_PER_STREAM as usize;
        let script = format!("head -c {total_bytes} /dev/zero | tr '\\0' 'x'");
        let rx = spawn_to_completion("sh", &["-c".to_string(), script], Some(dir.path()));
        let (lines, success) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(success);
        assert!(lines.iter().all(|l| !l.contains("truncated")));
        assert_eq!(lines.iter().map(|l| l.len()).sum::<usize>(), total_bytes);
    }
}
