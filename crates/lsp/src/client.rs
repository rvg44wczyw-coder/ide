use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::Duration;

use serde::de::{Deserialize, Deserializer, IgnoredAny, SeqAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender};
use url::Url;

use crate::error::LspError;
use crate::path::validate_path;
use crate::protocol::{
    encode_notification, encode_request, encode_response, read_message, write_message, ReadOutcome,
};
use crate::types::{
    CodeAction, Diagnostic, DiagnosticSeverity, FileEdit, GotoKind, InlayHint, Location, LspEvent,
    LspRequest, Position, Range, SemanticToken, SemanticTokenKind, Symbol, SymbolKind, TextEdit,
    WorkspaceEdit,
};

const INITIALIZE_ID: u64 = 1;
const SHUTDOWN_ID: u64 = 2;
const KILL_TIMEOUT: Duration = Duration::from_millis(500);

/// Per-connection request bookkeeping threaded through `handle_incoming`/
/// `send_request`: `didChange` version counters (unchanged from before
/// find-usages), plus the id allocator and the single-slot "currently
/// pending references/goto request" tracking that implements the
/// supersede-by-overwrite correlation `docs/features/find-usages.md`
/// §3/§4 describes -- sending a new `References` request overwrites
/// `pending_references_id`, so a response for an older id no longer
/// matches anything and is dropped in `handle_incoming`.
/// `pending_goto_id` is the same discipline shared across all three
/// `GotoKind`s (`docs/features/goto-definition.md` §3/§4): one slot, not
/// three, since a `Goto` query of any kind supersedes any other kind's
/// still-outstanding one.
///
/// `pending_hover_id`/`pending_document_highlight_id`/
/// `pending_inlay_hint` are each their *own* independent slot -- unlike
/// `Goto`'s three kinds, these three query kinds are conceptually
/// unrelated and can legitimately be in flight simultaneously (the caret
/// moves, firing a new `DocumentHighlight`, while a `Hover` from a moment
/// ago is still loading); sharing a slot across them would make an
/// unrelated query silently cancel this one's in-flight answer (see
/// `docs/features/inlay-hints-and-hover.md` §4). `pending_inlay_hint`
/// additionally carries the request's own `path` alongside its id --
/// unlike the other four, `LspEvent::InlayHint` needs to name which file
/// its `hints` belong to (`ide-ui` keeps a per-file map, not a single
/// slot, §2.2 of that doc), and the response itself never repeats the
/// path the request was for.
struct ConnectionState {
    versions: HashMap<PathBuf, i32>,
    next_request_id: u64,
    pending_references_id: Option<u64>,
    pending_goto_id: Option<u64>,
    pending_hover_id: Option<u64>,
    pending_document_highlight_id: Option<u64>,
    pending_inlay_hint: Option<(u64, PathBuf)>,
    /// Own slot, same reasoning as `pending_hover_id` et al. Carries
    /// `path` alongside the id for the same reason `pending_inlay_hint`
    /// does -- `LspEvent::CodeAction` needs to say which file's caret
    /// produced it (`docs/features/code-actions.md` §2.1, §3.2).
    pending_code_action_id: Option<(u64, PathBuf)>,
    /// Only ever set while resolving one specific selected action
    /// (`ApplyCodeAction` -> `codeAction/resolve`), independent of every
    /// other slot including `pending_code_action_id` itself -- an ambient
    /// re-query and an in-flight resolve must never be confused with each
    /// other (`docs/features/code-actions.md` §3.3).
    pending_resolve_id: Option<u64>,
    /// Set once from the `initialize` response's
    /// `capabilities.codeActionProvider.resolveProvider` -- `false` for
    /// absent/malformed/bare-`true` capabilities, fail closed
    /// (`docs/features/code-actions.md` §3.2).
    code_action_resolve_provider: bool,
    /// Every entry from the most recently received `CodeAction` response,
    /// replaced wholesale on the next one (supersede-by-overwrite, same
    /// as `document_highlights`) -- what `ApplyCodeAction { index }` looks
    /// `index` up in. Never exposed outside this file
    /// (`docs/features/code-actions.md` §3.2).
    last_code_actions: Vec<RawCodeAction>,
    /// Own slot, same reasoning as `pending_inlay_hint`/
    /// `pending_code_action_id` -- carries `path` alongside the id since
    /// `LspEvent::DocumentSymbol` needs to say which file's outline this
    /// is (`docs/features/search-everywhere.md` §2.2, §2.3).
    pending_document_symbol_id: Option<(u64, PathBuf)>,
    /// Own slot, independent of `pending_document_symbol_id` and every
    /// other slot -- a `WorkspaceSymbol` query and a `DocumentSymbol`
    /// query are conceptually unrelated and can legitimately be in flight
    /// simultaneously (`docs/features/search-everywhere.md` §2.2, §2.3).
    pending_workspace_symbol_id: Option<u64>,
    /// Shared by `Format` and `FormatRange` -- sending either while the
    /// other is outstanding supersedes it (`docs/features/formatting.md`
    /// §2.1). Carries `path` alongside the id for the same reason
    /// `pending_code_action_id`/`pending_inlay_hint` do: the response
    /// itself has no path field of its own (§3.1).
    pending_format: Option<(u64, PathBuf)>,
    /// Set once from the `initialize` response's
    /// `capabilities.documentFormattingProvider` -- `false` for
    /// absent/malformed capabilities, fail closed
    /// (`docs/features/formatting.md` §3.2).
    document_formatting_provider: bool,
    /// Same, for `capabilities.documentRangeFormattingProvider`.
    document_range_formatting_provider: bool,
    /// Own slot, independent of every other slot including `pending_rename`
    /// -- an in-flight `PrepareRename` and the `Rename` request the same
    /// popup sends moments later must never be confused with each other
    /// (`docs/features/rename-refactoring.md` §2.1, §2.2).
    pending_prepare_rename: Option<(u64, PathBuf)>,
    /// Own slot, carries `new_name` alongside the id/path since
    /// `LspEvent::RenameReady` echoes it back without caching it
    /// separately (`docs/features/rename-refactoring.md` §2.1, §2.2).
    pending_rename: Option<(u64, PathBuf, String)>,
    /// Set once from the `initialize` response's
    /// `capabilities.renameProvider` -- `false` for absent/malformed
    /// capabilities, fail closed (`docs/features/rename-refactoring.md`
    /// §2.2).
    rename_provider: bool,
    /// Same, for `capabilities.renameProvider.prepareProvider` -- only
    /// meaningful when `rename_provider` is also `true` (see
    /// `docs/features/rename-refactoring.md` §2.2).
    prepare_rename_provider: bool,
    /// Own slot, carrying `path` alongside the id for the same reason
    /// `pending_inlay_hint` does -- `ide-ui` keeps semantic tokens in a
    /// per-file map, and the response itself never repeats the path the
    /// request was for. Independent of every other slot, including
    /// `pending_inlay_hint`: an edit firing `SemanticTokensFull` must not
    /// race with an in-flight `Hover`/`DocumentHighlight`/`InlayHint`
    /// (`docs/features/semantic-highlighting.md` §3.1).
    pending_semantic_tokens: Option<(u64, PathBuf)>,
    /// Set once from the `initialize` response's
    /// `capabilities.semanticTokensProvider`'s `full` field -- `false` for
    /// absent/malformed capabilities or `full: false`, fail closed
    /// (`docs/features/semantic-highlighting.md` §3.1).
    semantic_tokens_provider: bool,
    /// The `legend.tokenTypes` array from the same response, needed to
    /// resolve each token's `token_type` index during decode (§3.2). Empty
    /// whenever `semantic_tokens_provider` is `false`.
    semantic_token_legend: Vec<String>,
    /// Own slot for `OrganizeImports`'s initial `textDocument/codeAction`
    /// query, independent of `pending_code_action_id` -- an ambient
    /// `⌥↩` re-query and an Optimize Imports one-shot request must not be
    /// confused with each other (`docs/features/code-generation.md` §2.1).
    pending_organize_imports_id: Option<u64>,
    /// Own slot for the follow-up `codeAction/resolve` `OrganizeImports`
    /// sends when its first entry needs resolving, independent of
    /// `pending_resolve_id` (which is only ever set by `ApplyCodeAction`'s
    /// own resolve step) (`docs/features/code-generation.md` §2.1).
    pending_organize_imports_resolve_id: Option<u64>,
}

impl ConnectionState {
    fn new() -> Self {
        Self {
            versions: HashMap::new(),
            // Disjoint from the reserved `INITIALIZE_ID`/`SHUTDOWN_ID`.
            next_request_id: 3,
            pending_references_id: None,
            pending_goto_id: None,
            pending_hover_id: None,
            pending_document_highlight_id: None,
            pending_inlay_hint: None,
            pending_code_action_id: None,
            pending_resolve_id: None,
            code_action_resolve_provider: false,
            last_code_actions: Vec::new(),
            pending_document_symbol_id: None,
            pending_workspace_symbol_id: None,
            pending_format: None,
            document_formatting_provider: false,
            document_range_formatting_provider: false,
            pending_prepare_rename: None,
            pending_rename: None,
            rename_provider: false,
            prepare_rename_provider: false,
            pending_semantic_tokens: None,
            semantic_tokens_provider: false,
            semantic_token_legend: Vec::new(),
            pending_organize_imports_id: None,
            pending_organize_imports_resolve_id: None,
        }
    }

    fn allocate_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }
}

/// One cached raw code-action entry from the most recent
/// `textDocument/codeAction` response -- never exposed outside this file
/// (`docs/features/code-actions.md` §3.2). `raw` is the server's own,
/// unmodified payload for this entry (a `Command` or `CodeAction` object),
/// sent back verbatim as `codeAction/resolve`'s params if `resolvable` is
/// true; `edit` is `Some` when the response already carried one (the
/// common case), applied directly with no resolve round trip.
struct RawCodeAction {
    raw: Value,
    edit: Option<lsp_types::WorkspaceEdit>,
    resolvable: bool,
}

/// Bounded rather than unbounded: a flooding or misbehaving language
/// server must not be able to force unbounded memory growth just because
/// the UI-side consumer isn't draining `try_recv` fast enough. Diagnostics
/// specifically are additionally coalesced per-path (see
/// `flush_pending_diagnostics`) so backpressure here degrades to "latest
/// diagnostics per file, delivered a little late" rather than blocking.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// How often the event loop retries flushing coalesced diagnostics into
/// `event_tx` when nothing else (an incoming server message, an outgoing
/// request) is happening to trigger a flush attempt on its own -- without
/// this, a backlog built up during a flood could sit forever once the
/// flood stops and the consumer starts draining, since nothing else would
/// ever call `try_send` again.
const DIAGNOSTICS_FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Bounded for the same reason as `EVENT_CHANNEL_CAPACITY`, one hop
/// earlier in the pipeline: without this, a flooding server can queue
/// unboundedly many raw, not-yet-decoded messages here (each up to
/// `MAX_CONTENT_LENGTH`) purely by writing faster than the main loop's
/// per-message JSON parsing and path validation can keep up, entirely
/// bypassing `event_tx`'s bound and the diagnostics coalescing -- neither
/// of which this channel's producer (the reader task) ever touches. Small
/// on purpose: each slot can hold up to a 16 MiB message, so even this
/// modest capacity bounds worst-case memory here to a low double-digit
/// number of MiB, while still giving the reader task enough of a lead to
/// keep the OS pipe draining smoothly under normal (non-flood) load.
const READER_CHANNEL_CAPACITY: usize = 8;

/// No legitimate LSP server (`rust-analyzer` included) sends anywhere
/// near this many diagnostics for one file in one notification -- this
/// exists purely to bound worst-case payload size from a malicious/buggy
/// server, the same way `MAX_CONTENT_LENGTH`/`MAX_HEADER_BYTES` bound the
/// raw wire framing. Without it, `EVENT_CHANNEL_CAPACITY`'s and
/// `READER_CHANNEL_CAPACITY`'s slot-count bounds are size-unaware: a
/// server sending an unbounded *number* of diagnostics in a single
/// message inflates each slot's actual byte size arbitrarily, defeating
/// both bounds' intent regardless of how many slots they have.
const MAX_DIAGNOSTICS_PER_MESSAGE: usize = 1000;

/// Same rationale as `MAX_DIAGNOSTICS_PER_MESSAGE`, applied to
/// `textDocument/references` responses: a malicious/buggy server can pack
/// a single response with far more `Location` entries than any real
/// "find usages" result would ever contain, forcing an expensive typed
/// deserialize over the whole array before path validation ever gets a
/// chance to discard anything. `Location` carries no message/tags/
/// relatedInformation strings the way `Diagnostic` does, so it's lighter
/// per-entry, but the same cap keeps worst-case cost in the same
/// ballpark rather than needing separate justification.
const MAX_LOCATIONS_PER_MESSAGE: usize = 1000;

/// Same rationale as `MAX_LOCATIONS_PER_MESSAGE`, applied to
/// `textDocument/documentSymbol` and `workspace/symbol` responses. Bounds
/// both the top-level array length deserialized for a flat
/// (`SymbolInformation[]`/`WorkspaceSymbol[]`) response, and the total
/// flattened symbol count `flatten_document_symbols` will ever produce
/// out of a hierarchical (`DocumentSymbol[]`) response, regardless of how
/// deep or wide the server's `children` nesting is
/// (`docs/features/search-everywhere.md` §2.3, §4).
const MAX_SYMBOLS_PER_MESSAGE: usize = 500;

/// Caps `flatten_document_symbols`'s own recursion into `children`,
/// independent of `serde_json`'s deserializer-level recursion limit.
/// Comfortably under `serde_json`'s current default of 128 (see the
/// hacker findings doc `docs/security-findings/rust-lsp-dev-search-
/// everywhere-2026-08-20.md` finding 1): today, a `DocumentSymbol` tree
/// deep enough to threaten this function's own recursion already fails
/// to deserialize first, landing in the permissive "unparseable ->
/// empty" fallback before `flatten_document_symbols` ever runs. This
/// constant makes that safety margin self-contained rather than
/// borrowed from an upstream default this crate doesn't pin or
/// re-assert -- a future `serde_json` upgrade that raises or disables
/// that limit elsewhere in the process must not silently reopen a
/// stack-overflow path here.
const MAX_SYMBOL_TREE_DEPTH: usize = 100;

/// Not a reuse of `MAX_LOCATIONS_PER_MESSAGE`: a real multi-thousand-line
/// source file legitimately produces far more semantic tokens than it
/// ever would `Location`/`InlayHint` entries (`docs/features/
/// semantic-highlighting.md` §3.2), so `MAX_LOCATIONS_PER_MESSAGE`'s
/// `1000` would visibly truncate highlighting on an ordinary large file,
/// not just an adversarial one. `20_000` is generous for any realistic
/// single file while still bounding the raw-array decode cost a
/// malicious/buggy server's response can force. Counts *tokens*, not raw
/// `u32`s -- `BoundedSemanticTokenData` caps the underlying array at
/// `MAX_SEMANTIC_TOKENS_PER_MESSAGE * 5` elements, preserving the wire's
/// 5-`u32`-per-token chunking exactly at the truncation boundary.
const MAX_SEMANTIC_TOKENS_PER_MESSAGE: usize = 20_000;

/// A running (or starting) `rust-analyzer` connection: a background
/// thread owns a private tokio runtime driving the subprocess and the
/// JSON-RPC event loop; the UI thread talks to it only through
/// `send`/`try_recv`, both non-blocking.
///
/// Dropping an `LspClient` tears down its subprocess: dropping the
/// request channel's sender signals the background loop to send a
/// best-effort LSP `shutdown`/`exit` notification, then kill the child
/// if it hasn't exited promptly. There is no separate `stop` method —
/// replacing an `Option<LspClient>` (project switch, or the "Restart
/// Rust Analyzer" action re-calling `start`) *is* the teardown path, and
/// always fully replaces any previous instance rather than running two
/// clients concurrently.
pub struct LspClient {
    request_tx: UnboundedSender<LspRequest>,
    event_rx: Receiver<LspEvent>,
}

impl LspClient {
    /// Spawns `rust-analyzer` (must already be on `PATH`) and starts the
    /// connection. See [`LspClient::start_with_command`].
    pub fn start(project_root: impl AsRef<Path>) -> Result<Self, LspError> {
        Self::start_with_command(project_root, "rust-analyzer", &[])
    }

    /// Like [`LspClient::start`], but spawns `command` (with `args`,
    /// `docs/features/language-server-arguments.md`) instead of the fixed
    /// `"rust-analyzer"` literal with no arguments — for tests, or a
    /// user-configured server.
    ///
    /// Sends the LSP `initialize` handshake and starts a background
    /// thread running the JSON-RPC event loop. Returns as soon as the
    /// process spawns successfully — `initialize` completes
    /// asynchronously; requests sent via `send` before it completes are
    /// queued internally and flushed once the server is ready.
    ///
    /// A spawn failure with `io::ErrorKind::NotFound` is reported as
    /// `LspError::ServerNotFound(command)`; every other spawn/I/O
    /// failure (including an `args` entry `std::process::Command` itself
    /// rejects, e.g. one containing an embedded NUL byte) is reported as
    /// `LspError::Io` — never a panic.
    pub fn start_with_command(
        project_root: impl AsRef<Path>,
        command: &str,
        args: &[String],
    ) -> Result<Self, LspError> {
        let project_root = fs::canonicalize(project_root.as_ref())?;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(LspError::Io)?;

        let command_owned = command.to_string();
        let child = runtime.block_on(spawn_child(&command_owned, args, &project_root))?;

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

    /// Non-blocking; queues `request` for the background thread. Every
    /// path is validated against `project_root` before being sent — a
    /// path outside the root is silently dropped rather than erroring,
    /// since the caller should never construct one (same provenance
    /// discipline as `ide_core::GitRepo::resolve_conflict`). Dropping is
    /// also what happens if the background loop has already ended (the
    /// channel is closed) — there's nowhere else to report that.
    pub fn send(&self, request: LspRequest) {
        let _ = self.request_tx.send(request);
    }

    /// Non-blocking poll; call once per UI frame. Only ever returns one
    /// event per call — callers should call in a loop to drain
    /// everything available in a frame, same as `ClaudePanel::poll`.
    pub fn try_recv(&mut self) -> Option<LspEvent> {
        self.event_rx.try_recv().ok()
    }
}

async fn spawn_child(
    command: &str,
    args: &[String],
    project_root: &Path,
) -> Result<Child, LspError> {
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
                LspError::ServerNotFound(command.to_string())
            } else {
                LspError::Io(e)
            }
        })
}

async fn run_event_loop(
    mut child: Child,
    project_root: PathBuf,
    mut request_rx: UnboundedReceiver<LspRequest>,
    event_tx: Sender<LspEvent>,
) {
    let Some(mut stdin) = child.stdin.take() else {
        let _ = event_tx
            .send(LspEvent::ServerExited {
                message: "rust-analyzer subprocess had no stdin pipe".to_string(),
            })
            .await;
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = event_tx
            .send(LspEvent::ServerExited {
                message: "rust-analyzer subprocess had no stdout pipe".to_string(),
            })
            .await;
        return;
    };

    if send_initialize(&mut stdin, &project_root).await.is_err() {
        let _ = event_tx
            .send(LspEvent::ServerExited {
                message: "failed to write initialize request to rust-analyzer".to_string(),
            })
            .await;
        let _ = kill_and_wait(&mut child).await;
        return;
    }

    let mut incoming = spawn_reader(BufReader::new(stdout));
    let mut ready = false;
    let mut pending: Vec<LspRequest> = Vec::new();
    let mut state = ConnectionState::new();
    // Coalescing backlog for `publishDiagnostics`: keyed by path, always
    // overwritten with the latest set (matches `LspEvent::Diagnostics`'s
    // documented "replaces the full diagnostic set for `path`" semantics),
    // so a flood targeting one file never grows this past one entry, and a
    // flood spread across many files is bounded by the project's real file
    // count rather than by how many messages the server chose to send.
    let mut pending_diagnostics: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();
    let mut flush_interval = tokio::time::interval(DIAGNOSTICS_FLUSH_INTERVAL);

    loop {
        tokio::select! {
            _ = flush_interval.tick() => {
                flush_pending_diagnostics(&mut pending_diagnostics, &event_tx);
            }
            maybe_msg = incoming.recv() => {
                let outcome = maybe_msg.unwrap_or(ReadOutcome::Eof);
                match outcome {
                    ReadOutcome::Message(bytes) => {
                        match handle_incoming(
                            &bytes,
                            &project_root,
                            &mut ready,
                            &mut stdin,
                            &mut pending,
                            &mut state,
                            &mut pending_diagnostics,
                            &event_tx,
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(e) => {
                                let _ = event_tx.send(LspEvent::ServerExited {
                                    message: format!("rust-analyzer protocol error: {e}"),
                                }).await;
                                break;
                            }
                        }
                    }
                    ReadOutcome::Eof => {
                        let _ = event_tx.send(LspEvent::ServerExited {
                            message: "rust-analyzer exited".to_string(),
                        }).await;
                        break;
                    }
                    ReadOutcome::Error(e) => {
                        let _ = event_tx.send(LspEvent::ServerExited {
                            message: format!("rust-analyzer protocol error: {e}"),
                        }).await;
                        break;
                    }
                }
            }
            maybe_req = request_rx.recv() => {
                match maybe_req {
                    Some(request) => {
                        if ready {
                            let _ = send_request(
                                &mut stdin,
                                &project_root,
                                request,
                                &mut state,
                                &event_tx,
                            )
                            .await;
                        } else {
                            pending.push(request);
                        }
                    }
                    None => {
                        let _ = shutdown_gracefully(&mut stdin).await;
                        break;
                    }
                }
            }
        }
    }

    let _ = kill_and_wait(&mut child).await;
}

/// Attempts to deliver every coalesced diagnostics entry, non-blockingly.
/// An entry that doesn't fit (channel full -- the consumer isn't draining
/// fast enough) stays in `pending` for the next attempt rather than being
/// dropped or blocking the caller; a closed channel (client dropped) drops
/// it, since there's nowhere left to deliver it.
fn flush_pending_diagnostics(
    pending: &mut HashMap<PathBuf, Vec<Diagnostic>>,
    event_tx: &Sender<LspEvent>,
) {
    pending.retain(|path, diagnostics| {
        let event = LspEvent::Diagnostics {
            path: path.clone(),
            diagnostics: diagnostics.clone(),
        };
        match event_tx.try_send(event) {
            Ok(()) => false,
            Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Closed(_)) => false,
        }
    });
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
/// whenever the other branch won a given iteration. A plain channel
/// `recv()` has no such problem.
fn spawn_reader(mut reader: BufReader<tokio::process::ChildStdout>) -> Receiver<ReadOutcome> {
    let (tx, rx) = mpsc::channel(READER_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        loop {
            let outcome = read_message(&mut reader).await;
            let is_terminal = matches!(outcome, ReadOutcome::Eof | ReadOutcome::Error(_));
            // Awaiting here (rather than a non-blocking try_send) is the
            // point: once the main loop falls behind, this stops pulling
            // further bytes off the pipe, throttling the OS pipe and, in
            // turn, the subprocess's own write() calls once its kernel
            // pipe buffer fills -- backpressure all the way back to the
            // producer instead of an ever-growing queue on our side.
            if tx.send(outcome).await.is_err() || is_terminal {
                break;
            }
        }
    });
    rx
}

async fn send_initialize(stdin: &mut ChildStdin, project_root: &Path) -> io::Result<()> {
    let root_uri = Url::from_file_path(project_root)
        .ok()
        .map(|u| u.to_string());
    let params = json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": {},
    });
    let body = encode_request(INITIALIZE_ID, "initialize", params);
    write_message(stdin, &body).await
}

async fn shutdown_gracefully(stdin: &mut ChildStdin) {
    // Best-effort: don't wait for a `shutdown` response before sending
    // `exit` — waiting risks hanging the background thread indefinitely
    // if the server never replies, and Drop only promises best-effort
    // teardown (the caller's `kill_and_wait` bounds the total time).
    let _ = write_message(stdin, &encode_request(SHUTDOWN_ID, "shutdown", Value::Null)).await;
    let _ = write_message(stdin, &encode_notification("exit", Value::Null)).await;
}

/// A JSON-RPC message from the subprocess, parsed just enough to route it
/// (`id`/`method`/`error`, same fields `handle_incoming` always needed) --
/// but `result` stays a borrowed, syntax-validated-but-not-yet-materialized
/// `RawValue` rather than a `Value`. A `RawValue` capture walks the raw
/// bytes once to find the value's extent without allocating a `Value`
/// node per array element/string/number the way a full `Value` parse
/// does, which matters for `result`: it's the one field that can
/// legitimately be a huge array (`textDocument/references`). See
/// `docs/security-findings/rust-lsp-dev-find-usages-*.md` fix-round-2 --
/// round 1 truncated `result` *after* fully parsing it into a `Value`
/// tree, which had already paid the dominant cost by the time truncation
/// ran; this avoids materializing more than `MAX_LOCATIONS_PER_MESSAGE`
/// entries as anything at all, not just capping what's kept afterward.
#[derive(serde::Deserialize)]
struct IncomingMessage<'a> {
    id: Option<Value>,
    method: Option<Value>,
    error: Option<Value>,
    params: Option<Value>,
    #[serde(borrow)]
    result: Option<&'a RawValue>,
}

/// The handful of `initialize` response fields this client reads --
/// everything else is ignored, same as before these phases
/// (`docs/features/code-actions.md` §3.2, `docs/features/formatting.md`
/// §3.2). A narrower struct than `lsp_types::InitializeResult` on
/// purpose: only the capability flags below are needed, and a server
/// sending a `capabilities` shape this client doesn't otherwise
/// understand still deserializes fine here.
#[derive(serde::Deserialize)]
struct InitializeResultCapabilities {
    capabilities: InitializeCapabilitiesFields,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeCapabilitiesFields {
    code_action_provider: Option<lsp_types::CodeActionProviderCapability>,
    document_formatting_provider:
        Option<lsp_types::OneOf<bool, lsp_types::DocumentFormattingOptions>>,
    document_range_formatting_provider:
        Option<lsp_types::OneOf<bool, lsp_types::DocumentRangeFormattingOptions>>,
    rename_provider: Option<lsp_types::OneOf<bool, lsp_types::RenameOptions>>,
    semantic_tokens_provider: Option<lsp_types::SemanticTokensServerCapabilities>,
}

/// `workspace/applyEdit`'s request params (`docs/features/code-actions.md`
/// §3.5) -- `label` is purely cosmetic, surfaced to the user alongside
/// success/failure; only `edit` is acted on.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyEditParams {
    #[serde(default)]
    label: Option<String>,
    edit: lsp_types::WorkspaceEdit,
}

impl InitializeResultCapabilities {
    /// `true` only for an explicit `CodeActionOptions { resolveProvider:
    /// true }` -- a bare `true`/`false` boolean, or an absent/malformed
    /// capability, all mean `false` (fail closed,
    /// `docs/features/code-actions.md` §3.2, §4).
    fn resolve_provider(&self) -> bool {
        match &self.capabilities.code_action_provider {
            Some(lsp_types::CodeActionProviderCapability::Options(opts)) => {
                opts.resolve_provider.unwrap_or(false)
            }
            _ => false,
        }
    }

    /// `true` for a bare `true` boolean or any options object; `false`
    /// for absent/`false`/malformed (fail closed,
    /// `docs/features/formatting.md` §3.2, §4). Unlike
    /// `codeActionProvider`, neither `DocumentFormattingOptions` nor
    /// `DocumentRangeFormattingOptions` carries a flag this client cares
    /// about, so presence alone (in either `OneOf` variant) is enough.
    fn document_formatting_provider(&self) -> bool {
        match &self.capabilities.document_formatting_provider {
            Some(lsp_types::OneOf::Left(supported)) => *supported,
            Some(lsp_types::OneOf::Right(_)) => true,
            None => false,
        }
    }

    /// Same rule as `document_formatting_provider`, for
    /// `documentRangeFormattingProvider`.
    fn document_range_formatting_provider(&self) -> bool {
        match &self.capabilities.document_range_formatting_provider {
            Some(lsp_types::OneOf::Left(supported)) => *supported,
            Some(lsp_types::OneOf::Right(_)) => true,
            None => false,
        }
    }

    /// `true` for a bare `true` boolean or any options object; `false` for
    /// absent/`false`/malformed (fail closed,
    /// `docs/features/rename-refactoring.md` §2.2, §4).
    fn rename_provider(&self) -> bool {
        match &self.capabilities.rename_provider {
            Some(lsp_types::OneOf::Left(supported)) => *supported,
            Some(lsp_types::OneOf::Right(_)) => true,
            None => false,
        }
    }

    /// Only `true` when the options object explicitly sets
    /// `prepareProvider: true` -- a bare boolean `renameProvider` carries
    /// no `prepareProvider` flag to read, so it means `false` here even
    /// though `rename_provider()` is `true` for that same shape
    /// (`docs/features/rename-refactoring.md` §2.2).
    fn prepare_rename_provider(&self) -> bool {
        match &self.capabilities.rename_provider {
            Some(lsp_types::OneOf::Right(opts)) => opts.prepare_provider.unwrap_or(false),
            _ => false,
        }
    }

    /// The options object shared by both `SemanticTokensServerCapabilities`
    /// variants -- `SemanticTokensOptions` directly, or flattened inside
    /// `SemanticTokensRegistrationOptions`. Both carry `legend`/`full`
    /// identically; this just picks the right field out of whichever
    /// variant the server sent.
    fn semantic_tokens_options(&self) -> Option<&lsp_types::SemanticTokensOptions> {
        match &self.capabilities.semantic_tokens_provider {
            Some(lsp_types::SemanticTokensServerCapabilities::SemanticTokensOptions(opts)) => {
                Some(opts)
            }
            Some(
                lsp_types::SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(
                    opts,
                ),
            ) => Some(&opts.semantic_tokens_options),
            None => None,
        }
    }

