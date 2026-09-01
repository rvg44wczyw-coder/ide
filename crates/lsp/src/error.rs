use std::io;

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("language server not found on PATH: {0}")]
    ServerNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("malformed LSP frame: {0}")]
    Protocol(String),
}
