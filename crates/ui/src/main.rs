//! Unified `ide` binary entry point: GUI by default, `--tui [project-dir]`
//! for the terminal UI (`docs/features/unified-binary.md`). CLI-argument
//! parsing only -- the GUI path calls `ide_ui::run()`, the TUI path calls
//! `ide_tui::main`, both defined in their own crate's `lib.rs`.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--tui") => {
            let root = args.next().map(PathBuf::from);
            ide_tui::main(root)
        }
        Some(other) => {
            eprintln!("ide: unrecognized argument '{other}' (expected `--tui [project-dir]`, or no arguments for the GUI)");
            ExitCode::FAILURE
        }
        None => match ide_ui::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("ide: {err:?}");
                ExitCode::FAILURE
            }
        },
    }
}