    /// `true` only when the options' `full` field is present and is either
    /// the bare `true` boolean or the `{ delta: .. }` object shape (delta
    /// support implies full support); `false` for `full: false`, an absent
    /// `full` field, or an absent `semanticTokensProvider` entirely (fail
    /// closed, `docs/features/semantic-highlighting.md` §3.1, §4).
    fn semantic_tokens_full_provider(&self) -> bool {
        match self.semantic_tokens_options().and_then(|o| o.full.as_ref()) {
            Some(lsp_types::SemanticTokensFullOptions::Bool(supported)) => *supported,
            Some(lsp_types::SemanticTokensFullOptions::Delta { .. }) => true,
            None => false,
        }
    }

    /// The options' `legend.token_types`, each converted to a plain
    /// `String`; an empty `Vec` for every failure case
    /// `semantic_tokens_full_provider` also treats as unsupported --
    /// there's nothing to decode indices against if `full` isn't even
    /// requested (§3.1).
    fn semantic_tokens_legend(&self) -> Vec<String> {
        if !self.semantic_tokens_full_provider() {
            return Vec::new();
        }
        self.semantic_tokens_options()
            .map(|o| {
                o.legend
                    .token_types
                    .iter()
                    .map(|t| t.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Deserializes at most `MAX_LOCATIONS_PER_MESSAGE` array elements as
/// `lsp_types::Location`, then drains any further elements via
/// `IgnoredAny` -- which validates their JSON shape without allocating a
/// `Value` or a `Location` for them -- instead of collecting the whole
/// array first (as `Vec<Location>::deserialize`, or a generic `Value`
/// parse, both would) and only discarding the excess afterward. Shared by
/// `textDocument/references`' response (always an array) and `Goto`'s
/// (an array in the multi-candidate case -- see `parse_goto_result`,
/// `docs/features/goto-definition.md` §3/§4) -- nothing about this type
/// is References-specific.
struct BoundedLocations(Vec<lsp_types::Location>);

impl<'de> Deserialize<'de> for BoundedLocations {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVisitor;

        impl<'de> Visitor<'de> for BoundedVisitor {
            type Value = BoundedLocations;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of LSP Location objects")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut locations = Vec::new();
                while locations.len() < MAX_LOCATIONS_PER_MESSAGE {
                    match seq.next_element::<lsp_types::Location>()? {
                        Some(loc) => locations.push(loc),
                        None => return Ok(BoundedLocations(locations)),
                    }
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BoundedLocations(locations))
            }
        }

        deserializer.deserialize_seq(BoundedVisitor)
    }
}

/// Same bounding discipline as `BoundedLocations`, applied to
/// `textDocument/documentHighlight`'s response shape
/// (`lsp_types::DocumentHighlight`, i.e. `{ range, kind? }`) instead of
/// `lsp_types::Location` -- see `docs/features/inlay-hints-and-hover.md`
/// §3.2. `.kind` is deserialized (it's part of `DocumentHighlight`'s own
/// shape) but never read past this point; only `.range` is ever used.
struct BoundedDocumentHighlights(Vec<lsp_types::DocumentHighlight>);

impl<'de> Deserialize<'de> for BoundedDocumentHighlights {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVisitor;

        impl<'de> Visitor<'de> for BoundedVisitor {
            type Value = BoundedDocumentHighlights;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of LSP DocumentHighlight objects")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut highlights = Vec::new();
                while highlights.len() < MAX_LOCATIONS_PER_MESSAGE {
                    match seq.next_element::<lsp_types::DocumentHighlight>()? {
                        Some(h) => highlights.push(h),
                        None => return Ok(BoundedDocumentHighlights(highlights)),
                    }
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BoundedDocumentHighlights(highlights))
            }
        }

        deserializer.deserialize_seq(BoundedVisitor)
    }
}

/// Same bounding discipline as `BoundedLocations`, applied to
/// `textDocument/inlayHint`'s response shape (`lsp_types::InlayHint`)
/// instead of `lsp_types::Location` -- see
/// `docs/features/inlay-hints-and-hover.md` §3.2.
struct BoundedInlayHints(Vec<lsp_types::InlayHint>);

impl<'de> Deserialize<'de> for BoundedInlayHints {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVisitor;

        impl<'de> Visitor<'de> for BoundedVisitor {
            type Value = BoundedInlayHints;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of LSP InlayHint objects")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut hints = Vec::new();
                while hints.len() < MAX_LOCATIONS_PER_MESSAGE {
                    match seq.next_element::<lsp_types::InlayHint>()? {
                        Some(h) => hints.push(h),
                        None => return Ok(BoundedInlayHints(hints)),
                    }
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BoundedInlayHints(hints))
            }
        }

        deserializer.deserialize_seq(BoundedVisitor)
    }
}

/// Same bounding discipline as `BoundedLocations`/`BoundedInlayHints`, one
/// level lower: caps the *raw* `u32` array `textDocument/semanticTokens
/// /full`'s `data` field carries, at `MAX_SEMANTIC_TOKENS_PER_MESSAGE * 5`
/// elements -- a multiple of 5, preserving the wire's 5-`u32`-per-token
/// chunking exactly at the truncation boundary, so `decode_semantic_tokens`
/// (§3.2) never sees a partial token (`docs/features/
/// semantic-highlighting.md` §3.2).
#[derive(Default)]
struct BoundedSemanticTokenData(Vec<u32>);

impl<'de> Deserialize<'de> for BoundedSemanticTokenData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVisitor;

        impl<'de> Visitor<'de> for BoundedVisitor {
            type Value = BoundedSemanticTokenData;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of u32 (LSP SemanticTokens.data)")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let cap = MAX_SEMANTIC_TOKENS_PER_MESSAGE * 5;
                let mut data = Vec::new();
                while data.len() < cap {
                    match seq.next_element::<u32>()? {
                        Some(n) => data.push(n),
                        None => return Ok(BoundedSemanticTokenData(data)),
                    }
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BoundedSemanticTokenData(data))
            }
        }

        deserializer.deserialize_seq(BoundedVisitor)
    }
}

/// The `textDocument/semanticTokens/full` response's shape -- only `data`
/// is read; `resultId` exists solely to support `.../full/delta`, which
/// v1 never sends (`docs/features/semantic-highlighting.md` §1).
#[derive(serde::Deserialize)]
struct RawSemanticTokensResult {
    #[serde(default)]
    data: BoundedSemanticTokenData,
}

/// Same bounding discipline as `BoundedLocations`, applied to
/// `textDocument/codeAction`'s response shape
/// (`lsp_types::CodeActionOrCommand`, an untagged `Command | CodeAction`)
/// instead of `lsp_types::Location` -- see
/// `docs/features/code-actions.md` §3.1.
struct BoundedCodeActions(Vec<lsp_types::CodeActionOrCommand>);

impl<'de> Deserialize<'de> for BoundedCodeActions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVisitor;

        impl<'de> Visitor<'de> for BoundedVisitor {
            type Value = BoundedCodeActions;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of LSP Command or CodeAction objects")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut actions = Vec::new();
                while actions.len() < MAX_LOCATIONS_PER_MESSAGE {
                    match seq.next_element::<lsp_types::CodeActionOrCommand>()? {
                        Some(a) => actions.push(a),
                        None => return Ok(BoundedCodeActions(actions)),
                    }
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BoundedCodeActions(actions))
            }
        }

        deserializer.deserialize_seq(BoundedVisitor)
    }
}

/// Same bounding discipline as `BoundedLocations`, applied to
/// `textDocument/formatting`'s/`textDocument/rangeFormatting`'s
/// `TextEdit[]` response shape (`lsp_types::TextEdit`) instead of
/// `lsp_types::Location` -- see `docs/features/formatting.md` §3.1.
struct BoundedTextEdits(Vec<lsp_types::TextEdit>);

impl<'de> Deserialize<'de> for BoundedTextEdits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVisitor;

        impl<'de> Visitor<'de> for BoundedVisitor {
            type Value = BoundedTextEdits;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of LSP TextEdit objects")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut edits = Vec::new();
                while edits.len() < MAX_LOCATIONS_PER_MESSAGE {
                    match seq.next_element::<lsp_types::TextEdit>()? {
                        Some(e) => edits.push(e),
                        None => return Ok(BoundedTextEdits(edits)),
                    }
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BoundedTextEdits(edits))
            }
        }

        deserializer.deserialize_seq(BoundedVisitor)
    }
}

/// Same bounding discipline as `BoundedLocations`, applied to
/// `textDocument/documentSymbol`'s hierarchical response shape
/// (`lsp_types::DocumentSymbol`) instead of `lsp_types::Location`. Bounds
/// only the top-level array length -- `flatten_document_symbols` bounds
/// the total count *after* recursing into `children`, since a single
/// top-level entry's nested tree isn't visible to this Visitor at all
/// (`docs/features/search-everywhere.md` §2.3, §4).
struct BoundedDocumentSymbols(Vec<lsp_types::DocumentSymbol>);

impl<'de> Deserialize<'de> for BoundedDocumentSymbols {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVisitor;

        impl<'de> Visitor<'de> for BoundedVisitor {
            type Value = BoundedDocumentSymbols;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of LSP DocumentSymbol objects")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut symbols = Vec::new();
                while symbols.len() < MAX_SYMBOLS_PER_MESSAGE {
                    match seq.next_element::<lsp_types::DocumentSymbol>()? {
                        Some(s) => symbols.push(s),
                        None => return Ok(BoundedDocumentSymbols(symbols)),
                    }
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BoundedDocumentSymbols(symbols))
            }
        }

        deserializer.deserialize_seq(BoundedVisitor)
    }
}

/// Same bounding discipline as `BoundedLocations`, applied to a flat
/// symbol-array response -- covers both `textDocument/documentSymbol`'s
/// `SymbolInformation[]` fallback shape and `workspace/symbol`'s
/// `SymbolInformation[] | WorkspaceSymbol[]` response. Every element is
/// deserialized as `lsp_types::WorkspaceSymbol` rather than needing to
/// disambiguate the two shapes: `WorkspaceSymbol.location` is
/// `OneOf<Location, WorkspaceLocation>`, which structurally accepts a
/// plain `SymbolInformation`-shaped `{ uri, range }` location object via
/// its `Location` variant, and every other `SymbolInformation` field
/// `WorkspaceSymbol` lacks (`deprecated`) is simply ignored rather than
/// rejected -- so a `SymbolInformation[]` response parses successfully as
/// `Vec<lsp_types::WorkspaceSymbol>` without a second attempt
/// (`docs/features/search-everywhere.md` §2.3).
struct BoundedWorkspaceSymbols(Vec<lsp_types::WorkspaceSymbol>);

impl<'de> Deserialize<'de> for BoundedWorkspaceSymbols {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVisitor;

        impl<'de> Visitor<'de> for BoundedVisitor {
            type Value = BoundedWorkspaceSymbols;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of LSP SymbolInformation or WorkspaceSymbol objects")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut symbols = Vec::new();
                while symbols.len() < MAX_SYMBOLS_PER_MESSAGE {
                    match seq.next_element::<lsp_types::WorkspaceSymbol>()? {
                        Some(s) => symbols.push(s),
                        None => return Ok(BoundedWorkspaceSymbols(symbols)),
                    }
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BoundedWorkspaceSymbols(symbols))
            }
        }

        deserializer.deserialize_seq(BoundedVisitor)
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_incoming(
    bytes: &[u8],
    project_root: &Path,
    ready: &mut bool,
    stdin: &mut ChildStdin,
    pending: &mut Vec<LspRequest>,
    state: &mut ConnectionState,
    pending_diagnostics: &mut HashMap<PathBuf, Vec<Diagnostic>>,
    event_tx: &Sender<LspEvent>,
) -> Result<(), LspError> {
    let message: IncomingMessage = serde_json::from_slice(bytes)
        .map_err(|e| LspError::Protocol(format!("invalid JSON body: {e}")))?;
    let id = message.id.as_ref().and_then(Value::as_u64);
    let method = message.method.as_ref().and_then(Value::as_str);

    if !*ready {
        if id == Some(INITIALIZE_ID) {
            if let Some(error) = &message.error {
                let msg = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("rust-analyzer rejected initialize")
                    .to_string();
                return Err(LspError::Protocol(msg));
            }
            let init_caps = message
                .result
                .and_then(|r| serde_json::from_str::<InitializeResultCapabilities>(r.get()).ok());
            state.code_action_resolve_provider = init_caps
                .as_ref()
                .map(InitializeResultCapabilities::resolve_provider)
                .unwrap_or(false);
            state.document_formatting_provider = init_caps
                .as_ref()
                .map(InitializeResultCapabilities::document_formatting_provider)
                .unwrap_or(false);
            state.document_range_formatting_provider = init_caps
                .as_ref()
                .map(InitializeResultCapabilities::document_range_formatting_provider)
                .unwrap_or(false);
            state.rename_provider = init_caps
                .as_ref()
                .map(InitializeResultCapabilities::rename_provider)
                .unwrap_or(false);
            state.prepare_rename_provider = init_caps
                .as_ref()
                .map(InitializeResultCapabilities::prepare_rename_provider)
                .unwrap_or(false);
            state.semantic_tokens_provider = init_caps
                .as_ref()
                .map(InitializeResultCapabilities::semantic_tokens_full_provider)
                .unwrap_or(false);
            state.semantic_token_legend = init_caps
                .as_ref()
                .map(InitializeResultCapabilities::semantic_tokens_legend)
                .unwrap_or_default();
            let _ = write_message(stdin, &encode_notification("initialized", json!({}))).await;
            *ready = true;
            for request in pending.drain(..) {
                let _ = send_request(stdin, project_root, request, state, event_tx).await;
            }
        }
        // Anything else received before `initialize` completes (e.g. a
        // server->client request like `window/workDoneProgress/create`)
        // is well-formed but not part of v1's scope — ignored, not fatal.
        return Ok(());
    }

    // A server-initiated *request* carries both an `id` (it wants a
    // reply) and a `method` (unlike one of our own responses arriving
    // back). The first such method this client has ever handled --
    // `docs/features/code-actions.md` §3.5. Checked before the
    // response-dispatch branch below since both would otherwise see
    // `id.is_some()`.
    if id.is_some() && method == Some("workspace/applyEdit") {
        let (applied, edit, label) = match message
            .params
            .and_then(|p| serde_json::from_value::<ApplyEditParams>(p).ok())
        {
            Some(params) => match convert_workspace_edit(project_root, params.edit) {
                Some(edit) => (true, Some(edit), params.label),
                None => (false, None, params.label),
            },
            None => (false, None, None),
        };
        let response = if applied {
            json!({ "applied": true })
        } else {
            json!({ "applied": false, "failureReason": "edit rejected by client" })
        };
        // `message.id` is required to exist here (the `id.is_some()`
        // guard above), so this `unwrap_or` branch is unreachable in
        // practice -- `Value::Null` is a harmless, spec-legal fallback
        // rather than panicking on an input this client already validated.
        let response_id = message.id.clone().unwrap_or(Value::Null);
        let _ = write_message(stdin, &encode_response(response_id, response)).await;
        if applied {
            let _ = event_tx
                .send(LspEvent::WorkspaceEditReady { edit, label })
                .await;
        }
        return Ok(());
    }

    // A legitimate JSON-RPC *response* never carries a `"method"` field --
    // only requests and notifications do. Gating on that too (not just
    // "has an id") matters because a malformed/adversarial notification
    // could otherwise carry a spurious numeric `"id"` and get silently
    // swallowed here before the method-based dispatch below ever runs,
    // even though it's shaped like (and should be handled as) e.g. a
    // `publishDiagnostics` notification.
    if method.is_none() && id.is_some() {
        // `OrganizeImports`'s initial query is special-cased ahead of the
        // `or_else` chain below: unlike every other response handler
        // there, it can react to a match by sending a brand-new outgoing
        // `codeAction/resolve` request (async, needs `stdin`) instead of
        // just producing an `LspEvent` -- see
        // `docs/features/code-generation.md` §3.4.
        if id == state.pending_organize_imports_id {
            state.pending_organize_imports_id = None;
            match parse_organize_imports_response(
                &message,
                project_root,
                state.code_action_resolve_provider,
            ) {
                OrganizeImportsOutcome::Empty => {
                    let _ = event_tx
                        .send(LspEvent::WorkspaceEditReady {
                            edit: None,
                            label: None,
                        })
                        .await;
                }
                OrganizeImportsOutcome::Ready(edit) => {
                    let label = edit.as_ref().map(|_| "Optimize Imports".to_string());
                    let _ = event_tx
                        .send(LspEvent::WorkspaceEditReady { edit, label })
                        .await;
                }
                OrganizeImportsOutcome::NeedsResolve(raw) => {
                    let resolve_id = state.allocate_request_id();
                    state.pending_organize_imports_resolve_id = Some(resolve_id);
                    let _ = write_message(
                        stdin,
                        &encode_request(resolve_id, "codeAction/resolve", raw),
                    )
                    .await;
                }
            }
            return Ok(());
        }
        // An id that doesn't match anything we're tracking is either a
        // stale/superseded response or an unhandled server->client
        // request -- v1 sends no other kind of id-bearing request and
        // handles no server->client requests, so there's nothing else to
        // dispatch an id-bearing message to either way. Every request kind
        // allocates from the same monotonic counter, so an id can never
        // match more than one -- trying them in sequence is safe, not a
        // race.
        let event = handle_references_response(&message, project_root, state)
            .or_else(|| handle_goto_response(&message, project_root, state))
            .or_else(|| handle_hover_response(&message, state))
            .or_else(|| handle_document_highlight_response(&message, state))
            .or_else(|| handle_inlay_hint_response(&message, state))
            .or_else(|| handle_code_action_response(&message, state))
            .or_else(|| handle_resolve_response(&message, project_root, state))
            .or_else(|| handle_document_symbol_response(&message, project_root, state))
            .or_else(|| handle_workspace_symbol_response(&message, project_root, state))
            .or_else(|| handle_format_response(&message, state))
            .or_else(|| handle_prepare_rename_response(&message, state))
            .or_else(|| handle_rename_response(&message, project_root, state))
            .or_else(|| handle_organize_imports_resolve_response(&message, project_root, state))
            .or_else(|| handle_semantic_tokens_response(&message, state));
        if let Some(event) = event {
            let _ = event_tx.send(event).await;
        }
        return Ok(());
    }

    if method == Some("textDocument/publishDiagnostics") {
        let Some(params) = message.params else {
            return Ok(());
        };
        // Truncate on the untyped `Value` before the typed deserialize
        // below, not after: `serde_json::from_value::<PublishDiagnosticsParams>`
        // allocates a fully-typed `lsp_types::Diagnostic` (several owned
        // `String`/`Option<String>` fields each) per entry, which is the
        // more expensive of the two parse stages -- doing that for a
        // malicious server's untruncated diagnostics count defeats the
        // point of `MAX_DIAGNOSTICS_PER_MESSAGE` even though the final
        // retained result would still end up capped. (`params` doesn't get
        // the `RawValue`-deferred treatment `result` does above, since
        // `Diagnostic`'s typed representation is heavier than its untyped
        // `Value` form -- this stage is where the diagnostics cost
        // actually lives, unlike the lightweight `Location`.)
        let params = truncate_diagnostics_array(params);
        let Ok(params) = serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(params)
        else {
            return Ok(());
        };
        let Ok(url) = Url::parse(params.uri.as_str()) else {
            return Ok(());
        };
        let Ok(path) = url.to_file_path() else {
            return Ok(());
        };
        let Some(validated) = validate_path(project_root, &path) else {
            return Ok(());
        };
        let diagnostics = limit_diagnostics(params.diagnostics);
        pending_diagnostics.insert(validated, diagnostics);
        flush_pending_diagnostics(pending_diagnostics, event_tx);
    }
    // Any other well-formed method (server->client requests, unrelated
    // notifications) is outside v1's diagnostics-only scope — ignored.
    Ok(())
}

async fn send_request(
    stdin: &mut ChildStdin,
    project_root: &Path,
    request: LspRequest,
    state: &mut ConnectionState,
    event_tx: &Sender<LspEvent>,
) -> io::Result<()> {
    match request {
        LspRequest::DidOpen { path, text } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            state.versions.insert(validated, 0);
            let params = json!({
                "textDocument": {
                    "uri": uri.to_string(),
                    "languageId": "rust",
                    "version": 0,
                    "text": text,
                }
            });
            write_message(stdin, &encode_notification("textDocument/didOpen", params)).await
        }
        LspRequest::DidChange { path, text } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            let version = state.versions.entry(validated).or_insert(0);
            *version += 1;
            let params = json!({
                "textDocument": {
                    "uri": uri.to_string(),
                    "version": *version,
                },
                "contentChanges": [{ "text": text }],
            });
            write_message(
                stdin,
                &encode_notification("textDocument/didChange", params),
            )
            .await
        }
        LspRequest::DidClose { path } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            state.versions.remove(&validated);
            let params = json!({ "textDocument": { "uri": uri.to_string() } });
            write_message(stdin, &encode_notification("textDocument/didClose", params)).await
        }
        LspRequest::References { path, position } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_references_id = Some(id);
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
                "position": { "line": position.line, "character": position.character },
                "context": { "includeDeclaration": true },
            });
            write_message(
                stdin,
                &encode_request(id, "textDocument/references", params),
            )
            .await
        }
        LspRequest::Goto {
            kind,
            path,
            position,
        } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_goto_id = Some(id);
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
                "position": { "line": position.line, "character": position.character },
            });
            write_message(stdin, &encode_request(id, goto_method(kind), params)).await
        }
        LspRequest::Hover { path, position } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_hover_id = Some(id);
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
                "position": { "line": position.line, "character": position.character },
            });
            write_message(stdin, &encode_request(id, "textDocument/hover", params)).await
        }
        LspRequest::DocumentHighlight { path, position } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_document_highlight_id = Some(id);
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
                "position": { "line": position.line, "character": position.character },
            });
            write_message(
                stdin,
                &encode_request(id, "textDocument/documentHighlight", params),
            )
            .await
        }
        LspRequest::InlayHint { path, range } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_inlay_hint = Some((id, validated));
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
                "range": {
                    "start": { "line": range.start.line, "character": range.start.character },
                    "end": { "line": range.end.line, "character": range.end.character },
                },
            });
            write_message(stdin, &encode_request(id, "textDocument/inlayHint", params)).await
        }
        LspRequest::CodeAction { path, position } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_code_action_id = Some((id, validated));
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
                "range": {
                    "start": { "line": position.line, "character": position.character },
                    "end": { "line": position.line, "character": position.character },
                },
                "context": { "diagnostics": [] },
            });
            write_message(
                stdin,
                &encode_request(id, "textDocument/codeAction", params),
            )
            .await
        }
        // `docs/features/code-actions.md` §3.3's four-way branch: not
        // found, has an edit already, needs resolving, or unsupported.
        // Only the "needs resolving" case sends anything over the wire --
        // the other three settle by emitting an event directly, since
        // there's nothing to wait for.
        LspRequest::ApplyCodeAction { index } => {
            let Some(action) = state.last_code_actions.get(index) else {
                let _ = event_tx
                    .send(LspEvent::WorkspaceEditReady {
                        edit: None,
                        label: None,
                    })
                    .await;
                return Ok(());
            };
            if let Some(edit) = action.edit.clone() {
                let label = action_title(&action.raw);
                let converted = convert_workspace_edit(project_root, edit);
                let _ = event_tx
                    .send(LspEvent::WorkspaceEditReady {
                        edit: converted,
                        label,
                    })
                    .await;
                return Ok(());
            }
            if !action.resolvable || !state.code_action_resolve_provider {
                let _ = event_tx
                    .send(LspEvent::WorkspaceEditReady {
                        edit: None,
                        label: None,
                    })
                    .await;
                return Ok(());
            }
            // Cloned out before the `&mut state` calls below -- `action`
            // borrows `state.last_code_actions` and can't stay alive
            // across a mutable borrow of `state` itself.
            let raw = action.raw.clone();
            let id = state.allocate_request_id();
            state.pending_resolve_id = Some(id);
            write_message(stdin, &encode_request(id, "codeAction/resolve", raw)).await
        }
        // Whole-document range, `context.only` scoped to
        // `source.organizeImports` -- unlike `CodeAction` above, this
        // never reads or writes `state.last_code_actions`, the
        // caret-position ambient cache (`docs/features/code-generation.md`
        // §3.4).
        LspRequest::OrganizeImports { path } => {
            let Some(validated) = validate_path(project_root, &path) else {
                let _ = event_tx
                    .send(LspEvent::WorkspaceEditReady {
                        edit: None,
                        label: None,
                    })
                    .await;
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                let _ = event_tx
                    .send(LspEvent::WorkspaceEditReady {
                        edit: None,
                        label: None,
                    })
                    .await;
                return Ok(());
            };
            // The one place in this request that needs the document's
            // extent -- every other request kind either takes a caller-
            // supplied `Range`/`Position` or needs none at all.
            let Ok(text) = fs::read_to_string(&validated) else {
                let _ = event_tx
                    .send(LspEvent::WorkspaceEditReady {
                        edit: None,
                        label: None,
                    })
                    .await;
                return Ok(());
            };
            let end =
                crate::position::byte_offset_to_position(&text, text.len()).unwrap_or(Position {
                    line: 0,
                    character: 0,
                });
            let id = state.allocate_request_id();
            state.pending_organize_imports_id = Some(id);
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": end.line, "character": end.character },
                },
                "context": { "only": ["source.organizeImports"], "diagnostics": [] },
            });
            write_message(
                stdin,
                &encode_request(id, "textDocument/codeAction", params),
            )
            .await
        }
        LspRequest::DocumentSymbol { path } => {
            let Some(validated) = validate_path(project_root, &path) else {
                return Ok(());
            };
            let Ok(uri) = Url::from_file_path(&validated) else {
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_document_symbol_id = Some((id, validated));
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
            });
            write_message(
                stdin,
                &encode_request(id, "textDocument/documentSymbol", params),
            )
            .await
        }
        LspRequest::WorkspaceSymbol { query } => {
            let id = state.allocate_request_id();
            state.pending_workspace_symbol_id = Some(id);
            let params = json!({ "query": query });
            write_message(stdin, &encode_request(id, "workspace/symbol", params)).await
        }
        LspRequest::Format {
            path,
            tab_size,
            insert_spaces,
        } => {
            send_format_request(
                stdin,
                project_root,
                FormatRequestArgs {
                    path,
                    range: None,
                    tab_size,
                    insert_spaces,
                },
                state,
                event_tx,
            )
            .await
        }
        LspRequest::FormatRange {
            path,
            range,
            tab_size,
            insert_spaces,
        } => {
            send_format_request(
                stdin,
                project_root,
                FormatRequestArgs {
                    path,
                    range: Some(range),
                    tab_size,
                    insert_spaces,
                },
                state,
                event_tx,
            )
            .await
        }
        // §2.1's "not a negative signal" rule: a path-validation failure
        // or an unsupported capability both settle as `renameable: true`,
        // never `false` -- only an explicit server answer says `false`
        // (`docs/features/rename-refactoring.md` §2.2, §3.2, §4).
        LspRequest::PrepareRename { path, position } => {
            let Some(validated) = validate_path(project_root, &path) else {
                let _ = event_tx
                    .send(LspEvent::PrepareRenameReady {
                        path,
                        renameable: true,
                    })
                    .await;
                return Ok(());
            };
            if !state.prepare_rename_provider {
                let _ = event_tx
                    .send(LspEvent::PrepareRenameReady {
                        path: validated,
                        renameable: true,
                    })
                    .await;
                return Ok(());
            }
            let Ok(uri) = Url::from_file_path(&validated) else {
                let _ = event_tx
                    .send(LspEvent::PrepareRenameReady {
                        path: validated,
                        renameable: true,
                    })
                    .await;
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_prepare_rename = Some((id, validated));
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
                "position": { "line": position.line, "character": position.character },
            });
            write_message(
                stdin,
                &encode_request(id, "textDocument/prepareRename", params),
            )
            .await
        }
        LspRequest::Rename {
            path,
            position,
            new_name,
        } => {
            let Some(validated) = validate_path(project_root, &path) else {
                let _ = event_tx
                    .send(LspEvent::RenameReady {
                        path,
                        new_name,
                        edit: None,
                    })
                    .await;
                return Ok(());
            };
            if !state.rename_provider {
                let _ = event_tx
                    .send(LspEvent::RenameReady {
                        path: validated,
                        new_name,
                        edit: None,
                    })
                    .await;
                return Ok(());
            }
            let Ok(uri) = Url::from_file_path(&validated) else {
                let _ = event_tx
                    .send(LspEvent::RenameReady {
                        path: validated,
                        new_name,
                        edit: None,
                    })
                    .await;
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_rename = Some((id, validated, new_name.clone()));
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
                "position": { "line": position.line, "character": position.character },
                "newName": new_name,
            });
            write_message(stdin, &encode_request(id, "textDocument/rename", params)).await
        }
        LspRequest::SemanticTokensFull { path } => {
            let Some(validated) = validate_path(project_root, &path) else {
                let _ = event_tx
                    .send(LspEvent::SemanticTokens {
                        path,
                        tokens: Vec::new(),
                    })
                    .await;
                return Ok(());
            };
            if !state.semantic_tokens_provider {
                let _ = event_tx
                    .send(LspEvent::SemanticTokens {
                        path: validated,
                        tokens: Vec::new(),
                    })
                    .await;
                return Ok(());
            }
            let Ok(uri) = Url::from_file_path(&validated) else {
                let _ = event_tx
                    .send(LspEvent::SemanticTokens {
                        path: validated,
                        tokens: Vec::new(),
                    })
                    .await;
                return Ok(());
            };
            let id = state.allocate_request_id();
            state.pending_semantic_tokens = Some((id, validated));
            let params = json!({
                "textDocument": { "uri": uri.to_string() },
            });
            write_message(
                stdin,
                &encode_request(id, "textDocument/semanticTokens/full", params),
            )
            .await
        }
    }
}

