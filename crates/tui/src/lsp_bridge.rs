//! Minimal `ide-lsp` bridge for `ide-tui` (`docs/features/
//! tui-goto-and-usages.md`, extended by `docs/features/tui-problems.md`
//! (`T9`) for diagnostics, by `docs/features/tui-semantic-highlighting.md`
//! (`T14`) for semantic tokens, by `docs/features/
//! tui-hover-and-inlay-hints.md` (`T12`) for hover/document-highlight/
//! inlay hints, and by `docs/features/tui-code-actions-and-rename.md`
//! (`T13`) for code actions and rename, and by `docs/features/
//! tui-go-to-file-and-symbol.md` (`T16`) for document/workspace symbols.
//! Scoped to exactly what these features need -- buffer lifecycle
//! notifications, Go to Declaration / Find Usages queries, diagnostics,
//! semantic tokens, hover, document highlight, inlay hints, code actions,
//! rename, and document/workspace symbols -- not the full surface
//! `crates/ui/src/lsp_bridge.rs` exposes (no formatting). Mirrors that
//! file's conventions at a fraction of its size: a per-query `finding_*`/
//! `*_ready` flag pair,
//! clear-at-send, replace-wholesale on response, `ServerExited` clears
//! all query state (except `hover`'s own text, and -- following `ide-ui`'s
//! own choice -- `workspace_edit`/`prepare_rename`/`rename`'s answer
//! fields, since their one-frame-true `*_ready` flags are already reset
//! by the time `ServerExited` runs; see that arm's own comment).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ide_lsp::{
    CodeAction, Diagnostic, GotoKind, InlayHint, Location, LspClient, LspEvent, LspRequest,
    Position, Range, SemanticToken, Symbol, WorkspaceEdit,
};

