//! `ide-ui`: native GUI shell (`egui`/`eframe`). Owned by `rust-ui-dev`.
//! A library, not just the `ide` binary's private code, since the unified
//! `ide` binary (`crates/ui/src/main.rs`) needs a callable entry point
//! for its default (no-flag) GUI path, symmetric with how it calls
//! `ide_tui::main` for `--tui` (`docs/features/unified-binary.md`).

mod app;
mod cargo_panel;
mod claude_panel;
mod claude_terminal;
mod clone_panel;
mod command;
mod debug_panel;
mod editor;
mod file_structure;
mod files_search;
mod find_bar;
mod git_panel;
mod keymap;
mod lsp_bridge;
mod nav_history;
mod search_in_path_panel;
mod search_panel;
mod theme;
mod tree_scan;

/// `initial_project`, when given, opens that path directly (skipping the
/// open-projects-registry restore logic entirely -- `docs/features/
/// git-worktrees.md` §2.2.3) -- used both for the ordinary CLI-argument
/// case and by `IdeApp::open_in_new_window`, which re-execs this same
/// binary with the worktree/project path as its one argument.
pub fn run(initial_project: Option<std::path::PathBuf>) -> eframe::Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "ide",
        options,
        Box::new(|cc| Ok(Box::new(app::IdeApp::new(cc, initial_project)))),
    )
}
