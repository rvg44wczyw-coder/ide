use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    /// UTF-16 code units into the line — LSP's mandatory baseline
    /// encoding; v1 doesn't negotiate `positionEncoding` (see
    /// `docs/features/rust-language-support.md` §4).
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

/// One location a symbol is referenced from — a path already validated
/// against `project_root` (see `docs/features/find-usages.md` §4), same
/// discipline as `LspEvent::Diagnostics`' `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

/// Which of the three symmetric "go to X" queries a `Goto` request/event
/// pair is for — `textDocument/definition`, `textDocument/typeDefinition`,
/// or `textDocument/implementation` respectively (see
/// `docs/features/goto-definition.md` §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GotoKind {
    Definition,
    TypeDefinition,
    Implementation,
}

/// One inlay hint's already-flattened shape -- `ide-lsp`'s own simplified
/// wire type, mirroring how `Location`/`Diagnostic` already wrap the
/// richer `lsp_types` shapes down to exactly what `ide-ui` needs (see
/// `docs/features/inlay-hints-and-hover.md` §2.1, §3.2 on why `label` is a
/// flat `String` rather than `lsp_types::InlayHintLabel`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub position: Position,
    pub label: String,
    pub padding_left: bool,
    pub padding_right: bool,
}

/// `ide-lsp`'s own small mirror of the subset of LSP standard semantic
/// token types this client can render distinctly -- mirrors how
/// `InlayHint`/`Location` already wrap richer `lsp_types` shapes down to
/// exactly what `ide-ui` needs. Deliberately smaller than
/// `lsp_types::SemanticTokenType`'s full standard set (see
/// `docs/features/semantic-highlighting.md` §3.2's mapping table for every
/// type this omits and why).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticTokenKind {
    Type,
    Function,
    Macro,
    Keyword,
    String,
    Number,
    Comment,
    Operator,
    Variable,
}

/// One decoded, already-delta-resolved semantic token -- absolute
/// position, not the wire's relative-to-previous-token encoding (see
/// `docs/features/semantic-highlighting.md` §3.2: that decoding happens
/// once, here in `ide-lsp`, so nothing downstream ever touches
/// `delta_line`/`delta_start`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticToken {
    pub position: Position,
    /// UTF-16 code units, matching `Position.character`'s own unit --
    /// converted to a byte length only in `ide-ui`, the same point where
    /// every other `Position` this crate returns is converted.
    pub length: u32,
    pub kind: SemanticTokenKind,
}

/// One text replacement inside a `WorkspaceEdit`, LSP-position-addressed
/// -- not a buffer byte offset, since `ide-lsp` has no dependency on
/// `ide-core` to build an `ide_core::Transaction` here even if it wanted
/// to. `ide-ui` converts to buffer byte offsets itself, the same way it
/// already does for diagnostics/highlights/hints (see
/// `docs/features/code-actions.md` §2.1, §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// Every edit a `WorkspaceEdit` makes to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: PathBuf,
    pub text_edits: Vec<TextEdit>,
}

/// A path-validated, already-flattened set of edits across (possibly)
/// multiple files -- mirrors how `Location`/`InlayHint` already wrap
/// richer `lsp_types` shapes down to what `ide-ui` needs. Resource
/// operations (create/rename/delete) are not represented here at all --
/// see `docs/features/code-actions.md` §1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEdit {
    pub edits: Vec<FileEdit>,
}

/// One code action offered at a position -- `ide-lsp`'s own flattened
/// summary of an `lsp_types::CodeAction` (or a bare `lsp_types::Command`,
/// folded into the same shape with `disabled_reason: None`). The raw
/// server payload (including any opaque `data` a `codeAction/resolve`
/// call would need) never crosses into this type or into `ide-ui` -- see
/// `docs/features/code-actions.md` §2.1, §3.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeAction {
    /// This action's position in the most recent `CodeAction` response --
    /// the token `ApplyCodeAction` uses to say "this one."
    pub index: usize,
    pub title: String,
    /// Raw `CodeActionKind` string (`"quickfix"`, `"refactor.extract"`,
    /// ...), shown as a subtitle -- not parsed into an enum.
    pub kind: Option<String>,
    pub is_preferred: bool,
    /// `Some(reason)` when the server's `CodeAction.disabled.reason` is
    /// set -- `ide-ui` renders this entry greyed-out, not selectable.
    pub disabled_reason: Option<String>,
}