#[derive(Default)]
pub(crate) struct LspBridge {
    client: Option<LspClient>,
    pub(crate) server_error: Option<String>,
    pub(crate) goto: Vec<Location>,
    pub(crate) finding_goto: bool,
    /// True for exactly one `poll()` call after a `Goto` response lands --
    /// reset to `false` at the top of every `poll()`, mirroring `ide-ui`'s
    /// `LspBridge::goto_ready` (`crates/ui/src/lsp_bridge.rs`).
    pub(crate) goto_ready: bool,
    pub(crate) references: Vec<Location>,
    pub(crate) finding_references: bool,
    pub(crate) references_ready: bool,
    /// Per-file, replaced wholesale on each `LspEvent::Diagnostics` --
    /// matches LSP's `publishDiagnostics` semantics (each notification is
    /// a complete snapshot for that file, not a delta), same as `ide-ui`'s
    /// `LspBridge::diagnostics`.
    pub(crate) diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    /// Raw, `Position`-based semantic tokens per file -- this bridge has
    /// no buffer text of its own to convert against, so the byte-offset
    /// conversion happens downstream in `highlight::semantic_token_marks`
    /// (`docs/features/tui-semantic-highlighting.md` §2.1/§2.3), the same
    /// split `ide-ui`'s own `LspBridge::semantic_tokens` field doc
    /// documents. Replaced wholesale per path on each response.
    pub(crate) semantic_tokens: HashMap<PathBuf, Vec<SemanticToken>>,
    /// Most recent (non-superseded) `Hover` answer -- replaced wholesale on
    /// each `LspEvent::Hover`, cleared the instant a new `Hover` request is
    /// sent. Left untouched by `ServerExited`: unlike ambient state, the
    /// popup already showing this text to the user isn't made misleading
    /// by the server dying, the way stale highlights/hints would be
    /// (`docs/features/tui-hover-and-inlay-hints.md` §2.1, ported from
    /// `ide-ui`'s own `LspBridge::hover` field doc verbatim).
    pub(crate) hover: Option<String>,
    pub(crate) finding_hover: bool,
    /// Most recent (non-superseded) `DocumentHighlight` answer, for
    /// whatever position was last queried -- cleared at send-time.
    pub(crate) document_highlights: Vec<Range>,
    /// Inlay hints, keyed by file -- same per-file, stale-but-plausible-
    /// until-replaced shape `semantic_tokens` uses, for the same reason.
    pub(crate) inlay_hints: HashMap<PathBuf, Vec<InlayHint>>,
    /// Code actions for whatever `(path, position)` was last queried --
    /// replaced wholesale on each `LspEvent::CodeAction`, cleared at
    /// send-time (same convention `document_highlights` follows;
    /// `docs/features/tui-code-actions-and-rename.md` §2.2).
    pub(crate) code_actions: Vec<CodeAction>,
    /// The `(path, position)` the current `code_actions` answers.
    pub(crate) code_actions_target: Option<(PathBuf, Position)>,
    /// The outcome of the most recently applied `WorkspaceEdit`, set by
    /// `LspEvent::WorkspaceEditReady` (from `apply_code_action`).
    pub(crate) workspace_edit: Option<WorkspaceEdit>,
    pub(crate) workspace_edit_label: Option<String>,
    /// One-frame-true, reset unconditionally at the top of `poll()` --
    /// safe for the same reason `goto_ready`/`references_ready` already
    /// are: never set synchronously outside `poll()`'s own drain loop.
    pub(crate) workspace_edit_ready: bool,
    /// The `(path, position)` the current `prepare_renameable` answers.
    pub(crate) prepare_rename_target: Option<(PathBuf, Position)>,
    pub(crate) prepare_renameable: Option<bool>,
    /// Same reset-at-top-of-`poll()` safety as `workspace_edit_ready`.
    pub(crate) prepare_rename_ready: bool,
    pub(crate) rename_edit: Option<WorkspaceEdit>,
    pub(crate) rename_new_name: Option<String>,
    /// Same reset-at-top-of-`poll()` safety as `workspace_edit_ready`.
    pub(crate) rename_ready: bool,
    /// The active tab's own outline, for Go to Symbol's empty-query
    /// branch (`docs/features/tui-go-to-file-and-symbol.md` §2.2) --
    /// replaced wholesale on each `LspEvent::DocumentSymbol`, same
    /// stale-but-plausible-until-replaced convention as `semantic_tokens`/
    /// `inlay_hints`.
    pub(crate) document_symbols: Vec<Symbol>,
    /// The path the current `document_symbols` answers.
    pub(crate) document_symbols_path: Option<PathBuf>,
    /// `true` for exactly one `poll()` call after a `DocumentSymbol`
    /// response lands -- reset to `false` at the top of every `poll()`,
    /// mirroring `goto_ready`. The Symbols view doesn't need this (it just
    /// live-renders `document_symbols`), but `App`'s interface-redirect
    /// check (`docs/features/goto-declaration-interface-redirect.md`
    /// §2.3, ported from `ide-ui`'s own `document_symbols_ready`) does:
    /// without a fresh-this-frame signal, an entry left over from an
    /// unrelated earlier query for the exact file this one also targets
    /// would be misread as this query's own answer.
    pub(crate) document_symbols_ready: bool,
    /// Go to Symbol's non-empty-query branch -- replaced wholesale on each
    /// `LspEvent::WorkspaceSymbol`.
    pub(crate) workspace_symbols: Vec<Symbol>,
}

impl LspBridge {
    pub(crate) fn is_running(&self) -> bool {
        self.client.is_some()
    }

