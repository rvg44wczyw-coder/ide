//! Standalone `ide-tui` binary -- CLI-argument parsing only. All real
//! logic lives in `lib.rs` (`ide_tui::main`), shared with the unified
//! `ide` binary's `--tui` flag (`crates/ui/src/main.rs`).

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let root = std::env::args().nth(1).map(PathBuf::from);
    ide_tui::main(root)
}