/// Flattened `lsp_types::SymbolKind` -- every variant the LSP spec
/// defines, used for both `documentSymbol` and `workspace/symbol` results
/// (same "own flattened enum, not the raw spec type" precedent as
/// `DiagnosticSeverity`; see `docs/features/search-everywhere.md` §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

/// One symbol, from either a `documentSymbol` or `workspace/symbol`
/// response -- the two share this one flattened shape (`ide-lsp`'s own
/// summary, same precedent as `Location`/`CodeAction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// The enclosing symbol's name, if any -- e.g. a method's containing
    /// type. For a flattened child of a hierarchical `documentSymbol`
    /// response, this is the parent's own `name`. `None` for a top-level
    /// symbol, or when the server didn't report one.
    pub container_name: Option<String>,
    /// Already validated against `project_root` -- same discipline as
    /// `Location`.
    pub location: Location,
}

/// True if any symbol in `symbols` (a file's `documentSymbol` result) has
/// kind `Interface` and its range contains `position` -- i.e. `position`
/// falls on, or inside, a trait/interface declaration. A `documentSymbol`
/// tree's parent ranges always span their children's ranges (LSP spec:
/// `DocumentSymbol.range` covers the whole declaration, `children` nest
/// inside it), so this single check catches both "resolved directly to
/// the interface/trait's own declaration" (a symbol of kind `Interface`
/// whose range contains `position` -- the position sits on the item
/// itself) and "resolved to a member declared only inside one" (an
/// ancestor of kind `Interface` whose range still contains `position`
/// even though the *innermost* containing symbol is e.g. `Method`) with
/// the same test, no name-based parent lookup needed
/// (`docs/features/goto-declaration-interface-redirect.md` §2.1/§3.3).
pub fn position_is_within_interface(symbols: &[Symbol], position: Position) -> bool {
    symbols
        .iter()
        .any(|s| s.kind == SymbolKind::Interface && range_contains(s.location.range, position))
}

/// Every symbol in `symbols` whose `location.range` contains `position`,
/// in the same relative order they appear in `symbols`. When `symbols` is
/// a whole file's `documentSymbol` result (as `flatten_document_symbols`
/// produces it -- always pushing a parent before recursing into its
/// children, so the list is already a pre-order, depth-first listing),
/// this order is automatically outermost-first: a `documentSymbol` tree's
/// ranges nest strictly, so two symbols that both contain the same point
/// cannot be unrelated siblings (siblings' ranges don't overlap) -- they
/// must be in an ancestor/descendant relationship, and the pre-order
/// listing already has ancestors before descendants. Used to build the
/// editor's breadcrumb trail (`docs/features/
/// file-structure-and-breadcrumbs.md` §2.2/§3.1) -- generalizes
/// `position_is_within_interface`'s range-containment test from "does an
/// `Interface` exist in the chain" to "give me the whole chain".
pub fn symbols_containing(symbols: &[Symbol], position: Position) -> Vec<&Symbol> {
    symbols
        .iter()
        .filter(|s| range_contains(s.location.range, position))
        .collect()
}

fn range_contains(range: Range, position: Position) -> bool {
    let start = (range.start.line, range.start.character);
    let end = (range.end.line, range.end.character);
    let pos = (position.line, position.character);
    start <= pos && pos <= end
}

