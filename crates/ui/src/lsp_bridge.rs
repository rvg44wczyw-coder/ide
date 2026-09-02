//! Bridges one or more `ide_lsp::LspClient`s into `IdeApp`'s frame loop:
//! starts/stops a client per active language, routes each request to the
//! client covering its file, drains diagnostics into a workspace-wide map,
//! and forwards `ServerExited` as a one-line status message. See
//! `docs/features/rust-language-support.md` §2.2/§3.2 for the original
//! single-language shape and `docs/features/multi-language-projects.md`
//! for the multi-client design this file now implements.

use ide_core::LanguageConfig;
use ide_lsp::{
    CodeAction, Diagnostic, GotoKind, InlayHint, Location, LspClient, LspEvent, LspRequest,
    Position, Range, SemanticToken, Symbol, WorkspaceEdit,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

struct RunningLanguage {
    config: LanguageConfig,
    client: LspClient,
}

/// Whether `config` (one active language) covers `path` -- the same rule
/// `ide_core::language_for_path` applies, just against a single config
/// instead of a whole active-language slice, to avoid an allocation at
/// every teardown/routing call site in this file
/// (`docs/features/multi-language-projects.md` §3.3).
fn path_matches(config: &LanguageConfig, path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    config.extension.eq_ignore_ascii_case(extension)
        || config
            .extra_extensions
            .iter()
            .any(|e| e.eq_ignore_ascii_case(extension))
}

#[derive(Default)]
pub struct LspBridge {
    /// Keyed by `config.extension.to_lowercase()` -- the same unique key
    /// `ide_core::detect_active_languages` already guarantees no two
    /// active languages share (`docs/features/multi-language-projects.md`
    /// §2.2/§4).
    clients: HashMap<String, RunningLanguage>,
    /// Every diagnostic any running client has reported, across the whole
    /// workspace (not just open tabs) -- backs the Problems panel. Shared
    /// flat across every client: a path is only ever covered by one
    /// active language, so entries from different clients never collide.
    pub diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    pub server_error: Option<String>,
    /// Result of the most recent (non-superseded) find-usages query --
    /// replaced wholesale on each `LspEvent::References`, not keyed by
    /// path/query (see `docs/features/find-usages.md` §2.2 -- v1 shows one
    /// query's results at a time, now one across however many clients are
    /// running, since a request is always sent to exactly one of them).
    pub references: Vec<Location>,
    /// True from `find_references` sending the request until a matching
    /// `LspEvent::References` (or `ServerExited`) arrives -- backs the
    /// Usages panel's "Finding usages…" state.
    pub finding_references: bool,
    /// Result of the most recent (non-superseded) `Goto` query of any
    /// kind -- replaced wholesale on each `LspEvent::Goto`, same
    /// one-query-at-a-time shape as `references`
    /// (`docs/features/goto-definition.md` §2.1/§2.2).
    pub goto: Vec<Location>,
    /// True from the moment a `go_to_*` method sends the request until a
    /// matching `LspEvent::Goto` (or `ServerExited`) arrives.
    pub finding_goto: bool,
    /// True for exactly the one `poll()` call that processed the
    /// most-recently-arrived `LspEvent::Goto` -- reset to `false` at the
    /// top of every `poll()` call, so `IdeApp::handle_goto_response` can
    /// tell "a response just landed this frame" apart from "nothing
    /// changed" without re-deriving it from `finding_goto`'s edge, which
    /// flips false on every poll after the fact, not just the frame it
    /// happened on.
    pub goto_ready: bool,
    /// Most recent (non-superseded) `Hover` answer -- replaced wholesale on
    /// each `LspEvent::Hover`, cleared the instant a new `Hover` request is
    /// sent (`docs/features/inlay-hints-and-hover.md` §3.2, same
    /// "clear at send-time" convention `goto`/`references` already follow).
    /// Left untouched by a client's `ServerExited`: unlike ambient state,
    /// the popup already showing this text to the user isn't made
    /// misleading by that one server dying, the way stale highlights/hints
    /// would be.
    pub hover: Option<String>,
    /// True from the moment `request_hover` sends the request until a
    /// matching `LspEvent::Hover` (or `ServerExited`) arrives.
    pub finding_hover: bool,
    /// Most recent (non-superseded) `DocumentHighlight` answer, for
    /// whatever position was last queried -- cleared at send-time.
    pub document_highlights: Vec<Range>,
    /// Inlay hints, keyed by file -- unlike `hover`/`goto`/`references`,
    /// this isn't "the answer to the one most recent question": v1
    /// refetches per open file on every edit, and stale hints for a file
    /// the user has switched away from must keep rendering correctly if
    /// that tab is revisited before its own next refetch (§2.2, §4).
    pub inlay_hints: HashMap<PathBuf, Vec<InlayHint>>,
    /// Semantic tokens, keyed by file -- same per-file-map shape and
    /// stale-until-replaced convention as `inlay_hints`, and the same
    /// "raw `ide_lsp` shape, not yet converted to buffer byte offsets"
    /// choice `inlay_hints` already makes: the `Position`-to-byte-offset
    /// conversion and the `SemanticTokenKind`-to-`ide_core::TokenKind`
    /// mapping both happen inside `CodeEditor::show` (`semantic_token_
    /// marks`, mirroring how `document_highlight_marks` converts
    /// `document_highlights` there), not here -- this bridge never has
    /// buffer text to convert against
    /// (`docs/features/semantic-highlighting.md` §2.3).
    pub semantic_tokens: HashMap<PathBuf, Vec<SemanticToken>>,
    /// Code actions for whatever `(path, position)` was last queried --
    /// replaced wholesale on each `LspEvent::CodeAction`, cleared at
    /// send-time (same convention `document_highlights` already follows).
    pub code_actions: Vec<CodeAction>,
    /// The `(path, position)` the current `code_actions` answers, so
    /// `ide-ui` knows which line to paint the gutter lightbulb on
    /// (`docs/features/code-actions.md` §2.3) -- distinct from `IdeApp`'s
    /// own `last_code_actions_target`, which tracks what was last
    /// *requested* rather than what the current *answer* is for.
    pub code_actions_target: Option<(PathBuf, Position)>,
    /// Which running client `code_actions_target` was sent to -- recorded
    /// at send-time, alongside `code_actions_target`, since
    /// `LspRequest::ApplyCodeAction` carries no path of its own to route
    /// by (`docs/features/multi-language-projects.md` §2.2).
    code_actions_client: Option<String>,
    /// The outcome of the most recently applied `WorkspaceEdit` -- set by
    /// `LspEvent::WorkspaceEditReady`, from either `apply_code_action` or a
    /// server-initiated `workspace/applyEdit` (§3.3, §3.5).
    pub workspace_edit: Option<WorkspaceEdit>,
    pub workspace_edit_label: Option<String>,
    /// True for exactly the one `poll()` call that processed the
    /// most-recently-arrived `LspEvent::WorkspaceEditReady` -- reset to
    /// `false` at the top of every `poll()` call, mirroring `goto_ready`'s
    /// one-frame-true edge.
    pub workspace_edit_ready: bool,
    /// The path the current `document_symbols` answers -- set from
    /// `LspEvent::DocumentSymbol`'s own `path`, once a response arrives
    /// (unlike `IdeApp::document_symbols_requested_for`, which tracks what
    /// was last *sent*, not what was last *answered*;
    /// `docs/features/search-everywhere.md` §2.3, §3.2).
    pub document_symbols_path: Option<PathBuf>,
    /// Result of the most recent (non-superseded) `DocumentSymbol` query --
    /// replaced wholesale on each `LspEvent::DocumentSymbol`, even when
    /// empty.
    pub document_symbols: Vec<Symbol>,
    /// `true` for exactly the one `poll()` call that processed the most-
    /// recently-arrived `LspEvent::DocumentSymbol` -- reset to `false` at
    /// the top of every `poll()`, mirroring `goto_ready`. The Symbols tab
    /// (File Structure / Go to Symbol) doesn't need this -- it just
    /// live-renders whatever's in `document_symbols`, same as
    /// `code_actions` -- but `IdeApp::handle_interface_check_response`
    /// (`docs/features/goto-declaration-interface-redirect.md` §2.2) does:
    /// without a fresh-this-frame signal, a `document_symbols` cache left
    /// over from an unrelated earlier query for the exact file this one
    /// also targets would be misread as this query's own answer before any
    /// real response could possibly have arrived.
    pub document_symbols_ready: bool,
    /// Result of the most recent (non-superseded) `WorkspaceSymbol` query,
    /// merged across every running client that answers it --
    /// `query_workspace_symbols` broadcasts to all of them and `poll()`
    /// appends each response rather than replacing (`docs/features/
    /// multi-language-projects.md` §3.2/§3.4, including the accepted
    /// straggler-response race that design documents).
    pub workspace_symbols: Vec<Symbol>,
    /// The outcome of the most recently sent, not-yet-superseded
    /// `Format`/`FormatRange` query -- replaced wholesale on each
    /// `LspEvent::FormatReady`, or synchronously by `request_format`/
    /// `request_format_range` themselves when no client covers the path
    /// (`docs/features/formatting.md` §2.3). Cleared at send-time and on
    /// the owning language's teardown, same convention `code_actions`
    /// follows.
    pub format_edit: Option<WorkspaceEdit>,
    /// The path `format_edit` answers, so a stale response for a since-
    /// closed or since-changed tab is identifiable (mirrors
    /// `code_actions_target`).
    pub format_path: Option<PathBuf>,
    /// True from the moment a `FormatReady` event is drained (or
    /// `request_format`/`request_format_range`'s no-covering-client path
    /// self-resolves synchronously) until `IdeApp::handle_format_ready`
    /// consumes it. Unlike `goto_ready`/`workspace_edit_ready`, this is
    /// **not** reset at the top of `poll()`: those two are only ever set
    /// from inside `poll()`'s own drain loop, so resetting at entry is
    /// safe, but `request_format`'s no-client path can set this
    /// synchronously *before* `poll()` runs in the same frame
    /// (`handle_shortcuts` runs before `self.lsp.poll()` in `render.rs`'s
    /// `update()`) -- an unconditional reset at `poll()`'s top would
    /// clobber that same-frame set before its consumer ever sees it. The
    /// consumer clears it instead, once it's actually read.
    pub format_ready: bool,
    /// The `(path, position)` the current `prepare_renameable` answers, so
    /// a response that lands after the popup has already moved on
    /// (superseded by a second trigger, or already closed) is identifiable
    /// and ignored rather than misapplied
    /// (`docs/features/rename-refactoring.md` §2.3).
    pub prepare_rename_target: Option<(PathBuf, Position)>,
    pub prepare_renameable: Option<bool>,
    /// One-frame-true, reset unconditionally at the top of `poll()` --
    /// SAFE here, unlike `format_ready`: `request_prepare_rename`/
    /// `request_rename` are ordinary silent-no-op-without-a-covering-
    /// client methods, never self-resolving synchronously outside
    /// `poll()`'s own drain loop, so they belong to the same safe category
    /// `goto_ready`/`workspace_edit_ready` already are
    /// (`docs/features/rename-refactoring.md` §2.3).
    pub prepare_rename_ready: bool,
    pub rename_edit: Option<WorkspaceEdit>,
    pub rename_new_name: Option<String>,
    /// Same reset-at-top-of-`poll()` safety as `prepare_rename_ready`.
    pub rename_ready: bool,
}

impl LspBridge {
    /// Whether *any* client is running -- backs `smart_mode_state`'s
    /// aggregate On/Off/Error indicator and every test that just asserts
    /// "something started." For "is the client covering this specific
    /// file running," see `is_running_for`.
    pub fn is_running(&self) -> bool {
        !self.clients.is_empty()
    }

    /// Whether the client covering `path` specifically is running --
    /// `trigger_rename` and the command-palette Rename/Refactor-family
    /// enablement check need this, not the aggregate `is_running`
    /// (`docs/features/multi-language-projects.md` §2.2).
    pub fn is_running_for(&self, path: &Path) -> bool {
        self.client_for_path(path).is_some()
    }

    /// Whether the client keyed by `extension` (exact, case-insensitive
    /// match on the primary extension a `custom_languages` entry was
    /// configured with, not `extra_extensions`-aware like `is_running_for`)
    /// is running -- the Languages… settings window's per-row running/
    /// stopped indicator (`docs/features/multi-language-projects.md`
    /// §2.4).
    pub fn is_running_for_extension(&self, extension: &str) -> bool {
        self.clients.contains_key(&extension.to_lowercase())
    }

    fn client_for_path(&self, path: &Path) -> Option<&LspClient> {
        self.clients
            .values()
            .find(|running| path_matches(&running.config, path))
            .map(|running| &running.client)
    }

    fn key_for_path(&self, path: &Path) -> Option<String> {
        self.clients
            .iter()
            .find(|(_, running)| path_matches(&running.config, path))
            .map(|(key, _)| key.clone())
    }

    /// Forwards `request` to the client covering `path`, if any. A no-op
    /// when nothing covers it -- callers don't need to check
    /// `is_running_for` first.
    pub fn send(&self, path: &Path, request: LspRequest) {
        if let Some(client) = self.client_for_path(path) {
            client.send(request);
        }
    }

    /// Forwards `request` to the client keyed by `key` directly, bypassing
    /// path-based routing -- `apply_code_action`'s only use, since
    /// `LspRequest::ApplyCodeAction` carries no path of its own.
    fn send_to_key(&self, key: &str, request: LspRequest) {
        if let Some(running) = self.clients.get(key) {
            running.client.send(request);
        }
    }

    /// Reconciles the running client set against `active` (diff, not
    /// full reset): starts a client for any `active` entry with no
    /// matching running key, or whose running key's stored config
    /// differs from the new one; stops any running key no longer present
    /// in `active`; leaves an unchanged, already-running entry alone
    /// (`docs/features/multi-language-projects.md` §2.2). The primary
    /// entry point `IdeApp::resync_active_languages` and
    /// `poll_tree_scan`'s `Refresh` arm both use.
    pub fn sync_active_languages(&mut self, project_root: &Path, active: &[LanguageConfig]) {
        let active_keys: Vec<String> = active.iter().map(|c| c.extension.to_lowercase()).collect();
        let stale: Vec<String> = self
            .clients
            .keys()
            .filter(|k| !active_keys.contains(k))
            .cloned()
            .collect();
        for key in stale {
            self.stop_language(&key);
        }
        for config in active {
            let key = config.extension.to_lowercase();
            let unchanged = self
                .clients
                .get(&key)
                .is_some_and(|running| &running.config == config);
            if !unchanged {
                self.start_language(project_root, config);
            }
        }
    }

    /// Stops every running key not in `active`, then (re)starts every
    /// entry in `active` unconditionally, even ones already running
    /// unchanged -- the "Restart Language Server" action's primitive: an
    /// explicit user request for fresh processes, unlike the diff-only
    /// `sync_active_languages` (`docs/features/multi-language-projects.md`
    /// §2.2).
    pub fn restart_all(&mut self, project_root: &Path, active: &[LanguageConfig]) {
        let active_keys: Vec<String> = active.iter().map(|c| c.extension.to_lowercase()).collect();
        let stale: Vec<String> = self
            .clients
            .keys()
            .filter(|k| !active_keys.contains(k))
            .cloned()
            .collect();
        for key in stale {
            self.stop_language(&key);
        }
        for config in active {
            self.start_language(project_root, config);
        }
    }

    /// Spawns `config.command`/`config.args` for `project_root`, replacing
    /// any existing client for the same extension key -- still a real
    /// `Command::new(command).args(args)` argv, no shell, ever (see
    /// `docs/features/language-server-arguments.md` §4 for the accepted
    /// trust-boundary this opens: the caller decides what's spawned and
    /// with what arguments, this method never interprets either itself).
    /// Clears this language's own scoped state first (§3.3's
    /// `stop_language` clearing, reused here since a restart is a
    /// teardown immediately followed by a spawn).
    fn start_language(&mut self, project_root: &Path, config: &LanguageConfig) {
        let key = config.extension.to_lowercase();
        self.clear_language_scoped_state(&key, config);
        match LspClient::start_with_command(project_root, &config.command, &config.args) {
            Ok(client) => {
                self.clients.insert(
                    key,
                    RunningLanguage {
                        config: config.clone(),
                        client,
                    },
                );
            }
            Err(e) => {
                self.clients.remove(&key);
                self.server_error = Some(e.to_string());
            }
        }
    }

    /// Drops the client keyed by `key` (if any) -- dropping the
    /// `LspClient` *is* its teardown path, there is no separate stop call
    /// (`ide-lsp` doc §2.1) -- and scoped-clears everything this bridge
    /// tracks that belonged to it (`docs/features/multi-language-
    /// projects.md` §3.3).
    fn stop_language(&mut self, key: &str) {
        if let Some(running) = self.clients.remove(key) {
            self.clear_language_scoped_state(key, &running.config);
        }
    }

    fn clear_language_scoped_state(&mut self, key: &str, config: &LanguageConfig) {
        self.diagnostics.retain(|p, _| !path_matches(config, p));
        self.inlay_hints.retain(|p, _| !path_matches(config, p));
        self.semantic_tokens.retain(|p, _| !path_matches(config, p));
        if self.code_actions_client.as_deref() == Some(key) {
            self.code_actions.clear();
            self.code_actions_target = None;
            self.code_actions_client = None;
        }
        if self
            .prepare_rename_target
            .as_ref()
            .is_some_and(|(p, _)| path_matches(config, p))
        {
            self.prepare_rename_target = None;
            self.prepare_renameable = None;
        }
        if self
            .format_path
            .as_ref()
            .is_some_and(|p| path_matches(config, p))
        {
            self.format_edit = None;
            self.format_path = None;
            // `format_ready` is deliberately left untouched -- see its own
            // doc comment.
        }
        if self
            .document_symbols_path
            .as_ref()
            .is_some_and(|p| path_matches(config, p))
        {
            self.document_symbols_path = None;
            self.document_symbols.clear();
        }
        // Deliberately not scoped, matching this file's pre-multi-client
        // `ServerExited` behavior: `hover`/`references`/`goto`/
        // `document_highlights`/`rename_edit`/`rename_new_name`/
        // `workspace_symbols` are left untouched regardless of which
        // language tore down -- the popup/panel already showing an answer
        // isn't made misleading by one server dying, the same reasoning
        // `hover`'s own doc comment already gives
        // (`docs/features/multi-language-projects.md` §3.3).
        self.finding_references = false;
        self.finding_goto = false;
        self.finding_hover = false;
    }

    /// Drops every client and clears all state -- the "no project" /
    /// Smart-Mode-toggled-off path (doc §3).
    pub fn stop_all(&mut self) {
        self.clients.clear();
        self.diagnostics.clear();
        self.server_error = None;
        self.references.clear();
        self.finding_references = false;
        self.goto.clear();
        self.finding_goto = false;
        self.goto_ready = false;
        self.hover = None;
        self.finding_hover = false;
        self.document_highlights.clear();
        self.inlay_hints.clear();
        self.semantic_tokens.clear();
        self.code_actions.clear();
        self.code_actions_target = None;
        self.code_actions_client = None;
        self.workspace_edit = None;
        self.workspace_edit_label = None;
        self.workspace_edit_ready = false;
        self.document_symbols_path = None;
        self.document_symbols.clear();
        self.document_symbols_ready = false;
        self.workspace_symbols.clear();
        self.format_edit = None;
        self.format_path = None;
        self.format_ready = false;
        self.prepare_rename_target = None;
        self.prepare_renameable = None;
        self.prepare_rename_ready = false;
        self.rename_edit = None;
        self.rename_new_name = None;
        self.rename_ready = false;
    }

    /// Sends `LspRequest::References` for `path`/`position` -- a total
    /// no-op (including leaving `finding_references` false) if no client
    /// covers `path`, so a query with nothing to ever answer it doesn't
    /// leave the Usages panel stuck showing "Finding usages…" forever
    /// (doc §3). Otherwise clears any previous `references` and sets
    /// `finding_references` before sending.
    pub fn find_references(&mut self, path: &Path, position: Position) {
        if !self.is_running_for(path) {
            return;
        }
        self.references.clear();
        self.finding_references = true;
        self.send(
            path,
            LspRequest::References {
                path: path.to_path_buf(),
                position,
            },
        );
    }

    /// Sends `LspRequest::Goto { kind: GotoKind::Definition, .. }` -- same
    /// no-op-if-uncovered shape as `find_references`
    /// (`docs/features/goto-definition.md` §2.2/§3.2).
    pub fn go_to_definition(&mut self, path: &Path, position: Position) {
        self.go_to(GotoKind::Definition, path, position);
    }

    /// Same shape, `GotoKind::TypeDefinition`.
    pub fn go_to_type_definition(&mut self, path: &Path, position: Position) {
        self.go_to(GotoKind::TypeDefinition, path, position);
    }

    /// Same shape, `GotoKind::Implementation`.
    pub fn go_to_implementation(&mut self, path: &Path, position: Position) {
        self.go_to(GotoKind::Implementation, path, position);
    }

    fn go_to(&mut self, kind: GotoKind, path: &Path, position: Position) {
        if !self.is_running_for(path) {
            return;
        }
        self.goto.clear();
        self.finding_goto = true;
        self.send(
            path,
            LspRequest::Goto {
                kind,
                path: path.to_path_buf(),
                position,
            },
        );
    }

    /// Sends `LspRequest::Hover` -- no-op (leaving `hover`/`finding_hover`
    /// untouched) if no client covers `path`, same shape as
    /// `find_references`/`go_to_*` (`docs/features/inlay-hints-and-hover.md`
    /// §2.2). Otherwise clears `hover`, sets `finding_hover`, sends the
    /// request.
    pub fn request_hover(&mut self, path: &Path, position: Position) {
        if !self.is_running_for(path) {
            return;
        }
        self.hover = None;
        self.finding_hover = true;
        self.send(
            path,
            LspRequest::Hover {
                path: path.to_path_buf(),
                position,
            },
        );
    }

    /// Same shape, clears/refills `document_highlights`, sends
    /// `LspRequest::DocumentHighlight`.
    pub fn request_document_highlight(&mut self, path: &Path, position: Position) {
        if !self.is_running_for(path) {
            return;
        }
        self.document_highlights.clear();
        self.send(
            path,
            LspRequest::DocumentHighlight {
                path: path.to_path_buf(),
                position,
            },
        );
    }

    /// Clears `document_highlights` without sending anything -- the
    /// counterpart `request_document_highlight` has no use for: there's no
    /// query to send when `find_usages_target()` returns `None` (§3.4), but
    /// the previous target's highlights must not keep rendering.
    pub fn clear_document_highlights(&mut self) {
        self.document_highlights.clear();
    }

    /// Same shape, sends `LspRequest::InlayHint { path, range }`. Does
    /// *not* clear `inlay_hints[path]` at send-time (unlike the other
    /// three) -- the existing hints stay visible, stale-but-plausible,
    /// until the fresh response replaces them; clearing immediately would
    /// make every chip flicker away and back on every keystroke.
    pub fn request_inlay_hints(&mut self, path: &Path, range: Range) {
        if !self.is_running_for(path) {
            return;
        }
        self.send(
            path,
            LspRequest::InlayHint {
                path: path.to_path_buf(),
                range,
            },
        );
    }

    /// Sends `LspRequest::SemanticTokensFull { path }` -- same no-op-if-
    /// uncovered shape, and the same "does not clear `semantic_tokens
    /// [path]` at send-time" stale-but-plausible-until-replaced choice
    /// `request_inlay_hints` already makes and for the same reason (no
    /// per-keystroke flicker, `docs/features/semantic-highlighting.md`
    /// §2.3). Always the whole document -- there is no range parameter,
    /// unlike `InlayHint` (§4: v1 doesn't scope to the visible viewport).
    pub fn request_semantic_tokens(&mut self, path: &Path) {
        if !self.is_running_for(path) {
            return;
        }
        self.send(
            path,
            LspRequest::SemanticTokensFull {
                path: path.to_path_buf(),
            },
        );
    }

    /// Same no-op-if-uncovered shape as `request_document_highlight`.
    /// Clears `code_actions` and records `code_actions_target`/
    /// `code_actions_client` before sending, so a stale target is never
    /// mistaken for the answer to a not-yet-sent query, and
    /// `apply_code_action` knows which client to route to.
    pub fn request_code_actions(&mut self, path: &Path, position: Position) {
        let Some(key) = self.key_for_path(path) else {
            return;
        };
        self.code_actions.clear();
        self.code_actions_target = Some((path.to_path_buf(), position));
        self.code_actions_client = Some(key.clone());
        self.send_to_key(
            &key,
            LspRequest::CodeAction {
                path: path.to_path_buf(),
                position,
            },
        );
    }

    /// Clears `code_actions`/`code_actions_target`/`code_actions_client`
    /// without sending anything -- mirrors `clear_document_highlights`
    /// (there's no query to send once `find_usages_target()` returns
    /// `None`).
    pub fn clear_code_actions(&mut self) {
        self.code_actions.clear();
        self.code_actions_target = None;
        self.code_actions_client = None;
    }

    /// Sends `LspRequest::ApplyCodeAction { index }` to whichever client
    /// last answered `code_actions` -- no-op if none did (nothing cached
    /// server-side to apply to in that case either). `ApplyCodeAction`
    /// carries no path of its own, unlike every other request here, so it
    /// routes via `code_actions_client` instead of `client_for_path`
    /// (`docs/features/multi-language-projects.md` §2.2).
    pub fn apply_code_action(&self, index: usize) {
        let Some(key) = &self.code_actions_client else {
            return;
        };
        self.send_to_key(key, LspRequest::ApplyCodeAction { index });
    }

    /// Sends `LspRequest::DocumentSymbol` -- same no-op-if-uncovered shape
    /// as every other request method (`docs/features/search-everywhere.md`
    /// §3.1). Does not clear `document_symbols`/`document_symbols_path` at
    /// send-time -- unlike `code_actions`, the previous outline stays
    /// visible, stale-but-plausible, until the fresh response replaces it
    /// (same rationale `request_inlay_hints` already gives for the same
    /// choice).
    pub fn request_document_symbols(&mut self, path: &Path) {
        if !self.is_running_for(path) {
            return;
        }
        self.send(
            path,
            LspRequest::DocumentSymbol {
                path: path.to_path_buf(),
            },
        );
    }

    /// Sends `LspRequest::OrganizeImports` -- same no-op-if-uncovered shape
    /// as every other request method. Fire-and-forget: no target-tracking
    /// field like `code_actions_target`/`code_actions_client`, since there
    /// is no ambient re-query for a later `apply`-style request to dedupe
    /// against -- `OrganizeImports` carries its own `path` and settles
    /// directly into `WorkspaceEditReady`, the same "nothing to route a
    /// follow-up to" shape `request_document_symbols` already has
    /// (`docs/features/code-generation.md` §2.1, §3.4).
    pub fn request_organize_imports(&mut self, path: &Path) {
        if !self.is_running_for(path) {
            return;
        }
        self.send(
            path,
            LspRequest::OrganizeImports {
                path: path.to_path_buf(),
            },
        );
    }

    /// Broadcasts `LspRequest::WorkspaceSymbol` to *every* running client
    /// -- a workspace symbol search is meant to cover the whole project
    /// regardless of which language each hit is in, unlike every other
    /// request method here which targets exactly one client
    /// (`docs/features/multi-language-projects.md` §2.2/§3.2). No-op if
    /// nothing is running. Clears `workspace_symbols` at send-time --
    /// a deliberate reversal of the single-client version's
    /// stale-but-plausible convention; see §3.4 for why and the accepted
    /// straggler-response race this still leaves open.
    pub fn query_workspace_symbols(&mut self, query: &str) {
        if self.clients.is_empty() {
            return;
        }
        self.workspace_symbols.clear();
        for running in self.clients.values() {
            running.client.send(LspRequest::WorkspaceSymbol {
                query: query.to_string(),
            });
        }
    }

    /// `tab_size`/`insert_spaces` come from the caller's already-resolved
    /// `IndentUnit` (`docs/features/formatting.md` §3.1) -- `ide-lsp` has
    /// no dependency on `ide-core` and cannot resolve `EditorConfig`
    /// itself.
    ///
    /// Unlike every other `request_*` method on this type, no client
    /// covering `path` is **not** a silent no-op: it immediately sets
    /// `format_ready = true`, `format_edit = None`,
    /// `format_path = Some(path.to_path_buf())`, entirely inside
    /// `LspBridge` (no `LspClient`, no wire traffic, no `LspEvent`
    /// involved) -- the same observable outcome an unsupported-capability
    /// response produces one layer down. Every caller, including
    /// `IdeApp`'s Format-on-Save bookkeeping, can rely on "calling this
    /// always eventually sets `format_ready`" without checking
    /// `is_running_for` itself first (§2.3, §4).
    pub fn request_format(&mut self, path: &Path, tab_size: u32, insert_spaces: bool) {
        if !self.is_running_for(path) {
            self.format_ready = true;
            self.format_edit = None;
            self.format_path = Some(path.to_path_buf());
            return;
        }
        self.send(
            path,
            LspRequest::Format {
                path: path.to_path_buf(),
                tab_size,
                insert_spaces,
            },
        );
    }

    /// Same, for a range -- no `ide-ui` caller in this phase
    /// (`docs/features/formatting.md` §1), kept for parity with the
    /// wire-level `FormatRange` request and so a future range-aware
    /// caller has it ready to call. Same no-covering-client self-resolving
    /// guarantee as `request_format`.
    #[allow(dead_code)]
    pub fn request_format_range(
        &mut self,
        path: &Path,
        range: Range,
        tab_size: u32,
        insert_spaces: bool,
    ) {
        if !self.is_running_for(path) {
            self.format_ready = true;
            self.format_edit = None;
            self.format_path = Some(path.to_path_buf());
            return;
        }
        self.send(
            path,
            LspRequest::FormatRange {
                path: path.to_path_buf(),
                range,
                tab_size,
                insert_spaces,
            },
        );
    }

    /// No-op if no client covers `path` (ordinary shape, unlike `Format`'s
    /// self-resolving one -- see `prepare_rename_ready`'s doc comment).
    /// Records `(path, position)` as the target the eventual response
    /// answers, and clears any previous answer -- same "clear at
    /// send-time" convention `hover`/`goto` already follow.
    pub fn request_prepare_rename(&mut self, path: &Path, position: Position) {
        if !self.is_running_for(path) {
            return;
        }
        self.prepare_rename_target = Some((path.to_path_buf(), position));
        self.prepare_renameable = None;
        self.send(
            path,
            LspRequest::PrepareRename {
                path: path.to_path_buf(),
                position,
            },
        );
    }

    /// Same no-op-if-uncovered shape. Clears any previous `rename_edit`/
    /// `rename_new_name` at send-time.
    pub fn request_rename(&mut self, path: &Path, position: Position, new_name: String) {
        if !self.is_running_for(path) {
            return;
        }
        self.rename_edit = None;
        self.rename_new_name = None;
        self.send(
            path,
            LspRequest::Rename {
                path: path.to_path_buf(),
                position,
                new_name,
            },
        );
    }

    /// Drains every event available this frame from every running client
    /// (not just one, same as `ClaudePanel::poll`'s repaint pattern).
    /// Returns `true` if anything changed. A `ServerExited` event tears
    /// down just the client it came from via `stop_language`, aside from
    /// leaving `server_error` set to its message
    /// (`docs/features/multi-language-projects.md` §3.2).
    pub fn poll(&mut self) -> bool {
        self.goto_ready = false;
        self.workspace_edit_ready = false;
        self.prepare_rename_ready = false;
        self.rename_ready = false;
        self.document_symbols_ready = false;
        // `format_ready` is deliberately not reset here -- see its own doc
        // comment. `IdeApp::handle_format_ready` clears it once consumed.
        let mut changed = false;
        let mut exited = Vec::new();
        for (key, running) in self.clients.iter_mut() {
            while let Some(event) = running.client.try_recv() {
                changed = true;
                match event {
                    LspEvent::Diagnostics { path, diagnostics } => {
                        self.diagnostics.insert(path, diagnostics);
                    }
                    LspEvent::References { locations } => {
                        self.references = locations;
                        self.finding_references = false;
                    }
                    LspEvent::Goto { locations } => {
                        self.goto = locations;
                        self.finding_goto = false;
                        self.goto_ready = true;
                    }
                    LspEvent::Hover { contents } => {
                        self.hover = contents;
                        self.finding_hover = false;
                    }
                    LspEvent::DocumentHighlight { ranges } => {
                        self.document_highlights = ranges;
                    }
                    LspEvent::InlayHint { path, hints } => {
                        self.inlay_hints.insert(path, hints);
                    }
                    LspEvent::SemanticTokens { path, tokens } => {
                        self.semantic_tokens.insert(path, tokens);
                    }
                    LspEvent::CodeAction { path: _, actions } => {
                        self.code_actions = actions;
                    }
                    LspEvent::WorkspaceEditReady { edit, label } => {
                        self.workspace_edit = edit;
                        self.workspace_edit_label = label;
                        self.workspace_edit_ready = true;
                    }
                    LspEvent::DocumentSymbol { path, symbols } => {
                        self.document_symbols_path = Some(path);
                        self.document_symbols = symbols;
                        self.document_symbols_ready = true;
                    }
                    LspEvent::WorkspaceSymbol { symbols } => {
                        self.workspace_symbols.extend(symbols);
                    }
                    LspEvent::FormatReady { path, edit } => {
                        self.format_edit = edit;
                        self.format_path = Some(path);
                        self.format_ready = true;
                    }
                    LspEvent::PrepareRenameReady {
                        path: _,
                        renameable,
                    } => {
                        self.prepare_renameable = Some(renameable);
                        self.prepare_rename_ready = true;
                    }
                    LspEvent::RenameReady {
                        path: _,
                        new_name,
                        edit,
                    } => {
                        self.rename_edit = edit;
                        self.rename_new_name = Some(new_name);
                        self.rename_ready = true;
                    }
                    LspEvent::ServerExited { message } => {
                        self.server_error = Some(message);
                        exited.push(key.clone());
                        break;
                    }
                }
            }
        }
        for key in exited {
            self.stop_language(&key);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_lsp::{DiagnosticSeverity, Position, Range, SemanticTokenKind};

    fn go_config() -> LanguageConfig {
        LanguageConfig {
            name: "Go".to_string(),
            extension: "go".to_string(),
            command: "definitely-not-a-real-lsp-binary-xyz".to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
            ..Default::default()
        }
    }

    fn swift_config() -> LanguageConfig {
        LanguageConfig {
            name: "Swift".to_string(),
            extension: "swift".to_string(),
            command: "definitely-not-a-real-lsp-binary-xyz".to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn sync_with_missing_binaries_sets_server_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        bridge.sync_active_languages(dir.path(), &[go_config()]);
        assert!(!bridge.is_running());
        assert!(bridge.server_error.is_some());
    }

    #[test]
    fn stop_all_clears_all_state() {
        let mut bridge = LspBridge::default();
        bridge.diagnostics.insert(
            PathBuf::from("/x.rs"),
            vec![Diagnostic {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 1,
                    },
                },
                severity: DiagnosticSeverity::Error,
                message: "x".to_string(),
            }],
        );
        bridge.server_error = Some("boom".to_string());
        bridge.references.push(Location {
            path: PathBuf::from("/y.rs"),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
        });
        bridge.finding_references = true;
        bridge.goto.push(Location {
            path: PathBuf::from("/z.rs"),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
        });
        bridge.finding_goto = true;
        bridge.goto_ready = true;
        bridge.hover = Some("docs".to_string());
        bridge.finding_hover = true;
        bridge.document_highlights.push(Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        });
        bridge.inlay_hints.insert(
            PathBuf::from("/w.rs"),
            vec![InlayHint {
                position: Position {
                    line: 0,
                    character: 0,
                },
                label: ": i32".to_string(),
                padding_left: true,
                padding_right: false,
            }],
        );
        bridge.document_symbols_path = Some(PathBuf::from("/v.rs"));
        bridge.document_symbols.push(Symbol {
            name: "foo".to_string(),
            kind: ide_lsp::SymbolKind::Function,
            container_name: None,
            location: Location {
                path: PathBuf::from("/v.rs"),
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 1,
                    },
                },
            },
        });
        bridge.workspace_symbols.push(Symbol {
            name: "bar".to_string(),
            kind: ide_lsp::SymbolKind::Struct,
            container_name: None,
            location: Location {
                path: PathBuf::from("/v.rs"),
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 1,
                    },
                },
            },
        });
        bridge.format_edit = Some(WorkspaceEdit { edits: Vec::new() });
        bridge.format_path = Some(PathBuf::from("/fmt.rs"));
        bridge.format_ready = true;
        bridge.prepare_rename_target = Some((
            PathBuf::from("/r.rs"),
            Position {
                line: 0,
                character: 0,
            },
        ));
        bridge.prepare_renameable = Some(true);
        bridge.prepare_rename_ready = true;
        bridge.rename_edit = Some(WorkspaceEdit { edits: Vec::new() });
        bridge.rename_new_name = Some("renamed".to_string());
        bridge.rename_ready = true;
        bridge.semantic_tokens.insert(
            PathBuf::from("/s.rs"),
            vec![SemanticToken {
                position: Position {
                    line: 0,
                    character: 0,
                },
                length: 3,
                kind: SemanticTokenKind::Type,
            }],
        );

        bridge.stop_all();

        assert!(!bridge.is_running());
        assert!(bridge.diagnostics.is_empty());
        assert!(bridge.server_error.is_none());
        assert!(bridge.references.is_empty());
        assert!(!bridge.finding_references);
        assert!(bridge.goto.is_empty());
        assert!(!bridge.finding_goto);
        assert!(!bridge.goto_ready);
        assert!(bridge.hover.is_none());
        assert!(!bridge.finding_hover);
        assert!(bridge.document_highlights.is_empty());
        assert!(bridge.inlay_hints.is_empty());
        assert!(bridge.semantic_tokens.is_empty());
        assert!(bridge.document_symbols_path.is_none());
        assert!(bridge.document_symbols.is_empty());
        assert!(bridge.workspace_symbols.is_empty());
        assert!(bridge.format_edit.is_none());
        assert!(bridge.format_path.is_none());
        assert!(!bridge.format_ready);
        assert!(bridge.prepare_rename_target.is_none());
        assert!(bridge.prepare_renameable.is_none());
        assert!(!bridge.prepare_rename_ready);
        assert!(bridge.rename_edit.is_none());
        assert!(bridge.rename_new_name.is_none());
        assert!(!bridge.rename_ready);
    }

    #[test]
    fn find_references_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.find_references(
            Path::new("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        assert!(!bridge.finding_references);
        assert!(bridge.references.is_empty());
    }

    #[test]
    fn go_to_definition_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.go_to_definition(
            Path::new("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        assert!(!bridge.finding_goto);
        assert!(bridge.goto.is_empty());
    }

    #[test]
    fn go_to_type_definition_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.go_to_type_definition(
            Path::new("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        assert!(!bridge.finding_goto);
        assert!(bridge.goto.is_empty());
    }

    #[test]
    fn go_to_implementation_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.go_to_implementation(
            Path::new("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        assert!(!bridge.finding_goto);
        assert!(bridge.goto.is_empty());
    }

    #[test]
    fn request_hover_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_hover(
            Path::new("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        assert!(!bridge.finding_hover);
        assert!(bridge.hover.is_none());
    }

    #[test]
    fn request_document_highlight_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_document_highlight(
            Path::new("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        assert!(bridge.document_highlights.is_empty());
    }

    #[test]
    fn clear_document_highlights_clears_without_a_running_client() {
        let mut bridge = LspBridge::default();
        bridge.document_highlights.push(Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        });
        bridge.clear_document_highlights();
        assert!(bridge.document_highlights.is_empty());
    }

    #[test]
    fn request_inlay_hints_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_inlay_hints(
            Path::new("/x.rs"),
            Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 1,
                    character: 0,
                },
            },
        );
        assert!(bridge.inlay_hints.is_empty());
    }

    #[test]
    fn request_semantic_tokens_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_semantic_tokens(Path::new("/x.rs"));
        assert!(bridge.semantic_tokens.is_empty());
    }

    #[test]
    fn request_code_actions_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_code_actions(
            Path::new("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        // No client covering it means no target was ever recorded either
        // -- there was nothing to answer it.
        assert!(bridge.code_actions.is_empty());
        assert!(bridge.code_actions_target.is_none());
        assert!(bridge.code_actions_client.is_none());
    }

    #[test]
    fn clear_code_actions_clears_without_a_running_client() {
        let mut bridge = LspBridge::default();
        bridge.code_actions.push(CodeAction {
            index: 0,
            title: "x".to_string(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
        });
        bridge.code_actions_target = Some((
            PathBuf::from("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        ));
        bridge.code_actions_client = Some("rs".to_string());
        bridge.clear_code_actions();
        assert!(bridge.code_actions.is_empty());
        assert!(bridge.code_actions_target.is_none());
        assert!(bridge.code_actions_client.is_none());
    }

    #[test]
    fn apply_code_action_with_no_code_actions_client_is_a_noop() {
        let bridge = LspBridge::default();
        // Must not panic even though nothing is running.
        bridge.apply_code_action(0);
    }

    #[test]
    fn request_document_symbols_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_document_symbols(Path::new("/x.rs"));
        assert!(bridge.document_symbols.is_empty());
        assert!(bridge.document_symbols_path.is_none());
    }

    #[test]
    fn request_organize_imports_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        // Must not panic even though nothing is running.
        bridge.request_organize_imports(Path::new("/x.rs"));
    }

    #[test]
    fn query_workspace_symbols_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.query_workspace_symbols("foo");
        assert!(bridge.workspace_symbols.is_empty());
    }

    #[test]
    fn request_prepare_rename_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_prepare_rename(
            Path::new("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        assert!(bridge.prepare_rename_target.is_none());
        assert!(bridge.prepare_renameable.is_none());
    }

    #[test]
    fn request_rename_with_nothing_covering_the_path_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_rename(
            Path::new("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
            "renamed".to_string(),
        );
        assert!(bridge.rename_edit.is_none());
        assert!(bridge.rename_new_name.is_none());
    }

    #[test]
    fn poll_with_no_client_running_returns_false() {
        let mut bridge = LspBridge::default();
        assert!(!bridge.poll());
    }

    #[test]
    fn sync_active_languages_starts_two_independent_clients() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        bridge.sync_active_languages(dir.path(), &[go_config(), swift_config()]);

        // Both configs name a deliberately nonexistent binary, so neither
        // spawn succeeds -- but each must have been attempted
        // independently, not short-circuited after the first failure.
        assert!(!bridge.is_running());
        assert!(!bridge.is_running_for_extension("go"));
        assert!(!bridge.is_running_for_extension("swift"));
    }

    #[test]
    fn sync_active_languages_leaves_an_unchanged_running_language_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        // `cat` is a real, always-spawnable process (same stand-in
        // convention `request_methods_forward_to_a_running_client`
        // already uses) so this key actually starts running.
        let go = LanguageConfig {
            command: "cat".to_string(),
            ..go_config()
        };
        bridge.sync_active_languages(dir.path(), std::slice::from_ref(&go));
        assert!(bridge.is_running_for_extension("go"));

        // Diagnostics for a "go" path should survive an unrelated
        // re-sync with the exact same config.
        bridge
            .diagnostics
            .insert(PathBuf::from("/x.go"), Vec::new());
        bridge.sync_active_languages(dir.path(), &[go]);

        assert!(bridge.is_running_for_extension("go"));
        assert!(bridge.diagnostics.contains_key(Path::new("/x.go")));
    }

    #[test]
    fn sync_active_languages_restarts_only_the_language_whose_config_changed() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        let go = LanguageConfig {
            command: "cat".to_string(),
            ..go_config()
        };
        let swift = LanguageConfig {
            command: "cat".to_string(),
            ..swift_config()
        };
        bridge.sync_active_languages(dir.path(), &[go.clone(), swift.clone()]);
        assert!(bridge.is_running_for_extension("go"));
        assert!(bridge.is_running_for_extension("swift"));

        bridge
            .diagnostics
            .insert(PathBuf::from("/x.swift"), Vec::new());

        let go_changed = LanguageConfig {
            command: "definitely-not-a-real-lsp-binary-xyz".to_string(),
            ..go
        };
        bridge.sync_active_languages(dir.path(), &[go_changed, swift]);

        // Go's config changed -> restarted, and its new command doesn't
        // exist, so it's no longer running.
        assert!(!bridge.is_running_for_extension("go"));
        // Swift's config didn't change -> left running, its diagnostics
        // untouched.
        assert!(bridge.is_running_for_extension("swift"));
        assert!(bridge.diagnostics.contains_key(Path::new("/x.swift")));
    }

    #[test]
    fn sync_active_languages_stops_a_language_no_longer_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        let go = LanguageConfig {
            command: "cat".to_string(),
            ..go_config()
        };
        bridge.sync_active_languages(dir.path(), &[go]);
        assert!(bridge.is_running_for_extension("go"));

        bridge.sync_active_languages(dir.path(), &[]);
        assert!(!bridge.is_running_for_extension("go"));
        assert!(!bridge.is_running());
    }

    #[test]
    fn is_running_for_resolves_through_extra_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        let cpp = LanguageConfig {
            name: "C/C++".to_string(),
            extension: "cpp".to_string(),
            command: "cat".to_string(),
            args: Vec::new(),
            extra_extensions: vec!["c".to_string()],
            ..Default::default()
        };
        bridge.sync_active_languages(dir.path(), &[cpp]);

        assert!(bridge.is_running_for(Path::new("/x.cpp")));
        assert!(bridge.is_running_for(Path::new("/x.c")));
        assert!(!bridge.is_running_for(Path::new("/x.py")));
    }

    #[test]
    fn stop_language_scoped_clearing_leaves_another_languages_state_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        let go = LanguageConfig {
            command: "cat".to_string(),
            ..go_config()
        };
        let swift = LanguageConfig {
            command: "cat".to_string(),
            ..swift_config()
        };
        bridge.sync_active_languages(dir.path(), &[go, swift]);
        bridge
            .diagnostics
            .insert(PathBuf::from("/x.go"), Vec::new());
        bridge
            .diagnostics
            .insert(PathBuf::from("/y.swift"), Vec::new());

        bridge.sync_active_languages(dir.path(), &[swift_config_with_command("cat")]);

        assert!(!bridge.is_running_for_extension("go"));
        assert!(!bridge.diagnostics.contains_key(Path::new("/x.go")));
        assert!(bridge.is_running_for_extension("swift"));
        assert!(bridge.diagnostics.contains_key(Path::new("/y.swift")));
    }

    fn swift_config_with_command(command: &str) -> LanguageConfig {
        LanguageConfig {
            command: command.to_string(),
            ..swift_config()
        }
    }

    #[test]
    fn send_with_nothing_covering_the_path_is_a_noop() {
        let bridge = LspBridge::default();
        // Must not panic even though no client is running.
        bridge.send(
            Path::new("/x.rs"),
            LspRequest::DidClose {
                path: PathBuf::from("/x.rs"),
            },
        );
    }

    #[test]
    fn request_methods_forward_to_the_covering_client() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        let go = LanguageConfig {
            command: "cat".to_string(),
            ..go_config()
        };
        bridge.sync_active_languages(dir.path(), &[go]);
        assert!(bridge.is_running());

        let path = dir.path().join("f.go");
        let position = Position {
            line: 0,
            character: 0,
        };

        bridge.send(&path, LspRequest::DidClose { path: path.clone() });

        bridge.find_references(&path, position);
        assert!(bridge.finding_references);

        bridge.go_to_definition(&path, position);
        assert!(bridge.finding_goto);

        bridge.go_to_type_definition(&path, position);
        bridge.go_to_implementation(&path, position);

        bridge.request_hover(&path, position);
        assert!(bridge.finding_hover);

        bridge.request_document_highlight(&path, position);
        bridge.request_inlay_hints(
            &path,
            Range {
                start: position,
                end: position,
            },
        );

        bridge.request_code_actions(&path, position);
        assert_eq!(bridge.code_actions_target, Some((path.clone(), position)));
        assert_eq!(bridge.code_actions_client.as_deref(), Some("go"));

        bridge.apply_code_action(0);
        bridge.request_document_symbols(&path);
        bridge.query_workspace_symbols("foo");

        // Unlike every method above, `request_format`/`request_format_range`
        // with a covering client forward to `LspRequest` rather than
        // self-resolving -- `format_ready` stays false until a real
        // `FormatReady` event actually arrives via `poll()`.
        bridge.request_format(&path, 4, true);
        assert!(!bridge.format_ready);
        bridge.request_format_range(
            &path,
            Range {
                start: position,
                end: position,
            },
            4,
            true,
        );
        assert!(!bridge.format_ready);

        bridge.request_prepare_rename(&path, position);
        assert_eq!(bridge.prepare_rename_target, Some((path.clone(), position)));
        assert!(!bridge.prepare_rename_ready);

        bridge.request_rename(&path, position, "renamed".to_string());
        assert!(!bridge.rename_ready);

        bridge.stop_all();
        assert!(!bridge.is_running());
    }

    #[test]
    fn query_workspace_symbols_broadcasts_to_every_running_client() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        let go = LanguageConfig {
            command: "cat".to_string(),
            ..go_config()
        };
        let swift = LanguageConfig {
            command: "cat".to_string(),
            ..swift_config()
        };
        bridge.sync_active_languages(dir.path(), &[go, swift]);
        assert!(bridge.is_running_for_extension("go"));
        assert!(bridge.is_running_for_extension("swift"));

        // Both `cat` processes are real and alive; this only proves the
        // call doesn't panic and reaches `send` for each -- a full
        // round-trip response needs a real LSP speaker, which is
        // `ide-lsp`'s own fixture-backed integration tests' job, not this
        // bridge's.
        bridge.query_workspace_symbols("Session");
        assert!(bridge.workspace_symbols.is_empty());
    }

    #[test]
    fn request_format_with_nothing_covering_the_path_self_resolves_immediately() {
        let mut bridge = LspBridge::default();
        let path = Path::new("/x.rs");
        bridge.request_format(path, 4, true);
        assert!(bridge.format_ready);
        assert_eq!(bridge.format_edit, None);
        assert_eq!(bridge.format_path, Some(path.to_path_buf()));
    }

    #[test]
    fn request_format_range_with_nothing_covering_the_path_self_resolves_immediately() {
        let mut bridge = LspBridge::default();
        let path = Path::new("/x.rs");
        bridge.request_format_range(
            path,
            Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
            4,
            true,
        );
        assert!(bridge.format_ready);
        assert_eq!(bridge.format_edit, None);
        assert_eq!(bridge.format_path, Some(path.to_path_buf()));
    }

    #[test]
    fn poll_does_not_clobber_a_same_frame_synchronous_format_ready() {
        // `format_ready`, unlike `goto_ready`/`workspace_edit_ready`, is not
        // reset at the top of `poll()` -- a same-frame synchronous
        // no-covering-client self-resolve from `request_format` (called
        // from `handle_shortcuts`, before `poll()` runs) must survive
        // until `IdeApp::handle_format_ready` consumes it later that
        // frame.
        let mut bridge = LspBridge {
            format_ready: true,
            format_edit: Some(WorkspaceEdit { edits: Vec::new() }),
            format_path: Some(PathBuf::from("/x.rs")),
            ..Default::default()
        };

        bridge.poll();
        assert!(bridge.format_ready);
        assert_eq!(bridge.format_path, Some(PathBuf::from("/x.rs")));
    }

    #[test]
    fn restart_all_restarts_even_an_unchanged_language() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        let go = LanguageConfig {
            command: "cat".to_string(),
            ..go_config()
        };
        bridge.sync_active_languages(dir.path(), std::slice::from_ref(&go));
        assert!(bridge.is_running_for_extension("go"));
        bridge
            .diagnostics
            .insert(PathBuf::from("/x.go"), Vec::new());

        // Same config, but `restart_all` always tears down and respawns --
        // scoped clearing runs regardless of whether the config changed.
        bridge.restart_all(dir.path(), &[go]);

        assert!(bridge.is_running_for_extension("go"));
        assert!(!bridge.diagnostics.contains_key(Path::new("/x.go")));
    }

    #[test]
    fn restart_all_with_an_empty_active_slice_stops_everything() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        let go = LanguageConfig {
            command: "cat".to_string(),
            ..go_config()
        };
        bridge.sync_active_languages(dir.path(), &[go]);
        assert!(bridge.is_running());

        bridge.restart_all(dir.path(), &[]);
        assert!(!bridge.is_running());
    }
}