/// Bundles `Format`/`FormatRange`'s shared fields into one argument for
/// `send_format_request`, keeping its parameter count within clippy's
/// `too_many_arguments` threshold -- no meaning beyond that.
struct FormatRequestArgs {
    path: PathBuf,
    range: Option<Range>,
    tab_size: u32,
    insert_spaces: bool,
}

/// Shared by `Format`/`FormatRange` -- both answer into the same
/// `FormatReady` channel and share `pending_format`
/// (`docs/features/formatting.md` §2.1). `range: None` sends
/// `textDocument/formatting`; `Some` sends `textDocument/
/// rangeFormatting`. Always answered by exactly one `FormatReady`, per
/// the doc's §3.3: a path-validation failure or an unadvertised
/// capability settle immediately with no wire traffic, since `ide-ui`'s
/// Format on Save (`docs/features/formatting.md` §3.4) depends on this
/// request kind never leaving a caller waiting forever, unlike every
/// other query kind's plain silent-drop-on-invalid-path precedent.
async fn send_format_request(
    stdin: &mut ChildStdin,
    project_root: &Path,
    args: FormatRequestArgs,
    state: &mut ConnectionState,
    event_tx: &Sender<LspEvent>,
) -> io::Result<()> {
    let FormatRequestArgs {
        path,
        range,
        tab_size,
        insert_spaces,
    } = args;
    let Some(validated) = validate_path(project_root, &path) else {
        let _ = event_tx
            .send(LspEvent::FormatReady { path, edit: None })
            .await;
        return Ok(());
    };
    let supported = if range.is_some() {
        state.document_range_formatting_provider
    } else {
        state.document_formatting_provider
    };
    if !supported {
        let _ = event_tx
            .send(LspEvent::FormatReady {
                path: validated,
                edit: None,
            })
            .await;
        return Ok(());
    }
    let Ok(uri) = Url::from_file_path(&validated) else {
        let _ = event_tx
            .send(LspEvent::FormatReady {
                path: validated,
                edit: None,
            })
            .await;
        return Ok(());
    };
    let id = state.allocate_request_id();
    state.pending_format = Some((id, validated));
    let mut params = json!({
        "textDocument": { "uri": uri.to_string() },
        "options": { "tabSize": tab_size, "insertSpaces": insert_spaces },
    });
    let method = match range {
        Some(range) => {
            params["range"] = json!({
                "start": { "line": range.start.line, "character": range.start.character },
                "end": { "line": range.end.line, "character": range.end.character },
            });
            "textDocument/rangeFormatting"
        }
        None => "textDocument/formatting",
    };
    write_message(stdin, &encode_request(id, method, params)).await
}

/// Best-effort `title` out of a cached raw `Command`/`CodeAction` JSON
/// value, for `WorkspaceEditReady.label` -- `None` only if the server's
/// own entry was missing it, which validation earlier already treated
/// permissively rather than rejecting the whole response.
fn action_title(raw: &Value) -> Option<String> {
    raw.get("title")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

/// The LSP method name for `kind` -- all three share the same request
/// (`textDocument`+`position`, no `context`) and response
/// (`Location | Location[] | null`, see `parse_goto_result`) shape, so
/// only the method string differs between them.
fn goto_method(kind: GotoKind) -> &'static str {
    match kind {
        GotoKind::Definition => "textDocument/definition",
        GotoKind::TypeDefinition => "textDocument/typeDefinition",
        GotoKind::Implementation => "textDocument/implementation",
    }
}

/// Returns the `LspEvent::References` to emit if `message`'s `id` matches
/// `state.pending_references_id` (clearing it in that case) -- `None` for
/// a stale/superseded or otherwise unrelated id-bearing message, which is
/// dropped without emitting an event (see `docs/features/find-usages.md`
/// §3/§4's supersede-by-overwrite correlation). Pure and synchronous
/// (no subprocess I/O) so it's unit-testable without a live connection.
fn handle_references_response(
    message: &IncomingMessage,
    project_root: &Path,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    if Some(id) != state.pending_references_id {
        return None;
    }
    state.pending_references_id = None;
    // Permissive on purpose (see docs/features/find-usages.md §3/§4): a
    // `null`/empty/unparseable `result` all still produce an (empty)
    // event rather than being dropped -- unlike a notification nobody's
    // synchronously waiting on, something must always clear a waiting
    // UI's "finding usages" state.
    Some(LspEvent::References {
        locations: parse_references_result(message, project_root),
    })
}

/// Parses a `textDocument/references` response permissively (see
/// `docs/features/find-usages.md` §3/§4): a JSON-RPC error, a `null`
/// result, or a result this client can't deserialize as
/// `BoundedLocations` all become an empty list rather than being treated
/// as fatal or dropped without a reply. Individual entries whose URI
/// doesn't convert to a path, or whose path fails `validate_path` against
/// `project_root`, are skipped without discarding the rest. `result` is
/// deserialized directly from its raw JSON text (see `IncomingMessage`)
/// via `BoundedLocations`, so an oversized array never gets materialized
/// as a `Value` tree or a `Vec<lsp_types::Location>` in the first place --
/// truncating an already-fully-parsed value (round 1's approach) turned
/// out not to bound memory meaningfully, since the untyped parse was
/// already the dominant cost by then.
fn parse_references_result(message: &IncomingMessage, project_root: &Path) -> Vec<Location> {
    if message.error.is_some() {
        return Vec::new();
    }
    let Some(result) = message.result else {
        return Vec::new();
    };
    let Ok(bounded) = serde_json::from_str::<BoundedLocations>(result.get()) else {
        return Vec::new();
    };
    bounded
        .0
        .into_iter()
        .filter_map(|loc| convert_location(loc, project_root))
        .collect()
}

/// Returns the `LspEvent::Goto` to emit if `message`'s `id` matches
/// `state.pending_goto_id` (clearing it in that case) -- `None` for a
/// stale/superseded or otherwise unrelated id-bearing message, same shape
/// as `handle_references_response` (`docs/features/goto-definition.md`
/// §3/§4).
fn handle_goto_response(
    message: &IncomingMessage,
    project_root: &Path,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    if Some(id) != state.pending_goto_id {
        return None;
    }
    state.pending_goto_id = None;
    Some(LspEvent::Goto {
        locations: parse_goto_result(message, project_root),
    })
}

/// Parses a `textDocument/definition`/`typeDefinition`/`implementation`
/// response permissively, same rationale as `parse_references_result`
/// (`docs/features/goto-definition.md` §3/§4): a JSON-RPC error or an
/// unparseable `result` becomes an empty list rather than being treated as
/// fatal or dropped without a reply. Unlike references, the result isn't
/// always an array -- since this client's `initialize` declares no
/// `textDocument.{definition,typeDefinition,implementation}.linkSupport`,
/// the LSP spec guarantees the server answers with `Location | Location[]
/// | null`, never `LocationLink[]` (§4), so the only shape this needs to
/// disambiguate is scalar-object-or-array: a `result` whose first
/// non-whitespace byte is `[` is parsed as a `MAX_LOCATIONS_PER_MESSAGE`-
/// bounded array via `BoundedLocations` (shared with references, not
/// duplicated); anything else -- a single `Location` object, or `null` --
/// is parsed as `Option<lsp_types::Location>`, so `null` and a malformed
/// scalar both fall out as `Vec::new()` on the same path rather than
/// needing a separate `null` check first. Individual/the one entry's URI-
/// to-path conversion and `project_root` validation reuse
/// `convert_location`, identically to references.
fn parse_goto_result(message: &IncomingMessage, project_root: &Path) -> Vec<Location> {
    if message.error.is_some() {
        return Vec::new();
    }
    let Some(result) = message.result else {
        return Vec::new();
    };
    let text = result.get();
    if text.trim_start().starts_with('[') {
        let Ok(bounded) = serde_json::from_str::<BoundedLocations>(text) else {
            return Vec::new();
        };
        bounded
            .0
            .into_iter()
            .filter_map(|loc| convert_location(loc, project_root))
            .collect()
    } else {
        match serde_json::from_str::<Option<lsp_types::Location>>(text) {
            Ok(Some(loc)) => convert_location(loc, project_root).into_iter().collect(),
            _ => Vec::new(),
        }
    }
}

fn convert_location(loc: lsp_types::Location, project_root: &Path) -> Option<Location> {
    let url = Url::parse(loc.uri.as_str()).ok()?;
    let path = url.to_file_path().ok()?;
    let validated = validate_path(project_root, &path)?;
    Some(Location {
        path: validated,
        range: Range {
            start: Position {
                line: loc.range.start.line,
                character: loc.range.start.character,
            },
            end: Position {
                line: loc.range.end.line,
                character: loc.range.end.character,
            },
        },
    })
}

fn convert_position(p: lsp_types::Position) -> Position {
    Position {
        line: p.line,
        character: p.character,
    }
}

fn convert_range(r: lsp_types::Range) -> Range {
    Range {
        start: convert_position(r.start),
        end: convert_position(r.end),
    }
}

/// Returns the `LspEvent::Hover` to emit if `message`'s `id` matches
/// `state.pending_hover_id` (clearing it in that case) -- `None` for a
/// stale/superseded or otherwise unrelated id-bearing message, same shape
/// as `handle_references_response`/`handle_goto_response`
/// (`docs/features/inlay-hints-and-hover.md` §3.2). Unlike those, a
/// `Hover` response never carries a path, so `project_root` isn't needed
/// here at all.
fn handle_hover_response(
    message: &IncomingMessage,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    if Some(id) != state.pending_hover_id {
        return None;
    }
    state.pending_hover_id = None;
    Some(LspEvent::Hover {
        contents: parse_hover_result(message),
    })
}

/// Parses a `textDocument/hover` response permissively, same rationale as
/// `parse_references_result`/`parse_goto_result`
/// (`docs/features/inlay-hints-and-hover.md` §3.2, §3.3): a JSON-RPC
/// error, a `null` result, or contents this client can't flatten to text
/// all become `None` rather than being treated as fatal or dropped
/// without a reply. `range` (`result.range`) is intentionally never read
/// (§1 of that doc). `contents` (`lsp_types::HoverContents`) is flattened
/// to plain text -- `Markup(MarkupContent { value, .. })` uses `value`
/// as-is (no markdown parsing, a deliberate security property, §3.3/§4);
/// `Scalar(MarkedString)`/`Array(Vec<MarkedString>)` each flatten to that
/// `MarkedString`'s plain string (`String(s) => s`,
/// `LanguageString(ls) => ls.value`, `.language` ignored), joined with a
/// blank line between array entries.
fn parse_hover_result(message: &IncomingMessage) -> Option<String> {
    if message.error.is_some() {
        return None;
    }
    let result = message.result?;
    let hover = serde_json::from_str::<Option<lsp_types::Hover>>(result.get()).ok()??;
    Some(flatten_hover_contents(hover.contents))
}