    /// Drops any previous client and clears all query state, then spawns
    /// `command` as the project's language server. A spawn failure (e.g. a
    /// missing binary) is recorded in `server_error`, not returned --
    /// there is nothing for a caller to retry differently, matching
    /// `ide-ui`'s `LspBridge::start_with_command`.
    pub(crate) fn start_with_command(
        &mut self,
        project_root: &Path,
        command: &str,
        args: &[String],
    ) {
        self.client = None;
        self.server_error = None;
        self.goto.clear();
        self.finding_goto = false;
        self.goto_ready = false;
        self.references.clear();
        self.finding_references = false;
        self.references_ready = false;
        self.diagnostics.clear();
        self.semantic_tokens.clear();
        self.hover = None;
        self.finding_hover = false;
        self.document_highlights.clear();
        self.inlay_hints.clear();
        self.code_actions.clear();
        self.code_actions_target = None;
        self.workspace_edit = None;
        self.workspace_edit_label = None;
        self.workspace_edit_ready = false;
        self.prepare_rename_target = None;
        self.prepare_renameable = None;
        self.prepare_rename_ready = false;
        self.rename_edit = None;
        self.rename_new_name = None;
        self.rename_ready = false;
        self.document_symbols.clear();
        self.document_symbols_path = None;
        self.document_symbols_ready = false;
        self.workspace_symbols.clear();
        match LspClient::start_with_command(project_root, command, args) {
            Ok(client) => self.client = Some(client),
            Err(e) => self.server_error = Some(e.to_string()),
        }
    }

    /// Forwards `request` to the running client, if any -- a no-op
    /// (including every notification, e.g. `DidOpen`/`DidChange`) when no
    /// client is running, so buffer-lifecycle call sites don't need to
    /// check `is_running()` first.
    pub(crate) fn send(&self, request: LspRequest) {
        if let Some(client) = &self.client {
            client.send(request);
        }
    }

    /// A total no-op (including `finding_goto` staying `false`) with no
    /// client running, so a query with nothing to ever answer it doesn't
    /// leave a caller waiting forever.
    pub(crate) fn go_to_definition(&mut self, path: &Path, position: Position) {
        if self.client.is_none() {
            return;
        }
        self.goto.clear();
        self.finding_goto = true;
        self.send(LspRequest::Goto {
            kind: GotoKind::Definition,
            path: path.to_path_buf(),
            position,
        });
    }

    /// Same shape, `GotoKind::Implementation` -- this crate had no
    /// standalone "Go to Implementation" gesture before the interface-
    /// redirect feature; this exists purely so that redirect can send it
    /// (`docs/features/goto-declaration-interface-redirect.md` §2.3).
    pub(crate) fn go_to_implementation(&mut self, path: &Path, position: Position) {
        if self.client.is_none() {
            return;
        }
        self.goto.clear();
        self.finding_goto = true;
        self.send(LspRequest::Goto {
            kind: GotoKind::Implementation,
            path: path.to_path_buf(),
            position,
        });
    }

    pub(crate) fn find_references(&mut self, path: &Path, position: Position) {
        if self.client.is_none() {
            return;
        }
        self.references.clear();
        self.finding_references = true;
        self.send(LspRequest::References {
            path: path.to_path_buf(),
            position,
        });
    }

    /// No-op with no client running. Doesn't clear the existing
    /// `semantic_tokens[path]` entry at send-time -- stale-but-plausible
    /// until the fresh response replaces it, the same convention every
    /// other query in this bridge follows.
    pub(crate) fn request_semantic_tokens(&mut self, path: &Path) {
        if self.client.is_none() {
            return;
        }
        self.send(LspRequest::SemanticTokensFull {
            path: path.to_path_buf(),
        });
    }

    /// No-op (leaving `hover`/`finding_hover` untouched) if no client is
    /// running. Otherwise clears `hover`, sets `finding_hover`, sends the
    /// request.
    pub(crate) fn request_hover(&mut self, path: &Path, position: Position) {
        if self.client.is_none() {
            return;
        }
        self.hover = None;
        self.finding_hover = true;
        self.send(LspRequest::Hover {
            path: path.to_path_buf(),
            position,
        });
    }

    /// Same shape, clears/refills `document_highlights`, sends
    /// `LspRequest::DocumentHighlight`.
    pub(crate) fn request_document_highlight(&mut self, path: &Path, position: Position) {
        if self.client.is_none() {
            return;
        }
        self.document_highlights.clear();
        self.send(LspRequest::DocumentHighlight {
            path: path.to_path_buf(),
            position,
        });
    }

