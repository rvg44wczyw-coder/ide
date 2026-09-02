use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender};

use crate::error::DapError;
use crate::path::validate_path;
use crate::protocol::{encode_request, encode_response, read_message, write_message, ReadOutcome};
use crate::types::{
    Capabilities, DapEvent, DapRequest, OutputCategory, StackFrame, StopReason, ThreadInfo,
    VerifiedBreakpoint,
};

const KILL_TIMEOUT: Duration = Duration::from_millis(500);
const EVENT_CHANNEL_CAPACITY: usize = 64;
/// Bounded for the same reason as `EVENT_CHANNEL_CAPACITY`, one hop
/// earlier: the dedicated reader task (`spawn_reader`) forwards each raw
/// wire framing outcome over this before it's parsed and dispatched.
const READER_CHANNEL_CAPACITY: usize = 8;
/// Caps any single adapter-reported array (`threads`, `stackFrames`,
/// `breakpoints`) before it's materialized into owned Rust values -- a
/// malicious or buggy adapter's array must not force unbounded
/// allocation, matching `ide-lsp`'s own bounded-array wrappers.
const MAX_ARRAY_ITEMS: usize = 10_000;

/// Which of our own outstanding requests a `request_seq` on an incoming
/// response corresponds to, and the extra context (unavailable in the
/// response body itself) needed to build the right `DapEvent`.
enum PendingRequestKind {
    Initialize,
    SetBreakpoints {
        path: PathBuf,
    },
    Threads,
    StackTrace {
        thread_id: i64,
    },
    /// `Launch`/`Attach`/`ConfigurationDone`/`Continue`/`Next`/`StepIn`/
    /// `StepOut`/`Pause`/`Disconnect`: no event is modeled for their
    /// success case in F5a (the caller learns what happened from the
    /// adapter's own follow-on events -- `stopped`, `continued`,
    /// `terminated`, ...); only their *failure* case needs surfacing,
    /// and the response itself already carries `command` for that.
    NoOp,
}

#[derive(serde::Deserialize)]
struct IncomingMessage {
    #[serde(rename = "type")]
    kind: String,
    seq: Option<i64>,
    command: Option<String>,
    request_seq: Option<i64>,
    success: Option<bool>,
    message: Option<String>,
    body: Option<Value>,
    event: Option<String>,
}

/// A live connection to one spawned debug adapter subprocess. See
/// `docs/features/debugger.md` §2.1 for the full public contract.
pub struct DapClient {
    request_tx: UnboundedSender<DapRequest>,
    event_rx: Receiver<DapEvent>,
}

impl DapClient {
    /// Spawns `command` (must be on `PATH` or an absolute path) and
    /// starts the connection. Returns as soon as the process spawns
    /// successfully -- the `initialize` handshake completes
    /// asynchronously; requests sent via `send` before
    /// `DapEvent::CapabilitiesReceived` fires are queued internally and
    /// flushed once it does.
    ///
    /// `project_root` bounds every path this session hands back in a
    /// `StackFrame` -- canonicalized once, here.
    ///
    /// A spawn failure with `io::ErrorKind::NotFound` is reported as
    /// `DapError::AdapterNotFound(command)`; every other spawn/I/O
    /// failure is reported as `DapError::Io` -- never a panic.
    pub fn start(
        command: &str,
        args: &[String],
        project_root: impl AsRef<Path>,
    ) -> Result<Self, DapError> {
        let project_root = fs::canonicalize(project_root.as_ref())?;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(DapError::Io)?;

        let command_owned = command.to_string();
        let args_owned = args.to_vec();
        let child = runtime.block_on(spawn_child(&command_owned, &args_owned, &project_root))?;

        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

        thread::spawn(move || {
            runtime.block_on(run_event_loop(child, project_root, request_rx, event_tx));
        });

        Ok(Self {
            request_tx,
            event_rx,
        })
    }

