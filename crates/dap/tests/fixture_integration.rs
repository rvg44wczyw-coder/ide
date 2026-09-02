//! Integration tests against a real subprocess (`fake-debug-adapter`,
//! `tests/fixtures/fake_debug_adapter.rs`), exercising `DapClient`'s
//! actual spawn + event-loop code path end to end rather than only its
//! internal parsing functions (covered by `src/client.rs`'s unit tests).

use ide_dap::{DapClient, DapEvent, DapRequest, SourceBreakpoint};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const FIXTURE: &str = env!("CARGO_BIN_EXE_fake-debug-adapter");
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// Polls `try_recv` until `matcher` returns `Some`, or panics after
/// `POLL_TIMEOUT` -- the event loop runs on its own background thread, so
/// a test can't just call `try_recv` once and expect the event to already
/// be there.
fn wait_for<T>(client: &mut DapClient, matcher: impl FnMut(&DapEvent) -> Option<T>) -> T {
    collect_until(client, matcher).0
}

/// Like [`wait_for`], but also returns every non-matching event seen
/// along the way -- for tests that need to prove something did *not*
/// happen by a certain point, not just that something did.
fn collect_until<T>(
    client: &mut DapClient,
    mut matcher: impl FnMut(&DapEvent) -> Option<T>,
) -> (T, Vec<DapEvent>) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    let mut seen = Vec::new();
    loop {
        if let Some(event) = client.try_recv() {
            match matcher(&event) {
                Some(value) => return (value, seen),
                None => seen.push(event),
            }
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for a matching DapEvent");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Selects the fixture's scenario via a `.fixture-scenario` marker file
/// dropped into the temp project root *before* `DapClient::start` spawns
/// the fixture there -- the scenario has to be knowable before the very
/// first response (`initialize`'s, whose capabilities some scenarios
/// vary), and `initialize`'s own arguments are entirely internal to
/// `DapClient::start` (not a public knob this crate exposes), so the
/// project root's `current_dir` (which the fixture reads back via
/// `std::env::current_dir`) is the one channel available that early.
/// Per-project rather than any shared/global state, so it stays safe
/// under `cargo test`'s parallel test threads -- the same problem
/// `crates/lsp/tests/fixtures/fake_lsp_server.rs` solves with a
/// rootUri-derived directory name, adapted here since DAP's `initialize`
/// carries no project-identifying argument to derive anything from.
fn project_with_scenario(scenario: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    if !scenario.is_empty() {
        std::fs::write(dir.path().join(".fixture-scenario"), scenario).unwrap();
    }
    let root = std::fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

fn start(scenario: &str) -> (tempfile::TempDir, DapClient) {
    let (dir, root) = project_with_scenario(scenario);
    let client = DapClient::start(FIXTURE, &[], &root).unwrap();
    (dir, client)
}

#[test]
fn full_handshake_reaches_capabilities_and_ready_for_breakpoints() {
    let (_dir, mut client) = start("");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::CapabilitiesReceived { .. }).then_some(())
    });
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::ReadyForBreakpoints).then_some(())
    });
}

#[test]
fn initialize_rejection_ends_the_session_with_adapter_exited() {
    let (_dir, mut client) = start("reject-init");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::AdapterExited { .. }).then_some(())
    });
}

#[test]
fn malformed_frame_after_init_ends_the_session_without_panicking() {
    let (_dir, mut client) = start("malformed-after-init");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::CapabilitiesReceived { .. }).then_some(())
    });
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::AdapterExited { .. }).then_some(())
    });
}

#[test]
fn set_breakpoints_reports_verified_and_unverified_lines() {
    let (dir, mut client) = start("");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::ReadyForBreakpoints).then_some(())
    });

    let path = dir.path().join("src/main.rs");
    client.send(DapRequest::SetBreakpoints {
        path: path.clone(),
        breakpoints: vec![
            SourceBreakpoint {
                line: 4,
                ..Default::default()
            },
            SourceBreakpoint {
                line: 5,
                ..Default::default()
            },
        ],
    });

    let breakpoints = wait_for(&mut client, |e| match e {
        DapEvent::BreakpointsConfirmed {
            path: p,
            breakpoints,
        } if *p == path => Some(breakpoints.clone()),
        _ => None,
    });
    assert_eq!(breakpoints.len(), 2);
    assert!(breakpoints.iter().find(|b| b.line == 4).unwrap().verified);
    assert!(!breakpoints.iter().find(|b| b.line == 5).unwrap().verified);
}

