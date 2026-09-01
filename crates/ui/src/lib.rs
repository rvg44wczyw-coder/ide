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

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "ide",
        options,
        Box::new(|cc| Ok(Box::new(app::IdeApp::new(cc)))),
    )
}
