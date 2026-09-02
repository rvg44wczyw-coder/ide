use std::io;

#[derive(Debug, thiserror::Error)]
pub enum DapError {
    #[error("debug adapter not found on PATH: {0}")]
    AdapterNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("malformed DAP frame: {0}")]
    Protocol(String),
}