#[test]
fn configuration_done_is_sent_when_the_adapter_advertises_support() {
    let (_dir, mut client) = start("configuration-done");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::ReadyForBreakpoints).then_some(())
    });
    client.send(DapRequest::ConfigurationDone);
    let text = wait_for(&mut client, |e| match e {
        DapEvent::Output { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(text, "got-configurationDone");
}

/// The default scenario's `initialize` response omits
/// `supportsConfigurationDoneRequest`, so `ConfigurationDone` must never
/// actually reach the adapter. Sends it immediately followed by `Launch`
/// (whose own `stopped` follow-on event only the "stopped-on-launch"
/// scenario produces) and collects every event up to that `Stopped` --
/// since both requests travel over one ordered pipe and the fixture
/// handles them strictly in arrival order, the fixture's own
/// `"got-configurationDone"` marker event would already be sitting
/// ahead of `Stopped` in that collected list if `ConfigurationDone` had
/// been transmitted at all.
#[test]
fn configuration_done_is_never_sent_when_the_adapter_does_not_advertise_support() {
    let (_dir, mut client) = start("stopped-on-launch");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::ReadyForBreakpoints).then_some(())
    });
    client.send(DapRequest::ConfigurationDone);
    client.send(DapRequest::Launch {
        arguments: json!({}),
    });
    let (_, seen) = collect_until(&mut client, |e| {
        matches!(e, DapEvent::Stopped { .. }).then_some(())
    });
    assert!(!seen.iter().any(|e| matches!(
        e,
        DapEvent::Output { text, .. } if text == "got-configurationDone"
    )));
}

#[test]
fn stopped_event_and_disconnect_round_trip() {
    let (_dir, mut client) = start("stopped-on-launch");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::ReadyForBreakpoints).then_some(())
    });
    client.send(DapRequest::Launch {
        arguments: json!({}),
    });
    let thread_id = wait_for(&mut client, |e| match e {
        DapEvent::Stopped {
            thread_id: Some(id),
            ..
        } => Some(*id),
        _ => None,
    });
    assert_eq!(thread_id, 1);

    client.send(DapRequest::Disconnect);
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::Terminated).then_some(())
    });
}

#[test]
fn threads_and_stack_trace_round_trip() {
    let (_dir, mut client) = start("");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::ReadyForBreakpoints).then_some(())
    });

    client.send(DapRequest::Threads);
    let threads = wait_for(&mut client, |e| match e {
        DapEvent::Threads { threads } => Some(threads.clone()),
        _ => None,
    });
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, 1);

    client.send(DapRequest::StackTrace { thread_id: 1 });
    let frames = wait_for(&mut client, |e| match e {
        DapEvent::StackTrace {
            thread_id: 1,
            frames,
        } => Some(frames.clone()),
        _ => None,
    });
    assert_eq!(frames.len(), 2);
}

/// The security-critical path-validation rule (`docs/features/
/// debugger.md` §3.6): the fixture's `stackTrace` response always
/// includes one frame whose source path is inside the spawned project
/// root and one whose source path is a fixed path far outside any
/// possible project root -- the client must resolve the former to
/// `Some` and the latter to `None`, never trusting the adapter's path
/// verbatim.
#[test]
fn stack_frame_source_outside_project_root_comes_back_as_none() {
    let (_dir, mut client) = start("");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::ReadyForBreakpoints).then_some(())
    });
    client.send(DapRequest::StackTrace { thread_id: 1 });
    let frames = wait_for(&mut client, |e| match e {
        DapEvent::StackTrace { frames, .. } => Some(frames.clone()),
        _ => None,
    });
    let inside = frames.iter().find(|f| f.name == "inside_root").unwrap();
    let outside = frames.iter().find(|f| f.name == "outside_root").unwrap();
    assert!(inside.source.is_some());
    assert_eq!(outside.source, None);
}

#[test]
fn reverse_request_gets_the_generic_decline() {
    let (_dir, mut client) = start("reverse-request");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::ReadyForBreakpoints).then_some(())
    });
    let text = wait_for(&mut client, |e| match e {
        DapEvent::Output { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(text, "decline-confirmed");
}

#[test]
fn a_failed_request_surfaces_as_request_failed_with_command_and_message() {
    let (_dir, mut client) = start("request-fails");
    wait_for(&mut client, |e| {
        matches!(e, DapEvent::ReadyForBreakpoints).then_some(())
    });
    client.send(DapRequest::Threads);
    let (command, message) = wait_for(&mut client, |e| match e {
        DapEvent::RequestFailed {
            command, message, ..
        } => Some((command.clone(), message.clone())),
        _ => None,
    });
    assert_eq!(command, "threads");
    assert_eq!(message, "fixture: threads always fails");
}