    /// Non-blocking; queues `request` for the background thread. See
    /// each `DapRequest` variant's own doc comment for the ordering
    /// contract this client does *not* enforce beyond the mandatory
    /// `initialize` handshake. Dropping is what happens if the
    /// background loop has already ended (the channel is closed) --
    /// there's nowhere else to report that.
    pub fn send(&self, request: DapRequest) {
        let _ = self.request_tx.send(request);
    }

    /// Non-blocking poll; call once per UI frame, in a loop, to drain
    /// everything available.
    pub fn try_recv(&mut self) -> Option<DapEvent> {
        self.event_rx.try_recv().ok()
    }
}

async fn spawn_child(
    command: &str,
    args: &[String],
    project_root: &Path,
) -> Result<Child, DapError> {
    Command::new(command)
        .args(args)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                DapError::AdapterNotFound(command.to_string())
            } else {
                DapError::Io(e)
            }
        })
}

fn initialize_arguments() -> Value {
    json!({
        "clientID": "ide",
        "clientName": "ide",
        "adapterID": "generic",
        "pathFormat": "path",
        "linesStartAt1": true,
        "columnsStartAt1": true,
        // Neither is implemented by this crate (`docs/features/
        // debugger.md` §3.3) -- advertised false up front so a
        // well-behaved adapter never tries either; a misbehaving one
        // that tries anyway still gets the generic decline.
        "supportsRunInTerminalRequest": false,
        "supportsStartDebuggingRequest": false,
    })
}

async fn run_event_loop(
    mut child: Child,
    project_root: PathBuf,
    mut request_rx: UnboundedReceiver<DapRequest>,
    event_tx: Sender<DapEvent>,
) {
    let Some(mut stdin) = child.stdin.take() else {
        let _ = event_tx
            .send(DapEvent::AdapterExited {
                message: "debug adapter subprocess had no stdin pipe".to_string(),
            })
            .await;
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = event_tx
            .send(DapEvent::AdapterExited {
                message: "debug adapter subprocess had no stdout pipe".to_string(),
            })
            .await;
        return;
    };

    let mut seq: i64 = 1;
    let mut inflight: HashMap<i64, PendingRequestKind> = HashMap::new();
    inflight.insert(seq, PendingRequestKind::Initialize);
    let init_bytes = encode_request(seq, "initialize", Some(initialize_arguments()));
    if write_message(&mut stdin, &init_bytes).await.is_err() {
        let _ = event_tx
            .send(DapEvent::AdapterExited {
                message: "failed to write initialize request to debug adapter".to_string(),
            })
            .await;
        let _ = kill_and_wait(&mut child).await;
        return;
    }

    let mut incoming = spawn_reader(BufReader::new(stdout));
    let mut ready = false;
    let mut configuration_done_supported = false;
    let mut queued: Vec<DapRequest> = Vec::new();

    loop {
        tokio::select! {
            maybe_msg = incoming.recv() => {
                let outcome = maybe_msg.unwrap_or(ReadOutcome::Eof);
                match outcome {
                    ReadOutcome::Message(bytes) => {
                        match handle_incoming(
                            &bytes,
                            &project_root,
                            &mut ready,
                            &mut configuration_done_supported,
                            &mut stdin,
                            &mut queued,
                            &mut seq,
                            &mut inflight,
                            &event_tx,
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(e) => {
                                let _ = event_tx.send(DapEvent::AdapterExited {
                                    message: format!("debug adapter protocol error: {e}"),
                                }).await;
                                break;
                            }
                        }
                    }
                    ReadOutcome::Eof => {
                        let _ = event_tx.send(DapEvent::AdapterExited {
                            message: "debug adapter exited".to_string(),
                        }).await;
                        break;
                    }
                    ReadOutcome::Error(e) => {
                        let _ = event_tx.send(DapEvent::AdapterExited {
                            message: format!("debug adapter protocol error: {e}"),
                        }).await;
                        break;
                    }
                }
            }
            maybe_req = request_rx.recv() => {
                match maybe_req {
                    Some(request) => {
                        if ready {
                            send_request(
                                &mut stdin,
                                request,
                                &mut seq,
                                &mut inflight,
                                configuration_done_supported,
                            )
                            .await;
                        } else {
                            queued.push(request);
                        }
                    }
                    // Client dropped with no explicit `Disconnect` sent
                    // first -- nothing left to negotiate over DAP, just
                    // tear the subprocess down below.
                    None => break,
                }
            }
        }
    }

    let _ = kill_and_wait(&mut child).await;
}

