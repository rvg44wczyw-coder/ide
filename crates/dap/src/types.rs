use std::path::PathBuf;

/// A request this crate can send to the debug adapter. See
/// `docs/features/debugger.md` §2.1 for the full ordering contract each
/// variant carries; this type itself enforces none of it beyond the
/// mandatory `initialize` handshake (`DapClient::send` queues everything
/// sent before the adapter is ready and flushes once it is).
#[derive(Debug, Clone, PartialEq)]
pub enum DapRequest {
    /// Starts the debuggee. `arguments` is opaque, adapter-specific JSON
    /// the caller builds (`program`/`args`/`cwd` for `codelldb`,
    /// `module`/`console` for `debugpy`, ...) -- this crate never
    /// inspects it.
    Launch {
        arguments: serde_json::Value,
    },
    /// Same shape as `Launch`, DAP's other debuggee-start verb.
    Attach {
        arguments: serde_json::Value,
    },
    /// Replaces the *entire* breakpoint set for `path` (DAP's own
    /// semantics -- a partial "add one more" isn't expressible; the
    /// caller always sends the full current list for that file). Must
    /// not be sent before `DapEvent::ReadyForBreakpoints` fires for this
    /// session -- sending earlier is a caller bug, not something this
    /// client corrects for.
    SetBreakpoints {
        path: PathBuf,
        breakpoints: Vec<SourceBreakpoint>,
    },
    /// Tells the adapter breakpoint configuration is finished and it may
    /// start running. A no-op (silently dropped, never sent) if
    /// `Capabilities::supports_configuration_done_request` is `false`.
    ConfigurationDone,
    Continue {
        thread_id: i64,
    },
    /// DAP's "step over".
    Next {
        thread_id: i64,
    },
    StepIn {
        thread_id: i64,
    },
    StepOut {
        thread_id: i64,
    },
    Pause {
        thread_id: i64,
    },
    Threads,
    StackTrace {
        thread_id: i64,
    },
    /// Detaches and kills the debuggee (`terminateDebuggee: true`) --
    /// every adapter must support `disconnect` per spec, unlike the
    /// optional `terminate` request, so this is what "Stop" always
    /// sends.
    Disconnect,
}

/// One line breakpoint. `condition`/`hit_condition`/`log_message` are
/// carried and sent through to the adapter (an adapter that supports
/// them will honor them), but this crate builds no UI to set them -- an
/// F5a caller only ever constructs one with all three `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceBreakpoint {
    pub line: u32,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

/// An event this crate delivers to the caller via `DapClient::try_recv`.
#[derive(Debug, Clone, PartialEq)]
pub enum DapEvent {
    /// The `initialize` **response** arrived -- not to be confused with
    /// the DAP **event** literally named `initialized`
    /// (`ReadyForBreakpoints`, below). This is the transport-level
    /// "ready" gate: queued `send()` calls flush right after this fires.
    CapabilitiesReceived {
        capabilities: Capabilities,
    },
    /// The adapter's `initialized` event arrived: safe to send
    /// `SetBreakpoints`/`ConfigurationDone` now.
    ReadyForBreakpoints,
    /// Answers a `SetBreakpoints` request: which lines the adapter
    /// actually accepted (a breakpoint on a comment/blank line is
    /// typically reported `verified: false`).
    BreakpointsConfirmed {
        path: PathBuf,
        breakpoints: Vec<VerifiedBreakpoint>,
    },
    Stopped {
        reason: StopReason,
        thread_id: Option<i64>,
        description: Option<String>,
        all_threads_stopped: bool,
    },
    Continued {
        thread_id: i64,
        all_threads_continued: bool,
    },
    ThreadStarted {
        thread_id: i64,
    },
    ThreadExited {
        thread_id: i64,
    },
    Threads {
        threads: Vec<ThreadInfo>,
    },
    StackTrace {
        thread_id: i64,
        frames: Vec<StackFrame>,
    },
    Output {
        category: OutputCategory,
        text: String,
    },
    Exited {
        exit_code: i64,
    },
    Terminated,
    /// The adapter answered one of our requests with `success: false`
    /// (e.g. `launch` failed because the binary doesn't exist).
    /// `request_seq` is the DAP `seq` of the original request, carried
    /// through so a caller can disambiguate two same-`command` requests
    /// in flight at once (e.g. `StackTrace` for two different threads
    /// issued back-to-back before either resolves).
    RequestFailed {
        command: String,
        request_seq: i64,
        message: String,
    },
    /// Subprocess exited or a fatal protocol error occurred.
    AdapterExited {
        message: String,
    },
}

/// Capability flags from the `initialize` response. Fields beyond what
/// F5a's caller reads (`supports_configuration_done_request`) are
/// captured now specifically so a later phase needs no `ide-dap` change,
/// only caller-side wiring -- capabilities come from `initialize`, never
/// assumed present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub supports_configuration_done_request: bool,
    pub supports_conditional_breakpoints: bool,
    pub supports_hit_conditional_breakpoints: bool,
    pub supports_log_points: bool,
    pub supports_terminate_request: bool,
    pub supports_restart_request: bool,
    pub supports_step_back: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadInfo {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    pub id: i64,
    pub name: String,
    /// `None` if the adapter gave no source path, or gave one that
    /// canonicalizes outside the session's `project_root`
    /// (`docs/features/debugger.md` §3.6).
    pub source: Option<PathBuf>,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBreakpoint {
    pub line: u32,
    pub verified: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Step,
    Breakpoint,
    Exception,
    Pause,
    Entry,
    Goto,
    FunctionBreakpoint,
    DataBreakpoint,
    InstructionBreakpoint,
    /// Any `reason` string DAP defines that isn't one of the above, or an
    /// adapter-specific one -- carried verbatim rather than dropped.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputCategory {
    Console,
    Stdout,
    Stderr,
    Telemetry,
    Other(String),
}
