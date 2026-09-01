//! ide-lsp: an LSP client for `rust-analyzer` providing diagnostics and
//! find-usages. See `docs/features/rust-language-support.md` and
//! `docs/features/find-usages.md` for the full design — scope,
//! error-handling contract, and security constraints (path validation
//! against the project root, the incoming-message size cap, fatal
//! handling of malformed JSON-RPC frames).

mod client;
mod error;
mod path;
mod position;
mod protocol;
mod types;

pub use client::LspClient;
pub use error::LspError;
pub use position::{byte_offset_to_position, position_to_byte_offset};
pub use protocol::MAX_CONTENT_LENGTH;
pub use types::{
    position_is_within_interface, symbols_containing, CodeAction, Diagnostic, DiagnosticSeverity,
    FileEdit, GotoKind, InlayHint, Location, LspEvent, LspRequest, Position, Range, SemanticToken,
    SemanticTokenKind, Symbol, SymbolKind, TextEdit, WorkspaceEdit,
};