/// What the UI tells the client to do; sent via `LspClient::send`.
/// `text` is the *entire* current document content — v1 uses
/// full-document sync, not incremental.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspRequest {
    DidOpen {
        path: PathBuf,
        text: String,
    },
    DidChange {
        path: PathBuf,
        text: String,
    },
    DidClose {
        path: PathBuf,
    },
    /// Query every reference to the symbol at `position` in `path`.
    /// Sending a new `References` request while one is already
    /// outstanding supersedes it — see `docs/features/find-usages.md`
    /// §3/§4.
    References {
        path: PathBuf,
        position: Position,
    },
    /// Query where the symbol at `position` in `path` is declared
    /// (`GotoKind::Definition`), typed (`TypeDefinition`), or implemented
    /// (`Implementation`). Shares one supersede-by-overwrite pending-id
    /// slot across all three kinds — sending any `Goto` request while
    /// another is outstanding (regardless of which `GotoKind`) supersedes
    /// it, the same discipline `References` already uses (see
    /// `docs/features/goto-definition.md` §3/§4).
    Goto {
        kind: GotoKind,
        path: PathBuf,
        position: Position,
    },
    /// Query hover text for the symbol at `position` in `path`. Own
    /// pending-id slot, independent of every other request kind's slot --
    /// sending a new `Hover` request never supersedes an in-flight
    /// `References`/`Goto`/`DocumentHighlight`/`InlayHint` query and vice
    /// versa (see `docs/features/inlay-hints-and-hover.md` §3.2, §4).
    Hover {
        path: PathBuf,
        position: Position,
    },
    /// Query every occurrence of the symbol at `position` in `path`,
    /// within that same file. Own pending-id slot.
    DocumentHighlight {
        path: PathBuf,
        position: Position,
    },
    /// Query inlay hints for `range` in `path`. Own pending-id slot.
    InlayHint {
        path: PathBuf,
        range: Range,
    },
    /// Query code actions available for the zero-width range at
    /// `position` in `path`. Own pending-id slot, independent of every
    /// other request kind's slot (see `docs/features/code-actions.md`
    /// §3.2).
    CodeAction {
        path: PathBuf,
        position: Position,
    },
    /// Apply the action at `index` from the most recent `CodeAction`
    /// response -- resolving it first via `codeAction/resolve` if it
    /// needs resolving and the server supports that; otherwise applies
    /// its `edit` directly. Either way, ends in exactly one
    /// `LspEvent::WorkspaceEditReady` (see `docs/features/code-actions.md`
    /// §3.3).
    ApplyCodeAction {
        index: usize,
    },
    /// Query every symbol in `path`'s current document. Own pending-id
    /// slot, independent of every other request kind's slot (see
    /// `docs/features/search-everywhere.md` §2.2, §2.3).
    DocumentSymbol {
        path: PathBuf,
    },
    /// Query every symbol in the whole project whose name matches `query`
    /// server-side (the server does its own fuzzy/substring matching --
    /// this is not `ide_core::fuzzy_score`, which only ever runs
    /// client-side over *files*, never over LSP symbol results). Own
    /// pending-id slot. An empty `query` is sent as-is -- servers commonly
    /// treat it as "list everything" or "list nothing"; `ide-ui` never
    /// relies on either behavior.
    WorkspaceSymbol {
        query: String,
    },
    /// Query a whole-document formatting edit for `path`. Own pending-id
    /// slot, shared with `FormatRange` -- sending either while the other
    /// is outstanding supersedes it, since both answer into the same
    /// `FormatReady` channel and only one "format this file" request is
    /// ever meaningfully in flight from a single caller. No-op over the
    /// wire (never sent) unless the server declared
    /// `documentFormattingProvider` in its `initialize` response -- `ide-
    /// ui` never has to check this itself before calling. Always answered
    /// by exactly one `LspEvent::FormatReady` (see
    /// `docs/features/formatting.md` §2.1, §3.2, §3.3).
    Format {
        path: PathBuf,
        tab_size: u32,
        insert_spaces: bool,
    },
    /// Same, but for `range` only -- requires
    /// `documentRangeFormattingProvider`. Shares `Format`'s pending-id
    /// slot (see `docs/features/formatting.md` §2.1).
    FormatRange {
        path: PathBuf,
        range: Range,
        tab_size: u32,
        insert_spaces: bool,
    },
    /// "Can the symbol at `position` be renamed?" Own pending-id slot
    /// (`pending_prepare_rename`), independent of every other request
    /// kind's slot and of `Rename`'s own slot below -- a `PrepareRename` in
    /// flight must not be confused with or superseded by an unrelated
    /// ambient query, nor by the `Rename` request the same popup will send
    /// moments later (see `docs/features/rename-refactoring.md` §2.1).
    PrepareRename {
        path: PathBuf,
        position: Position,
    },
    /// `new_name` is the user's already-finalized choice -- there is no
    /// per-keystroke traffic. Own pending-id slot (`pending_rename`).
    Rename {
        path: PathBuf,
        position: Position,
        new_name: String,
    },
    /// Query semantic tokens for the whole of `path`. Own pending-id slot
    /// (`pending_semantic_tokens`), independent of every other request
    /// kind's slot -- same reasoning `InlayHint`'s slot already documents
    /// (see `docs/features/semantic-highlighting.md` §2.2, §4).
    SemanticTokensFull {
        path: PathBuf,
    },
    /// "Organize imports for `path`, and apply the result immediately if
    /// there is one." A `textDocument/codeAction` request scoped to
    /// `context.only: ["source.organizeImports"]` over the whole
    /// document, resolved via `codeAction/resolve` first if the server
    /// marked the first entry unresolved and advertises
    /// `resolveProvider` (same resolve-or-not branch `ApplyCodeAction`
    /// already has), ending in exactly one `LspEvent::WorkspaceEditReady`
    /// -- never a menu, never populates the ambient `last_code_actions`
    /// cache `CodeAction`'s own response fills. Own pending-id slot
    /// (`pending_organize_imports_id`, plus its own follow-up
    /// `pending_organize_imports_resolve_id`), independent of
    /// `pending_code_action_id`/`pending_resolve_id` -- an ambient
    /// `⌥↩`/gutter-lightbulb re-query firing while an Optimize Imports
    /// request is in flight (or vice versa) must not be confused with it
    /// (see `docs/features/code-generation.md` §2.1, §3.4).
    OrganizeImports {
        path: PathBuf,
    },
}