async fn kill_and_wait(child: &mut Child) -> io::Result<()> {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(KILL_TIMEOUT, child.wait()).await;
    Ok(())
}

/// Reads framed messages on a dedicated task, forwarding each outcome
/// over a channel. Cancel-safety of the caller's `select!` loop depends
/// on this: a raw multi-`.await` read future spliced directly into
/// `select!` would lose partially-read bytes (and desync the stream)
/// whenever the other branch won a given iteration.
fn spawn_reader(mut reader: BufReader<tokio::process::ChildStdout>) -> Receiver<ReadOutcome> {
    let (tx, rx) = mpsc::channel(READER_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        loop {
            let outcome = read_message(&mut reader).await;
            let is_terminal = matches!(outcome, ReadOutcome::Eof | ReadOutcome::Error(_));
            if tx.send(outcome).await.is_err() || is_terminal {
                break;
            }
        }
    });
    rx
}

#[allow(clippy::too_many_arguments)]
async fn handle_incoming(
    bytes: &[u8],
    project_root: &Path,
    ready: &mut bool,
    configuration_done_supported: &mut bool,
    stdin: &mut ChildStdin,
    queued: &mut Vec<DapRequest>,
    seq: &mut i64,
    inflight: &mut HashMap<i64, PendingRequestKind>,
    event_tx: &Sender<DapEvent>,
) -> Result<(), DapError> {
    let message: IncomingMessage = serde_json::from_slice(bytes)
        .map_err(|e| DapError::Protocol(format!("invalid DAP message: {e}")))?;

    match message.kind.as_str() {
        "response" => {
            handle_response(
                message,
                project_root,
                ready,
                configuration_done_supported,
                stdin,
                queued,
                seq,
                inflight,
                event_tx,
            )
            .await
        }
        "event" => {
            handle_event(message, event_tx).await;
            Ok(())
        }
        "request" => {
            handle_reverse_request(message, stdin, seq).await;
            Ok(())
        }
        // Anything else is well-formed JSON but not a recognized DAP
        // envelope kind -- ignored, not fatal (§3.7's best-effort
        // discipline: only a broken *frame*, not an odd message shape,
        // ends the session).
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_response(
    message: IncomingMessage,
    project_root: &Path,
    ready: &mut bool,
    configuration_done_supported: &mut bool,
    stdin: &mut ChildStdin,
    queued: &mut Vec<DapRequest>,
    seq: &mut i64,
    inflight: &mut HashMap<i64, PendingRequestKind>,
    event_tx: &Sender<DapEvent>,
) -> Result<(), DapError> {
    let Some(request_seq) = message.request_seq else {
        return Ok(());
    };
    // An id that doesn't match anything we're tracking is a stale or
    // unrelated response -- dropped, not an error.
    let Some(kind) = inflight.remove(&request_seq) else {
        return Ok(());
    };
    let success = message.success.unwrap_or(false);

    if matches!(kind, PendingRequestKind::Initialize) {
        if !success {
            return Err(DapError::Protocol(
                message
                    .message
                    .unwrap_or_else(|| "adapter rejected initialize".to_string()),
            ));
        }
        let capabilities = parse_capabilities(message.body.as_ref().unwrap_or(&Value::Null));
        *configuration_done_supported = capabilities.supports_configuration_done_request;
        let _ = event_tx
            .send(DapEvent::CapabilitiesReceived { capabilities })
            .await;
        *ready = true;
        for request in queued.drain(..) {
            send_request(stdin, request, seq, inflight, *configuration_done_supported).await;
        }
        return Ok(());
    }

    if !success {
        let _ = event_tx
            .send(DapEvent::RequestFailed {
                command: message.command.unwrap_or_default(),
                request_seq,
                message: message.message.unwrap_or_default(),
            })
            .await;
        return Ok(());
    }

    let body = message.body.unwrap_or(Value::Null);
    match kind {
        PendingRequestKind::Initialize => unreachable!("handled above"),
        PendingRequestKind::SetBreakpoints { path } => {
            let breakpoints = parse_verified_breakpoints(&body);
            let _ = event_tx
                .send(DapEvent::BreakpointsConfirmed { path, breakpoints })
                .await;
        }
        PendingRequestKind::Threads => {
            let threads = parse_threads(&body);
            let _ = event_tx.send(DapEvent::Threads { threads }).await;
        }
        PendingRequestKind::StackTrace { thread_id } => {
            let frames = parse_stack_frames(&body, project_root);
            let _ = event_tx
                .send(DapEvent::StackTrace { thread_id, frames })
                .await;
        }
        PendingRequestKind::NoOp => {}
    }
    Ok(())
}

async fn handle_event(message: IncomingMessage, event_tx: &Sender<DapEvent>) {
    let Some(event_name) = message.event.as_deref() else {
        return;
    };
    let body = message.body.unwrap_or(Value::Null);
    let event = match event_name {
        "initialized" => Some(DapEvent::ReadyForBreakpoints),
        "stopped" => Some(DapEvent::Stopped {
            reason: parse_stop_reason(body.get("reason").and_then(Value::as_str)),
            thread_id: body.get("threadId").and_then(Value::as_i64),
            description: body
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            all_threads_stopped: body
                .get("allThreadsStopped")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        // `threadId` has no honest default for a non-optional field --
        // a `continued` event missing it is dropped rather than
        // fabricating a thread id (§3.7's "never invent data").
        "continued" => body
            .get("threadId")
            .and_then(Value::as_i64)
            .map(|thread_id| DapEvent::Continued {
                thread_id,
                all_threads_continued: body
                    .get("allThreadsContinued")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
        "thread" => {
            let thread_id = body.get("threadId").and_then(Value::as_i64);
            match (body.get("reason").and_then(Value::as_str), thread_id) {
                (Some("started"), Some(thread_id)) => Some(DapEvent::ThreadStarted { thread_id }),
                (Some("exited"), Some(thread_id)) => Some(DapEvent::ThreadExited { thread_id }),
                _ => None,
            }
        }
        "output" => body
            .get("output")
            .and_then(Value::as_str)
            .map(|text| DapEvent::Output {
                category: parse_output_category(body.get("category").and_then(Value::as_str)),
                text: text.to_string(),
            }),
        "exited" => Some(DapEvent::Exited {
            exit_code: body.get("exitCode").and_then(Value::as_i64).unwrap_or(0),
        }),
        "terminated" => Some(DapEvent::Terminated),
        // `breakpoint`/`module`/`process`/`capabilities`/progress events
        // and anything else DAP or a specific adapter defines: not
        // modeled in F5a's scope, ignored rather than erroring.
        _ => None,
    };
    if let Some(event) = event {
        let _ = event_tx.send(event).await;
    }
}

/// Generic decline for any adapter-initiated request
/// (`docs/features/debugger.md` §3.3) -- this crate implements none of
/// them, so every one gets the same `{success: false}` regardless of
/// `command`, never silence and never a panic on an unrecognized name.
async fn handle_reverse_request(message: IncomingMessage, stdin: &mut ChildStdin, seq: &mut i64) {
    let Some(incoming_seq) = message.seq else {
        return;
    };
    let command = message.command.unwrap_or_default();
    *seq += 1;
    let response = encode_response(*seq, incoming_seq, &command, false, Some("not supported"));
    let _ = write_message(stdin, &response).await;
}

async fn send_request(
    stdin: &mut ChildStdin,
    request: DapRequest,
    seq: &mut i64,
    inflight: &mut HashMap<i64, PendingRequestKind>,
    configuration_done_supported: bool,
) {
    if matches!(request, DapRequest::ConfigurationDone) && !configuration_done_supported {
        return;
    }

    *seq += 1;
    let this_seq = *seq;
    let (command, arguments, kind) = match request {
        DapRequest::Launch { arguments } => ("launch", Some(arguments), PendingRequestKind::NoOp),
        DapRequest::Attach { arguments } => ("attach", Some(arguments), PendingRequestKind::NoOp),
        DapRequest::SetBreakpoints { path, breakpoints } => {
            let args = json!({
                "source": { "path": path.to_string_lossy() },
                "breakpoints": breakpoints.iter().map(encode_source_breakpoint).collect::<Vec<_>>(),
            });
            (
                "setBreakpoints",
                Some(args),
                PendingRequestKind::SetBreakpoints { path },
            )
        }
        DapRequest::ConfigurationDone => ("configurationDone", None, PendingRequestKind::NoOp),
        DapRequest::Continue { thread_id } => (
            "continue",
            Some(json!({ "threadId": thread_id })),
            PendingRequestKind::NoOp,
        ),
        DapRequest::Next { thread_id } => (
            "next",
            Some(json!({ "threadId": thread_id })),
            PendingRequestKind::NoOp,
        ),
        DapRequest::StepIn { thread_id } => (
            "stepIn",
            Some(json!({ "threadId": thread_id })),
            PendingRequestKind::NoOp,
        ),
        DapRequest::StepOut { thread_id } => (
            "stepOut",
            Some(json!({ "threadId": thread_id })),
            PendingRequestKind::NoOp,
        ),
        DapRequest::Pause { thread_id } => (
            "pause",
            Some(json!({ "threadId": thread_id })),
            PendingRequestKind::NoOp,
        ),
        DapRequest::Threads => ("threads", None, PendingRequestKind::Threads),
        DapRequest::StackTrace { thread_id } => (
            "stackTrace",
            Some(json!({ "threadId": thread_id })),
            PendingRequestKind::StackTrace { thread_id },
        ),
        // Every adapter must support `disconnect` per spec, unlike the
        // optional `terminate` -- this is what "Stop" always sends
        // (`docs/features/debugger.md` §3.5).
        DapRequest::Disconnect => (
            "disconnect",
            Some(json!({ "terminateDebuggee": true })),
            PendingRequestKind::NoOp,
        ),
    };

    inflight.insert(this_seq, kind);
    let bytes = encode_request(this_seq, command, arguments);
    let _ = write_message(stdin, &bytes).await;
}

fn encode_source_breakpoint(b: &crate::types::SourceBreakpoint) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("line".to_string(), json!(b.line));
    if let Some(condition) = &b.condition {
        obj.insert("condition".to_string(), json!(condition));
    }
    if let Some(hit_condition) = &b.hit_condition {
        obj.insert("hitCondition".to_string(), json!(hit_condition));
    }
    if let Some(log_message) = &b.log_message {
        obj.insert("logMessage".to_string(), json!(log_message));
    }
    Value::Object(obj)
}

fn parse_capabilities(body: &Value) -> Capabilities {
    let flag = |key: &str| body.get(key).and_then(Value::as_bool).unwrap_or(false);
    Capabilities {
        supports_configuration_done_request: flag("supportsConfigurationDoneRequest"),
        supports_conditional_breakpoints: flag("supportsConditionalBreakpoints"),
        supports_hit_conditional_breakpoints: flag("supportsHitConditionalBreakpoints"),
        supports_log_points: flag("supportsLogPoints"),
        supports_terminate_request: flag("supportsTerminateRequest"),
        supports_restart_request: flag("supportsRestartRequest"),
        supports_step_back: flag("supportsStepBack"),
    }
}

fn parse_threads(body: &Value) -> Vec<ThreadInfo> {
    body.get("threads")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_ARRAY_ITEMS)
        .filter_map(|t| {
            let id = t.get("id")?.as_i64()?;
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(ThreadInfo { id, name })
        })
        .collect()
}

fn parse_verified_breakpoints(body: &Value) -> Vec<VerifiedBreakpoint> {
    body.get("breakpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_ARRAY_ITEMS)
        .filter_map(|b| {
            let line = b.get("line").and_then(Value::as_u64)? as u32;
            let verified = b.get("verified").and_then(Value::as_bool).unwrap_or(false);
            let message = b.get("message").and_then(Value::as_str).map(str::to_string);
            Some(VerifiedBreakpoint {
                line,
                verified,
                message,
            })
        })
        .collect()
}

fn parse_stack_frames(body: &Value, project_root: &Path) -> Vec<StackFrame> {
    body.get("stackFrames")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_ARRAY_ITEMS)
        .filter_map(|f| {
            let id = f.get("id")?.as_i64()?;
            let name = f
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let line = f.get("line").and_then(Value::as_u64).unwrap_or(0) as u32;
            let column = f.get("column").and_then(Value::as_u64).unwrap_or(0) as u32;
            let source = f
                .get("source")
                .and_then(|s| s.get("path"))
                .and_then(Value::as_str)
                .map(Path::new)
                .and_then(|p| validate_path(project_root, p));
            Some(StackFrame {
                id,
                name,
                source,
                line,
                column,
            })
        })
        .collect()
}

fn parse_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("step") => StopReason::Step,
        Some("breakpoint") => StopReason::Breakpoint,
        Some("exception") => StopReason::Exception,
        Some("pause") => StopReason::Pause,
        Some("entry") => StopReason::Entry,
        Some("goto") => StopReason::Goto,
        Some("function breakpoint") => StopReason::FunctionBreakpoint,
        Some("data breakpoint") => StopReason::DataBreakpoint,
        Some("instruction breakpoint") => StopReason::InstructionBreakpoint,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::Other(String::new()),
    }
}

fn parse_output_category(category: Option<&str>) -> OutputCategory {
    match category {
        None | Some("console") => OutputCategory::Console,
        Some("stdout") => OutputCategory::Stdout,
        Some("stderr") => OutputCategory::Stderr,
        Some("telemetry") => OutputCategory::Telemetry,
        Some(other) => OutputCategory::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_with_missing_binary_reports_adapter_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = DapClient::start(
            "definitely-not-a-real-debug-adapter-binary",
            &[],
            dir.path(),
        );
        match result {
            Err(DapError::AdapterNotFound(cmd)) => {
                assert_eq!(cmd, "definitely-not-a-real-debug-adapter-binary");
            }
            Ok(_) => panic!("expected AdapterNotFound, got Ok"),
            Err(other) => panic!("expected AdapterNotFound, got {other}"),
        }
    }

    #[test]
    fn start_rejects_a_nonexistent_project_root() {
        let result = DapClient::start("echo", &[], "/definitely/does/not/exist/anywhere");
        assert!(matches!(result, Err(DapError::Io(_))));
    }

    #[test]
    fn parse_capabilities_defaults_every_flag_to_false_when_absent() {
        let caps = parse_capabilities(&Value::Null);
        assert_eq!(caps, Capabilities::default());
    }

    #[test]
    fn parse_capabilities_reads_every_flag_present() {
        let body = json!({
            "supportsConfigurationDoneRequest": true,
            "supportsConditionalBreakpoints": true,
            "supportsHitConditionalBreakpoints": true,
            "supportsLogPoints": true,
            "supportsTerminateRequest": true,
            "supportsRestartRequest": true,
            "supportsStepBack": true,
        });
        let caps = parse_capabilities(&body);
        assert_eq!(
            caps,
            Capabilities {
                supports_configuration_done_request: true,
                supports_conditional_breakpoints: true,
                supports_hit_conditional_breakpoints: true,
                supports_log_points: true,
                supports_terminate_request: true,
                supports_restart_request: true,
                supports_step_back: true,
            }
        );
    }

    #[test]
    fn parse_threads_skips_entries_missing_an_id() {
        let body = json!({ "threads": [{"name": "no id"}, {"id": 2, "name": "ok"}] });
        let threads = parse_threads(&body);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, 2);
    }

    #[test]
    fn parse_threads_caps_an_oversized_array() {
        let huge: Vec<Value> = (0..(MAX_ARRAY_ITEMS + 500))
            .map(|i| json!({"id": i, "name": "t"}))
            .collect();
        let body = json!({ "threads": huge });
        assert_eq!(parse_threads(&body).len(), MAX_ARRAY_ITEMS);
    }

    #[test]
    fn parse_verified_breakpoints_skips_entries_missing_a_line() {
        let body = json!({ "breakpoints": [{"verified": true}, {"line": 5, "verified": false}] });
        let bps = parse_verified_breakpoints(&body);
        assert_eq!(bps.len(), 1);
        assert_eq!(bps[0].line, 5);
        assert!(!bps[0].verified);
    }

    #[test]
    fn parse_stack_frames_skips_entries_missing_an_id() {
        let body = json!({ "stackFrames": [{"name": "no id"}] });
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        assert!(parse_stack_frames(&body, &root).is_empty());
    }

    #[test]
    fn parse_stack_frames_source_none_when_path_escapes_project_root() {
        let root_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let outside_file = fs::canonicalize(outside_dir.path())
            .unwrap()
            .join("secret.rs");
        std::fs::File::create(&outside_file).unwrap();

        let body = json!({
            "stackFrames": [{
                "id": 1,
                "name": "main",
                "source": { "path": outside_file.to_string_lossy() },
                "line": 10,
                "column": 1,
            }]
        });
        let frames = parse_stack_frames(&body, &root);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].source, None);
    }

    #[test]
    fn parse_stack_frames_source_some_when_path_is_inside_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let file = root.join("src.rs");
        std::fs::File::create(&file).unwrap();

        let body = json!({
            "stackFrames": [{
                "id": 1,
                "name": "main",
                "source": { "path": file.to_string_lossy() },
                "line": 10,
                "column": 1,
            }]
        });
        let frames = parse_stack_frames(&body, &root);
        assert_eq!(frames[0].source, Some(file));
    }

    #[test]
    fn parse_stop_reason_maps_known_reasons_and_carries_unknown_ones_verbatim() {
        assert_eq!(
            parse_stop_reason(Some("breakpoint")),
            StopReason::Breakpoint
        );
        assert_eq!(
            parse_stop_reason(Some("something-adapter-specific")),
            StopReason::Other("something-adapter-specific".to_string())
        );
        assert_eq!(parse_stop_reason(None), StopReason::Other(String::new()));
    }

    #[test]
    fn parse_output_category_defaults_to_console_when_absent() {
        assert_eq!(parse_output_category(None), OutputCategory::Console);
        assert_eq!(
            parse_output_category(Some("stderr")),
            OutputCategory::Stderr
        );
        assert_eq!(
            parse_output_category(Some("weird")),
            OutputCategory::Other("weird".to_string())
        );
    }

    #[test]
    fn encode_source_breakpoint_omits_optional_fields_when_none() {
        let bp = crate::types::SourceBreakpoint {
            line: 5,
            ..Default::default()
        };
        let value = encode_source_breakpoint(&bp);
        assert_eq!(value["line"], 5);
        assert!(value.get("condition").is_none());
        assert!(value.get("hitCondition").is_none());
        assert!(value.get("logMessage").is_none());
    }

    #[test]
    fn encode_source_breakpoint_includes_condition_when_present() {
        let bp = crate::types::SourceBreakpoint {
            line: 5,
            condition: Some("x > 1".to_string()),
            ..Default::default()
        };
        let value = encode_source_breakpoint(&bp);
        assert_eq!(value["condition"], "x > 1");
    }
}