    /// Clears `document_highlights` without sending anything -- the
    /// counterpart `request_document_highlight` has no use for: there's no
    /// query to send when `App::lsp_query_target` returns `None`, but the
    /// previous target's highlights must not keep rendering.
    pub(crate) fn clear_document_highlights(&mut self) {
        self.document_highlights.clear();
    }

    /// No-op with no client running. Doesn't clear `document_symbols` at
    /// send-time -- same stale-but-plausible convention as
    /// `request_semantic_tokens`/`request_inlay_hints`
    /// (`docs/features/tui-go-to-file-and-symbol.md` §2.2).
    pub(crate) fn request_document_symbols(&mut self, path: &Path) {
        if self.client.is_none() {
            return;
        }
        self.send(LspRequest::DocumentSymbol {
            path: path.to_path_buf(),
        });
    }

    /// Same shape, sends `LspRequest::WorkspaceSymbol { query }`.
    pub(crate) fn query_workspace_symbols(&mut self, query: &str) {
        if self.client.is_none() {
            return;
        }
        self.send(LspRequest::WorkspaceSymbol {
            query: query.to_string(),
        });
    }

    /// Same shape, sends `LspRequest::InlayHint { path, range }`. Does
    /// *not* clear `inlay_hints[path]` at send-time -- the existing hints
    /// stay visible, stale-but-plausible, until the fresh response
    /// replaces them, same reason `request_semantic_tokens` doesn't clear
    /// its own entry either.
    pub(crate) fn request_inlay_hints(&mut self, path: &Path, range: Range) {
        if self.client.is_none() {
            return;
        }
        self.send(LspRequest::InlayHint {
            path: path.to_path_buf(),
            range,
        });
    }

    /// No-op with no client running. Clears `code_actions` and records
    /// `code_actions_target` before sending, so a stale target is never
    /// mistaken for the answer to a not-yet-sent query.
    pub(crate) fn request_code_actions(&mut self, path: &Path, position: Position) {
        if self.client.is_none() {
            return;
        }
        self.code_actions.clear();
        self.code_actions_target = Some((path.to_path_buf(), position));
        self.send(LspRequest::CodeAction {
            path: path.to_path_buf(),
            position,
        });
    }

    /// Clears `code_actions`/`code_actions_target` without sending
    /// anything -- mirrors `clear_document_highlights` (there's no query
    /// to send once `App::lsp_query_target` returns `None`).
    pub(crate) fn clear_code_actions(&mut self) {
        self.code_actions.clear();
        self.code_actions_target = None;
    }

    /// Sends `LspRequest::ApplyCodeAction { index }` -- no-op if no client
    /// is running (nothing cached server-side to apply to in that case
    /// either).
    pub(crate) fn apply_code_action(&self, index: usize) {
        if self.client.is_none() {
            return;
        }
        self.send(LspRequest::ApplyCodeAction { index });
    }

    /// No-op with no client running. Records `(path, position)` as the
    /// target the eventual response answers, and clears any previous
    /// answer -- same "clear at send-time" convention `request_hover`
    /// already follows.
    pub(crate) fn request_prepare_rename(&mut self, path: &Path, position: Position) {
        if self.client.is_none() {
            return;
        }
        self.prepare_rename_target = Some((path.to_path_buf(), position));
        self.prepare_renameable = None;
        self.send(LspRequest::PrepareRename {
            path: path.to_path_buf(),
            position,
        });
    }

    /// Same no-op-if-no-client shape. Clears any previous `rename_edit`/
    /// `rename_new_name` at send-time.
    pub(crate) fn request_rename(&mut self, path: &Path, position: Position, new_name: String) {
        if self.client.is_none() {
            return;
        }
        self.rename_edit = None;
        self.rename_new_name = None;
        self.send(LspRequest::Rename {
            path: path.to_path_buf(),
            position,
            new_name,
        });
    }