/// What the client tells the UI; received via `LspClient::try_recv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspEvent {
    /// Replaces the full diagnostic set for `path` — matches LSP's
    /// `publishDiagnostics` semantics: each notification is a complete
    /// snapshot for that file, not a delta against the previous one.
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
    /// The language server process exited (crash, normal exit, or a
    /// fatal protocol error). v1 does not auto-restart — the UI surfaces
    /// this and the user re-triggers `LspClient::start` via a manual
    /// action.
    ServerExited { message: String },
    /// The result of the most recently sent, not-yet-superseded
    /// `LspRequest::References` query — delivered exactly once per
    /// non-superseded request, even when empty (see
    /// `docs/features/find-usages.md` §3/§4).
    References { locations: Vec<Location> },
    /// The result of the most recently sent, not-yet-superseded `Goto`
    /// query of any kind — delivered exactly once per non-superseded
    /// request, even when empty. Does not repeat the `GotoKind` — the UI
    /// already knows what it asked for, and only one `Goto` query is ever
    /// meaningfully in flight at a time (see
    /// `docs/features/goto-definition.md` §3/§4).
    Goto { locations: Vec<Location> },
    /// The result of the most recently sent, not-yet-superseded `Hover`
    /// query, even when empty. `None` for a `null` result, a JSON-RPC
    /// error, or contents this client can't flatten to text -- never
    /// distinguished from each other, same "a definite empty answer beats
    /// a permanently-waiting UI" permissiveness `References`/`Goto`
    /// already establish (see `docs/features/inlay-hints-and-hover.md`
    /// §3.2, §3.3).
    Hover { contents: Option<String> },
    /// The result of the most recently sent, not-yet-superseded
    /// `DocumentHighlight` query, even when empty.
    DocumentHighlight { ranges: Vec<Range> },
    /// The result of the most recently sent, not-yet-superseded
    /// `InlayHint` query, even when empty. Carries `path` (unlike every
    /// other event above) because `ide-ui` keeps inlay hints in a
    /// per-file map, not a single "answer to the last question" slot (see
    /// `docs/features/inlay-hints-and-hover.md` §2.2, §4).
    InlayHint {
        path: PathBuf,
        hints: Vec<InlayHint>,
    },
    /// The result of the most recently sent, not-yet-superseded
    /// `CodeAction` query, even when empty. Carries `path` for the same
    /// reason `InlayHint`'s event does (see `docs/features/code-actions.md`
    /// §2.1).
    CodeAction {
        path: PathBuf,
        actions: Vec<CodeAction>,
    },
    /// The outcome of applying a `WorkspaceEdit` -- from either
    /// `LspRequest::ApplyCodeAction` (`label` = that action's own
    /// `title`) or an unprompted server `workspace/applyEdit` request
    /// (`label` = that request's own `label` field, if present).
    /// `edit: None` means nothing to apply: resolve failed, the action
    /// had no edit and wasn't resolvable, the request named a stale/
    /// out-of-range index, or path validation rejected the edit entirely
    /// (see `docs/features/code-actions.md` §3.3, §3.5, §4).
    WorkspaceEditReady {
        edit: Option<WorkspaceEdit>,
        label: Option<String>,
    },
    /// The result of the most recently sent, not-yet-superseded
    /// `DocumentSymbol` query, even when empty. Carries `path`, same
    /// reason as `InlayHint`'s/`CodeAction`'s events.
    DocumentSymbol { path: PathBuf, symbols: Vec<Symbol> },
    /// The result of the most recently sent, not-yet-superseded
    /// `WorkspaceSymbol` query, even when empty.
    WorkspaceSymbol { symbols: Vec<Symbol> },
    /// The result of the most recently sent, not-yet-superseded `Format`
    /// or `FormatRange` query, even when empty/unsupported. Carries
    /// `path`, same reason `InlayHint`'s/`CodeAction`'s events do. `edit:
    /// None` covers every "nothing to apply" case alike -- unsupported
    /// capability, a `null`/empty-array result (file is already correctly
    /// formatted), and a JSON-RPC error -- deliberately not distinguished
    /// from each other, the same permissiveness `Hover`/`CodeAction`
    /// already establish. A non-empty result is always exactly one file
    /// (this file), so `edit` is a single-`FileEdit` `WorkspaceEdit`
    /// rather than `Vec<TextEdit>` directly (see
    /// `docs/features/formatting.md` §2.1, §3.1, §3.3).
    FormatReady {
        path: PathBuf,
        edit: Option<WorkspaceEdit>,
    },
    /// Answers `PrepareRename`. `renameable: true` covers three cases
    /// alike: the server explicitly said yes (any non-null, non-error
    /// response shape), the server doesn't support `prepareRename` at all
    /// (permissive default -- this is only ever a fast, optional
    /// early-reject; the real gate is `RenameReady` below), or the
    /// request's path failed validation (deliberately *not* treated as a
    /// negative signal here, since a stale/invalid path says nothing about
    /// whether the *position* is renameable -- `false` only for an
    /// explicit "not renameable" answer from a server that does support
    /// the capability: a `null` result, or a JSON-RPC error) (see
    /// `docs/features/rename-refactoring.md` §2.1).
    PrepareRenameReady { path: PathBuf, renameable: bool },
    /// Answers `Rename`. `edit: None` covers: unsupported capability
    /// (`renameProvider` absent -- no wire traffic), a path-validation
    /// failure on the response, or the server returned `null`/an error --
    /// permissively folded into one outcome, the same shape `FormatReady`
    /// already establishes. `new_name` is echoed back from the request so
    /// `ide-ui` can build a result message without caching it separately.
    RenameReady {
        path: PathBuf,
        new_name: String,
        edit: Option<WorkspaceEdit>,
    },
    /// The result of the most recently sent, not-yet-superseded
    /// `SemanticTokensFull` query, even when empty -- carries `path`, same
    /// reason `InlayHint`'s event does (`ide-ui` keeps this per-file, not
    /// as a single "answer to the last question" slot). See
    /// `docs/features/semantic-highlighting.md` §2.2, §3.2.
    SemanticTokens {
        path: PathBuf,
        tokens: Vec<SemanticToken>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn symbol(kind: SymbolKind, start: Position, end: Position) -> Symbol {
        Symbol {
            name: "x".to_string(),
            kind,
            container_name: None,
            location: Location {
                path: PathBuf::from("/f.rs"),
                range: Range { start, end },
            },
        }
    }

    #[test]
    fn true_when_position_is_on_the_interface_symbol_itself() {
        let symbols = vec![symbol(SymbolKind::Interface, pos(2, 0), pos(2, 10))];
        assert!(position_is_within_interface(&symbols, pos(2, 5)));
    }

    #[test]
    fn true_when_position_is_inside_a_member_nested_in_an_interface() {
        // The trait spans lines 0-5; a method inside it spans lines 1-2,
        // itself kind `Method` not `Interface` -- the containing
        // `Interface` symbol's own (wider) range is what makes this true.
        let symbols = vec![
            symbol(SymbolKind::Interface, pos(0, 0), pos(5, 1)),
            symbol(SymbolKind::Method, pos(1, 4), pos(2, 20)),
        ];
        assert!(position_is_within_interface(&symbols, pos(1, 10)));
    }

    #[test]
    fn false_for_a_position_outside_every_interface_range() {
        let symbols = vec![symbol(SymbolKind::Interface, pos(0, 0), pos(5, 1))];
        assert!(!position_is_within_interface(&symbols, pos(9, 0)));
    }

    #[test]
    fn false_when_the_containing_symbol_is_not_an_interface() {
        let symbols = vec![symbol(SymbolKind::Struct, pos(0, 0), pos(5, 1))];
        assert!(!position_is_within_interface(&symbols, pos(1, 0)));
    }

    #[test]
    fn empty_symbols_is_false() {
        assert!(!position_is_within_interface(&[], pos(0, 0)));
    }

    #[test]
    fn range_boundaries_are_inclusive() {
        let symbols = vec![symbol(SymbolKind::Interface, pos(2, 0), pos(2, 10))];
        assert!(position_is_within_interface(&symbols, pos(2, 0)));
        assert!(position_is_within_interface(&symbols, pos(2, 10)));
        assert!(!position_is_within_interface(&symbols, pos(2, 11)));
        assert!(!position_is_within_interface(&symbols, pos(1, 9)));
    }

    fn named_symbol(name: &str, kind: SymbolKind, start: Position, end: Position) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            container_name: None,
            location: Location {
                path: PathBuf::from("/f.rs"),
                range: Range { start, end },
            },
        }
    }

    #[test]
    fn symbols_containing_returns_the_chain_outermost_first() {
        // Pre-order, depth-first, exactly as `flatten_document_symbols`
        // would produce it: Foo (0-10) contains bar (2-8) contains baz (4-6).
        let symbols = vec![
            named_symbol("Foo", SymbolKind::Class, pos(0, 0), pos(10, 0)),
            named_symbol("bar", SymbolKind::Method, pos(2, 0), pos(8, 0)),
            named_symbol("baz", SymbolKind::Variable, pos(4, 0), pos(6, 0)),
        ];
        let chain = symbols_containing(&symbols, pos(5, 0));
        let names: Vec<&str> = chain.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Foo", "bar", "baz"]);
    }

    #[test]
    fn symbols_containing_excludes_a_sibling_that_does_not_contain_position() {
        let symbols = vec![
            named_symbol("Foo", SymbolKind::Class, pos(0, 0), pos(10, 0)),
            named_symbol("bar", SymbolKind::Method, pos(1, 0), pos(3, 0)),
            named_symbol("qux", SymbolKind::Method, pos(5, 0), pos(7, 0)),
        ];
        let chain = symbols_containing(&symbols, pos(6, 0));
        let names: Vec<&str> = chain.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Foo", "qux"]);
    }

    #[test]
    fn symbols_containing_position_outside_everything_is_empty() {
        let symbols = vec![named_symbol(
            "Foo",
            SymbolKind::Class,
            pos(0, 0),
            pos(10, 0),
        )];
        assert!(symbols_containing(&symbols, pos(20, 0)).is_empty());
    }

    #[test]
    fn symbols_containing_empty_input_is_empty() {
        assert!(symbols_containing(&[], pos(0, 0)).is_empty());
    }
}
