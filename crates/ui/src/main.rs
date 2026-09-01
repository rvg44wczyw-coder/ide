//! Unified `ide` binary entry point: GUI by default, `--tui [project-dir]`
//! for the terminal UI (`docs/features/unified-binary.md`). CLI-argument
//! parsing only -- the GUI path calls `ide_ui::run(..)`, the TUI path calls
//! `ide_tui::main`, both defined in their own crate's `lib.rs`. The GUI
//! path also takes an optional positional project-dir argument
//! (`docs/features/git-worktrees.md` §2.2.3/§5) -- `IdeApp::open_in_new_window`
//! re-execs this binary with a worktree/project path as its one argument.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--tui") => {
            let root = args.next().map(PathBuf::from);
            ide_tui::main(root)
        }
        Some(arg) if !arg.starts_with('-') => run_gui(Some(PathBuf::from(arg))),
        Some(other) => {
            eprintln!("ide: unrecognized argument '{other}' (expected `--tui [project-dir]`, or an optional project-dir for the GUI)");
            ExitCode::FAILURE
        }
        None => run_gui(None),
    }
}

fn run_gui(initial_project: Option<PathBuf>) -> ExitCode {
    match ide_ui::run(initial_project) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ide: {err:?}");
            ExitCode::FAILURE
        }
    }
}
