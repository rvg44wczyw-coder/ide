//! A generic Debug Adapter Protocol (DAP) client: one client for every
//! language, the adapter is user configuration, never a hardcoded branch
//! in this crate (`docs/features/debugger.md`).

mod client;
mod error;
mod path;
mod protocol;
mod types;

pub use client::DapClient;
pub use error::DapError;
pub use types::{
    Capabilities, DapEvent, DapRequest, OutputCategory, SourceBreakpoint, StackFrame, StopReason,
    ThreadInfo, VerifiedBreakpoint,
};