fn flatten_hover_contents(contents: lsp_types::HoverContents) -> String {
    match contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        lsp_types::HoverContents::Scalar(m) => flatten_marked_string(m),
        lsp_types::HoverContents::Array(parts) => parts
            .into_iter()
            .map(flatten_marked_string)
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn flatten_marked_string(m: lsp_types::MarkedString) -> String {
    match m {
        lsp_types::MarkedString::String(s) => s,
        lsp_types::MarkedString::LanguageString(ls) => ls.value,
    }
}

/// Returns the `LspEvent::DocumentHighlight` to emit if `message`'s `id`
/// matches `state.pending_document_highlight_id` (clearing it in that
/// case) -- `None` for a stale/superseded or otherwise unrelated
/// id-bearing message, same shape as `handle_references_response`
/// (`docs/features/inlay-hints-and-hover.md` §3.2).
fn handle_document_highlight_response(
    message: &IncomingMessage,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    if Some(id) != state.pending_document_highlight_id {
        return None;
    }
    state.pending_document_highlight_id = None;
    Some(LspEvent::DocumentHighlight {
        ranges: parse_document_highlight_result(message),
    })
}

/// Parses a `textDocument/documentHighlight` response permissively, same
/// rationale as `parse_references_result`
/// (`docs/features/inlay-hints-and-hover.md` §3.2): a JSON-RPC error, a
/// `null` result, or an unparseable result all become an empty list.
/// Deserialized via `BoundedDocumentHighlights` (bounded the same way
/// `BoundedLocations` bounds `References`/`Goto`'s array responses);
/// `.kind` is discarded, only `.range` is kept -- a `DocumentHighlight`
/// entry has no path of its own to validate (it's always within the file
/// the request already named and validated), unlike a `Location`.
fn parse_document_highlight_result(message: &IncomingMessage) -> Vec<Range> {
    if message.error.is_some() {
        return Vec::new();
    }
    let Some(result) = message.result else {
        return Vec::new();
    };
    let Ok(bounded) = serde_json::from_str::<BoundedDocumentHighlights>(result.get()) else {
        return Vec::new();
    };
    bounded
        .0
        .into_iter()
        .map(|h| convert_range(h.range))
        .collect()
}

/// Returns the `LspEvent::InlayHint` to emit if `message`'s `id` matches
/// `state.pending_inlay_hint`'s id (clearing the slot in that case) --
/// `None` for a stale/superseded or otherwise unrelated id-bearing
/// message. Unlike every other `handle_*_response` in this file, the
/// emitted event needs `path` -- taken from the same slot the id was
/// matched against, since `state.pending_inlay_hint` stores both together
/// (`docs/features/inlay-hints-and-hover.md` §2.1, §4: `ide-ui` keeps
/// inlay hints in a per-file map, so the event must name which file this
/// snapshot belongs to).
fn handle_inlay_hint_response(
    message: &IncomingMessage,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    let (pending_id, path) = state.pending_inlay_hint.clone()?;
    if id != pending_id {
        return None;
    }
    state.pending_inlay_hint = None;
    Some(LspEvent::InlayHint {
        path,
        hints: parse_inlay_hint_result(message),
    })
}

/// Parses a `textDocument/inlayHint` response permissively, same
/// rationale as `parse_references_result`
/// (`docs/features/inlay-hints-and-hover.md` §3.2): a JSON-RPC error, a
/// `null` result, or an unparseable result all become an empty list.
/// Deserialized via `BoundedInlayHints`. Each `lsp_types::InlayHint`
/// converts to `ide_lsp::InlayHint`: `label` flattens
/// `InlayHintLabel::String(s) => s` directly,
/// `LabelParts(parts) => parts` each part's `.value` concatenated
/// (`.tooltip`/`.location`/`.command` are never read -- §1 of that doc);
/// `padding_left`/`padding_right` default to `false` when absent
/// (`Option<bool>` -> `bool`, matching the LSP spec's own "omitted means
/// false" note). No per-entry path validation -- an `InlayHint` carries a
/// `position`, not a URI; it's implicitly within the file the request
/// already named and validated, unlike a `Location`.
fn parse_inlay_hint_result(message: &IncomingMessage) -> Vec<InlayHint> {
    if message.error.is_some() {
        return Vec::new();
    }
    let Some(result) = message.result else {
        return Vec::new();
    };
    let Ok(bounded) = serde_json::from_str::<BoundedInlayHints>(result.get()) else {
        return Vec::new();
    };
    bounded.0.into_iter().map(convert_inlay_hint).collect()
}

fn convert_inlay_hint(hint: lsp_types::InlayHint) -> InlayHint {
    let label = match hint.label {
        lsp_types::InlayHintLabel::String(s) => s,
        lsp_types::InlayHintLabel::LabelParts(parts) => {
            parts.into_iter().map(|p| p.value).collect::<String>()
        }
    };
    InlayHint {
        position: convert_position(hint.position),
        label,
        padding_left: hint.padding_left.unwrap_or(false),
        padding_right: hint.padding_right.unwrap_or(false),
    }
}

/// Returns the `LspEvent::SemanticTokens` to emit if `message`'s `id`
/// matches `state.pending_semantic_tokens`'s id (clearing the slot in that
/// case) -- `None` for a stale/superseded or otherwise unrelated id-bearing
/// message. Same "carries its own path from the pending slot" shape as
/// `handle_inlay_hint_response` (`docs/features/semantic-highlighting.md`
/// §2.2, §3.2).
fn handle_semantic_tokens_response(
    message: &IncomingMessage,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    let (pending_id, path) = state.pending_semantic_tokens.clone()?;
    if id != pending_id {
        return None;
    }
    state.pending_semantic_tokens = None;
    let tokens = parse_semantic_tokens_result(message, &state.semantic_token_legend);
    Some(LspEvent::SemanticTokens { path, tokens })
}

/// Parses a `textDocument/semanticTokens/full` response permissively, same
/// rationale as every other `parse_*_result` in this file
/// (`docs/features/semantic-highlighting.md` §3.2): a JSON-RPC error, a
/// `null` result, or an unparseable result all become an empty list.
/// `SemanticTokensResult::Partial` is unreachable by construction -- this
/// client never sends `partialResultToken`, so only the `data` field is
/// ever read (via `RawSemanticTokensResult`, whose `data` is bounded by
/// `BoundedSemanticTokenData`). Delegates the actual delta-decode to
/// `decode_semantic_tokens`.
fn parse_semantic_tokens_result(
    message: &IncomingMessage,
    legend: &[String],
) -> Vec<SemanticToken> {
    if message.error.is_some() {
        return Vec::new();
    }
    let Some(result) = message.result else {
        return Vec::new();
    };
    let Ok(raw) = serde_json::from_str::<RawSemanticTokensResult>(result.get()) else {
        return Vec::new();
    };
    decode_semantic_tokens(&raw.data.0, legend)
}

/// Decodes the LSP semantic-tokens wire encoding: `raw` is a flat array of
/// `u32`s, read in chunks of 5 -- `(delta_line, delta_start, length,
/// token_type, token_modifiers_bitset)` per token
/// (`docs/features/semantic-highlighting.md` §3.2). A trailing partial
/// chunk (not a multiple of 5) is dropped silently, same permissive
/// convention as every other malformed-input case in this decode.
///
/// Maintains a running `(line, character)` cursor starting at `(0, 0)`:
/// `line += delta_line`; if `delta_line != 0`, `character = delta_start`
/// (a new line resets the column); otherwise `character += delta_start`
/// (same line as the previous token, delta relative to *it*). This cursor
/// update happens for **every** chunk within the bounded array, including
/// ones whose `token_type` doesn't resolve to a `SemanticTokenKind` --
/// skipping a dropped entry's cursor contribution would desynchronize
/// every subsequent token's decoded position (`docs/features/
/// semantic-highlighting.md` §4).
///
/// `token_type` indexes into `legend` (the server's advertised
/// `SemanticTokensLegend.token_types`, captured once at `initialize` time);
/// out-of-bounds indexes, and legend names outside the mapping table
/// below, both drop the entry (cursor still advances, per above).
fn decode_semantic_tokens(raw: &[u32], legend: &[String]) -> Vec<SemanticToken> {
    let mut tokens = Vec::new();
    let mut line = 0u32;
    let mut character = 0u32;
    for chunk in raw.chunks_exact(5) {
        let [delta_line, delta_start, length, token_type, _modifiers] = chunk else {
            unreachable!("chunks_exact(5) always yields length-5 slices");
        };
        // A malicious/buggy server can send an arbitrary delta -- e.g.
        // `delta_line: u32::MAX` on two consecutive tokens overflows a
        // plain `+=`. `saturating_add` degrades that to "this token (and
        // everything after it) pins to the max representable line/column"
        // instead of panicking the background event-loop thread and
        // silently killing the whole LSP connection
        // (`docs/security-findings/rust-lsp-dev-semantic-highlighting-2026-08-25.md`
        // finding 1).
        if *delta_line != 0 {
            line = line.saturating_add(*delta_line);
            character = *delta_start;
        } else {
            line = line.saturating_add(*delta_line);
            character = character.saturating_add(*delta_start);
        }
        let Some(kind) = legend
            .get(*token_type as usize)
            .and_then(|name| map_semantic_token_type(name))
        else {
            continue;
        };
        tokens.push(SemanticToken {
            position: Position { line, character },
            length: *length,
            kind,
        });
    }
    tokens
}

/// The doc's exact §3.2 type-mapping table -- an LSP standard
/// `SemanticTokenType` name (as it appears in a server's legend) to this
/// crate's own smaller `SemanticTokenKind`. Any name outside this table
/// (including `regexp`, or any server-defined non-standard type) maps to
/// `None` -- dropped by the caller, not an error.
fn map_semantic_token_type(name: &str) -> Option<SemanticTokenKind> {
    match name {
        "type" | "class" | "enum" | "interface" | "struct" | "typeParameter" | "namespace" => {
            Some(SemanticTokenKind::Type)
        }
        "function" | "method" => Some(SemanticTokenKind::Function),
        "macro" | "decorator" => Some(SemanticTokenKind::Macro),
        "keyword" | "modifier" => Some(SemanticTokenKind::Keyword),
        "comment" => Some(SemanticTokenKind::Comment),
        "string" => Some(SemanticTokenKind::String),
        "number" => Some(SemanticTokenKind::Number),
        "operator" => Some(SemanticTokenKind::Operator),
        "variable" | "parameter" | "property" | "enumMember" | "event" => {
            Some(SemanticTokenKind::Variable)
        }
        _ => None,
    }
}

/// Returns the `LspEvent::CodeAction` to emit if `message`'s `id` matches
/// `state.pending_code_action_id`'s id (clearing the slot in that case)
/// -- `None` for a stale/superseded or otherwise unrelated id-bearing
/// message. Same "carries its own path from the pending slot" shape as
/// `handle_inlay_hint_response` (`docs/features/code-actions.md` §2.1,
/// §3.2). Builds `state.last_code_actions` (the internal raw cache
/// `ApplyCodeAction` looks `index` up in) in the same pass as the public
/// summaries, replacing it wholesale -- supersede-by-overwrite, same as
/// `document_highlights`.
fn handle_code_action_response(
    message: &IncomingMessage,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    let (pending_id, path) = state.pending_code_action_id.clone()?;
    if id != pending_id {
        return None;
    }
    state.pending_code_action_id = None;
    let (actions, raw) = parse_code_action_result(message, state.code_action_resolve_provider);
    state.last_code_actions = raw;
    Some(LspEvent::CodeAction { path, actions })
}

/// Parses a `textDocument/codeAction` response permissively, same
/// rationale as every other `parse_*_result` in this file
/// (`docs/features/code-actions.md` §3.1): a JSON-RPC error, a `null`
/// result, or an unparseable result all become an empty list. Deserialized
/// via `BoundedCodeActions`. Each entry becomes both a public `CodeAction`
/// summary and an internal `RawCodeAction` (same index in both,
/// `docs/features/code-actions.md` §3.1's three cases): a bare `Command`
/// -> `edit: None, resolvable: false` (nothing to apply, still shown);
/// a `CodeAction` with `.edit` already present -> that edit, cached
/// directly; a `CodeAction` with no `.edit` but `.data` present and the
/// server having declared `resolveProvider` -> `edit: None, resolvable:
/// true` (needs `codeAction/resolve`, §3.3); anything else -> `edit:
/// None, resolvable: false` (unsupported).
fn parse_code_action_result(
    message: &IncomingMessage,
    resolve_provider: bool,
) -> (Vec<CodeAction>, Vec<RawCodeAction>) {
    if message.error.is_some() {
        return (Vec::new(), Vec::new());
    }
    let Some(result) = message.result else {
        return (Vec::new(), Vec::new());
    };
    let Ok(bounded) = serde_json::from_str::<BoundedCodeActions>(result.get()) else {
        return (Vec::new(), Vec::new());
    };
    let mut actions = Vec::with_capacity(bounded.0.len());
    let mut raw = Vec::with_capacity(bounded.0.len());
    for (index, entry) in bounded.0.into_iter().enumerate() {
        let value = serde_json::to_value(&entry).unwrap_or(Value::Null);
        match entry {
            lsp_types::CodeActionOrCommand::Command(command) => {
                actions.push(CodeAction {
                    index,
                    title: command.title,
                    kind: None,
                    is_preferred: false,
                    disabled_reason: None,
                });
                raw.push(RawCodeAction {
                    raw: value,
                    edit: None,
                    resolvable: false,
                });
            }
            lsp_types::CodeActionOrCommand::CodeAction(action) => {
                let resolvable = action.edit.is_none() && action.data.is_some() && resolve_provider;
                actions.push(CodeAction {
                    index,
                    title: action.title,
                    kind: action.kind.map(|k| k.as_str().to_string()),
                    is_preferred: action.is_preferred.unwrap_or(false),
                    disabled_reason: action.disabled.map(|d| d.reason),
                });
                raw.push(RawCodeAction {
                    raw: value,
                    edit: action.edit,
                    resolvable,
                });
            }
        }
    }
    (actions, raw)
}

/// What `OrganizeImports`'s one-shot `textDocument/codeAction` response
/// resolves to, once its first entry (only ever the first --
/// `docs/features/code-generation.md` §3.4 -- there's no menu to pick
/// among the rest) is inspected. Mirrors `ApplyCodeAction`'s own
/// found/has-edit/needs-resolve/unsupported branch, but reached from a
/// fully-automated query rather than a user-selected index.
#[derive(Debug)]
enum OrganizeImportsOutcome {
    /// Empty/error response, a bare `Command` (never resolvable, no
    /// edit), or a `CodeAction` with neither an edit nor resolve data.
    Empty,
    /// The first entry already carried a usable edit -- `None` here means
    /// `convert_workspace_edit` itself rejected it (e.g. a path outside
    /// the project root), which still counts as "nothing to apply".
    Ready(Option<WorkspaceEdit>),
    /// The first entry needs `codeAction/resolve` first -- carries its
    /// own raw JSON (computed before the match below moves any of its
    /// fields, same trick `parse_code_action_result` already uses) to
    /// resend as that request's params.
    NeedsResolve(Value),
}

/// Parses an `OrganizeImports` response permissively, same rationale as
/// `parse_code_action_result` -- a JSON-RPC error, `null` result, or
/// unparseable/empty result all become `OrganizeImportsOutcome::Empty`.
/// Deserialized via `BoundedCodeActions`, the same size-capped type
/// `parse_code_action_result` uses, so a flooding/misbehaving server
/// can't force unbounded parse work here either.
fn parse_organize_imports_response(
    message: &IncomingMessage,
    project_root: &Path,
    resolve_provider: bool,
) -> OrganizeImportsOutcome {
    if message.error.is_some() {
        return OrganizeImportsOutcome::Empty;
    }
    let Some(result) = message.result else {
        return OrganizeImportsOutcome::Empty;
    };
    let Ok(bounded) = serde_json::from_str::<BoundedCodeActions>(result.get()) else {
        return OrganizeImportsOutcome::Empty;
    };
    let Some(first) = bounded.0.into_iter().next() else {
        return OrganizeImportsOutcome::Empty;
    };
    let raw_value = serde_json::to_value(&first).unwrap_or(Value::Null);
    match first {
        lsp_types::CodeActionOrCommand::Command(_) => OrganizeImportsOutcome::Empty,
        lsp_types::CodeActionOrCommand::CodeAction(action) => {
            if let Some(edit) = action.edit {
                return OrganizeImportsOutcome::Ready(convert_workspace_edit(project_root, edit));
            }
            if action.data.is_some() && resolve_provider {
                return OrganizeImportsOutcome::NeedsResolve(raw_value);
            }
            OrganizeImportsOutcome::Empty
        }
    }
}

/// Returns the `LspEvent::WorkspaceEditReady` to emit if `message`'s `id`
/// matches `state.pending_organize_imports_resolve_id` (clearing it in
/// that case) -- `None` for a stale/superseded or otherwise unrelated
/// id-bearing message. Unlike `handle_resolve_response` (`ApplyCodeAction`'s
/// own resolve, which always reports the resolved action's own title as
/// `label`), this pairs `label` with `edit` -- both `Some` or both `None`
/// -- since `OrganizeImports` always reports the fixed label `"Optimize
/// Imports"`, never the server's own action title
/// (`docs/features/code-generation.md` §2.1, §3.4).
fn handle_organize_imports_resolve_response(
    message: &IncomingMessage,
    project_root: &Path,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    if Some(id) != state.pending_organize_imports_resolve_id {
        return None;
    }
    state.pending_organize_imports_resolve_id = None;
    let edit = if message.error.is_some() {
        None
    } else {
        message
            .result
            .and_then(|result| serde_json::from_str::<lsp_types::CodeAction>(result.get()).ok())
            .and_then(|action| action.edit)
            .and_then(|edit| convert_workspace_edit(project_root, edit))
    };
    let label = edit.as_ref().map(|_| "Optimize Imports".to_string());
    Some(LspEvent::WorkspaceEditReady { edit, label })
}

/// Returns the `LspEvent::WorkspaceEditReady` to emit if `message`'s `id`
/// matches `state.pending_resolve_id` (clearing it in that case) --
/// `None` for a stale/superseded or otherwise unrelated id-bearing
/// message (`docs/features/code-actions.md` §3.3). The resolved
/// `CodeAction`'s `edit` (if the server actually supplied one this time)
/// is converted via `convert_workspace_edit`; anything else (JSON-RPC
/// error, unparseable result, or a still-absent `edit`) becomes `edit:
/// None` -- the same "something must always clear a waiting UI"
/// permissiveness every other response handler in this file establishes.
fn handle_resolve_response(
    message: &IncomingMessage,
    project_root: &Path,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    if Some(id) != state.pending_resolve_id {
        return None;
    }
    state.pending_resolve_id = None;
    if message.error.is_some() {
        return Some(LspEvent::WorkspaceEditReady {
            edit: None,
            label: None,
        });
    }
    let Some(result) = message.result else {
        return Some(LspEvent::WorkspaceEditReady {
            edit: None,
            label: None,
        });
    };
    let Ok(action) = serde_json::from_str::<lsp_types::CodeAction>(result.get()) else {
        return Some(LspEvent::WorkspaceEditReady {
            edit: None,
            label: None,
        });
    };
    let label = Some(action.title);
    let edit = action
        .edit
        .and_then(|e| convert_workspace_edit(project_root, e));
    Some(LspEvent::WorkspaceEditReady { edit, label })
}

/// Returns the `LspEvent::DocumentSymbol` to emit if `message`'s `id`
/// matches `state.pending_document_symbol_id`'s id (clearing the slot in
/// that case) -- `None` for a stale/superseded or otherwise unrelated
/// id-bearing message. Same "carries its own path from the pending slot"
/// shape as `handle_inlay_hint_response`/`handle_code_action_response`
/// (`docs/features/search-everywhere.md` §2.2, §2.3).
fn handle_document_symbol_response(
    message: &IncomingMessage,
    project_root: &Path,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    let (pending_id, path) = state.pending_document_symbol_id.clone()?;
    if id != pending_id {
        return None;
    }
    state.pending_document_symbol_id = None;
    Some(LspEvent::DocumentSymbol {
        symbols: parse_document_symbol_result(message, project_root, &path),
        path,
    })
}

/// Parses a `textDocument/documentSymbol` response permissively, same
/// rationale as every other `parse_*_result` in this file: a JSON-RPC
/// error, a `null` result, or a result matching neither expected shape all
/// become an empty list. The result is `DocumentSymbol[] |
/// SymbolInformation[] | null` (`docs/features/search-everywhere.md`
/// §2.3) -- tries the hierarchical, `MAX_SYMBOLS_PER_MESSAGE`-bounded
/// `BoundedDocumentSymbols` shape first (it has required fields
/// `SymbolInformation` lacks -- `range`/`selectionRange` -- so a genuine
/// flat response always fails this attempt cleanly rather than partially
/// matching), flattening via `flatten_document_symbols` on success;
/// falls back to the flat, equally bounded `BoundedWorkspaceSymbols`
/// shape (covers the `SymbolInformation[]` fallback), converting each
/// entry the same way `parse_workspace_symbol_result` does, per-entry
/// permissive (a bad path drops that one symbol, not the whole list --
/// unlike `WorkspaceEdit`, a navigation list has no batch-atomicity
/// concern).
fn parse_document_symbol_result(
    message: &IncomingMessage,
    project_root: &Path,
    path: &Path,
) -> Vec<Symbol> {
    if message.error.is_some() {
        return Vec::new();
    }
    let Some(result) = message.result else {
        return Vec::new();
    };
    let text = result.get();
    if let Ok(bounded) = serde_json::from_str::<BoundedDocumentSymbols>(text) {
        let mut symbols = Vec::new();
        flatten_document_symbols(bounded.0, path, None, 0, &mut symbols);
        return symbols;
    }
    let Ok(bounded) = serde_json::from_str::<BoundedWorkspaceSymbols>(text) else {
        return Vec::new();
    };
    bounded
        .0
        .into_iter()
        .filter_map(|s| convert_workspace_symbol(s, project_root))
        .collect()
}

/// Flattens a `documentSymbol` response's hierarchical `children` nesting
/// into `out`, depth-first, stopping (not recursing further, not
/// processing remaining siblings) the moment `out.len()` reaches
/// `MAX_SYMBOLS_PER_MESSAGE` -- so `out` never grows past the cap
/// regardless of how deep or wide the server's tree is, and no
/// intermediate unbounded collection is ever built first
/// (`docs/features/search-everywhere.md` §2.3, §4). `depth` (0 at the
/// top level) is this function's own recursion guard, capped at
/// `MAX_SYMBOL_TREE_DEPTH` independent of `serde_json`'s deserializer-
/// level recursion limit -- see that constant's doc comment for why this
/// isn't purely redundant with the deserializer already rejecting a tree
/// this deep. `container_name` is the immediate parent's own `name`,
/// `None` at the top level.
fn flatten_document_symbols(
    entries: Vec<lsp_types::DocumentSymbol>,
    path: &Path,
    container_name: Option<&str>,
    depth: usize,
    out: &mut Vec<Symbol>,
) {
    if depth >= MAX_SYMBOL_TREE_DEPTH {
        return;
    }
    for entry in entries {
        if out.len() >= MAX_SYMBOLS_PER_MESSAGE {
            return;
        }
        let name = entry.name;
        let kind = convert_symbol_kind(entry.kind);
        let range = convert_range(entry.range);
        let children = entry.children;
        out.push(Symbol {
            name: name.clone(),
            kind,
            container_name: container_name.map(|s| s.to_string()),
            location: Location {
                path: path.to_path_buf(),
                range,
            },
        });
        if let Some(children) = children {
            if out.len() >= MAX_SYMBOLS_PER_MESSAGE {
                return;
            }
            flatten_document_symbols(children, path, Some(&name), depth + 1, out);
        }
    }
}

/// Returns the `LspEvent::WorkspaceSymbol` to emit if `message`'s `id`
/// matches `state.pending_workspace_symbol_id` (clearing it in that case)
/// -- `None` for a stale/superseded or otherwise unrelated id-bearing
/// message.
fn handle_workspace_symbol_response(
    message: &IncomingMessage,
    project_root: &Path,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    if Some(id) != state.pending_workspace_symbol_id {
        return None;
    }
    state.pending_workspace_symbol_id = None;
    Some(LspEvent::WorkspaceSymbol {
        symbols: parse_workspace_symbol_result(message, project_root),
    })
}

/// Parses a `workspace/symbol` response permissively, same rationale as
/// `parse_document_symbol_result`'s fallback path: a JSON-RPC error, a
/// `null` result, or an unparseable result all become an empty list.
/// Deserialized via `BoundedWorkspaceSymbols`; each entry converts via
/// `convert_workspace_symbol`, filtered permissively per-entry.
fn parse_workspace_symbol_result(message: &IncomingMessage, project_root: &Path) -> Vec<Symbol> {
    if message.error.is_some() {
        return Vec::new();
    }
    let Some(result) = message.result else {
        return Vec::new();
    };
    let Ok(bounded) = serde_json::from_str::<BoundedWorkspaceSymbols>(result.get()) else {
        return Vec::new();
    };
    bounded
        .0
        .into_iter()
        .filter_map(|s| convert_workspace_symbol(s, project_root))
        .collect()
}

/// Converts one `lsp_types::WorkspaceSymbol` to `ide_lsp::Symbol`,
/// validating its path against `project_root`. `None` if `location` is
/// the lazy, range-less `WorkspaceLocation` shape (v1 does not implement
/// `workspaceSymbol/resolve`, `docs/features/search-everywhere.md` §2.3)
/// or if the path fails `validate_path` -- both drop just this one entry,
/// never the whole list (§4).
fn convert_workspace_symbol(
    symbol: lsp_types::WorkspaceSymbol,
    project_root: &Path,
) -> Option<Symbol> {
    let location = match symbol.location {
        lsp_types::OneOf::Left(loc) => convert_location(loc, project_root)?,
        lsp_types::OneOf::Right(_) => return None,
    };
    Some(Symbol {
        name: symbol.name,
        kind: convert_symbol_kind(symbol.kind),
        container_name: symbol.container_name,
        location,
    })
}

fn convert_symbol_kind(kind: lsp_types::SymbolKind) -> SymbolKind {
    match kind {
        lsp_types::SymbolKind::FILE => SymbolKind::File,
        lsp_types::SymbolKind::MODULE => SymbolKind::Module,
        lsp_types::SymbolKind::NAMESPACE => SymbolKind::Namespace,
        lsp_types::SymbolKind::PACKAGE => SymbolKind::Package,
        lsp_types::SymbolKind::CLASS => SymbolKind::Class,
        lsp_types::SymbolKind::METHOD => SymbolKind::Method,
        lsp_types::SymbolKind::PROPERTY => SymbolKind::Property,
        lsp_types::SymbolKind::FIELD => SymbolKind::Field,
        lsp_types::SymbolKind::CONSTRUCTOR => SymbolKind::Constructor,
        lsp_types::SymbolKind::ENUM => SymbolKind::Enum,
        lsp_types::SymbolKind::INTERFACE => SymbolKind::Interface,
        lsp_types::SymbolKind::FUNCTION => SymbolKind::Function,
        lsp_types::SymbolKind::VARIABLE => SymbolKind::Variable,
        lsp_types::SymbolKind::CONSTANT => SymbolKind::Constant,
        lsp_types::SymbolKind::STRING => SymbolKind::String,
        lsp_types::SymbolKind::NUMBER => SymbolKind::Number,
        lsp_types::SymbolKind::BOOLEAN => SymbolKind::Boolean,
        lsp_types::SymbolKind::ARRAY => SymbolKind::Array,
        lsp_types::SymbolKind::OBJECT => SymbolKind::Object,
        lsp_types::SymbolKind::KEY => SymbolKind::Key,
        lsp_types::SymbolKind::NULL => SymbolKind::Null,
        lsp_types::SymbolKind::ENUM_MEMBER => SymbolKind::EnumMember,
        lsp_types::SymbolKind::STRUCT => SymbolKind::Struct,
        lsp_types::SymbolKind::EVENT => SymbolKind::Event,
        lsp_types::SymbolKind::OPERATOR => SymbolKind::Operator,
        lsp_types::SymbolKind::TYPE_PARAMETER => SymbolKind::TypeParameter,
        // `lsp_types::SymbolKind` is a raw `i32` wrapper, not a closed
        // Rust enum -- a future LSP spec revision or a nonconforming
        // server can send a value none of the named consts above match.
        // Same "fail to a reasonable neutral default rather than reject
        // the whole entry" precedent `convert_severity`'s `_ =>
        // DiagnosticSeverity::Information` already establishes.
        _ => SymbolKind::Variable,
    }
}

/// Returns the `LspEvent::FormatReady` to emit if `message`'s `id`
/// matches `state.pending_format`'s id (clearing the slot in that case)
/// -- `None` for a stale/superseded or otherwise unrelated id-bearing
/// message. Carries `path` from the pending slot, same "carries its own
/// path" shape as `handle_code_action_response`/`handle_inlay_hint_
/// response` -- the response itself has no path field of its own to
/// validate (`docs/features/formatting.md` §2.1, §3.1, §3.3, §4).
fn handle_format_response(
    message: &IncomingMessage,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    let (pending_id, path) = state.pending_format.clone()?;
    if id != pending_id {
        return None;
    }
    state.pending_format = None;
    let edit = parse_format_result(message, &path);
    Some(LspEvent::FormatReady { path, edit })
}

/// Returns the `LspEvent::PrepareRenameReady` to emit if `message`'s `id`
/// matches `state.pending_prepare_rename`'s id (clearing the slot in that
/// case) -- `None` for a stale/superseded or otherwise unrelated id-bearing
/// message. `result: null` or a JSON-RPC error is the only path to
/// `renameable: false`; any other shape (`Range`, `{range, placeholder}`,
/// or `{defaultBehavior}` -- `PrepareRenameResponse` is `#[serde(untagged)]`
/// in `lsp_types`, so this is a single permissive "did it parse as any of
/// the three, or not" check) is `renameable: true`
/// (`docs/features/rename-refactoring.md` §2.2, §3.2).
fn handle_prepare_rename_response(
    message: &IncomingMessage,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    let (pending_id, path) = state.pending_prepare_rename.clone()?;
    if id != pending_id {
        return None;
    }
    state.pending_prepare_rename = None;
    let renameable = if message.error.is_some() {
        false
    } else {
        message
            .result
            .map(|r| serde_json::from_str::<lsp_types::PrepareRenameResponse>(r.get()).is_ok())
            .unwrap_or(false)
    };
    Some(LspEvent::PrepareRenameReady { path, renameable })
}

/// Returns the `LspEvent::RenameReady` to emit if `message`'s `id` matches
/// `state.pending_rename`'s id (clearing the slot in that case) -- `None`
/// for a stale/superseded or otherwise unrelated id-bearing message.
/// Result is `WorkspaceEdit | null`, parsed with the exact same
/// permissiveness `handle_code_action_response`'s resolved-action `.edit`
/// field already gets: `null`/a JSON-RPC error -> `edit: None`; present ->
/// `convert_workspace_edit(project_root, raw)`, which itself may still
/// yield `None` (a path inside the edit fails validation, or it contains a
/// resource operation) (`docs/features/rename-refactoring.md` §2.2, §3.4).
fn handle_rename_response(
    message: &IncomingMessage,
    project_root: &Path,
    state: &mut ConnectionState,
) -> Option<LspEvent> {
    let id = message.id.as_ref().and_then(Value::as_u64)?;
    let (pending_id, path, new_name) = state.pending_rename.clone()?;
    if id != pending_id {
        return None;
    }
    state.pending_rename = None;
    let edit = if message.error.is_some() {
        None
    } else {
        message
            .result
            .and_then(|r| serde_json::from_str::<lsp_types::WorkspaceEdit>(r.get()).ok())
            .and_then(|raw| convert_workspace_edit(project_root, raw))
    };
    Some(LspEvent::RenameReady {
        path,
        new_name,
        edit,
    })
}

/// Parses a `textDocument/formatting`/`textDocument/rangeFormatting`
/// response permissively (`docs/features/formatting.md` §3.1): a
/// JSON-RPC error, a `null` result, an empty array, or an unparseable
/// result all become `None` -- not distinguished from each other, same
/// permissiveness `parse_code_action_result` already establishes. A
/// non-empty array becomes a single-`FileEdit` `WorkspaceEdit` for
/// `path` -- the *request's* own path, never anything parsed out of the
/// response, since the response carries no path of its own at all.
fn parse_format_result(message: &IncomingMessage, path: &Path) -> Option<WorkspaceEdit> {
    if message.error.is_some() {
        return None;
    }
    let result = message.result?;
    let bounded = serde_json::from_str::<BoundedTextEdits>(result.get()).ok()?;
    if bounded.0.is_empty() {
        return None;
    }
    let text_edits = bounded
        .0
        .into_iter()
        .map(|t| TextEdit {
            range: convert_range(t.range),
            new_text: t.new_text,
        })
        .collect();
    Some(WorkspaceEdit {
        edits: vec![FileEdit {
            path: path.to_path_buf(),
            text_edits,
        }],
    })
}

/// Converts a raw `lsp_types::WorkspaceEdit` into `ide_lsp::WorkspaceEdit`,
/// validating every file path against `project_root`
/// (`docs/features/code-actions.md` §3.3, §4). Unlike `References`/
/// `Goto`/etc.'s per-entry permissiveness, **any** failure here --a path
/// that fails `validate_path`, a URI that doesn't parse, or a resource
/// operation (`CreateFile`/`RenameFile`/`DeleteFile`) anywhere in
/// `documentChanges` (§1: not represented in `ide_lsp::WorkspaceEdit` at
/// all) -- fails the *entire* conversion, returning `None` rather than a
/// partial result: a `WorkspaceEdit`'s entries are pieces of one intended
/// change, not independently-droppable answers. Prefers `documentChanges`
/// over the older `changes` map when both are present, per the LSP spec's
/// own precedence note on `WorkspaceEdit`.
///
/// Every file-entry collection here (`documentChanges`'s `Vec`, the
/// `changes` map) is capped at `MAX_LOCATIONS_PER_MESSAGE`, the same as
/// every other array-shaped response this client parses -- unlike those,
/// this isn't about the cost of a typed deserialize (the whole
/// `lsp_types::WorkspaceEdit` is already fully deserialized by the time
/// this function runs), it's that each file entry costs one
/// `validate_path` call (a `canonicalize`/stat syscall), and
/// `documentChanges` is a plain `Vec` -- unlike `changes`'s `HashMap`, it
/// isn't deduplicated by URI, so a malicious server can force the same
/// real file's path through `validate_path` an unbounded number of times
/// by repeating one entry (verified live: see
/// `docs/security-findings/rust-lsp-dev-code-actions-*.md` finding 1).
/// Exceeding the cap fails the whole conversion, consistent with every
/// other failure mode here.
fn convert_workspace_edit(
    project_root: &Path,
    edit: lsp_types::WorkspaceEdit,
) -> Option<WorkspaceEdit> {
    if let Some(document_changes) = edit.document_changes {
        let text_document_edits = match document_changes {
            lsp_types::DocumentChanges::Edits(edits) => edits,
            lsp_types::DocumentChanges::Operations(ops) => {
                if ops.len() > MAX_LOCATIONS_PER_MESSAGE {
                    return None;
                }
                let mut edits = Vec::with_capacity(ops.len());
                for op in ops {
                    match op {
                        lsp_types::DocumentChangeOperation::Edit(e) => edits.push(e),
                        // A create/rename/delete resource operation
                        // anywhere in the list fails the whole edit --
                        // §1's deferral, applied as a hard boundary here
                        // rather than silently dropping just this piece.
                        lsp_types::DocumentChangeOperation::Op(_) => return None,
                    }
                }
                edits
            }
        };
        if text_document_edits.len() > MAX_LOCATIONS_PER_MESSAGE {
            return None;
        }
        let mut edits = Vec::with_capacity(text_document_edits.len());
        for tde in text_document_edits {
            let url = Url::parse(tde.text_document.uri.as_str()).ok()?;
            let path = url.to_file_path().ok()?;
            let validated = validate_path(project_root, &path)?;
            let mut text_edits = Vec::with_capacity(tde.edits.len());
            for edit in tde.edits {
                let text_edit = match edit {
                    lsp_types::OneOf::Left(t) => t,
                    lsp_types::OneOf::Right(a) => a.text_edit,
                };
                text_edits.push(TextEdit {
                    range: convert_range(text_edit.range),
                    new_text: text_edit.new_text,
                });
            }
            edits.push(FileEdit {
                path: validated,
                text_edits,
            });
        }
        return Some(WorkspaceEdit { edits });
    }

    let Some(changes) = edit.changes else {
        return Some(WorkspaceEdit { edits: Vec::new() });
    };
    if changes.len() > MAX_LOCATIONS_PER_MESSAGE {
        return None;
    }
    let mut edits = Vec::with_capacity(changes.len());
    for (uri, text_edits) in changes {
        let url = Url::parse(uri.as_str()).ok()?;
        let path = url.to_file_path().ok()?;
        let validated = validate_path(project_root, &path)?;
        let text_edits = text_edits
            .into_iter()
            .map(|t| TextEdit {
                range: convert_range(t.range),
                new_text: t.new_text,
            })
            .collect();
        edits.push(FileEdit {
            path: validated,
            text_edits,
        });
    }
    Some(WorkspaceEdit { edits })
}

fn truncate_diagnostics_array(mut params: Value) -> Value {
    if let Some(diagnostics) = params.get_mut("diagnostics").and_then(Value::as_array_mut) {
        diagnostics.truncate(MAX_DIAGNOSTICS_PER_MESSAGE);
    }
    params
}

fn limit_diagnostics(diagnostics: Vec<lsp_types::Diagnostic>) -> Vec<Diagnostic> {
    diagnostics
        .into_iter()
        .take(MAX_DIAGNOSTICS_PER_MESSAGE)
        .map(convert_diagnostic)
        .collect()
}

fn convert_diagnostic(diag: lsp_types::Diagnostic) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: diag.range.start.line,
                character: diag.range.start.character,
            },
            end: Position {
                line: diag.range.end.line,
                character: diag.range.end.character,
            },
        },
        severity: match diag.severity {
            Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
            Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
            Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
            // INFORMATION and an absent severity (server declined to
            // classify it) both default to Information -- the least
            // alarming non-Hint bucket.
            _ => DiagnosticSeverity::Information,
        },
        message: diag.message,
    }
}