    /// Drains every event the client has ready. Returns whether anything
    /// changed, matching `ide-ui`'s `poll()` return contract even though
    /// this bridge's callers don't currently use it.
    pub(crate) fn poll(&mut self) -> bool {
        self.goto_ready = false;
        self.references_ready = false;
        self.workspace_edit_ready = false;
        self.prepare_rename_ready = false;
        self.rename_ready = false;
        self.document_symbols_ready = false;
        let mut changed = false;
        while let Some(client) = &mut self.client {
            let Some(event) = client.try_recv() else {
                break;
            };
            changed = true;
            match event {
                LspEvent::Goto { locations } => {
                    self.goto = locations;
                    self.finding_goto = false;
                    self.goto_ready = true;
                }
                LspEvent::References { locations } => {
                    self.references = locations;
                    self.finding_references = false;
                    self.references_ready = true;
                }
                LspEvent::Diagnostics { path, diagnostics } => {
                    self.diagnostics.insert(path, diagnostics);
                }
                LspEvent::SemanticTokens { path, tokens } => {
                    self.semantic_tokens.insert(path, tokens);
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
                LspEvent::CodeAction { path: _, actions } => {
                    self.code_actions = actions;
                }
                LspEvent::DocumentSymbol { path, symbols } => {
                    self.document_symbols = symbols;
                    self.document_symbols_path = Some(path);
                    self.document_symbols_ready = true;
                }
                LspEvent::WorkspaceSymbol { symbols } => {
                    self.workspace_symbols = symbols;
                }
                LspEvent::WorkspaceEditReady { edit, label } => {
                    self.workspace_edit = edit;
                    self.workspace_edit_label = label;
                    self.workspace_edit_ready = true;
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
                    self.client = None;
                    self.server_error = Some(message);
                    self.goto.clear();
                    self.finding_goto = false;
                    self.references.clear();
                    self.finding_references = false;
                    self.diagnostics.clear();
                    self.semantic_tokens.clear();
                    self.finding_hover = false;
                    self.document_highlights.clear();
                    self.inlay_hints.clear();
                    self.code_actions.clear();
                    self.code_actions_target = None;
                    self.document_symbols.clear();
                    self.document_symbols_path = None;
                    self.document_symbols_ready = false;
                    self.workspace_symbols.clear();
                    // `workspace_edit*`/`prepare_rename*`/`rename_edit`/
                    // `rename_new_name` are deliberately left untouched --
                    // matches `ide-ui`'s own `LspBridge::poll`'s
                    // `ServerExited` arm exactly (`docs/features/
                    // tui-code-actions-and-rename.md` §2.2): their `*_ready`
                    // flags are already `false` by now (reset unconditionally
                    // at the top of this call), so there is nothing stale
                    // left to observe even without an explicit clear.
                }
                // `FormatReady` is the only remaining event kind with no
                // state in this crate to update -- this bridge doesn't send
                // `Format`/`FormatRange` requests, so it never arrives.
                _ => {}
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn position() -> Position {
        Position {
            line: 0,
            character: 0,
        }
    }

    #[test]
    fn a_fresh_bridge_is_not_running() {
        assert!(!LspBridge::default().is_running());
    }

    #[test]
    fn starting_a_nonexistent_binary_records_a_server_error_and_stays_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        bridge.start_with_command(dir.path(), "definitely-not-a-real-lsp-binary-xyz", &[]);
        assert!(!bridge.is_running());
        assert!(bridge.server_error.is_some());
    }

    #[test]
    fn starting_replaces_previous_state_even_on_a_second_failure() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        bridge.goto.push(Location {
            path: PathBuf::from("/stale.rs"),
            range: ide_lsp::Range {
                start: position(),
                end: position(),
            },
        });
        bridge.diagnostics.insert(
            PathBuf::from("/stale.rs"),
            vec![Diagnostic {
                range: ide_lsp::Range {
                    start: position(),
                    end: position(),
                },
                severity: ide_lsp::DiagnosticSeverity::Error,
                message: "stale".to_string(),
            }],
        );
        bridge.semantic_tokens.insert(
            PathBuf::from("/stale.rs"),
            vec![SemanticToken {
                position: position(),
                length: 3,
                kind: ide_lsp::SemanticTokenKind::Variable,
            }],
        );
        bridge.hover = Some("stale hover text".to_string());
        bridge.finding_hover = true;
        bridge.document_highlights.push(ide_lsp::Range {
            start: position(),
            end: position(),
        });
        bridge.inlay_hints.insert(
            PathBuf::from("/stale.rs"),
            vec![InlayHint {
                position: position(),
                label: "stale".to_string(),
                padding_left: false,
                padding_right: false,
            }],
        );
        bridge.code_actions.push(CodeAction {
            index: 0,
            title: "stale".to_string(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
        });
        bridge.code_actions_target = Some((PathBuf::from("/stale.rs"), position()));
        bridge.workspace_edit = Some(WorkspaceEdit { edits: vec![] });
        bridge.workspace_edit_label = Some("stale".to_string());
        bridge.workspace_edit_ready = true;
        bridge.prepare_rename_target = Some((PathBuf::from("/stale.rs"), position()));
        bridge.prepare_renameable = Some(true);
        bridge.prepare_rename_ready = true;
        bridge.rename_edit = Some(WorkspaceEdit { edits: vec![] });
        bridge.rename_new_name = Some("stale".to_string());
        bridge.rename_ready = true;
        bridge.start_with_command(dir_a.path(), "definitely-not-a-real-lsp-binary-xyz", &[]);
        bridge.start_with_command(dir_b.path(), "definitely-not-a-real-lsp-binary-xyz", &[]);
        assert!(bridge.goto.is_empty());
        assert!(bridge.diagnostics.is_empty());
        assert!(bridge.semantic_tokens.is_empty());
        assert_eq!(
            bridge.hover, None,
            "unlike ServerExited, a fresh start clears hover too"
        );
        assert!(!bridge.finding_hover);
        assert!(bridge.document_highlights.is_empty());
        assert!(bridge.inlay_hints.is_empty());
        assert!(bridge.code_actions.is_empty());
        assert!(bridge.code_actions_target.is_none());
        assert!(bridge.workspace_edit.is_none());
        assert!(bridge.workspace_edit_label.is_none());
        assert!(!bridge.workspace_edit_ready);
        assert!(bridge.prepare_rename_target.is_none());
        assert!(bridge.prepare_renameable.is_none());
        assert!(!bridge.prepare_rename_ready);
        assert!(bridge.rename_edit.is_none());
        assert!(bridge.rename_new_name.is_none());
        assert!(!bridge.rename_ready);
    }

    #[test]
    fn go_to_definition_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.go_to_definition(Path::new("/f.rs"), position());
        assert!(!bridge.finding_goto);
    }

    #[test]
    fn find_references_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.find_references(Path::new("/f.rs"), position());
        assert!(!bridge.finding_references);
    }

    #[test]
    fn request_semantic_tokens_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_semantic_tokens(Path::new("/f.rs"));
        assert!(bridge.semantic_tokens.is_empty());
    }

    #[test]
    fn request_document_symbols_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_document_symbols(Path::new("/f.rs"));
        assert!(bridge.document_symbols.is_empty());
        assert!(bridge.document_symbols_path.is_none());
    }

    #[test]
    fn query_workspace_symbols_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.query_workspace_symbols("needle");
        assert!(bridge.workspace_symbols.is_empty());
    }

    #[test]
    fn request_hover_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge {
            hover: Some("stale".to_string()),
            ..LspBridge::default()
        };
        bridge.request_hover(Path::new("/f.rs"), position());
        assert_eq!(bridge.hover, Some("stale".to_string()));
        assert!(!bridge.finding_hover);
    }

    #[test]
    fn request_document_highlight_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.document_highlights.push(ide_lsp::Range {
            start: position(),
            end: position(),
        });
        bridge.request_document_highlight(Path::new("/f.rs"), position());
        assert_eq!(bridge.document_highlights.len(), 1);
    }

    #[test]
    fn clear_document_highlights_clears_without_a_running_client() {
        let mut bridge = LspBridge::default();
        bridge.document_highlights.push(ide_lsp::Range {
            start: position(),
            end: position(),
        });
        bridge.clear_document_highlights();
        assert!(bridge.document_highlights.is_empty());
    }

    #[test]
    fn request_inlay_hints_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_inlay_hints(
            Path::new("/f.rs"),
            ide_lsp::Range {
                start: position(),
                end: position(),
            },
        );
        assert!(bridge.inlay_hints.is_empty());
    }

    #[test]
    fn request_code_actions_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_code_actions(Path::new("/f.rs"), position());
        assert!(bridge.code_actions.is_empty());
        assert!(bridge.code_actions_target.is_none());
    }

    #[test]
    fn clear_code_actions_clears_without_a_running_client() {
        let mut bridge = LspBridge::default();
        bridge.code_actions.push(CodeAction {
            index: 0,
            title: "stale".to_string(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
        });
        bridge.code_actions_target = Some((PathBuf::from("/f.rs"), position()));
        bridge.clear_code_actions();
        assert!(bridge.code_actions.is_empty());
        assert!(bridge.code_actions_target.is_none());
    }

    #[test]
    fn apply_code_action_with_no_client_running_is_a_noop() {
        let bridge = LspBridge::default();
        bridge.apply_code_action(0);
    }

    #[test]
    fn request_prepare_rename_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_prepare_rename(Path::new("/f.rs"), position());
        assert!(bridge.prepare_rename_target.is_none());
        assert!(bridge.prepare_renameable.is_none());
    }

    #[test]
    fn request_rename_with_no_client_running_is_a_noop() {
        let mut bridge = LspBridge::default();
        bridge.request_rename(Path::new("/f.rs"), position(), "new_name".to_string());
        assert!(bridge.rename_edit.is_none());
        assert!(bridge.rename_new_name.is_none());
    }

    /// `"true"` is a real, always-available binary that exits immediately
    /// -- spawns successfully (so `start_with_command` reports running),
    /// then its stdout closing drives `ide-lsp`'s background event loop to
    /// emit a real `LspEvent::ServerExited`, letting this test exercise
    /// `poll()`'s actual `ServerExited` match arm rather than duplicating
    /// its clearing logic inline.
    #[test]
    fn poll_on_server_exit_clears_ambient_overlay_state_but_leaves_hover_text_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        bridge.start_with_command(dir.path(), "true", &[]);
        assert!(bridge.is_running());

        // The one deliberate asymmetry this bridge has (`hover`'s own doc
        // comment): a popup already showing hover text isn't made
        // misleading by the server dying, unlike ambient highlights/hints
        // which would silently render as current if left in place.
        bridge.hover = Some("still showing this".to_string());
        bridge.finding_hover = true;
        bridge.document_highlights.push(ide_lsp::Range {
            start: position(),
            end: position(),
        });
        bridge.inlay_hints.insert(
            PathBuf::from("/f.rs"),
            vec![InlayHint {
                position: position(),
                label: "x".to_string(),
                padding_left: false,
                padding_right: false,
            }],
        );
        bridge.code_actions.push(CodeAction {
            index: 0,
            title: "still cached".to_string(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
        });
        bridge.code_actions_target = Some((PathBuf::from("/f.rs"), position()));
        bridge.document_symbols.push(Symbol {
            name: "stale".to_string(),
            kind: ide_lsp::SymbolKind::Function,
            container_name: None,
            location: Location {
                path: PathBuf::from("/f.rs"),
                range: ide_lsp::Range {
                    start: position(),
                    end: position(),
                },
            },
        });
        bridge.document_symbols_path = Some(PathBuf::from("/f.rs"));
        bridge.workspace_symbols.push(Symbol {
            name: "stale".to_string(),
            kind: ide_lsp::SymbolKind::Function,
            container_name: None,
            location: Location {
                path: PathBuf::from("/f.rs"),
                range: ide_lsp::Range {
                    start: position(),
                    end: position(),
                },
            },
        });
        // Same asymmetry as `hover`: these three answer fields are left
        // untouched by `ServerExited` (matching `ide-ui`'s own bridge), only
        // their one-frame-true `*_ready` flags matter, and those are already
        // reset unconditionally at the top of every `poll()` call.
        bridge.workspace_edit = Some(WorkspaceEdit { edits: vec![] });
        bridge.prepare_renameable = Some(true);
        bridge.rename_new_name = Some("still cached".to_string());

        let start = std::time::Instant::now();
        while bridge.is_running() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "ServerExited never arrived"
            );
            bridge.poll();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        assert_eq!(bridge.hover, Some("still showing this".to_string()));
        assert!(!bridge.finding_hover);
        assert!(bridge.document_highlights.is_empty());
        assert!(bridge.inlay_hints.is_empty());
        assert!(bridge.code_actions.is_empty());
        assert!(bridge.code_actions_target.is_none());
        assert!(bridge.document_symbols.is_empty());
        assert!(bridge.document_symbols_path.is_none());
        assert!(bridge.workspace_symbols.is_empty());
        assert!(bridge.workspace_edit.is_some());
        assert!(!bridge.workspace_edit_ready);
        assert_eq!(bridge.prepare_renameable, Some(true));
        assert!(!bridge.prepare_rename_ready);
        assert_eq!(bridge.rename_new_name, Some("still cached".to_string()));
        assert!(!bridge.rename_ready);
    }

    #[test]
    fn poll_with_no_client_running_returns_false() {
        let mut bridge = LspBridge::default();
        assert!(!bridge.poll());
    }

    /// `cat` is a real, always-spawnable process that just blocks reading
    /// stdin and never replies -- stands in for a language server to
    /// prove each request method's send-time field mutation past the
    /// `is_running()` gate, the same technique `crates/ui/src/
    /// lsp_bridge.rs`'s `request_methods_forward_to_a_running_client`
    /// test uses. It never answers, so this doesn't exercise `poll()`'s
    /// response-handling arms -- those need a real LSP speaker, which is
    /// `ide-lsp`'s own fixture-backed tests' job, not this bridge's.
    #[test]
    fn request_methods_set_finding_flags_on_a_running_client() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = LspBridge::default();
        bridge.start_with_command(dir.path(), "cat", &[]);
        assert!(bridge.is_running());

        bridge.go_to_definition(Path::new("/f.rs"), position());
        assert!(bridge.finding_goto);

        // `request_semantic_tokens` has no `finding_*` flag of its own
        // (unlike Goto/Find Usages, it's a background refetch, not a
        // user-visible "in progress" state) -- this only proves the
        // `is_running()` gate lets the send through without panicking;
        // `poll()`'s response handling is covered separately below.
        bridge.request_semantic_tokens(Path::new("/f.rs"));

        bridge.find_references(Path::new("/f.rs"), position());
        assert!(bridge.finding_references);

        bridge.request_hover(Path::new("/f.rs"), position());
        assert!(bridge.finding_hover);

        bridge.request_document_highlight(Path::new("/f.rs"), position());
        assert!(bridge.document_highlights.is_empty());

        bridge.request_inlay_hints(
            Path::new("/f.rs"),
            ide_lsp::Range {
                start: position(),
                end: position(),
            },
        );

        bridge.request_code_actions(Path::new("/f.rs"), position());
        assert_eq!(
            bridge.code_actions_target,
            Some((PathBuf::from("/f.rs"), position()))
        );

        bridge.apply_code_action(0);

        bridge.request_prepare_rename(Path::new("/f.rs"), position());
        assert_eq!(
            bridge.prepare_rename_target,
            Some((PathBuf::from("/f.rs"), position()))
        );

        bridge.request_rename(Path::new("/f.rs"), position(), "new_name".to_string());
    }
}