// Fixture-based end-to-end tests (real handshake, real diagnostics, real
// process teardown, against `tests/fixtures/fake_lsp_server.rs`) live in
// `tests/fixture_integration.rs` instead of here: `CARGO_BIN_EXE_*` is
// only populated for integration-test targets, not for a lib's own
// `#[cfg(test)]` unit tests.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_diagnostics_array_caps_an_oversized_diagnostics_array_before_typed_deserialize() {
        let diagnostics: Vec<Value> = std::iter::repeat_n(
            json!({
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "message": "x",
            }),
            MAX_DIAGNOSTICS_PER_MESSAGE + 500,
        )
        .collect();
        let params = json!({ "uri": "file:///x", "diagnostics": diagnostics });

        let truncated = truncate_diagnostics_array(params);
        let len = truncated["diagnostics"].as_array().unwrap().len();
        assert_eq!(len, MAX_DIAGNOSTICS_PER_MESSAGE);
    }

    #[test]
    fn truncate_diagnostics_array_leaves_a_message_under_the_cap_untouched() {
        let params = json!({ "uri": "file:///x", "diagnostics": [{}, {}, {}] });
        let truncated = truncate_diagnostics_array(params);
        assert_eq!(truncated["diagnostics"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn limit_diagnostics_truncates_a_message_with_too_many_diagnostics() {
        let diagnostics = vec![lsp_types::Diagnostic::default(); MAX_DIAGNOSTICS_PER_MESSAGE + 500];
        let limited = limit_diagnostics(diagnostics);
        assert_eq!(limited.len(), MAX_DIAGNOSTICS_PER_MESSAGE);
    }

    #[test]
    fn limit_diagnostics_leaves_a_message_under_the_cap_untouched() {
        let diagnostics = vec![lsp_types::Diagnostic::default(); 3];
        let limited = limit_diagnostics(diagnostics);
        assert_eq!(limited.len(), 3);
    }

    #[test]
    fn start_with_missing_binary_reports_server_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result =
            LspClient::start_with_command(dir.path(), "definitely-not-a-real-lsp-binary-xyz", &[]);
        match result {
            Err(LspError::ServerNotFound(command)) => {
                assert_eq!(command, "definitely-not-a-real-lsp-binary-xyz");
            }
            Err(other) => panic!("expected ServerNotFound, got a different error: {other}"),
            Ok(_) => panic!("expected ServerNotFound, but spawn unexpectedly succeeded"),
        }
    }

    #[test]
    fn start_defaults_to_rust_analyzer_as_the_command() {
        let dir = tempfile::tempdir().unwrap();
        // Tolerant of whether `rust-analyzer` happens to be on this
        // machine's PATH: either a successful spawn (promptly dropped)
        // or `ServerNotFound("rust-analyzer")` both prove `start`
        // defaults to that command name; only an unrelated error would
        // be a real failure.
        match LspClient::start(dir.path()) {
            Ok(client) => drop(client),
            Err(LspError::ServerNotFound(command)) => assert_eq!(command, "rust-analyzer"),
            Err(other) => panic!("unexpected error from start(): {other}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn start_with_non_executable_file_reports_io_error_not_server_not_found() {
        use std::os::unix::fs::PermissionsExt;

        let bin_dir = tempfile::tempdir().unwrap();
        let not_executable = bin_dir.path().join("not-a-server");
        fs::write(&not_executable, "not a real binary").unwrap();
        let mut perms = fs::metadata(&not_executable).unwrap().permissions();
        perms.set_mode(0o600); // read/write, no execute
        fs::set_permissions(&not_executable, perms).unwrap();

        let project_root = tempfile::tempdir().unwrap();
        let result = LspClient::start_with_command(
            project_root.path(),
            not_executable.to_str().unwrap(),
            &[],
        );
        match result {
            Err(LspError::Io(_)) => {}
            Err(LspError::ServerNotFound(_)) => {
                panic!("expected LspError::Io, got ServerNotFound")
            }
            Err(LspError::Protocol(_)) => panic!("expected LspError::Io, got Protocol"),
            Ok(_) => panic!("expected LspError::Io, but spawn unexpectedly succeeded"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawn_child_sets_current_dir_to_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(async {
                spawn_child("pwd", &[], &root)
                    .await
                    .unwrap()
                    .wait_with_output()
                    .await
            })
            .unwrap();

        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.trim(), root.to_str().unwrap());
    }

    /// `docs/features/language-server-arguments.md` §5: `args` must
    /// actually reach the spawned process's real argv, observed via its
    /// own output -- not just type-checked at the call site.
    #[cfg(unix)]
    #[test]
    fn spawn_child_passes_args_through_to_the_spawned_process() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let args = vec!["hello".to_string(), "world".to_string()];

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(async {
                spawn_child("echo", &args, &root)
                    .await
                    .unwrap()
                    .wait_with_output()
                    .await
            })
            .unwrap();

        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.trim(), "hello world");
    }

    /// `docs/features/language-server-arguments.md` §4: an `args` entry
    /// `std::process::Command` itself rejects (an embedded NUL byte,
    /// reachable via a hand-crafted `preferences.json` even though no
    /// real text field can type one) must surface as a clean `LspError`,
    /// never a panic.
    #[test]
    fn start_with_command_rejects_a_nul_byte_argument_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let args = vec!["embedded\0nul".to_string()];

        let result = LspClient::start_with_command(dir.path(), "echo", &args);

        assert!(matches!(result, Err(LspError::Io(_))));
    }

    /// Same invariant as above, for an unusually large argv -- must
    /// still be a clean spawn attempt (success or a normal `LspError`),
    /// never a panic or unbounded resource use.
    #[cfg(unix)]
    #[test]
    fn start_with_command_handles_a_large_number_of_arguments_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let args: Vec<String> = (0..10_000).map(|i| format!("arg{i}")).collect();

        let result = LspClient::start_with_command(dir.path(), "echo", &args);

        assert!(result.is_ok() || matches!(result, Err(LspError::Io(_))));
    }

    /// Adversarial regression proof for `docs/features/
    /// language-server-arguments.md` §4's "still no shell" claim: an
    /// `args` entry packed with shell metacharacters and a command
    /// substitution that would, under a shell, create a canary file --
    /// spawns `echo` with it for real and confirms (a) the canary file is
    /// never created and (b) the metacharacters come back verbatim in
    /// stdout, proving `Command::args` passed it through as one literal
    /// argv element rather than any shell ever getting a chance to parse
    /// it.
    #[cfg(unix)]
    #[test]
    fn args_containing_shell_metacharacters_are_never_shell_interpreted() {
        let dir = tempfile::tempdir().unwrap();
        let canary = dir.path().join("hacker-canary-should-never-exist");
        let payload = format!(
            "; touch {} ; $(touch {}) `touch {}` && touch {}",
            canary.display(),
            canary.display(),
            canary.display(),
            canary.display()
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(async {
                spawn_child("echo", std::slice::from_ref(&payload), dir.path())
                    .await
                    .unwrap()
                    .wait_with_output()
                    .await
            })
            .unwrap();

        assert!(
            !canary.exists(),
            "shell metacharacters in an arg were interpreted -- canary file was created"
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.trim(), payload);
    }

    /// A canonicalized project root plus one real file inside it --
    /// `validate_path` requires the target to actually exist on disk.
    fn project_with_file() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let file = root.join("main.rs");
        fs::write(&file, "fn main() {}").unwrap();
        (dir, root, file)
    }

    /// Builds a `textDocument/references` response's raw JSON text (not a
    /// `Value`) -- `IncomingMessage` borrows its `result` field straight
    /// out of the original text via `RawValue`, so every test needs the
    /// wire-shaped text to deserialize from, matching how `handle_incoming`
    /// really receives messages, not a pre-built `Value` tree.
    fn references_response_text(id: u64, result: Value) -> String {
        serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).unwrap()
    }

    fn parse_message(text: &str) -> IncomingMessage<'_> {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn connection_state_allocates_sequential_ids_starting_after_reserved_ones() {
        let mut state = ConnectionState::new();
        let first = state.allocate_request_id();
        let second = state.allocate_request_id();
        assert_eq!(first, 3);
        assert_eq!(second, 4);
        assert_ne!(first, INITIALIZE_ID);
        assert_ne!(first, SHUTDOWN_ID);
    }

    #[test]
    fn handle_references_response_ignores_a_message_with_no_id() {
        let (_dir, root, _file) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_references_id = Some(3);

        let text = serde_json::to_string(
            &json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics" }),
        )
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(
            handle_references_response(&message, &root, &mut state),
            None
        );
        // Not consumed -- this message was never a references response.
        assert_eq!(state.pending_references_id, Some(3));
    }

    #[test]
    fn handle_references_response_ignores_a_stale_or_unrelated_id() {
        let (_dir, root, _file) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_references_id = Some(4); // superseded by a newer query

        let text = references_response_text(3, Value::Array(Vec::new()));
        let stale = parse_message(&text);
        assert_eq!(handle_references_response(&stale, &root, &mut state), None);
        // The still-current pending id must survive an unrelated response.
        assert_eq!(state.pending_references_id, Some(4));
    }

    #[test]
    fn handle_references_response_matches_pending_id_and_clears_it() {
        let (_dir, root, file) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_references_id = Some(3);

        let uri = Url::from_file_path(&file).unwrap().to_string();
        let text = references_response_text(
            3,
            json!([{
                "uri": uri,
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}},
            }]),
        );
        let response = parse_message(&text);

        let event = handle_references_response(&response, &root, &mut state);
        assert_eq!(state.pending_references_id, None);
        match event {
            Some(LspEvent::References { locations }) => {
                assert_eq!(locations.len(), 1);
                assert_eq!(locations[0].path, file);
            }
            other => panic!("expected a References event, got {other:?}"),
        }
    }

    #[test]
    fn handle_references_response_delivers_empty_locations_for_a_json_rpc_error() {
        let (_dir, root, _file) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_references_id = Some(3);

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_references_response(&response, &root, &mut state) {
            Some(LspEvent::References { locations }) => assert!(locations.is_empty()),
            other => panic!("expected an empty References event, got {other:?}"),
        }
        // A definite answer must always clear the pending state, error or not.
        assert_eq!(state.pending_references_id, None);
    }

    #[test]
    fn parse_references_result_null_result_is_empty() {
        let (_dir, root, _file) = project_with_file();
        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);
        assert!(parse_references_result(&response, &root).is_empty());
    }

    #[test]
    fn parse_references_result_missing_result_field_is_empty() {
        let (_dir, root, _file) = project_with_file();
        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);
        assert!(parse_references_result(&response, &root).is_empty());
    }

    #[test]
    fn parse_references_result_malformed_result_shape_is_empty() {
        let (_dir, root, _file) = project_with_file();
        // A single object instead of an array of locations -- doesn't
        // deserialize via `BoundedLocations`' `deserialize_seq`.
        let text = references_response_text(3, json!({ "not": "a locations array" }));
        let response = parse_message(&text);
        assert!(parse_references_result(&response, &root).is_empty());
    }

    #[test]
    fn parse_references_result_skips_entries_outside_project_root_keeps_the_rest() {
        let (_dir, root, file) = project_with_file();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = fs::canonicalize(outside_dir.path())
            .unwrap()
            .join("evil.rs");
        fs::write(&outside_file, "fn evil() {}").unwrap();

        let good_uri = Url::from_file_path(&file).unwrap().to_string();
        let bad_uri = Url::from_file_path(&outside_file).unwrap().to_string();
        let text = references_response_text(
            3,
            json!([
                {
                    "uri": bad_uri,
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                },
                {
                    "uri": good_uri,
                    "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 3}},
                },
            ]),
        );
        let response = parse_message(&text);

        let locations = parse_references_result(&response, &root);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, file);
    }

    #[test]
    fn parse_references_result_caps_an_oversized_result_and_still_returns_valid_entries() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let entry = json!({
            "uri": uri,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        });
        let text = references_response_text(
            3,
            Value::Array(std::iter::repeat_n(entry, MAX_LOCATIONS_PER_MESSAGE + 500).collect()),
        );
        let response = parse_message(&text);

        let locations = parse_references_result(&response, &root);
        assert_eq!(locations.len(), MAX_LOCATIONS_PER_MESSAGE);
    }

    #[test]
    fn bounded_locations_leaves_an_array_under_the_cap_untouched() {
        let entry = json!({
            "uri": "file:///nonexistent/x.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        });
        let text = serde_json::to_string(&json!([entry.clone(), entry.clone(), entry])).unwrap();

        let bounded = serde_json::from_str::<BoundedLocations>(&text).unwrap();
        assert_eq!(bounded.0.len(), 3);
    }

    #[test]
    fn bounded_locations_rejects_a_non_array_top_level_value() {
        for text in ["null", "{}", "\"not an array\"", "42"] {
            assert!(
                serde_json::from_str::<BoundedLocations>(text).is_err(),
                "expected {text:?} to be rejected as a non-array top-level value"
            );
        }
    }

    #[test]
    fn bounded_locations_caps_an_oversized_array_before_materializing_it() {
        let valid_entry = json!({
            "uri": "file:///nonexistent/x.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        });
        let entries: Vec<Value> =
            std::iter::repeat_n(valid_entry, MAX_LOCATIONS_PER_MESSAGE + 500).collect();
        let text = serde_json::to_string(&Value::Array(entries)).unwrap();

        let bounded = serde_json::from_str::<BoundedLocations>(&text).unwrap();
        assert_eq!(bounded.0.len(), MAX_LOCATIONS_PER_MESSAGE);
    }

    #[test]
    fn bounded_locations_ignores_malformed_entries_beyond_the_cap_without_erroring() {
        let valid_entry = json!({
            "uri": "file:///nonexistent/x.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        });
        let mut entries: Vec<Value> =
            std::iter::repeat_n(valid_entry, MAX_LOCATIONS_PER_MESSAGE).collect();
        // Every entry past the cap is a shape `lsp_types::Location` could
        // never deserialize (a bare string, not an object with a `uri`/
        // `range`) -- if `BoundedLocations` were still attempting a typed
        // `Location` deserialize for these instead of draining them via
        // `IgnoredAny`, this whole parse would fail with a deserialize
        // error rather than succeeding with a capped result. This is the
        // load-bearing test for the fix: it fails under round 1's
        // "truncate the already-parsed `Value` after the fact" approach
        // just as easily as it passes here, so it only distinguishes
        // "genuinely never materialized as a typed `Location`" from
        // "materialized then discarded."
        entries.extend(std::iter::repeat_n(
            Value::String("not a location".into()),
            500,
        ));
        let text = serde_json::to_string(&Value::Array(entries)).unwrap();

        let bounded = serde_json::from_str::<BoundedLocations>(&text).unwrap();
        assert_eq!(bounded.0.len(), MAX_LOCATIONS_PER_MESSAGE);
    }

    #[test]
    fn convert_location_rejects_an_unparseable_uri() {
        let (_dir, root, _file) = project_with_file();
        let loc = lsp_types::Location {
            uri: "not a uri".parse().unwrap_or_else(|_| {
                // `lsp_types::Uri` (an `fluent_uri::Uri`) rejects this at
                // parse time already -- fall back to a syntactically valid
                // but non-`file://` URI, which must fail at the
                // `Url::to_file_path` step in `convert_location` instead.
                "https://example.com/x".parse().unwrap()
            }),
            range: lsp_types::Range::default(),
        };
        assert_eq!(convert_location(loc, &root), None);
    }

    /// Same shape as `references_response_text` -- see its doc comment.
    fn goto_response_text(id: u64, result: Value) -> String {
        serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).unwrap()
    }

    #[test]
    fn goto_method_maps_each_kind_to_its_lsp_method_name() {
        assert_eq!(goto_method(GotoKind::Definition), "textDocument/definition");
        assert_eq!(
            goto_method(GotoKind::TypeDefinition),
            "textDocument/typeDefinition"
        );
        assert_eq!(
            goto_method(GotoKind::Implementation),
            "textDocument/implementation"
        );
    }

    #[test]
    fn handle_goto_response_ignores_a_message_with_no_id() {
        let (_dir, root, _file) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_goto_id = Some(3);

        let text = serde_json::to_string(
            &json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics" }),
        )
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(handle_goto_response(&message, &root, &mut state), None);
        assert_eq!(state.pending_goto_id, Some(3));
    }

    #[test]
    fn handle_goto_response_ignores_a_stale_or_unrelated_id() {
        let (_dir, root, _file) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_goto_id = Some(4); // superseded by a newer query

        let text = goto_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(handle_goto_response(&stale, &root, &mut state), None);
        assert_eq!(state.pending_goto_id, Some(4));
    }

    #[test]
    fn handle_goto_response_matches_pending_id_and_clears_it() {
        let (_dir, root, file) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_goto_id = Some(3);

        let uri = Url::from_file_path(&file).unwrap().to_string();
        let text = goto_response_text(
            3,
            json!({
                "uri": uri,
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}},
            }),
        );
        let response = parse_message(&text);

        let event = handle_goto_response(&response, &root, &mut state);
        assert_eq!(state.pending_goto_id, None);
        match event {
            Some(LspEvent::Goto { locations }) => {
                assert_eq!(locations.len(), 1);
                assert_eq!(locations[0].path, file);
            }
            other => panic!("expected a Goto event, got {other:?}"),
        }
    }

    #[test]
    fn handle_goto_response_delivers_empty_locations_for_a_json_rpc_error() {
        let (_dir, root, _file) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_goto_id = Some(3);

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_goto_response(&response, &root, &mut state) {
            Some(LspEvent::Goto { locations }) => assert!(locations.is_empty()),
            other => panic!("expected an empty Goto event, got {other:?}"),
        }
        assert_eq!(state.pending_goto_id, None);
    }

    /// The one behavior genuinely new relative to References: `Goto`
    /// shares a single pending-id slot across all three `GotoKind`s, so a
    /// second `Goto` request of a *different* kind still supersedes the
    /// first exactly like a same-kind resend would (`send_request`'s
    /// `Goto` arm always writes `state.pending_goto_id`, never keyed by
    /// `kind`) -- a response for the first kind's id must be dropped once
    /// the second kind's request has gone out.
    #[test]
    fn handle_goto_response_a_different_kind_request_supersedes_the_pending_one() {
        let (_dir, root, _file) = project_with_file();
        let mut state = ConnectionState::new();

        // Definition query goes out first.
        let definition_id = state.allocate_request_id();
        state.pending_goto_id = Some(definition_id);

        // Before it answers, an Implementation query supersedes it --
        // same shared slot, different GotoKind.
        let implementation_id = state.allocate_request_id();
        assert_ne!(definition_id, implementation_id);
        state.pending_goto_id = Some(implementation_id);

        let stale_text = goto_response_text(definition_id, Value::Null);
        let stale = parse_message(&stale_text);
        assert_eq!(handle_goto_response(&stale, &root, &mut state), None);
        assert_eq!(state.pending_goto_id, Some(implementation_id));

        let text = goto_response_text(implementation_id, Value::Null);
        let response = parse_message(&text);
        match handle_goto_response(&response, &root, &mut state) {
            Some(LspEvent::Goto { locations }) => assert!(locations.is_empty()),
            other => panic!("expected an empty Goto event, got {other:?}"),
        }
        assert_eq!(state.pending_goto_id, None);
    }

    #[test]
    fn parse_goto_result_null_result_is_empty() {
        let (_dir, root, _file) = project_with_file();
        let text = goto_response_text(3, Value::Null);
        let response = parse_message(&text);
        assert!(parse_goto_result(&response, &root).is_empty());
    }

    #[test]
    fn parse_goto_result_missing_result_field_is_empty() {
        let (_dir, root, _file) = project_with_file();
        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);
        assert!(parse_goto_result(&response, &root).is_empty());
    }

    #[test]
    fn parse_goto_result_malformed_scalar_shape_is_empty() {
        let (_dir, root, _file) = project_with_file();
        // Neither a `Location` object nor an array -- doesn't deserialize
        // as `Option<lsp_types::Location>`.
        let text = goto_response_text(3, json!("not a location"));
        let response = parse_message(&text);
        assert!(parse_goto_result(&response, &root).is_empty());
    }

    #[test]
    fn parse_goto_result_malformed_array_shape_is_empty() {
        let (_dir, root, _file) = project_with_file();
        // Starts with `[`, so it takes the array branch, but its entries
        // aren't locations -- doesn't deserialize via `BoundedLocations`.
        let text = goto_response_text(3, json!([1, 2, 3]));
        let response = parse_message(&text);
        assert!(parse_goto_result(&response, &root).is_empty());
    }

    #[test]
    fn parse_goto_result_parses_a_scalar_location_object() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let text = goto_response_text(
            3,
            json!({
                "uri": uri,
                "range": {"start": {"line": 4, "character": 0}, "end": {"line": 4, "character": 3}},
            }),
        );
        let response = parse_message(&text);

        let locations = parse_goto_result(&response, &root);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, file);
    }

    #[test]
    fn parse_goto_result_parses_an_array_of_locations() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let text = goto_response_text(
            3,
            json!([
                {
                    "uri": uri,
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                },
                {
                    "uri": uri,
                    "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 3}},
                },
            ]),
        );
        let response = parse_message(&text);

        let locations = parse_goto_result(&response, &root);
        assert_eq!(locations.len(), 2);
    }

    #[test]
    fn parse_goto_result_skips_entries_outside_project_root_keeps_the_rest() {
        let (_dir, root, file) = project_with_file();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = fs::canonicalize(outside_dir.path())
            .unwrap()
            .join("evil.rs");
        fs::write(&outside_file, "fn evil() {}").unwrap();

        let good_uri = Url::from_file_path(&file).unwrap().to_string();
        let bad_uri = Url::from_file_path(&outside_file).unwrap().to_string();
        let text = goto_response_text(
            3,
            json!([
                {
                    "uri": bad_uri,
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                },
                {
                    "uri": good_uri,
                    "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 3}},
                },
            ]),
        );
        let response = parse_message(&text);

        let locations = parse_goto_result(&response, &root);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, file);
    }

    #[test]
    fn parse_goto_result_caps_an_oversized_array_and_still_returns_valid_entries() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let entry = json!({
            "uri": uri,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        });
        let text = goto_response_text(
            3,
            Value::Array(std::iter::repeat_n(entry, MAX_LOCATIONS_PER_MESSAGE + 500).collect()),
        );
        let response = parse_message(&text);

        let locations = parse_goto_result(&response, &root);
        assert_eq!(locations.len(), MAX_LOCATIONS_PER_MESSAGE);
    }

    #[test]
    fn convert_range_converts_both_endpoints() {
        let r = lsp_types::Range {
            start: lsp_types::Position {
                line: 1,
                character: 2,
            },
            end: lsp_types::Position {
                line: 3,
                character: 4,
            },
        };
        let converted = convert_range(r);
        assert_eq!(
            converted.start,
            Position {
                line: 1,
                character: 2
            }
        );
        assert_eq!(
            converted.end,
            Position {
                line: 3,
                character: 4
            }
        );
    }

    // ---- Hover ----

    #[test]
    fn handle_hover_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_hover_id = Some(3);

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(handle_hover_response(&message, &mut state), None);
        assert_eq!(state.pending_hover_id, Some(3));
    }

    #[test]
    fn handle_hover_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_hover_id = Some(4);

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(handle_hover_response(&stale, &mut state), None);
        assert_eq!(state.pending_hover_id, Some(4));
    }

    #[test]
    fn handle_hover_response_matches_pending_id_and_clears_it() {
        let mut state = ConnectionState::new();
        state.pending_hover_id = Some(3);

        let text = references_response_text(
            3,
            json!({ "contents": { "kind": "markdown", "value": "fn foo() -> bool" } }),
        );
        let response = parse_message(&text);

        let event = handle_hover_response(&response, &mut state);
        assert_eq!(state.pending_hover_id, None);
        match event {
            Some(LspEvent::Hover { contents }) => {
                assert_eq!(contents, Some("fn foo() -> bool".to_string()))
            }
            other => panic!("expected a Hover event, got {other:?}"),
        }
    }

    #[test]
    fn handle_hover_response_delivers_none_for_a_json_rpc_error() {
        let mut state = ConnectionState::new();
        state.pending_hover_id = Some(3);

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_hover_response(&response, &mut state) {
            Some(LspEvent::Hover { contents }) => assert_eq!(contents, None),
            other => panic!("expected an empty Hover event, got {other:?}"),
        }
        assert_eq!(state.pending_hover_id, None);
    }

    #[test]
    fn parse_hover_result_null_result_is_none() {
        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);
        assert_eq!(parse_hover_result(&response), None);
    }

    #[test]
    fn parse_hover_result_missing_result_field_is_none() {
        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);
        assert_eq!(parse_hover_result(&response), None);
    }

    #[test]
    fn parse_hover_result_malformed_shape_is_none() {
        let text = references_response_text(3, json!("not a hover object"));
        let response = parse_message(&text);
        assert_eq!(parse_hover_result(&response), None);
    }

    #[test]
    fn flatten_hover_contents_markup_uses_value_as_is_no_markdown_parsing() {
        let contents = lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: "**bold** and `code`".to_string(),
        });
        assert_eq!(flatten_hover_contents(contents), "**bold** and `code`");
    }

    #[test]
    fn flatten_hover_contents_scalar_string() {
        let contents =
            lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String("plain".to_string()));
        assert_eq!(flatten_hover_contents(contents), "plain");
    }

    #[test]
    fn flatten_hover_contents_scalar_language_string_uses_value_ignores_language() {
        let contents = lsp_types::HoverContents::Scalar(lsp_types::MarkedString::LanguageString(
            lsp_types::LanguageString {
                language: "rust".to_string(),
                value: "fn foo()".to_string(),
            },
        ));
        assert_eq!(flatten_hover_contents(contents), "fn foo()");
    }

    #[test]
    fn flatten_hover_contents_array_joins_with_a_blank_line() {
        let contents = lsp_types::HoverContents::Array(vec![
            lsp_types::MarkedString::String("first".to_string()),
            lsp_types::MarkedString::String("second".to_string()),
        ]);
        assert_eq!(flatten_hover_contents(contents), "first\n\nsecond");
    }

    // ---- DocumentHighlight ----

    #[test]
    fn handle_document_highlight_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_document_highlight_id = Some(3);

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(
            handle_document_highlight_response(&message, &mut state),
            None
        );
        assert_eq!(state.pending_document_highlight_id, Some(3));
    }

    #[test]
    fn handle_document_highlight_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_document_highlight_id = Some(4);

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(handle_document_highlight_response(&stale, &mut state), None);
        assert_eq!(state.pending_document_highlight_id, Some(4));
    }

    #[test]
    fn handle_document_highlight_response_matches_pending_id_and_clears_it() {
        let mut state = ConnectionState::new();
        state.pending_document_highlight_id = Some(3);

        let text = references_response_text(
            3,
            json!([{
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}},
                "kind": 2,
            }]),
        );
        let response = parse_message(&text);

        let event = handle_document_highlight_response(&response, &mut state);
        assert_eq!(state.pending_document_highlight_id, None);
        match event {
            Some(LspEvent::DocumentHighlight { ranges }) => assert_eq!(ranges.len(), 1),
            other => panic!("expected a DocumentHighlight event, got {other:?}"),
        }
    }

    #[test]
    fn handle_document_highlight_response_delivers_empty_ranges_for_a_json_rpc_error() {
        let mut state = ConnectionState::new();
        state.pending_document_highlight_id = Some(3);

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_document_highlight_response(&response, &mut state) {
            Some(LspEvent::DocumentHighlight { ranges }) => assert!(ranges.is_empty()),
            other => panic!("expected an empty DocumentHighlight event, got {other:?}"),
        }
        assert_eq!(state.pending_document_highlight_id, None);
    }

    #[test]
    fn parse_document_highlight_result_null_result_is_empty() {
        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);
        assert!(parse_document_highlight_result(&response).is_empty());
    }

    #[test]
    fn parse_document_highlight_result_missing_result_field_is_empty() {
        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);
        assert!(parse_document_highlight_result(&response).is_empty());
    }

    #[test]
    fn parse_document_highlight_result_malformed_shape_is_empty() {
        let text = references_response_text(3, json!({ "not": "an array" }));
        let response = parse_message(&text);
        assert!(parse_document_highlight_result(&response).is_empty());
    }

    #[test]
    fn parse_document_highlight_result_parses_an_array_and_discards_kind() {
        let text = references_response_text(
            3,
            json!([
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "kind": 1},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 3}}, "kind": 3},
            ]),
        );
        let response = parse_message(&text);
        let ranges = parse_document_highlight_result(&response);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].start.line, 0);
        assert_eq!(ranges[1].start.line, 2);
    }

    #[test]
    fn parse_document_highlight_result_caps_an_oversized_array() {
        let entry = json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        });
        let text = references_response_text(
            3,
            Value::Array(std::iter::repeat_n(entry, MAX_LOCATIONS_PER_MESSAGE + 500).collect()),
        );
        let response = parse_message(&text);
        assert_eq!(
            parse_document_highlight_result(&response).len(),
            MAX_LOCATIONS_PER_MESSAGE
        );
    }

    #[test]
    fn bounded_document_highlights_rejects_a_non_array_top_level_value() {
        for text in ["null", "{}", "\"not an array\"", "42"] {
            assert!(
                serde_json::from_str::<BoundedDocumentHighlights>(text).is_err(),
                "expected {text:?} to be rejected as a non-array top-level value"
            );
        }
    }

    #[test]
    fn bounded_document_highlights_ignores_malformed_entries_beyond_the_cap_without_erroring() {
        let valid_entry = json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        });
        let mut entries: Vec<Value> =
            std::iter::repeat_n(valid_entry, MAX_LOCATIONS_PER_MESSAGE).collect();
        entries.extend(std::iter::repeat_n(
            Value::String("not a highlight".into()),
            500,
        ));
        let text = serde_json::to_string(&Value::Array(entries)).unwrap();

        let bounded = serde_json::from_str::<BoundedDocumentHighlights>(&text).unwrap();
        assert_eq!(bounded.0.len(), MAX_LOCATIONS_PER_MESSAGE);
    }

    // ---- InlayHint ----

    #[test]
    fn handle_inlay_hint_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_inlay_hint = Some((3, PathBuf::from("/x/main.rs")));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(handle_inlay_hint_response(&message, &mut state), None);
        assert!(state.pending_inlay_hint.is_some());
    }

    #[test]
    fn handle_inlay_hint_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_inlay_hint = Some((4, PathBuf::from("/x/main.rs")));

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(handle_inlay_hint_response(&stale, &mut state), None);
        assert_eq!(
            state.pending_inlay_hint,
            Some((4, PathBuf::from("/x/main.rs")))
        );
    }

    #[test]
    fn handle_inlay_hint_response_matches_pending_id_clears_it_and_carries_the_stored_path() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_inlay_hint = Some((3, path.clone()));

        let text = references_response_text(
            3,
            json!([{
                "position": {"line": 0, "character": 5},
                "label": ": i32",
                "paddingLeft": true,
            }]),
        );
        let response = parse_message(&text);

        let event = handle_inlay_hint_response(&response, &mut state);
        assert_eq!(state.pending_inlay_hint, None);
        match event {
            Some(LspEvent::InlayHint {
                path: event_path,
                hints,
            }) => {
                assert_eq!(event_path, path);
                assert_eq!(hints.len(), 1);
                assert_eq!(hints[0].label, ": i32");
                assert!(hints[0].padding_left);
                assert!(!hints[0].padding_right);
            }
            other => panic!("expected an InlayHint event, got {other:?}"),
        }
    }

    #[test]
    fn handle_inlay_hint_response_delivers_empty_hints_for_a_json_rpc_error() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_inlay_hint = Some((3, path.clone()));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_inlay_hint_response(&response, &mut state) {
            Some(LspEvent::InlayHint {
                path: event_path,
                hints,
            }) => {
                assert_eq!(event_path, path);
                assert!(hints.is_empty());
            }
            other => panic!("expected an empty InlayHint event, got {other:?}"),
        }
        assert_eq!(state.pending_inlay_hint, None);
    }

    #[test]
    fn parse_inlay_hint_result_null_result_is_empty() {
        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);
        assert!(parse_inlay_hint_result(&response).is_empty());
    }

    #[test]
    fn parse_inlay_hint_result_missing_result_field_is_empty() {
        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);
        assert!(parse_inlay_hint_result(&response).is_empty());
    }

    #[test]
    fn parse_inlay_hint_result_malformed_shape_is_empty() {
        let text = references_response_text(3, json!({ "not": "an array" }));
        let response = parse_message(&text);
        assert!(parse_inlay_hint_result(&response).is_empty());
    }

    #[test]
    fn parse_inlay_hint_result_parses_a_string_label() {
        let text = references_response_text(
            3,
            json!([{ "position": {"line": 0, "character": 0}, "label": "x" }]),
        );
        let response = parse_message(&text);
        let hints = parse_inlay_hint_result(&response);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].label, "x");
    }

    #[test]
    fn parse_inlay_hint_result_concatenates_label_parts() {
        let text = references_response_text(
            3,
            json!([{
                "position": {"line": 0, "character": 0},
                "label": [{"value": "x: "}, {"value": "i32"}],
            }]),
        );
        let response = parse_message(&text);
        let hints = parse_inlay_hint_result(&response);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].label, "x: i32");
    }

    #[test]
    fn parse_inlay_hint_result_defaults_padding_to_false_when_absent() {
        let text = references_response_text(
            3,
            json!([{ "position": {"line": 0, "character": 0}, "label": "x" }]),
        );
        let response = parse_message(&text);
        let hints = parse_inlay_hint_result(&response);
        assert!(!hints[0].padding_left);
        assert!(!hints[0].padding_right);
    }

    #[test]
    fn parse_inlay_hint_result_respects_explicit_padding() {
        let text = references_response_text(
            3,
            json!([{
                "position": {"line": 0, "character": 0},
                "label": "x",
                "paddingLeft": true,
                "paddingRight": true,
            }]),
        );
        let response = parse_message(&text);
        let hints = parse_inlay_hint_result(&response);
        assert!(hints[0].padding_left);
        assert!(hints[0].padding_right);
    }

    #[test]
    fn parse_inlay_hint_result_caps_an_oversized_array() {
        let entry = json!({ "position": {"line": 0, "character": 0}, "label": "x" });
        let text = references_response_text(
            3,
            Value::Array(std::iter::repeat_n(entry, MAX_LOCATIONS_PER_MESSAGE + 500).collect()),
        );
        let response = parse_message(&text);
        assert_eq!(
            parse_inlay_hint_result(&response).len(),
            MAX_LOCATIONS_PER_MESSAGE
        );
    }

    #[test]
    fn bounded_inlay_hints_rejects_a_non_array_top_level_value() {
        for text in ["null", "{}", "\"not an array\"", "42"] {
            assert!(
                serde_json::from_str::<BoundedInlayHints>(text).is_err(),
                "expected {text:?} to be rejected as a non-array top-level value"
            );
        }
    }

    #[test]
    fn bounded_inlay_hints_ignores_malformed_entries_beyond_the_cap_without_erroring() {
        let valid_entry = json!({ "position": {"line": 0, "character": 0}, "label": "x" });
        let mut entries: Vec<Value> =
            std::iter::repeat_n(valid_entry, MAX_LOCATIONS_PER_MESSAGE).collect();
        entries.extend(std::iter::repeat_n(Value::String("not a hint".into()), 500));
        let text = serde_json::to_string(&Value::Array(entries)).unwrap();

        let bounded = serde_json::from_str::<BoundedInlayHints>(&text).unwrap();
        assert_eq!(bounded.0.len(), MAX_LOCATIONS_PER_MESSAGE);
    }

    #[test]
    fn connection_state_new_has_no_pending_hover_highlight_or_inlay_hint() {
        let state = ConnectionState::new();
        assert_eq!(state.pending_hover_id, None);
        assert_eq!(state.pending_document_highlight_id, None);
        assert_eq!(state.pending_inlay_hint, None);
    }

    #[test]
    fn a_hover_request_and_a_document_highlight_request_use_independent_slots() {
        // Unlike Goto's three kinds (deliberately shared slot), Hover and
        // DocumentHighlight must never cancel each other's in-flight query.
        let mut state = ConnectionState::new();
        let hover_id = state.allocate_request_id();
        state.pending_hover_id = Some(hover_id);
        let highlight_id = state.allocate_request_id();
        state.pending_document_highlight_id = Some(highlight_id);

        assert_eq!(state.pending_hover_id, Some(hover_id));
        assert_eq!(state.pending_document_highlight_id, Some(highlight_id));

        let hover_text = references_response_text(hover_id, Value::Null);
        let hover_response = parse_message(&hover_text);
        assert!(handle_hover_response(&hover_response, &mut state).is_some());
        // The still-outstanding DocumentHighlight query must be untouched.
        assert_eq!(state.pending_document_highlight_id, Some(highlight_id));
    }

    // ---- code-actions helpers ----

    fn range_json(sl: u32, sc: u32, el: u32, ec: u32) -> Value {
        json!({ "start": {"line": sl, "character": sc}, "end": {"line": el, "character": ec} })
    }

    fn raw_workspace_edit(value: Value) -> lsp_types::WorkspaceEdit {
        serde_json::from_value(value).unwrap()
    }

    fn resolve_provider_from_json(text: &str) -> bool {
        serde_json::from_str::<InitializeResultCapabilities>(text)
            .ok()
            .map(|r| r.resolve_provider())
            .unwrap_or(false)
    }

    // ---- initialize capability parsing (resolveProvider) ----

    #[test]
    fn resolve_provider_true_for_explicit_code_action_options() {
        assert!(resolve_provider_from_json(
            r#"{"capabilities":{"codeActionProvider":{"resolveProvider":true}}}"#
        ));
    }

    #[test]
    fn resolve_provider_false_for_code_action_options_with_resolve_provider_false() {
        assert!(!resolve_provider_from_json(
            r#"{"capabilities":{"codeActionProvider":{"resolveProvider":false}}}"#
        ));
    }

    #[test]
    fn resolve_provider_false_for_bare_true_boolean() {
        assert!(!resolve_provider_from_json(
            r#"{"capabilities":{"codeActionProvider":true}}"#
        ));
    }

    #[test]
    fn resolve_provider_false_when_code_action_provider_absent() {
        assert!(!resolve_provider_from_json(r#"{"capabilities":{}}"#));
    }

    #[test]
    fn resolve_provider_false_for_malformed_or_missing_capabilities() {
        for text in [
            r#"{"capabilities":{"codeActionProvider":"nonsense"}}"#,
            r#"{"capabilities":{"codeActionProvider":42}}"#,
            "not json at all",
            "{}",
        ] {
            assert!(
                !resolve_provider_from_json(text),
                "expected {text:?} to fail closed to false"
            );
        }
    }

    #[test]
    fn apply_edit_params_deserializes_label_and_edit() {
        let value = json!({ "label": "Organize imports", "edit": { "changes": {} } });
        let params: ApplyEditParams = serde_json::from_value(value).unwrap();
        assert_eq!(params.label, Some("Organize imports".to_string()));
    }

    #[test]
    fn apply_edit_params_label_defaults_to_none_when_absent() {
        let value = json!({ "edit": { "changes": {} } });
        let params: ApplyEditParams = serde_json::from_value(value).unwrap();
        assert_eq!(params.label, None);
    }

    // ---- CodeAction ----

    #[test]
    fn handle_code_action_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_code_action_id = Some((3, PathBuf::from("/x/main.rs")));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(handle_code_action_response(&message, &mut state), None);
        assert!(state.pending_code_action_id.is_some());
    }

    #[test]
    fn handle_code_action_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_code_action_id = Some((4, PathBuf::from("/x/main.rs")));

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(handle_code_action_response(&stale, &mut state), None);
        assert_eq!(
            state.pending_code_action_id,
            Some((4, PathBuf::from("/x/main.rs")))
        );
    }

    #[test]
    fn handle_code_action_response_matches_pending_id_clears_it_carries_path_and_caches_raw_entries(
    ) {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_code_action_id = Some((3, path.clone()));

        let text = references_response_text(
            3,
            json!([
                { "title": "Bare command", "command": "noop.command" },
                { "title": "Direct edit", "edit": { "changes": {} } },
            ]),
        );
        let response = parse_message(&text);

        let event = handle_code_action_response(&response, &mut state);
        assert_eq!(state.pending_code_action_id, None);
        match event {
            Some(LspEvent::CodeAction {
                path: event_path,
                actions,
            }) => {
                assert_eq!(event_path, path);
                assert_eq!(actions.len(), 2);
                assert_eq!(actions[0].title, "Bare command");
                assert_eq!(actions[1].title, "Direct edit");
            }
            other => panic!("expected a CodeAction event, got {other:?}"),
        }
        assert_eq!(state.last_code_actions.len(), 2);
    }

    #[test]
    fn handle_code_action_response_delivers_empty_actions_for_a_json_rpc_error() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_code_action_id = Some((3, path.clone()));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_code_action_response(&response, &mut state) {
            Some(LspEvent::CodeAction {
                path: event_path,
                actions,
            }) => {
                assert_eq!(event_path, path);
                assert!(actions.is_empty());
            }
            other => panic!("expected an empty CodeAction event, got {other:?}"),
        }
        assert_eq!(state.pending_code_action_id, None);
        assert!(state.last_code_actions.is_empty());
    }

    #[test]
    fn parse_code_action_result_null_result_is_empty() {
        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);
        let (actions, raw) = parse_code_action_result(&response, true);
        assert!(actions.is_empty());
        assert!(raw.is_empty());
    }

    #[test]
    fn parse_code_action_result_missing_result_field_is_empty() {
        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);
        let (actions, raw) = parse_code_action_result(&response, true);
        assert!(actions.is_empty());
        assert!(raw.is_empty());
    }

    #[test]
    fn parse_code_action_result_malformed_shape_is_empty() {
        let text = references_response_text(3, json!({ "not": "an array" }));
        let response = parse_message(&text);
        let (actions, raw) = parse_code_action_result(&response, true);
        assert!(actions.is_empty());
        assert!(raw.is_empty());
    }

    #[test]
    fn parse_code_action_result_bare_command_entry_has_no_edit_and_is_not_resolvable() {
        let text = references_response_text(
            3,
            json!([{ "title": "Format document", "command": "editor.format", "arguments": [] }]),
        );
        let response = parse_message(&text);
        let (actions, raw) = parse_code_action_result(&response, true);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].title, "Format document");
        assert_eq!(actions[0].kind, None);
        assert!(!actions[0].is_preferred);
        assert_eq!(actions[0].disabled_reason, None);
        assert!(raw[0].edit.is_none());
        assert!(!raw[0].resolvable);
    }

    #[test]
    fn parse_code_action_result_code_action_with_edit_is_applied_directly_not_resolvable() {
        let text = references_response_text(
            3,
            json!([{
                "title": "Add missing import",
                "kind": "quickfix",
                "isPreferred": true,
                "edit": { "changes": {} },
                "data": { "marker": "would-need-resolve-if-no-edit" },
            }]),
        );
        let response = parse_message(&text);
        let (actions, raw) = parse_code_action_result(&response, true);
        assert_eq!(actions[0].title, "Add missing import");
        assert_eq!(actions[0].kind.as_deref(), Some("quickfix"));
        assert!(actions[0].is_preferred);
        assert!(raw[0].edit.is_some());
        assert!(!raw[0].resolvable);
    }

    #[test]
    fn parse_code_action_result_no_edit_with_data_is_resolvable_only_when_server_supports_resolve()
    {
        let text = references_response_text(
            3,
            json!([{ "title": "Extract function", "data": { "id": 1 } }]),
        );
        let response = parse_message(&text);

        let (_, raw_supported) = parse_code_action_result(&response, true);
        assert!(raw_supported[0].edit.is_none());
        assert!(raw_supported[0].resolvable);

        let (_, raw_unsupported) = parse_code_action_result(&response, false);
        assert!(!raw_unsupported[0].resolvable);
    }

    #[test]
    fn parse_code_action_result_carries_disabled_reason() {
        let text = references_response_text(
            3,
            json!([{ "title": "Rename", "disabled": { "reason": "ambiguous symbol" } }]),
        );
        let response = parse_message(&text);
        let (actions, _) = parse_code_action_result(&response, true);
        assert_eq!(
            actions[0].disabled_reason.as_deref(),
            Some("ambiguous symbol")
        );
    }

    #[test]
    fn parse_code_action_result_caps_an_oversized_array() {
        let entry = json!({ "title": "x", "command": "noop" });
        let text = references_response_text(
            3,
            Value::Array(std::iter::repeat_n(entry, MAX_LOCATIONS_PER_MESSAGE + 500).collect()),
        );
        let response = parse_message(&text);
        let (actions, raw) = parse_code_action_result(&response, true);
        assert_eq!(actions.len(), MAX_LOCATIONS_PER_MESSAGE);
        assert_eq!(raw.len(), MAX_LOCATIONS_PER_MESSAGE);
    }

    #[test]
    fn bounded_code_actions_rejects_a_non_array_top_level_value() {
        for text in ["null", "{}", "\"not an array\"", "42"] {
            assert!(
                serde_json::from_str::<BoundedCodeActions>(text).is_err(),
                "expected {text:?} to be rejected as a non-array top-level value"
            );
        }
    }

    #[test]
    fn bounded_code_actions_ignores_malformed_entries_beyond_the_cap_without_erroring() {
        let valid_entry = json!({ "title": "x", "command": "noop" });
        let mut entries: Vec<Value> =
            std::iter::repeat_n(valid_entry, MAX_LOCATIONS_PER_MESSAGE).collect();
        entries.extend(std::iter::repeat_n(Value::Number(42.into()), 500));
        let text = serde_json::to_string(&Value::Array(entries)).unwrap();

        let bounded = serde_json::from_str::<BoundedCodeActions>(&text).unwrap();
        assert_eq!(bounded.0.len(), MAX_LOCATIONS_PER_MESSAGE);
    }

    #[test]
    fn action_title_reads_the_title_field_from_raw_json() {
        let raw = json!({ "title": "Fix it", "command": "x" });
        assert_eq!(action_title(&raw), Some("Fix it".to_string()));
    }

    #[test]
    fn action_title_is_none_when_title_is_missing_or_not_a_string() {
        assert_eq!(action_title(&json!({})), None);
        assert_eq!(action_title(&json!({ "title": 42 })), None);
    }

    // ---- codeAction/resolve ----

    #[test]
    fn handle_resolve_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_resolve_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(
            handle_resolve_response(&message, root.path(), &mut state),
            None
        );
        assert_eq!(state.pending_resolve_id, Some(3));
    }

    #[test]
    fn handle_resolve_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_resolve_id = Some(4);
        let root = tempfile::tempdir().unwrap();

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(
            handle_resolve_response(&stale, root.path(), &mut state),
            None
        );
        assert_eq!(state.pending_resolve_id, Some(4));
    }

    #[test]
    fn handle_resolve_response_delivers_edit_none_for_a_json_rpc_error() {
        let mut state = ConnectionState::new();
        state.pending_resolve_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_resolve_response(&response, root.path(), &mut state) {
            Some(LspEvent::WorkspaceEditReady { edit, label }) => {
                assert_eq!(edit, None);
                assert_eq!(label, None);
            }
            other => panic!("expected an empty WorkspaceEditReady event, got {other:?}"),
        }
        assert_eq!(state.pending_resolve_id, None);
    }

    #[test]
    fn handle_resolve_response_delivers_edit_none_for_missing_result() {
        let mut state = ConnectionState::new();
        state.pending_resolve_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);

        match handle_resolve_response(&response, root.path(), &mut state) {
            Some(LspEvent::WorkspaceEditReady { edit, label }) => {
                assert_eq!(edit, None);
                assert_eq!(label, None);
            }
            other => panic!("expected an empty WorkspaceEditReady event, got {other:?}"),
        }
    }

    #[test]
    fn handle_resolve_response_delivers_edit_none_for_a_malformed_result() {
        let mut state = ConnectionState::new();
        state.pending_resolve_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = references_response_text(3, json!({ "not": "a code action" }));
        let response = parse_message(&text);

        match handle_resolve_response(&response, root.path(), &mut state) {
            Some(LspEvent::WorkspaceEditReady { edit, label }) => {
                assert_eq!(edit, None);
                assert_eq!(label, None);
            }
            other => panic!("expected an empty WorkspaceEditReady event, got {other:?}"),
        }
    }

    #[test]
    fn handle_resolve_response_converts_the_resolved_edit_and_sets_label_to_its_title() {
        let (_dir, root, main_rs) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_resolve_id = Some(3);
        let uri = Url::from_file_path(&main_rs).unwrap().to_string();

        let text = references_response_text(
            3,
            json!({
                "title": "Extract function",
                "edit": { "changes": { (uri): [
                    { "range": range_json(0, 0, 0, 0), "newText": "fn extracted() {}\n" }
                ] } },
            }),
        );
        let response = parse_message(&text);

        match handle_resolve_response(&response, &root, &mut state) {
            Some(LspEvent::WorkspaceEditReady { edit, label }) => {
                assert_eq!(label, Some("Extract function".to_string()));
                let edit = edit.expect("expected a converted edit");
                assert_eq!(edit.edits.len(), 1);
                assert_eq!(edit.edits[0].path, main_rs);
            }
            other => panic!("expected a WorkspaceEditReady event with an edit, got {other:?}"),
        }
        assert_eq!(state.pending_resolve_id, None);
    }

    #[test]
    fn handle_resolve_response_still_reports_the_title_when_resolve_yields_no_edit() {
        let mut state = ConnectionState::new();
        state.pending_resolve_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = references_response_text(3, json!({ "title": "No-op action" }));
        let response = parse_message(&text);

        match handle_resolve_response(&response, root.path(), &mut state) {
            Some(LspEvent::WorkspaceEditReady { edit, label }) => {
                assert_eq!(edit, None);
                assert_eq!(label, Some("No-op action".to_string()));
            }
            other => panic!("expected an edit-less WorkspaceEditReady event, got {other:?}"),
        }
    }

    // ---- convert_workspace_edit ----

    #[test]
    fn convert_workspace_edit_with_neither_changes_nor_document_changes_is_a_no_op_edit() {
        let root = tempfile::tempdir().unwrap();
        let raw = raw_workspace_edit(json!({}));
        let converted =
            convert_workspace_edit(root.path(), raw).expect("expected Some(empty edit)");
        assert!(converted.edits.is_empty());
    }

    #[test]
    fn convert_workspace_edit_prefers_document_changes_over_changes_when_both_present() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let mut changes = serde_json::Map::new();
        changes.insert(
            uri.clone(),
            json!([{ "range": range_json(0, 0, 0, 0), "newText": "from-changes" }]),
        );
        let raw = raw_workspace_edit(json!({
            "changes": Value::Object(changes),
            "documentChanges": [{
                "textDocument": { "uri": uri, "version": null },
                "edits": [{ "range": range_json(0, 0, 0, 0), "newText": "from-document-changes" }],
            }],
        }));

        let converted = convert_workspace_edit(&root, raw).unwrap();
        assert_eq!(converted.edits.len(), 1);
        assert_eq!(
            converted.edits[0].text_edits[0].new_text,
            "from-document-changes"
        );
    }

    #[test]
    fn convert_workspace_edit_document_changes_edits_variant_converts_text_edits() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let raw = raw_workspace_edit(json!({
            "documentChanges": [{
                "textDocument": { "uri": uri, "version": 1 },
                "edits": [
                    { "range": range_json(0, 0, 0, 0), "newText": "one" },
                    { "range": range_json(1, 0, 1, 0), "newText": "two" },
                ],
            }],
        }));

        let converted = convert_workspace_edit(&root, raw).unwrap();
        assert_eq!(converted.edits.len(), 1);
        assert_eq!(converted.edits[0].path, file);
        assert_eq!(converted.edits[0].text_edits.len(), 2);
        assert_eq!(converted.edits[0].text_edits[0].new_text, "one");
        assert_eq!(converted.edits[0].text_edits[1].new_text, "two");
    }

    #[test]
    fn convert_workspace_edit_unwraps_annotated_text_edits() {
        // Built as Rust struct literals, not JSON: `OneOf`'s untagged
        // deserialize always matches `Left` (`TextEdit`) first, and an
        // `AnnotatedTextEdit`'s JSON shape is a strict superset of
        // `TextEdit`'s (the extra `annotationId` field just gets ignored)
        // -- so there is no JSON payload that round-trips into `Right`.
        // Constructing the value directly is the only way to exercise
        // that arm.
        let (_dir, root, file) = project_with_file();
        let uri: lsp_types::Uri = Url::from_file_path(&file)
            .unwrap()
            .to_string()
            .parse()
            .unwrap();
        let annotated = lsp_types::AnnotatedTextEdit {
            text_edit: lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 0),
                ),
                new_text: "annotated".to_string(),
            },
            annotation_id: "ann-1".to_string(),
        };
        let tde = lsp_types::TextDocumentEdit {
            text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                uri,
                version: None,
            },
            edits: vec![lsp_types::OneOf::Right(annotated)],
        };
        let raw = lsp_types::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp_types::DocumentChanges::Edits(vec![tde])),
            change_annotations: None,
        };

        let converted = convert_workspace_edit(&root, raw).unwrap();
        assert_eq!(converted.edits[0].text_edits[0].new_text, "annotated");
    }

    #[test]
    fn convert_workspace_edit_resource_operation_anywhere_fails_the_whole_batch() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let raw = raw_workspace_edit(json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": uri.clone(), "version": null },
                    "edits": [{ "range": range_json(0, 0, 0, 0), "newText": "ok" }],
                },
                { "kind": "create", "uri": format!("{uri}.new") },
            ],
        }));

        assert!(convert_workspace_edit(&root, raw).is_none());
    }

    #[test]
    fn convert_workspace_edit_a_bad_path_among_document_changes_fails_the_whole_batch() {
        let (_dir, root, file) = project_with_file();
        let good_uri = Url::from_file_path(&file).unwrap().to_string();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = fs::canonicalize(outside_dir.path())
            .unwrap()
            .join("evil.rs");
        fs::write(&outside_file, "").unwrap();
        let bad_uri = Url::from_file_path(&outside_file).unwrap().to_string();

        let raw = raw_workspace_edit(json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": good_uri, "version": null },
                    "edits": [{ "range": range_json(0, 0, 0, 0), "newText": "ok" }],
                },
                {
                    "textDocument": { "uri": bad_uri, "version": null },
                    "edits": [{ "range": range_json(0, 0, 0, 0), "newText": "escape" }],
                },
            ],
        }));

        assert!(convert_workspace_edit(&root, raw).is_none());
    }

    #[test]
    fn convert_workspace_edit_a_non_file_uri_fails_the_whole_batch() {
        let (_dir, root, file) = project_with_file();
        let good_uri = Url::from_file_path(&file).unwrap().to_string();
        let raw = raw_workspace_edit(json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": good_uri, "version": null },
                    "edits": [{ "range": range_json(0, 0, 0, 0), "newText": "ok" }],
                },
                {
                    "textDocument": { "uri": "https://example.com/not-a-file", "version": null },
                    "edits": [{ "range": range_json(0, 0, 0, 0), "newText": "nope" }],
                },
            ],
        }));

        assert!(convert_workspace_edit(&root, raw).is_none());
    }

    #[test]
    fn convert_workspace_edit_changes_map_converts_text_edits() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let mut changes = serde_json::Map::new();
        changes.insert(
            uri,
            json!([{ "range": range_json(0, 0, 0, 0), "newText": "via-changes" }]),
        );
        let raw = raw_workspace_edit(json!({ "changes": Value::Object(changes) }));

        let converted = convert_workspace_edit(&root, raw).unwrap();
        assert_eq!(converted.edits.len(), 1);
        assert_eq!(converted.edits[0].path, file);
        assert_eq!(converted.edits[0].text_edits[0].new_text, "via-changes");
    }

    #[test]
    fn convert_workspace_edit_rejects_a_document_changes_array_over_the_cap() {
        // Regression coverage for the hacker-pass finding that
        // `documentChanges` (a plain `Vec`, not deduplicated by URI like
        // `changes`) let a malicious server force an unbounded number of
        // `validate_path` calls by repeating one entry
        // (docs/security-findings/rust-lsp-dev-code-actions-*.md finding
        // 1). The entries don't need to be individually valid -- the cap
        // check runs before any URI parsing/path validation.
        let entry = json!({
            "textDocument": { "uri": "file:///does/not/matter", "version": null },
            "edits": [{ "range": range_json(0, 0, 0, 0), "newText": "x" }],
        });
        let raw = raw_workspace_edit(json!({
            "documentChanges": std::iter::repeat_n(entry, MAX_LOCATIONS_PER_MESSAGE + 1)
                .collect::<Vec<_>>(),
        }));

        let root = tempfile::tempdir().unwrap();
        assert!(convert_workspace_edit(root.path(), raw).is_none());
    }

    #[test]
    fn convert_workspace_edit_rejects_a_changes_map_over_the_cap() {
        let mut changes = serde_json::Map::new();
        for i in 0..(MAX_LOCATIONS_PER_MESSAGE + 1) {
            changes.insert(
                format!("file:///does/not/matter/{i}"),
                json!([{ "range": range_json(0, 0, 0, 0), "newText": "x" }]),
            );
        }
        let raw = raw_workspace_edit(json!({ "changes": Value::Object(changes) }));

        let root = tempfile::tempdir().unwrap();
        assert!(convert_workspace_edit(root.path(), raw).is_none());
    }

    #[test]
    fn convert_workspace_edit_a_bad_path_in_the_changes_map_fails_the_whole_batch() {
        let (_dir, root, file) = project_with_file();
        let good_uri = Url::from_file_path(&file).unwrap().to_string();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = fs::canonicalize(outside_dir.path())
            .unwrap()
            .join("evil.rs");
        fs::write(&outside_file, "").unwrap();
        let bad_uri = Url::from_file_path(&outside_file).unwrap().to_string();

        let mut changes = serde_json::Map::new();
        changes.insert(
            good_uri,
            json!([{ "range": range_json(0, 0, 0, 0), "newText": "ok" }]),
        );
        changes.insert(
            bad_uri,
            json!([{ "range": range_json(0, 0, 0, 0), "newText": "escape" }]),
        );
        let raw = raw_workspace_edit(json!({ "changes": Value::Object(changes) }));

        assert!(convert_workspace_edit(&root, raw).is_none());
    }

    // ---- cross-slot independence ----

    #[test]
    fn connection_state_new_has_no_pending_code_action_or_resolve() {
        let state = ConnectionState::new();
        assert_eq!(state.pending_code_action_id, None);
        assert_eq!(state.pending_resolve_id, None);
        assert!(!state.code_action_resolve_provider);
        assert!(state.last_code_actions.is_empty());
    }

    #[test]
    fn a_code_action_request_and_a_resolve_request_use_slots_independent_of_each_other_and_of_hover(
    ) {
        let mut state = ConnectionState::new();
        let hover_id = state.allocate_request_id();
        state.pending_hover_id = Some(hover_id);
        let code_action_id = state.allocate_request_id();
        state.pending_code_action_id = Some((code_action_id, PathBuf::from("/x/main.rs")));
        let resolve_id = state.allocate_request_id();
        state.pending_resolve_id = Some(resolve_id);

        let code_action_text = references_response_text(code_action_id, json!([]));
        let code_action_response = parse_message(&code_action_text);
        assert!(handle_code_action_response(&code_action_response, &mut state).is_some());
        assert_eq!(state.pending_code_action_id, None);
        assert_eq!(state.pending_hover_id, Some(hover_id));
        assert_eq!(state.pending_resolve_id, Some(resolve_id));

        let root = tempfile::tempdir().unwrap();
        let resolve_text = references_response_text(resolve_id, json!({ "title": "x" }));
        let resolve_response = parse_message(&resolve_text);
        assert!(handle_resolve_response(&resolve_response, root.path(), &mut state).is_some());
        assert_eq!(state.pending_resolve_id, None);
        assert_eq!(state.pending_hover_id, Some(hover_id));
    }

    // ---- OrganizeImports ----

    #[test]
    fn connection_state_new_has_no_pending_organize_imports() {
        let state = ConnectionState::new();
        assert_eq!(state.pending_organize_imports_id, None);
        assert_eq!(state.pending_organize_imports_resolve_id, None);
    }

    #[test]
    fn parse_organize_imports_response_is_empty_for_an_empty_array() {
        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(3, json!([]));
        let message = parse_message(&text);
        assert!(matches!(
            parse_organize_imports_response(&message, root.path(), true),
            OrganizeImportsOutcome::Empty
        ));
    }

    #[test]
    fn parse_organize_imports_response_is_empty_for_a_json_rpc_error() {
        let root = tempfile::tempdir().unwrap();
        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "boom" },
        }))
        .unwrap();
        let message = parse_message(&text);
        assert!(matches!(
            parse_organize_imports_response(&message, root.path(), true),
            OrganizeImportsOutcome::Empty
        ));
    }

    #[test]
    fn parse_organize_imports_response_is_empty_for_a_bare_command() {
        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(
            3,
            json!([{ "title": "Bare command", "command": "noop.command" }]),
        );
        let message = parse_message(&text);
        assert!(matches!(
            parse_organize_imports_response(&message, root.path(), true),
            OrganizeImportsOutcome::Empty
        ));
    }

    #[test]
    fn parse_organize_imports_response_converts_the_first_entrys_direct_edit_and_ignores_the_rest()
    {
        let (_dir, root, main_rs) = project_with_file();
        let uri = Url::from_file_path(&main_rs).unwrap().to_string();
        let text = references_response_text(
            3,
            json!([
                { "title": "Organize imports", "edit": { "changes": { (uri): [
                    { "range": range_json(0, 0, 0, 0), "newText": "use a::b;\n" }
                ] } } },
                { "title": "Second entry, never inspected", "command": "noop.command" },
            ]),
        );
        let message = parse_message(&text);
        match parse_organize_imports_response(&message, &root, true) {
            OrganizeImportsOutcome::Ready(Some(edit)) => {
                assert_eq!(edit.edits.len(), 1);
                assert_eq!(edit.edits[0].path, main_rs);
            }
            other => panic!("expected Ready(Some(..)), got {other:?}"),
        }
    }

    #[test]
    fn parse_organize_imports_response_reports_ready_none_when_the_edit_escapes_the_project_root() {
        let root_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let outside_file = fs::canonicalize(outside_dir.path())
            .unwrap()
            .join("secret.rs");
        fs::write(&outside_file, "").unwrap();
        let uri = Url::from_file_path(&outside_file).unwrap().to_string();

        let text = references_response_text(
            3,
            json!([{ "title": "Organize imports", "edit": { "changes": { (uri): [
                { "range": range_json(0, 0, 0, 0), "newText": "escape" }
            ] } } }]),
        );
        let message = parse_message(&text);
        match parse_organize_imports_response(&message, &root, true) {
            OrganizeImportsOutcome::Ready(None) => {}
            other => panic!("expected Ready(None), got {other:?}"),
        }
    }

    #[test]
    fn parse_organize_imports_response_needs_resolve_when_unresolved_and_resolve_provider_is_true()
    {
        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(
            3,
            json!([{ "title": "Organize imports", "data": { "id": "organize" } }]),
        );
        let message = parse_message(&text);
        match parse_organize_imports_response(&message, root.path(), true) {
            OrganizeImportsOutcome::NeedsResolve(raw) => {
                assert_eq!(raw["title"], "Organize imports");
                assert_eq!(raw["data"]["id"], "organize");
            }
            other => panic!("expected NeedsResolve, got {other:?}"),
        }
    }

    #[test]
    fn parse_organize_imports_response_is_empty_when_unresolved_and_resolve_provider_is_false() {
        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(
            3,
            json!([{ "title": "Organize imports", "data": { "id": "organize" } }]),
        );
        let message = parse_message(&text);
        assert!(matches!(
            parse_organize_imports_response(&message, root.path(), false),
            OrganizeImportsOutcome::Empty
        ));
    }

    #[test]
    fn handle_organize_imports_resolve_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_organize_imports_resolve_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(
            handle_organize_imports_resolve_response(&message, root.path(), &mut state),
            None
        );
        assert!(state.pending_organize_imports_resolve_id.is_some());
    }

    #[test]
    fn handle_organize_imports_resolve_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_organize_imports_resolve_id = Some(4);
        let root = tempfile::tempdir().unwrap();

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(
            handle_organize_imports_resolve_response(&stale, root.path(), &mut state),
            None
        );
        assert_eq!(state.pending_organize_imports_resolve_id, Some(4));
    }

    #[test]
    fn handle_organize_imports_resolve_response_delivers_edit_none_for_a_json_rpc_error() {
        let mut state = ConnectionState::new();
        state.pending_organize_imports_resolve_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "boom" },
        }))
        .unwrap();
        let response = parse_message(&text);
        match handle_organize_imports_resolve_response(&response, root.path(), &mut state) {
            Some(LspEvent::WorkspaceEditReady { edit, label }) => {
                assert_eq!(edit, None);
                assert_eq!(label, None);
            }
            other => panic!("expected a WorkspaceEditReady event, got {other:?}"),
        }
        assert_eq!(state.pending_organize_imports_resolve_id, None);
    }

    #[test]
    fn handle_organize_imports_resolve_response_converts_the_resolved_edit_and_uses_the_fixed_label(
    ) {
        let (_dir, root, main_rs) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_organize_imports_resolve_id = Some(3);
        let uri = Url::from_file_path(&main_rs).unwrap().to_string();

        let text = references_response_text(
            3,
            json!({
                "title": "Whatever the server calls it internally",
                "edit": { "changes": { (uri): [
                    { "range": range_json(0, 0, 0, 0), "newText": "use a::b;\n" }
                ] } },
            }),
        );
        let response = parse_message(&text);

        match handle_organize_imports_resolve_response(&response, &root, &mut state) {
            Some(LspEvent::WorkspaceEditReady { edit, label }) => {
                assert_eq!(label, Some("Optimize Imports".to_string()));
                let edit = edit.expect("expected a converted edit");
                assert_eq!(edit.edits[0].path, main_rs);
            }
            other => panic!("expected a WorkspaceEditReady event with an edit, got {other:?}"),
        }
        assert_eq!(state.pending_organize_imports_resolve_id, None);
    }

    #[test]
    fn handle_organize_imports_resolve_response_pairs_none_label_with_none_edit_when_resolve_yields_no_edit(
    ) {
        let mut state = ConnectionState::new();
        state.pending_organize_imports_resolve_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = references_response_text(3, json!({ "title": "No-op action" }));
        let response = parse_message(&text);

        match handle_organize_imports_resolve_response(&response, root.path(), &mut state) {
            Some(LspEvent::WorkspaceEditReady { edit, label }) => {
                assert_eq!(edit, None);
                assert_eq!(label, None);
            }
            other => panic!("expected a WorkspaceEditReady event, got {other:?}"),
        }
    }

    #[test]
    fn organize_imports_slots_are_independent_of_code_action_and_its_own_resolve_slot() {
        let mut state = ConnectionState::new();
        let code_action_id = state.allocate_request_id();
        state.pending_code_action_id = Some((code_action_id, PathBuf::from("/x/main.rs")));
        let apply_resolve_id = state.allocate_request_id();
        state.pending_resolve_id = Some(apply_resolve_id);
        let organize_id = state.allocate_request_id();
        state.pending_organize_imports_id = Some(organize_id);
        let organize_resolve_id = state.allocate_request_id();
        state.pending_organize_imports_resolve_id = Some(organize_resolve_id);

        let root = tempfile::tempdir().unwrap();
        let resolve_text = references_response_text(organize_resolve_id, json!({ "title": "x" }));
        let resolve_response = parse_message(&resolve_text);
        assert!(handle_organize_imports_resolve_response(
            &resolve_response,
            root.path(),
            &mut state
        )
        .is_some());

        assert_eq!(state.pending_organize_imports_resolve_id, None);
        assert_eq!(
            state.pending_code_action_id,
            Some((code_action_id, PathBuf::from("/x/main.rs")))
        );
        assert_eq!(state.pending_resolve_id, Some(apply_resolve_id));
        assert_eq!(state.pending_organize_imports_id, Some(organize_id));
    }

    /// Exercises `send_request`'s real `OrganizeImports` arm end-to-end
    /// over a genuine child process's stdin (`cat`, which just echoes
    /// whatever it's fed back out on its own stdout) -- the params this
    /// crate actually writes to the wire, not a hand-built stand-in.
    #[test]
    fn send_request_organize_imports_writes_the_expected_whole_document_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        // Two lines, the second non-empty and single-character -- end
        // position must be {line: 1, character: 1}, not {0, 0} or the
        // byte length.
        let file = root.join("main.rs");
        fs::write(&file, "fn main() {}\nx").unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let body = runtime.block_on(async {
            let mut child = spawn_child("cat", &[], &root).await.unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let mut state = ConnectionState::new();
            let (event_tx, _event_rx) = mpsc::channel(8);

            send_request(
                &mut stdin,
                &root,
                LspRequest::OrganizeImports { path: file.clone() },
                &mut state,
                &event_tx,
            )
            .await
            .unwrap();
            assert!(state.pending_organize_imports_id.is_some());
            drop(stdin);

            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let body = match read_message(&mut stdout).await {
                ReadOutcome::Message(bytes) => bytes,
                _ => panic!("expected a well-framed message echoed back"),
            };
            let _ = child.wait().await;
            body
        });

        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["method"], "textDocument/codeAction");
        assert_eq!(
            value["params"]["context"]["only"],
            json!(["source.organizeImports"])
        );
        assert_eq!(
            value["params"]["range"]["start"],
            range_json(0, 0, 0, 0)["start"]
        );
        assert_eq!(value["params"]["range"]["end"]["line"], 1);
        assert_eq!(value["params"]["range"]["end"]["character"], 1);
    }

    #[test]
    fn send_request_organize_imports_emits_workspace_edit_ready_none_for_a_path_outside_the_project_root(
    ) {
        let root_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let outside_file = fs::canonicalize(outside_dir.path())
            .unwrap()
            .join("evil.rs");
        fs::write(&outside_file, "").unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let event = runtime.block_on(async {
            let mut child = spawn_child("cat", &[], &root).await.unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let mut state = ConnectionState::new();
            let (event_tx, mut event_rx) = mpsc::channel(8);

            send_request(
                &mut stdin,
                &root,
                LspRequest::OrganizeImports { path: outside_file },
                &mut state,
                &event_tx,
            )
            .await
            .unwrap();
            drop(stdin);
            let _ = child.wait().await;
            event_rx.try_recv().ok()
        });

        assert_eq!(
            event,
            Some(LspEvent::WorkspaceEditReady {
                edit: None,
                label: None
            })
        );
    }

    #[test]
    fn send_request_organize_imports_emits_workspace_edit_ready_none_when_the_path_is_not_a_readable_file(
    ) {
        // `validate_path` accepts a directory (it only checks
        // canonicalization + containment) -- `fs::read_to_string` then
        // fails, exercising this arm's own read-failure branch.
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let subdir = root.join("subdir");
        fs::create_dir(&subdir).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let event = runtime.block_on(async {
            let mut child = spawn_child("cat", &[], &root).await.unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let mut state = ConnectionState::new();
            let (event_tx, mut event_rx) = mpsc::channel(8);

            send_request(
                &mut stdin,
                &root,
                LspRequest::OrganizeImports { path: subdir },
                &mut state,
                &event_tx,
            )
            .await
            .unwrap();
            drop(stdin);
            let _ = child.wait().await;
            event_rx.try_recv().ok()
        });

        assert_eq!(
            event,
            Some(LspEvent::WorkspaceEditReady {
                edit: None,
                label: None
            })
        );
        // No pending id should be left dangling from a request that
        // never actually reached the wire.
    }

    // ---- DocumentSymbol ----

    #[test]
    fn handle_document_symbol_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_document_symbol_id = Some((3, PathBuf::from("/x/main.rs")));
        let root = tempfile::tempdir().unwrap();

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(
            handle_document_symbol_response(&message, root.path(), &mut state),
            None
        );
        assert!(state.pending_document_symbol_id.is_some());
    }

    #[test]
    fn handle_document_symbol_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_document_symbol_id = Some((4, PathBuf::from("/x/main.rs")));
        let root = tempfile::tempdir().unwrap();

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(
            handle_document_symbol_response(&stale, root.path(), &mut state),
            None
        );
        assert_eq!(
            state.pending_document_symbol_id,
            Some((4, PathBuf::from("/x/main.rs")))
        );
    }

    #[test]
    fn handle_document_symbol_response_matches_pending_id_clears_it_and_carries_the_stored_path() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_document_symbol_id = Some((3, path.clone()));
        let root = tempfile::tempdir().unwrap();

        let text = references_response_text(
            3,
            json!([{
                "name": "foo",
                "kind": 12,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
                "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
            }]),
        );
        let response = parse_message(&text);

        let event = handle_document_symbol_response(&response, root.path(), &mut state);
        assert_eq!(state.pending_document_symbol_id, None);
        match event {
            Some(LspEvent::DocumentSymbol {
                path: event_path,
                symbols,
            }) => {
                assert_eq!(event_path, path);
                assert_eq!(symbols.len(), 1);
                assert_eq!(symbols[0].name, "foo");
                assert_eq!(symbols[0].kind, SymbolKind::Function);
                assert_eq!(symbols[0].container_name, None);
            }
            other => panic!("expected a DocumentSymbol event, got {other:?}"),
        }
    }

    #[test]
    fn handle_document_symbol_response_delivers_empty_symbols_for_a_json_rpc_error() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_document_symbol_id = Some((3, path.clone()));
        let root = tempfile::tempdir().unwrap();

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_document_symbol_response(&response, root.path(), &mut state) {
            Some(LspEvent::DocumentSymbol {
                path: event_path,
                symbols,
            }) => {
                assert_eq!(event_path, path);
                assert!(symbols.is_empty());
            }
            other => panic!("expected an empty DocumentSymbol event, got {other:?}"),
        }
        assert_eq!(state.pending_document_symbol_id, None);
    }

    #[test]
    fn parse_document_symbol_result_null_result_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);
        assert!(
            parse_document_symbol_result(&response, root.path(), Path::new("/x/main.rs"))
                .is_empty()
        );
    }

    #[test]
    fn parse_document_symbol_result_missing_result_field_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);
        assert!(
            parse_document_symbol_result(&response, root.path(), Path::new("/x/main.rs"))
                .is_empty()
        );
    }

    #[test]
    fn parse_document_symbol_result_malformed_shape_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(3, json!({ "not": "an array" }));
        let response = parse_message(&text);
        assert!(
            parse_document_symbol_result(&response, root.path(), Path::new("/x/main.rs"))
                .is_empty()
        );
    }

    #[test]
    fn parse_document_symbol_result_flattens_hierarchical_children_with_container_names() {
        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(
            3,
            json!([{
                "name": "Outer",
                "kind": 5,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 10, "character": 0}},
                "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
                "children": [{
                    "name": "inner_method",
                    "kind": 6,
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}},
                    "selectionRange": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 12}},
                }],
            }]),
        );
        let response = parse_message(&text);
        let symbols = parse_document_symbol_result(&response, root.path(), Path::new("/x/main.rs"));
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "Outer");
        assert_eq!(symbols[0].kind, SymbolKind::Class);
        assert_eq!(symbols[0].container_name, None);
        assert_eq!(symbols[1].name, "inner_method");
        assert_eq!(symbols[1].kind, SymbolKind::Method);
        assert_eq!(symbols[1].container_name, Some("Outer".to_string()));
        assert_eq!(symbols[1].location.path, PathBuf::from("/x/main.rs"));
    }

    #[test]
    fn parse_document_symbol_result_falls_back_to_flat_symbol_information_shape() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap();

        let text = references_response_text(
            3,
            json!([{
                "name": "flat_symbol",
                "kind": 13,
                "location": {
                    "uri": uri.to_string(),
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                },
            }]),
        );
        let response = parse_message(&text);
        let symbols = parse_document_symbol_result(&response, &root, &file);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "flat_symbol");
        assert_eq!(symbols[0].kind, SymbolKind::Variable);
    }

    #[test]
    fn flatten_document_symbols_stops_at_the_cap_without_processing_remaining_siblings() {
        let child = |name: &str| {
            serde_json::from_value::<lsp_types::DocumentSymbol>(json!({
                "name": name,
                "kind": 13,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            }))
            .unwrap()
        };
        let entries: Vec<lsp_types::DocumentSymbol> = (0..MAX_SYMBOLS_PER_MESSAGE + 10)
            .map(|i| child(&format!("s{i}")))
            .collect();
        let mut out = Vec::new();
        flatten_document_symbols(entries, Path::new("/x/main.rs"), None, 0, &mut out);
        assert_eq!(out.len(), MAX_SYMBOLS_PER_MESSAGE);
    }

    #[test]
    fn flatten_document_symbols_stops_mid_children_once_the_cap_is_reached() {
        let leaf = |name: &str| {
            serde_json::from_value::<lsp_types::DocumentSymbol>(json!({
                "name": name,
                "kind": 13,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            }))
            .unwrap()
        };
        let mut parent = serde_json::from_value::<lsp_types::DocumentSymbol>(json!({
            "name": "parent",
            "kind": 5,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        }))
        .unwrap();
        let children: Vec<lsp_types::DocumentSymbol> =
            (0..10).map(|i| leaf(&format!("c{i}"))).collect();
        parent.children = Some(children);

        // `MAX_SYMBOLS_PER_MESSAGE - 1` filler entries first, so `parent`
        // is the entry that pushes `out` to exactly the cap -- the check
        // before descending into `parent`'s own `children` then sees the
        // cap already reached and skips them, along with any further
        // top-level siblings after `parent`.
        let mut entries: Vec<lsp_types::DocumentSymbol> = (0..MAX_SYMBOLS_PER_MESSAGE - 1)
            .map(|i| leaf(&format!("filler{i}")))
            .collect();
        entries.push(parent);
        entries.push(leaf("trailing_sibling"));

        let mut out = Vec::new();
        flatten_document_symbols(entries, Path::new("/x/main.rs"), None, 0, &mut out);
        assert_eq!(out.len(), MAX_SYMBOLS_PER_MESSAGE);
        assert!(!out.iter().any(|s| s.name.starts_with('c')));
        assert!(!out.iter().any(|s| s.name == "trailing_sibling"));
    }

    #[test]
    fn flatten_document_symbols_stops_at_max_symbol_tree_depth_independent_of_the_breadth_cap() {
        // 110 levels of single-child nesting, built directly as
        // `lsp_types::DocumentSymbol` values (not via `serde_json::
        // from_str`, which would hit the deserializer's own ~128-level
        // recursion limit well before this shape reaches 110 levels,
        // since each level costs the deserializer more than one unit of
        // its own recursion budget) -- isolates this function's *own*
        // depth guard from the deserializer's, proving the guard added
        // in response to `docs/security-findings/rust-lsp-dev-search-
        // everywhere-2026-08-20.md` finding 1 is self-contained, not
        // merely redundant with the deserializer's limit.
        #[allow(deprecated)]
        fn leaf(
            name: &str,
            children: Option<Vec<lsp_types::DocumentSymbol>>,
        ) -> lsp_types::DocumentSymbol {
            let r = lsp_types::Range {
                start: lsp_types::Position::new(0, 0),
                end: lsp_types::Position::new(0, 1),
            };
            lsp_types::DocumentSymbol {
                name: name.to_string(),
                detail: None,
                kind: lsp_types::SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range: r,
                selection_range: r,
                children,
            }
        }
        let depth = 110;
        let mut node = leaf("innermost", None);
        for i in 0..depth {
            node = leaf(&format!("level{i}"), Some(vec![node]));
        }
        let entries = vec![node];

        let mut out = Vec::new();
        flatten_document_symbols(entries, Path::new("/x/main.rs"), None, 0, &mut out);
        assert_eq!(out.len(), MAX_SYMBOL_TREE_DEPTH);
        assert!(!out.iter().any(|s| s.name == "innermost"));
    }

    #[test]
    fn bounded_document_symbols_rejects_a_non_array_top_level_value() {
        for text in ["null", "{}", "\"not an array\"", "42"] {
            assert!(
                serde_json::from_str::<BoundedDocumentSymbols>(text).is_err(),
                "expected {text:?} to be rejected as a non-array top-level value"
            );
        }
    }

    #[test]
    fn bounded_document_symbols_ignores_malformed_entries_beyond_the_cap_without_erroring() {
        let valid_entry = json!({
            "name": "x",
            "kind": 13,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        });
        let mut entries: Vec<Value> =
            std::iter::repeat_n(valid_entry, MAX_SYMBOLS_PER_MESSAGE).collect();
        entries.extend(std::iter::repeat_n(
            Value::String("not a symbol".into()),
            50,
        ));
        let text = serde_json::to_string(&Value::Array(entries)).unwrap();
        let bounded: BoundedDocumentSymbols = serde_json::from_str(&text).unwrap();
        assert_eq!(bounded.0.len(), MAX_SYMBOLS_PER_MESSAGE);
    }

    // ---- WorkspaceSymbol ----

    #[test]
    fn handle_workspace_symbol_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_workspace_symbol_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(
            handle_workspace_symbol_response(&message, root.path(), &mut state),
            None
        );
        assert!(state.pending_workspace_symbol_id.is_some());
    }

    #[test]
    fn handle_workspace_symbol_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_workspace_symbol_id = Some(4);
        let root = tempfile::tempdir().unwrap();

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(
            handle_workspace_symbol_response(&stale, root.path(), &mut state),
            None
        );
        assert_eq!(state.pending_workspace_symbol_id, Some(4));
    }

    #[test]
    fn handle_workspace_symbol_response_matches_pending_id_and_clears_it() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap();
        let mut state = ConnectionState::new();
        state.pending_workspace_symbol_id = Some(3);

        let text = references_response_text(
            3,
            json!([{
                "name": "MyStruct",
                "kind": 23,
                "location": {
                    "uri": uri.to_string(),
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                },
            }]),
        );
        let response = parse_message(&text);

        let event = handle_workspace_symbol_response(&response, &root, &mut state);
        assert_eq!(state.pending_workspace_symbol_id, None);
        match event {
            Some(LspEvent::WorkspaceSymbol { symbols }) => {
                assert_eq!(symbols.len(), 1);
                assert_eq!(symbols[0].name, "MyStruct");
                assert_eq!(symbols[0].kind, SymbolKind::Struct);
            }
            other => panic!("expected a WorkspaceSymbol event, got {other:?}"),
        }
    }

    #[test]
    fn handle_workspace_symbol_response_delivers_empty_symbols_for_a_json_rpc_error() {
        let mut state = ConnectionState::new();
        state.pending_workspace_symbol_id = Some(3);
        let root = tempfile::tempdir().unwrap();

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_workspace_symbol_response(&response, root.path(), &mut state) {
            Some(LspEvent::WorkspaceSymbol { symbols }) => assert!(symbols.is_empty()),
            other => panic!("expected an empty WorkspaceSymbol event, got {other:?}"),
        }
        assert_eq!(state.pending_workspace_symbol_id, None);
    }

    #[test]
    fn parse_workspace_symbol_result_null_result_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);
        assert!(parse_workspace_symbol_result(&response, root.path()).is_empty());
    }

    #[test]
    fn parse_workspace_symbol_result_missing_result_field_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);
        assert!(parse_workspace_symbol_result(&response, root.path()).is_empty());
    }

    #[test]
    fn parse_workspace_symbol_result_malformed_shape_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(3, json!({ "not": "an array" }));
        let response = parse_message(&text);
        assert!(parse_workspace_symbol_result(&response, root.path()).is_empty());
    }

    #[test]
    fn parse_workspace_symbol_result_skips_entries_outside_project_root_keeps_the_rest() {
        let (_dir, root, file) = project_with_file();
        let good_uri = Url::from_file_path(&file).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = fs::canonicalize(outside.path()).unwrap().join("evil.rs");
        fs::write(&outside_file, "x").unwrap();
        let bad_uri = Url::from_file_path(&outside_file).unwrap();

        let text = references_response_text(
            3,
            json!([
                {
                    "name": "good",
                    "kind": 13,
                    "location": {
                        "uri": good_uri.to_string(),
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                    },
                },
                {
                    "name": "evil",
                    "kind": 13,
                    "location": {
                        "uri": bad_uri.to_string(),
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                    },
                },
            ]),
        );
        let response = parse_message(&text);
        let symbols = parse_workspace_symbol_result(&response, &root);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "good");
    }

    #[test]
    fn parse_workspace_symbol_result_caps_an_oversized_array() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap();
        let entry = json!({
            "name": "x",
            "kind": 13,
            "location": {
                "uri": uri.to_string(),
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            },
        });
        let text = references_response_text(
            3,
            Value::Array(std::iter::repeat_n(entry, MAX_SYMBOLS_PER_MESSAGE + 50).collect()),
        );
        let response = parse_message(&text);
        assert_eq!(
            parse_workspace_symbol_result(&response, &root).len(),
            MAX_SYMBOLS_PER_MESSAGE
        );
    }

    #[test]
    fn bounded_workspace_symbols_rejects_a_non_array_top_level_value() {
        for text in ["null", "{}", "\"not an array\"", "42"] {
            assert!(
                serde_json::from_str::<BoundedWorkspaceSymbols>(text).is_err(),
                "expected {text:?} to be rejected as a non-array top-level value"
            );
        }
    }

    #[test]
    fn bounded_workspace_symbols_ignores_malformed_entries_beyond_the_cap_without_erroring() {
        let (_dir, root, file) = project_with_file();
        let uri = Url::from_file_path(&file).unwrap();
        let valid_entry = json!({
            "name": "x",
            "kind": 13,
            "location": {
                "uri": uri.to_string(),
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            },
        });
        let mut entries: Vec<Value> =
            std::iter::repeat_n(valid_entry, MAX_SYMBOLS_PER_MESSAGE).collect();
        entries.extend(std::iter::repeat_n(
            Value::String("not a symbol".into()),
            50,
        ));
        let text = serde_json::to_string(&Value::Array(entries)).unwrap();
        let bounded: BoundedWorkspaceSymbols = serde_json::from_str(&text).unwrap();
        assert_eq!(bounded.0.len(), MAX_SYMBOLS_PER_MESSAGE);
        let _ = root;
    }

    #[test]
    fn convert_workspace_symbol_returns_none_for_the_lazy_workspace_location_shape() {
        let root = tempfile::tempdir().unwrap();
        let symbol: lsp_types::WorkspaceSymbol = serde_json::from_value(json!({
            "name": "lazy",
            "kind": 13,
            "location": { "uri": "file:///some/where.rs" },
        }))
        .unwrap();
        assert!(convert_workspace_symbol(symbol, root.path()).is_none());
    }

    #[test]
    fn convert_symbol_kind_maps_every_named_constant() {
        let cases = [
            (lsp_types::SymbolKind::FILE, SymbolKind::File),
            (lsp_types::SymbolKind::MODULE, SymbolKind::Module),
            (lsp_types::SymbolKind::NAMESPACE, SymbolKind::Namespace),
            (lsp_types::SymbolKind::PACKAGE, SymbolKind::Package),
            (lsp_types::SymbolKind::CLASS, SymbolKind::Class),
            (lsp_types::SymbolKind::METHOD, SymbolKind::Method),
            (lsp_types::SymbolKind::PROPERTY, SymbolKind::Property),
            (lsp_types::SymbolKind::FIELD, SymbolKind::Field),
            (lsp_types::SymbolKind::CONSTRUCTOR, SymbolKind::Constructor),
            (lsp_types::SymbolKind::ENUM, SymbolKind::Enum),
            (lsp_types::SymbolKind::INTERFACE, SymbolKind::Interface),
            (lsp_types::SymbolKind::FUNCTION, SymbolKind::Function),
            (lsp_types::SymbolKind::VARIABLE, SymbolKind::Variable),
            (lsp_types::SymbolKind::CONSTANT, SymbolKind::Constant),
            (lsp_types::SymbolKind::STRING, SymbolKind::String),
            (lsp_types::SymbolKind::NUMBER, SymbolKind::Number),
            (lsp_types::SymbolKind::BOOLEAN, SymbolKind::Boolean),
            (lsp_types::SymbolKind::ARRAY, SymbolKind::Array),
            (lsp_types::SymbolKind::OBJECT, SymbolKind::Object),
            (lsp_types::SymbolKind::KEY, SymbolKind::Key),
            (lsp_types::SymbolKind::NULL, SymbolKind::Null),
            (lsp_types::SymbolKind::ENUM_MEMBER, SymbolKind::EnumMember),
            (lsp_types::SymbolKind::STRUCT, SymbolKind::Struct),
            (lsp_types::SymbolKind::EVENT, SymbolKind::Event),
            (lsp_types::SymbolKind::OPERATOR, SymbolKind::Operator),
            (
                lsp_types::SymbolKind::TYPE_PARAMETER,
                SymbolKind::TypeParameter,
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(convert_symbol_kind(raw), expected);
        }
    }

    #[test]
    fn convert_symbol_kind_falls_back_to_variable_for_an_unrecognized_value() {
        let unrecognized: lsp_types::SymbolKind = serde_json::from_value(json!(9999)).unwrap();
        assert_eq!(convert_symbol_kind(unrecognized), SymbolKind::Variable);
    }

    // ---- DocumentSymbol/WorkspaceSymbol cross-slot independence ----

    #[test]
    fn connection_state_new_has_no_pending_document_symbol_or_workspace_symbol() {
        let state = ConnectionState::new();
        assert_eq!(state.pending_document_symbol_id, None);
        assert_eq!(state.pending_workspace_symbol_id, None);
    }

    #[test]
    fn a_document_symbol_request_and_a_workspace_symbol_request_use_slots_independent_of_each_other_and_of_every_other_slot(
    ) {
        let mut state = ConnectionState::new();
        let hover_id = state.allocate_request_id();
        state.pending_hover_id = Some(hover_id);
        let goto_id = state.allocate_request_id();
        state.pending_goto_id = Some(goto_id);
        let references_id = state.allocate_request_id();
        state.pending_references_id = Some(references_id);
        let highlight_id = state.allocate_request_id();
        state.pending_document_highlight_id = Some(highlight_id);
        let inlay_id = state.allocate_request_id();
        state.pending_inlay_hint = Some((inlay_id, PathBuf::from("/x/inlay.rs")));
        let code_action_id = state.allocate_request_id();
        state.pending_code_action_id = Some((code_action_id, PathBuf::from("/x/ca.rs")));
        let resolve_id = state.allocate_request_id();
        state.pending_resolve_id = Some(resolve_id);
        let document_symbol_id = state.allocate_request_id();
        state.pending_document_symbol_id = Some((document_symbol_id, PathBuf::from("/x/ds.rs")));
        let workspace_symbol_id = state.allocate_request_id();
        state.pending_workspace_symbol_id = Some(workspace_symbol_id);

        let root = tempfile::tempdir().unwrap();

        let document_symbol_text = references_response_text(document_symbol_id, json!([]));
        let document_symbol_response = parse_message(&document_symbol_text);
        assert!(handle_document_symbol_response(
            &document_symbol_response,
            root.path(),
            &mut state
        )
        .is_some());
        assert_eq!(state.pending_document_symbol_id, None);
        assert_eq!(state.pending_workspace_symbol_id, Some(workspace_symbol_id));
        assert_eq!(state.pending_hover_id, Some(hover_id));
        assert_eq!(state.pending_goto_id, Some(goto_id));
        assert_eq!(state.pending_references_id, Some(references_id));
        assert_eq!(state.pending_document_highlight_id, Some(highlight_id));
        assert_eq!(
            state.pending_inlay_hint,
            Some((inlay_id, PathBuf::from("/x/inlay.rs")))
        );
        assert_eq!(
            state.pending_code_action_id,
            Some((code_action_id, PathBuf::from("/x/ca.rs")))
        );
        assert_eq!(state.pending_resolve_id, Some(resolve_id));

        let workspace_symbol_text = references_response_text(workspace_symbol_id, json!([]));
        let workspace_symbol_response = parse_message(&workspace_symbol_text);
        assert!(handle_workspace_symbol_response(
            &workspace_symbol_response,
            root.path(),
            &mut state
        )
        .is_some());
        assert_eq!(state.pending_workspace_symbol_id, None);
        assert_eq!(state.pending_hover_id, Some(hover_id));
    }

    // ---- initialize capability parsing (documentFormattingProvider / documentRangeFormattingProvider) ----

    fn document_formatting_provider_from_json(text: &str) -> bool {
        serde_json::from_str::<InitializeResultCapabilities>(text)
            .ok()
            .map(|r| r.document_formatting_provider())
            .unwrap_or(false)
    }

    fn document_range_formatting_provider_from_json(text: &str) -> bool {
        serde_json::from_str::<InitializeResultCapabilities>(text)
            .ok()
            .map(|r| r.document_range_formatting_provider())
            .unwrap_or(false)
    }

    #[test]
    fn document_formatting_provider_true_for_bare_true_boolean() {
        assert!(document_formatting_provider_from_json(
            r#"{"capabilities":{"documentFormattingProvider":true}}"#
        ));
    }

    #[test]
    fn document_formatting_provider_true_for_an_options_object() {
        assert!(document_formatting_provider_from_json(
            r#"{"capabilities":{"documentFormattingProvider":{}}}"#
        ));
    }

    #[test]
    fn document_formatting_provider_false_for_bare_false_boolean() {
        assert!(!document_formatting_provider_from_json(
            r#"{"capabilities":{"documentFormattingProvider":false}}"#
        ));
    }

    #[test]
    fn document_formatting_provider_false_when_absent() {
        assert!(!document_formatting_provider_from_json(
            r#"{"capabilities":{}}"#
        ));
    }

    #[test]
    fn document_formatting_provider_false_for_malformed_or_missing_capabilities() {
        for text in [
            r#"{"capabilities":{"documentFormattingProvider":"nonsense"}}"#,
            r#"{"capabilities":{"documentFormattingProvider":42}}"#,
            "not json at all",
            "{}",
        ] {
            assert!(
                !document_formatting_provider_from_json(text),
                "expected {text:?} to fail closed to false"
            );
        }
    }

    #[test]
    fn document_range_formatting_provider_true_for_bare_true_boolean() {
        assert!(document_range_formatting_provider_from_json(
            r#"{"capabilities":{"documentRangeFormattingProvider":true}}"#
        ));
    }

    #[test]
    fn document_range_formatting_provider_true_for_an_options_object() {
        assert!(document_range_formatting_provider_from_json(
            r#"{"capabilities":{"documentRangeFormattingProvider":{}}}"#
        ));
    }

    #[test]
    fn document_range_formatting_provider_false_for_bare_false_boolean() {
        assert!(!document_range_formatting_provider_from_json(
            r#"{"capabilities":{"documentRangeFormattingProvider":false}}"#
        ));
    }

    #[test]
    fn document_range_formatting_provider_false_when_absent() {
        assert!(!document_range_formatting_provider_from_json(
            r#"{"capabilities":{}}"#
        ));
    }

    #[test]
    fn document_range_formatting_provider_false_for_malformed_or_missing_capabilities() {
        for text in [
            r#"{"capabilities":{"documentRangeFormattingProvider":"nonsense"}}"#,
            r#"{"capabilities":{"documentRangeFormattingProvider":42}}"#,
            "not json at all",
            "{}",
        ] {
            assert!(
                !document_range_formatting_provider_from_json(text),
                "expected {text:?} to fail closed to false"
            );
        }
    }

    #[test]
    fn formatting_and_range_formatting_capability_flags_are_independent() {
        assert!(document_formatting_provider_from_json(
            r#"{"capabilities":{"documentFormattingProvider":true}}"#
        ));
        assert!(!document_range_formatting_provider_from_json(
            r#"{"capabilities":{"documentFormattingProvider":true}}"#
        ));
        assert!(document_range_formatting_provider_from_json(
            r#"{"capabilities":{"documentRangeFormattingProvider":true}}"#
        ));
        assert!(!document_formatting_provider_from_json(
            r#"{"capabilities":{"documentRangeFormattingProvider":true}}"#
        ));
    }

    // ---- Format/FormatRange ----

    #[test]
    fn connection_state_new_has_no_pending_format_and_fails_closed_on_both_capabilities() {
        let state = ConnectionState::new();
        assert_eq!(state.pending_format, None);
        assert!(!state.document_formatting_provider);
        assert!(!state.document_range_formatting_provider);
    }

    #[test]
    fn handle_format_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_format = Some((3, PathBuf::from("/x/main.rs")));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(handle_format_response(&message, &mut state), None);
        assert!(state.pending_format.is_some());
    }

    #[test]
    fn handle_format_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_format = Some((4, PathBuf::from("/x/main.rs")));

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(handle_format_response(&stale, &mut state), None);
        assert_eq!(state.pending_format, Some((4, PathBuf::from("/x/main.rs"))));
    }

    #[test]
    fn handle_format_response_matches_pending_id_clears_it_and_converts_a_non_empty_edit() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_format = Some((3, path.clone()));

        let text = references_response_text(
            3,
            json!([{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "newText": "fn main() {}\n",
            }]),
        );
        let response = parse_message(&text);

        let event = handle_format_response(&response, &mut state);
        assert_eq!(state.pending_format, None);
        match event {
            Some(LspEvent::FormatReady {
                path: event_path,
                edit,
            }) => {
                assert_eq!(event_path, path);
                let edit = edit.expect("expected a converted edit");
                assert_eq!(edit.edits.len(), 1);
                assert_eq!(edit.edits[0].path, path);
                assert_eq!(edit.edits[0].text_edits[0].new_text, "fn main() {}\n");
            }
            other => panic!("expected a FormatReady event with an edit, got {other:?}"),
        }
    }

    #[test]
    fn handle_format_response_treats_a_null_result_as_no_edit() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_format = Some((3, path.clone()));

        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);

        match handle_format_response(&response, &mut state) {
            Some(LspEvent::FormatReady {
                path: event_path,
                edit,
            }) => {
                assert_eq!(event_path, path);
                assert_eq!(edit, None);
            }
            other => panic!("expected an empty FormatReady event, got {other:?}"),
        }
    }

    #[test]
    fn handle_format_response_treats_an_empty_array_as_no_edit() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_format = Some((3, path.clone()));

        let text = references_response_text(3, json!([]));
        let response = parse_message(&text);

        match handle_format_response(&response, &mut state) {
            Some(LspEvent::FormatReady { edit, .. }) => assert_eq!(edit, None),
            other => panic!("expected an empty FormatReady event, got {other:?}"),
        }
    }

    #[test]
    fn handle_format_response_treats_a_json_rpc_error_as_no_edit() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_format = Some((3, path.clone()));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_format_response(&response, &mut state) {
            Some(LspEvent::FormatReady {
                path: event_path,
                edit,
            }) => {
                assert_eq!(event_path, path);
                assert_eq!(edit, None);
            }
            other => panic!("expected an empty FormatReady event, got {other:?}"),
        }
    }

    #[test]
    fn format_and_format_range_share_one_pending_slot_and_the_newer_one_supersedes() {
        let mut state = ConnectionState::new();
        // Simulates `Format` having been sent first (id 3), then
        // `FormatRange` superseding it (id 4) before a reply for 3 ever
        // arrived -- `send_format_request` overwrites `pending_format`
        // rather than tracking both, so id 3's eventual reply (if any)
        // must be ignored.
        state.pending_format = Some((3, PathBuf::from("/x/first.rs")));
        state.pending_format = Some((4, PathBuf::from("/x/second.rs")));

        let stale_text = references_response_text(3, json!([]));
        let stale = parse_message(&stale_text);
        assert_eq!(handle_format_response(&stale, &mut state), None);
        assert_eq!(
            state.pending_format,
            Some((4, PathBuf::from("/x/second.rs")))
        );

        let current_text = references_response_text(4, json!([]));
        let current = parse_message(&current_text);
        assert!(handle_format_response(&current, &mut state).is_some());
        assert_eq!(state.pending_format, None);
    }

    #[test]
    fn a_format_request_uses_a_slot_independent_of_every_other_slot() {
        let mut state = ConnectionState::new();
        let hover_id = state.allocate_request_id();
        state.pending_hover_id = Some(hover_id);
        let goto_id = state.allocate_request_id();
        state.pending_goto_id = Some(goto_id);
        let references_id = state.allocate_request_id();
        state.pending_references_id = Some(references_id);
        let highlight_id = state.allocate_request_id();
        state.pending_document_highlight_id = Some(highlight_id);
        let inlay_id = state.allocate_request_id();
        state.pending_inlay_hint = Some((inlay_id, PathBuf::from("/x/inlay.rs")));
        let code_action_id = state.allocate_request_id();
        state.pending_code_action_id = Some((code_action_id, PathBuf::from("/x/ca.rs")));
        let resolve_id = state.allocate_request_id();
        state.pending_resolve_id = Some(resolve_id);
        let document_symbol_id = state.allocate_request_id();
        state.pending_document_symbol_id = Some((document_symbol_id, PathBuf::from("/x/ds.rs")));
        let workspace_symbol_id = state.allocate_request_id();
        state.pending_workspace_symbol_id = Some(workspace_symbol_id);
        let format_id = state.allocate_request_id();
        state.pending_format = Some((format_id, PathBuf::from("/x/format.rs")));

        let text = references_response_text(format_id, json!([]));
        let response = parse_message(&text);
        assert!(handle_format_response(&response, &mut state).is_some());

        assert_eq!(state.pending_format, None);
        assert_eq!(state.pending_hover_id, Some(hover_id));
        assert_eq!(state.pending_goto_id, Some(goto_id));
        assert_eq!(state.pending_references_id, Some(references_id));
        assert_eq!(state.pending_document_highlight_id, Some(highlight_id));
        assert_eq!(
            state.pending_inlay_hint,
            Some((inlay_id, PathBuf::from("/x/inlay.rs")))
        );
        assert_eq!(
            state.pending_code_action_id,
            Some((code_action_id, PathBuf::from("/x/ca.rs")))
        );
        assert_eq!(state.pending_resolve_id, Some(resolve_id));
        assert_eq!(
            state.pending_document_symbol_id,
            Some((document_symbol_id, PathBuf::from("/x/ds.rs")))
        );
        assert_eq!(state.pending_workspace_symbol_id, Some(workspace_symbol_id));
    }

    // ---- initialize capability parsing (renameProvider / prepareProvider) ----

    fn rename_provider_from_json(text: &str) -> bool {
        serde_json::from_str::<InitializeResultCapabilities>(text)
            .ok()
            .map(|r| r.rename_provider())
            .unwrap_or(false)
    }

    fn prepare_rename_provider_from_json(text: &str) -> bool {
        serde_json::from_str::<InitializeResultCapabilities>(text)
            .ok()
            .map(|r| r.prepare_rename_provider())
            .unwrap_or(false)
    }

    #[test]
    fn rename_provider_true_for_bare_true_boolean() {
        assert!(rename_provider_from_json(
            r#"{"capabilities":{"renameProvider":true}}"#
        ));
    }

    #[test]
    fn rename_provider_true_for_an_options_object() {
        assert!(rename_provider_from_json(
            r#"{"capabilities":{"renameProvider":{"prepareProvider":true}}}"#
        ));
        assert!(rename_provider_from_json(
            r#"{"capabilities":{"renameProvider":{}}}"#
        ));
    }

    #[test]
    fn rename_provider_false_for_bare_false_boolean() {
        assert!(!rename_provider_from_json(
            r#"{"capabilities":{"renameProvider":false}}"#
        ));
    }

    #[test]
    fn rename_provider_false_when_absent_or_malformed() {
        for text in [
            r#"{"capabilities":{}}"#,
            r#"{"capabilities":{"renameProvider":"nonsense"}}"#,
            r#"{"capabilities":{"renameProvider":42}}"#,
            "not json at all",
            "{}",
        ] {
            assert!(
                !rename_provider_from_json(text),
                "expected {text:?} to fail closed to false"
            );
        }
    }

    #[test]
    fn prepare_rename_provider_true_only_when_the_options_object_sets_it() {
        assert!(prepare_rename_provider_from_json(
            r#"{"capabilities":{"renameProvider":{"prepareProvider":true}}}"#
        ));
    }

    #[test]
    fn prepare_rename_provider_false_for_a_bare_true_boolean() {
        // A bare `renameProvider: true` carries no `prepareProvider` flag
        // to read -- `rename_provider()` is true for this shape, but
        // `prepare_rename_provider()` must still fail closed
        // (`docs/features/rename-refactoring.md` §2.2).
        assert!(!prepare_rename_provider_from_json(
            r#"{"capabilities":{"renameProvider":true}}"#
        ));
    }

    #[test]
    fn prepare_rename_provider_false_when_the_options_object_omits_it() {
        assert!(!prepare_rename_provider_from_json(
            r#"{"capabilities":{"renameProvider":{}}}"#
        ));
        assert!(!prepare_rename_provider_from_json(
            r#"{"capabilities":{"renameProvider":{"prepareProvider":false}}}"#
        ));
    }

    #[test]
    fn prepare_rename_provider_false_when_rename_provider_absent_or_malformed() {
        for text in [
            r#"{"capabilities":{}}"#,
            r#"{"capabilities":{"renameProvider":"nonsense"}}"#,
            "not json at all",
        ] {
            assert!(
                !prepare_rename_provider_from_json(text),
                "expected {text:?} to fail closed to false"
            );
        }
    }

    // ---- PrepareRename / Rename ----

    #[test]
    fn connection_state_new_has_no_pending_rename_and_fails_closed_on_both_capabilities() {
        let state = ConnectionState::new();
        assert_eq!(state.pending_prepare_rename, None);
        assert_eq!(state.pending_rename, None);
        assert!(!state.rename_provider);
        assert!(!state.prepare_rename_provider);
    }

    #[test]
    fn handle_prepare_rename_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_prepare_rename = Some((3, PathBuf::from("/x/main.rs")));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(handle_prepare_rename_response(&message, &mut state), None);
        assert!(state.pending_prepare_rename.is_some());
    }

    #[test]
    fn handle_prepare_rename_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_prepare_rename = Some((4, PathBuf::from("/x/main.rs")));

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(handle_prepare_rename_response(&stale, &mut state), None);
        assert_eq!(
            state.pending_prepare_rename,
            Some((4, PathBuf::from("/x/main.rs")))
        );
    }

    #[test]
    fn handle_prepare_rename_response_true_for_a_bare_range() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_prepare_rename = Some((3, path.clone()));

        let text = references_response_text(3, range_json(0, 3, 0, 7));
        let response = parse_message(&text);

        let event = handle_prepare_rename_response(&response, &mut state);
        assert_eq!(state.pending_prepare_rename, None);
        match event {
            Some(LspEvent::PrepareRenameReady {
                path: event_path,
                renameable,
            }) => {
                assert_eq!(event_path, path);
                assert!(renameable);
            }
            other => panic!("expected a PrepareRenameReady event, got {other:?}"),
        }
    }

    #[test]
    fn handle_prepare_rename_response_true_for_a_range_with_placeholder() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_prepare_rename = Some((3, path));

        let text = references_response_text(
            3,
            json!({ "range": range_json(0, 3, 0, 7), "placeholder": "count" }),
        );
        let response = parse_message(&text);

        match handle_prepare_rename_response(&response, &mut state) {
            Some(LspEvent::PrepareRenameReady { renameable, .. }) => assert!(renameable),
            other => panic!("expected a PrepareRenameReady event, got {other:?}"),
        }
    }

    #[test]
    fn handle_prepare_rename_response_true_for_a_default_behavior_shape() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_prepare_rename = Some((3, path));

        let text = references_response_text(3, json!({ "defaultBehavior": true }));
        let response = parse_message(&text);

        match handle_prepare_rename_response(&response, &mut state) {
            Some(LspEvent::PrepareRenameReady { renameable, .. }) => assert!(renameable),
            other => panic!("expected a PrepareRenameReady event, got {other:?}"),
        }
    }

    #[test]
    fn handle_prepare_rename_response_false_for_a_null_result() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_prepare_rename = Some((3, path));

        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);

        match handle_prepare_rename_response(&response, &mut state) {
            Some(LspEvent::PrepareRenameReady { renameable, .. }) => assert!(!renameable),
            other => panic!("expected a PrepareRenameReady event, got {other:?}"),
        }
    }

    #[test]
    fn handle_prepare_rename_response_false_for_a_json_rpc_error() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_prepare_rename = Some((3, path));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_prepare_rename_response(&response, &mut state) {
            Some(LspEvent::PrepareRenameReady { renameable, .. }) => assert!(!renameable),
            other => panic!("expected a PrepareRenameReady event, got {other:?}"),
        }
        assert_eq!(state.pending_prepare_rename, None);
    }

    #[test]
    fn handle_rename_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_rename = Some((3, PathBuf::from("/x/main.rs"), "renamed".to_string()));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            handle_rename_response(&message, root.path(), &mut state),
            None
        );
        assert!(state.pending_rename.is_some());
    }

    #[test]
    fn handle_rename_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_rename = Some((4, PathBuf::from("/x/main.rs"), "renamed".to_string()));

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            handle_rename_response(&stale, root.path(), &mut state),
            None
        );
        assert_eq!(
            state.pending_rename,
            Some((4, PathBuf::from("/x/main.rs"), "renamed".to_string()))
        );
    }

    #[test]
    fn handle_rename_response_converts_the_edit_and_echoes_new_name() {
        let (_dir, root, main_rs) = project_with_file();
        let mut state = ConnectionState::new();
        state.pending_rename = Some((3, main_rs.clone(), "count".to_string()));
        let uri = Url::from_file_path(&main_rs).unwrap().to_string();

        let text = references_response_text(
            3,
            json!({ "changes": { (uri): [
                { "range": range_json(0, 3, 0, 7), "newText": "count" }
            ] } }),
        );
        let response = parse_message(&text);

        let event = handle_rename_response(&response, &root, &mut state);
        assert_eq!(state.pending_rename, None);
        match event {
            Some(LspEvent::RenameReady {
                path,
                new_name,
                edit,
            }) => {
                assert_eq!(path, main_rs);
                assert_eq!(new_name, "count");
                let edit = edit.expect("expected a converted edit");
                assert_eq!(edit.edits.len(), 1);
                assert_eq!(edit.edits[0].path, main_rs);
                assert_eq!(edit.edits[0].text_edits[0].new_text, "count");
            }
            other => panic!("expected a RenameReady event with an edit, got {other:?}"),
        }
    }

    #[test]
    fn handle_rename_response_treats_a_null_result_as_no_edit() {
        let path = PathBuf::from("/project/main.rs");
        let root = tempfile::tempdir().unwrap();
        let mut state = ConnectionState::new();
        state.pending_rename = Some((3, path.clone(), "count".to_string()));

        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);

        match handle_rename_response(&response, root.path(), &mut state) {
            Some(LspEvent::RenameReady {
                path: event_path,
                new_name,
                edit,
            }) => {
                assert_eq!(event_path, path);
                assert_eq!(new_name, "count");
                assert_eq!(edit, None);
            }
            other => panic!("expected an empty RenameReady event, got {other:?}"),
        }
    }

    #[test]
    fn handle_rename_response_treats_a_json_rpc_error_as_no_edit() {
        let path = PathBuf::from("/project/main.rs");
        let root = tempfile::tempdir().unwrap();
        let mut state = ConnectionState::new();
        state.pending_rename = Some((3, path.clone(), "count".to_string()));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_rename_response(&response, root.path(), &mut state) {
            Some(LspEvent::RenameReady { edit, .. }) => assert_eq!(edit, None),
            other => panic!("expected an empty RenameReady event, got {other:?}"),
        }
        assert_eq!(state.pending_rename, None);
    }

    #[test]
    fn prepare_rename_and_rename_use_independent_slots_from_each_other_and_every_other_kind() {
        let mut state = ConnectionState::new();
        let hover_id = state.allocate_request_id();
        state.pending_hover_id = Some(hover_id);
        let format_id = state.allocate_request_id();
        state.pending_format = Some((format_id, PathBuf::from("/x/format.rs")));
        let prepare_id = state.allocate_request_id();
        state.pending_prepare_rename = Some((prepare_id, PathBuf::from("/x/pr.rs")));
        let rename_id = state.allocate_request_id();
        state.pending_rename = Some((rename_id, PathBuf::from("/x/r.rs"), "n".to_string()));

        let root = tempfile::tempdir().unwrap();
        let text = references_response_text(prepare_id, range_json(0, 0, 0, 1));
        let response = parse_message(&text);
        assert!(handle_prepare_rename_response(&response, &mut state).is_some());

        assert_eq!(state.pending_prepare_rename, None);
        assert_eq!(state.pending_hover_id, Some(hover_id));
        assert_eq!(
            state.pending_format,
            Some((format_id, PathBuf::from("/x/format.rs")))
        );
        assert_eq!(
            state.pending_rename,
            Some((rename_id, PathBuf::from("/x/r.rs"), "n".to_string()))
        );

        let text = references_response_text(rename_id, Value::Null);
        let response = parse_message(&text);
        assert!(handle_rename_response(&response, root.path(), &mut state).is_some());
        assert_eq!(state.pending_rename, None);
        assert_eq!(state.pending_hover_id, Some(hover_id));
        assert_eq!(
            state.pending_format,
            Some((format_id, PathBuf::from("/x/format.rs")))
        );
    }

    // ---- initialize capability parsing (semanticTokensProvider) ----

    fn semantic_tokens_full_provider_from_json(text: &str) -> bool {
        serde_json::from_str::<InitializeResultCapabilities>(text)
            .ok()
            .map(|r| r.semantic_tokens_full_provider())
            .unwrap_or(false)
    }

    fn semantic_tokens_legend_from_json(text: &str) -> Vec<String> {
        serde_json::from_str::<InitializeResultCapabilities>(text)
            .ok()
            .map(|r| r.semantic_tokens_legend())
            .unwrap_or_default()
    }

    #[test]
    fn semantic_tokens_full_provider_true_for_bare_true_full() {
        assert!(semantic_tokens_full_provider_from_json(
            r#"{"capabilities":{"semanticTokensProvider":{
                "legend": {"tokenTypes": ["type"], "tokenModifiers": []},
                "full": true
            }}}"#
        ));
    }

    #[test]
    fn semantic_tokens_full_provider_true_for_delta_options_object() {
        assert!(semantic_tokens_full_provider_from_json(
            r#"{"capabilities":{"semanticTokensProvider":{
                "legend": {"tokenTypes": [], "tokenModifiers": []},
                "full": {"delta": true}
            }}}"#
        ));
    }

    #[test]
    fn semantic_tokens_full_provider_true_via_registration_options_variant() {
        assert!(semantic_tokens_full_provider_from_json(
            r#"{"capabilities":{"semanticTokensProvider":{
                "legend": {"tokenTypes": ["function"], "tokenModifiers": []},
                "full": true,
                "id": "abc"
            }}}"#
        ));
    }

    #[test]
    fn semantic_tokens_full_provider_false_for_bare_full_false() {
        assert!(!semantic_tokens_full_provider_from_json(
            r#"{"capabilities":{"semanticTokensProvider":{
                "legend": {"tokenTypes": [], "tokenModifiers": []},
                "full": false
            }}}"#
        ));
    }

    #[test]
    fn semantic_tokens_full_provider_false_when_full_absent() {
        assert!(!semantic_tokens_full_provider_from_json(
            r#"{"capabilities":{"semanticTokensProvider":{
                "legend": {"tokenTypes": [], "tokenModifiers": []}
            }}}"#
        ));
    }

    #[test]
    fn semantic_tokens_full_provider_false_when_provider_absent_entirely() {
        assert!(!semantic_tokens_full_provider_from_json(
            r#"{"capabilities":{}}"#
        ));
    }

    #[test]
    fn semantic_tokens_full_provider_false_for_malformed_capabilities() {
        for text in [
            r#"{"capabilities":{"semanticTokensProvider":"nonsense"}}"#,
            r#"{"capabilities":{"semanticTokensProvider":42}}"#,
            "not json at all",
            "{}",
        ] {
            assert!(
                !semantic_tokens_full_provider_from_json(text),
                "expected {text:?} to fail closed to false"
            );
        }
    }

    #[test]
    fn semantic_tokens_legend_extracted_when_full_provider_supported() {
        let legend = semantic_tokens_legend_from_json(
            r#"{"capabilities":{"semanticTokensProvider":{
                "legend": {"tokenTypes": ["type", "function", "comment"], "tokenModifiers": []},
                "full": true
            }}}"#,
        );
        assert_eq!(legend, vec!["type", "function", "comment"]);
    }

    #[test]
    fn semantic_tokens_legend_empty_when_full_provider_unsupported() {
        let legend = semantic_tokens_legend_from_json(
            r#"{"capabilities":{"semanticTokensProvider":{
                "legend": {"tokenTypes": ["type"], "tokenModifiers": []},
                "full": false
            }}}"#,
        );
        assert!(legend.is_empty());
    }

    #[test]
    fn semantic_tokens_legend_empty_when_provider_absent() {
        assert!(semantic_tokens_legend_from_json(r#"{"capabilities":{}}"#).is_empty());
    }

    // ---- SemanticTokensFull ----

    #[test]
    fn connection_state_new_has_no_pending_semantic_tokens_and_fails_closed() {
        let state = ConnectionState::new();
        assert_eq!(state.pending_semantic_tokens, None);
        assert!(!state.semantic_tokens_provider);
        assert!(state.semantic_token_legend.is_empty());
    }

    #[test]
    fn handle_semantic_tokens_response_ignores_a_message_with_no_id() {
        let mut state = ConnectionState::new();
        state.pending_semantic_tokens = Some((3, PathBuf::from("/x/main.rs")));

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
        }))
        .unwrap();
        let message = parse_message(&text);
        assert_eq!(handle_semantic_tokens_response(&message, &mut state), None);
        assert!(state.pending_semantic_tokens.is_some());
    }

    #[test]
    fn handle_semantic_tokens_response_ignores_a_stale_or_unrelated_id() {
        let mut state = ConnectionState::new();
        state.pending_semantic_tokens = Some((4, PathBuf::from("/x/main.rs")));

        let text = references_response_text(3, Value::Null);
        let stale = parse_message(&text);
        assert_eq!(handle_semantic_tokens_response(&stale, &mut state), None);
        assert_eq!(
            state.pending_semantic_tokens,
            Some((4, PathBuf::from("/x/main.rs")))
        );
    }

    #[test]
    fn handle_semantic_tokens_response_matches_pending_id_clears_it_and_carries_the_stored_path() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_semantic_tokens = Some((3, path.clone()));
        state.semantic_token_legend = vec!["keyword".to_string()];

        let text = references_response_text(3, json!({ "data": [0, 0, 3, 0, 0] }));
        let response = parse_message(&text);

        let event = handle_semantic_tokens_response(&response, &mut state);
        assert_eq!(state.pending_semantic_tokens, None);
        match event {
            Some(LspEvent::SemanticTokens {
                path: event_path,
                tokens,
            }) => {
                assert_eq!(event_path, path);
                assert_eq!(tokens.len(), 1);
                assert_eq!(tokens[0].kind, SemanticTokenKind::Keyword);
            }
            other => panic!("expected a SemanticTokens event, got {other:?}"),
        }
    }

    #[test]
    fn handle_semantic_tokens_response_delivers_empty_tokens_for_a_json_rpc_error() {
        let path = PathBuf::from("/project/main.rs");
        let mut state = ConnectionState::new();
        state.pending_semantic_tokens = Some((3, path.clone()));
        state.semantic_token_legend = vec!["keyword".to_string()];

        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);

        match handle_semantic_tokens_response(&response, &mut state) {
            Some(LspEvent::SemanticTokens {
                path: event_path,
                tokens,
            }) => {
                assert_eq!(event_path, path);
                assert!(tokens.is_empty());
            }
            other => panic!("expected an empty SemanticTokens event, got {other:?}"),
        }
        assert_eq!(state.pending_semantic_tokens, None);
    }

    #[test]
    fn parse_semantic_tokens_result_null_result_is_empty() {
        let text = references_response_text(3, Value::Null);
        let response = parse_message(&text);
        assert!(parse_semantic_tokens_result(&response, &[]).is_empty());
    }

    #[test]
    fn parse_semantic_tokens_result_missing_result_field_is_empty() {
        let text = serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": 3 })).unwrap();
        let response = parse_message(&text);
        assert!(parse_semantic_tokens_result(&response, &[]).is_empty());
    }

    #[test]
    fn parse_semantic_tokens_result_malformed_shape_is_empty() {
        let text = references_response_text(3, json!({ "data": "not an array" }));
        let response = parse_message(&text);
        assert!(parse_semantic_tokens_result(&response, &[]).is_empty());
    }

    #[test]
    fn parse_semantic_tokens_result_json_rpc_error_is_empty() {
        let text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "fixture: boom" },
        }))
        .unwrap();
        let response = parse_message(&text);
        assert!(parse_semantic_tokens_result(&response, &["keyword".to_string()]).is_empty());
    }

    // ---- decode_semantic_tokens ----

    #[test]
    fn decode_semantic_tokens_empty_data_is_empty() {
        assert!(decode_semantic_tokens(&[], &[]).is_empty());
    }

    #[test]
    fn decode_semantic_tokens_drops_a_trailing_partial_chunk() {
        let legend = vec!["keyword".to_string()];
        // One full 5-tuple token, then 3 stray u32s (not a multiple of 5).
        let tokens = decode_semantic_tokens(&[0, 0, 3, 0, 0, 1, 2, 3], &legend);
        assert_eq!(tokens.len(), 1);
    }

    #[test]
    fn decode_semantic_tokens_first_token_position_is_absolute() {
        let legend = vec!["keyword".to_string()];
        let tokens = decode_semantic_tokens(&[2, 5, 4, 0, 0], &legend);
        assert_eq!(tokens.len(), 1);
        assert_eq!(
            tokens[0].position,
            Position {
                line: 2,
                character: 5
            }
        );
        assert_eq!(tokens[0].length, 4);
    }

    #[test]
    fn decode_semantic_tokens_same_line_delta_start_is_relative_to_previous_token() {
        let legend = vec!["keyword".to_string()];
        // Token 1 at (0, 5); token 2 on the same line, delta_start=3 -> (0, 8).
        let tokens = decode_semantic_tokens(&[0, 5, 1, 0, 0, 0, 3, 1, 0, 0], &legend);
        assert_eq!(tokens.len(), 2);
        assert_eq!(
            tokens[0].position,
            Position {
                line: 0,
                character: 5
            }
        );
        assert_eq!(
            tokens[1].position,
            Position {
                line: 0,
                character: 8
            }
        );
    }

    #[test]
    fn decode_semantic_tokens_new_line_delta_start_is_absolute_column_not_relative() {
        let legend = vec!["keyword".to_string()];
        // Token 1 at (0, 5); token 2 on a new line (delta_line=1), delta_start=2 -> (1, 2), not (1, 7).
        let tokens = decode_semantic_tokens(&[0, 5, 1, 0, 0, 1, 2, 1, 0, 0], &legend);
        assert_eq!(tokens.len(), 2);
        assert_eq!(
            tokens[0].position,
            Position {
                line: 0,
                character: 5
            }
        );
        assert_eq!(
            tokens[1].position,
            Position {
                line: 1,
                character: 2
            }
        );
    }

    #[test]
    fn decode_semantic_tokens_dropped_entry_still_advances_the_cursor() {
        // Legend has only index 0 mapped ("keyword"); index 1 is a name this
        // client's mapping table doesn't recognise, so it must be dropped --
        // but its delta contribution must still land on token 3.
        let legend = vec!["keyword".to_string(), "totally-unrecognised".to_string()];
        let raw = [
            0, 5, 1, 0, 0, // token 1: (0,5), kind=keyword
            0, 3, 1, 1, 0, // token 2: (0,8), kind=unmapped -> dropped, cursor still advances
            0, 2, 1, 0, 0, // token 3: delta_start=2 relative to token 2's (0,8) -> (0,10)
        ];
        let tokens = decode_semantic_tokens(&raw, &legend);
        assert_eq!(tokens.len(), 2);
        assert_eq!(
            tokens[0].position,
            Position {
                line: 0,
                character: 5
            }
        );
        assert_eq!(
            tokens[1].position,
            Position {
                line: 0,
                character: 10
            }
        );
    }

    #[test]
    fn decode_semantic_tokens_out_of_bounds_token_type_index_is_dropped_but_advances_cursor() {
        let legend = vec!["keyword".to_string()];
        let raw = [
            0, 5, 1, 99, 0, // token_type index 99 doesn't exist in a 1-entry legend
            0, 4, 1, 0, 0, // next token's position still correctly relative
        ];
        let tokens = decode_semantic_tokens(&raw, &legend);
        assert_eq!(tokens.len(), 1);
        assert_eq!(
            tokens[0].position,
            Position {
                line: 0,
                character: 9
            }
        );
    }

    #[test]
    fn decode_semantic_tokens_saturates_instead_of_overflowing_on_extreme_deltas() {
        let legend = vec!["keyword".to_string(), "type".to_string()];
        // Two consecutive tokens each with delta_line = u32::MAX -- a plain
        // `+=` would overflow on the second one. Must not panic.
        let raw = [
            u32::MAX,
            u32::MAX,
            u32::MAX,
            0,
            0,
            u32::MAX,
            5,
            1,
            1,
            0,
            0,
            u32::MAX,
            1,
            0,
            0,
        ];
        let tokens = decode_semantic_tokens(&raw, &legend);
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].position.line, u32::MAX);
        assert_eq!(tokens[0].position.character, u32::MAX);
        // second token: line saturates at u32::MAX + u32::MAX -> u32::MAX
        assert_eq!(tokens[1].position.line, u32::MAX);
        assert_eq!(tokens[1].position.character, 5);
        // third token: same line, character saturates at u32::MAX + u32::MAX -> u32::MAX
        assert_eq!(tokens[2].position.line, u32::MAX);
        assert_eq!(tokens[2].position.character, u32::MAX);
    }

    // ---- map_semantic_token_type ----

    #[test]
    fn map_semantic_token_type_covers_every_row_of_the_mapping_table() {
        let cases = [
            ("type", SemanticTokenKind::Type),
            ("class", SemanticTokenKind::Type),
            ("enum", SemanticTokenKind::Type),
            ("interface", SemanticTokenKind::Type),
            ("struct", SemanticTokenKind::Type),
            ("typeParameter", SemanticTokenKind::Type),
            ("namespace", SemanticTokenKind::Type),
            ("function", SemanticTokenKind::Function),
            ("method", SemanticTokenKind::Function),
            ("macro", SemanticTokenKind::Macro),
            ("decorator", SemanticTokenKind::Macro),
            ("keyword", SemanticTokenKind::Keyword),
            ("modifier", SemanticTokenKind::Keyword),
            ("comment", SemanticTokenKind::Comment),
            ("string", SemanticTokenKind::String),
            ("number", SemanticTokenKind::Number),
            ("operator", SemanticTokenKind::Operator),
            ("variable", SemanticTokenKind::Variable),
            ("parameter", SemanticTokenKind::Variable),
            ("property", SemanticTokenKind::Variable),
            ("enumMember", SemanticTokenKind::Variable),
            ("event", SemanticTokenKind::Variable),
        ];
        for (name, expected) in cases {
            assert_eq!(
                map_semantic_token_type(name),
                Some(expected),
                "mismatch for {name:?}"
            );
        }
    }

    #[test]
    fn map_semantic_token_type_unmapped_names_return_none() {
        for name in ["regexp", "totally-server-defined-nonsense", ""] {
            assert_eq!(
                map_semantic_token_type(name),
                None,
                "expected {name:?} to be unmapped"
            );
        }
    }

    // ---- BoundedSemanticTokenData ----

    #[test]
    fn bounded_semantic_token_data_caps_the_raw_array_at_the_message_limit() {
        let raw: Vec<u32> = (0..(MAX_SEMANTIC_TOKENS_PER_MESSAGE as u32 + 500) * 5)
            .map(|n| n % 7)
            .collect();
        let text =
            serde_json::to_string(&Value::Array(raw.iter().map(|n| json!(n)).collect())).unwrap();

        let bounded = serde_json::from_str::<BoundedSemanticTokenData>(&text).unwrap();
        assert_eq!(bounded.0.len(), MAX_SEMANTIC_TOKENS_PER_MESSAGE * 5);
    }

    #[test]
    fn bounded_semantic_token_data_ignores_malformed_entries_beyond_the_cap_without_erroring() {
        let mut entries: Vec<Value> = (0..MAX_SEMANTIC_TOKENS_PER_MESSAGE * 5)
            .map(|n| json!(n % 7))
            .collect();
        entries.extend(std::iter::repeat_n(Value::String("not a u32".into()), 50));
        let text = serde_json::to_string(&Value::Array(entries)).unwrap();

        let bounded = serde_json::from_str::<BoundedSemanticTokenData>(&text).unwrap();
        assert_eq!(bounded.0.len(), MAX_SEMANTIC_TOKENS_PER_MESSAGE * 5);
    }

    #[test]
    fn bounded_semantic_token_data_default_is_empty() {
        assert!(BoundedSemanticTokenData::default().0.is_empty());
    }

    #[test]
    fn raw_semantic_tokens_result_data_defaults_when_absent() {
        let result: RawSemanticTokensResult = serde_json::from_str(r#"{}"#).unwrap();
        assert!(result.data.0.is_empty());
    }
}
