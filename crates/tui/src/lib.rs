//! `ide-tui`'s terminal-owning entry point (`docs/features/
//! tui-shell-and-editor.md` §2.1/§3.1/§3.2). Owns the terminal: raw mode,
//! the alternate screen, and restoring both on every exit path -- normal
//! exit, a startup error, or a panic. A library, not just the `ide-tui`
//! binary's private code, since `docs/features/unified-binary.md` has the
//! `ide` binary (`crates/ui`) call `main()` here directly for its `--tui`
//! flag -- `crates/tui/src/main.rs` is now a thin CLI wrapper around this
//! crate, not the only caller of it.
//!
//! Also opts into the Kitty/CSI-u keyboard enhancement protocol
//! (`KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES`) when
//! `crossterm::terminal::supports_keyboard_enhancement` reports the
//! terminal supports it. Without this, a bare terminal computes
//! `Ctrl+<char>` by masking the char's low 5 bits, which collapses
//! distinct chords onto the same byte -- `Ctrl+1` is indistinguishable
//! from `Ctrl+Q`, and (more broadly) `Ctrl+Shift+<letter>` is
//! indistinguishable from plain `Ctrl+<letter>`, since Ctrl-masking
//! discards case entirely. Every `Ctrl+Shift+*` binding this crate
//! registers (`commands.rs`'s `Redo`/`NextTab`/`PreviousTab`, `app.rs`'s
//! inline `Ctrl+Shift+A`/`Ctrl+Shift+G`/`Ctrl+Shift+R` checks) was
//! silently unreachable on an unenhanced terminal before this. On a
//! terminal that doesn't support the query (`supports_keyboard_enhancement`
//! returns `Ok(false)` or errors), this is skipped entirely and behaviour
//! is unchanged from before -- no regression, just no fix for that
//! terminal either. `PopKeyboardEnhancementFlags` in `restore_terminal`
//! is unconditional, matching that function's existing best-effort
//! contract: popping when nothing was pushed is a defined no-op per the
//! protocol's own spec, and an unrecognized escape sequence on a
//! non-supporting terminal is simply ignored, the same as any other
//! sequence this crate already sends such a terminal.

mod app;
mod cargo_panel;
mod claude_panel;
mod claude_terminal;
mod commands;
mod debug_config;
mod debug_panel;
mod docker_panel;
mod editor;
mod files_search;
mod find;
mod folding;
mod git_panel;
mod highlight;
mod k8s_panel;
mod keymap;
mod lsp_bridge;
mod project_state;
mod scratch;
mod search_panel;
mod state;
mod subprocess;
mod todo_panel;
mod tree;
mod ui;

use std::io::Stdout;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, LoopSignal};

/// Runs the TUI until the user exits, then returns the process exit code.
/// The sole public entry point -- both `crates/tui/src/main.rs` (the
/// standalone `ide-tui` binary) and `crates/ui/src/main.rs` (the unified
/// `ide` binary's `--tui` flag) call this and nothing else.
///
/// `root`, if given, is an explicit project directory (a CLI argument) and
/// always wins. Otherwise the last successfully-opened project is reused
/// if it still exists on disk (`docs/features/tui-persist-last-project.md`),
/// falling back to the current working directory -- the same resolution
/// both callers used to duplicate individually before this phase
/// centralized it here.
pub fn main(root: Option<PathBuf>) -> ExitCode {
    let remembered = state::load().last_project;
    let resolved_root = resolve_root(root, remembered, current_dir());

    let app = match App::new(resolved_root.clone()) {
        Ok(app) => app,
        Err(err) => {
            eprintln!("ide-tui: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Only remembered on a successful open (§4.2 of the doc above) -- a
    // path that fails to open as a project must never overwrite a
    // previously-good remembered one.
    state::save(&state::PersistedState {
        last_project: Some(resolved_root),
    });

    std::panic::set_hook(Box::new(|info| {
        restore_terminal();
        eprintln!("{info}");
    }));

    if let Err(err) = setup_terminal() {
        eprintln!("ide-tui: failed to set up the terminal: {err}");
        return ExitCode::FAILURE;
    }

    let result = (|| -> std::io::Result<()> {
        let backend = CrosstermBackend::new(std::io::stdout());
        let mut terminal = Terminal::new(backend)?;
        run(&mut terminal, app)
    })();

    restore_terminal();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ide-tui: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Pure resolution logic, split out from [`main`] so it's testable without
/// a real filesystem/environment (`docs/features/
/// tui-persist-last-project.md` §6, test 5): an explicit path always wins;
/// otherwise the remembered path is used only if it still exists on disk;
/// otherwise `cwd`.
fn resolve_root(explicit: Option<PathBuf>, remembered: Option<PathBuf>, cwd: PathBuf) -> PathBuf {
    explicit
        .or_else(|| remembered.filter(|path| path.is_dir()))
        .unwrap_or(cwd)
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn setup_terminal() -> std::io::Result<()> {
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    std::io::stdout().execute(EnableMouseCapture)?;
    if supports_keyboard_enhancement().unwrap_or(false) {
        std::io::stdout().execute(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ))?;
    }
    Ok(())
}

/// Best-effort: called from both the normal exit path and the panic hook,
/// so it must never itself panic on a terminal already in a weird state.
/// Mirrors `setup_terminal`'s ordering in reverse: mouse capture, enabled
/// after entering the alternate screen there, is disabled before leaving
/// it here (`docs/features/tui-mouse-support.md` §2.1).
fn restore_terminal() {
    let _ = std::io::stdout().execute(PopKeyboardEnhancementFlags);
    let _ = std::io::stdout().execute(DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = std::io::stdout().execute(LeaveAlternateScreen);
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, mut app: App) -> std::io::Result<()> {
    // Populated by the previous iteration's `terminal.draw` call below;
    // read here (one-frame lag) so a mouse event can be hit-tested against
    // what was actually last drawn (`docs/features/tui-mouse-support.md`
    // §2.2).
    let mut hit_map = ui::HitMap::default();
    loop {
        // Queried every iteration (not just once at startup) so a resized
        // terminal is picked up before the next keystroke's scroll-follow
        // calculation, not just before the next frame renders.
        let term_size = terminal.size()?;
        app.set_editor_viewport_rows(term_size.height.saturating_sub(ui::EDITOR_CHROME_ROWS));
        // Same reasoning, for the Claude Terminal view's grid -- computed
        // from the same per-frame terminal size query via the single
        // sizing function `ui::render`'s own popup layout also uses, so
        // the two never drift apart (`docs/features/tui-claude-panel.md`
        // §3.3).
        let (claude_rows, claude_cols) =
            ui::claude_terminal_grid_size(term_size.width, term_size.height);
        app.sync_claude_terminal_size(claude_rows, claude_cols);
        // Polled every iteration, not just after a key event, so a Goto/
        // Find Usages response lands (and the popup/jump happens) even if
        // the user doesn't press another key while waiting for it.
        app.poll_lsp();
        // Fires a fresh DocumentHighlight query whenever the caret has
        // moved to a new target since the last frame -- must run once per
        // frame regardless of whether a key was pressed, the same reason
        // `poll_lsp`/`poll_cargo` do (`docs/features/
        // tui-hover-and-inlay-hints.md` §2.2).
        app.sync_document_highlights();
        // Same reasoning, for Show Intention Actions' ambient `CodeAction`
        // refetch (`docs/features/tui-code-actions-and-rename.md` §3.1).
        app.sync_code_actions();
        // Same reasoning, for the Git Panel's ambient working-tree-diff
        // refetch (`docs/features/tui-git-panel.md` §3.1).
        app.sync_git_working_tree_diff();
        // Same reasoning, for Go to File's live per-keystroke refresh and
        // Go to Symbol's outline/workspace-query refetch (`docs/features/
        // tui-go-to-file-and-symbol.md` §3.1/§3.2).
        app.sync_go_to_file();
        app.sync_go_to_symbol();
        // Polled unconditionally, not just while the Cargo panel is open,
        // so a build/test run keeps streaming into `output` in the
        // background even while the panel is closed (`docs/features/
        // tui-cargo-panel.md` §3/§4).
        app.poll_cargo();
        // Same reasoning, for a Find in Path search running in the
        // background while the panel is closed (`docs/features/
        // tui-find-in-path.md` §3.1).
        app.poll_search();
        // Same reasoning, for a TODO panel scan running in the background
        // while the panel is closed (`docs/features/tui-todo-panel.md`
        // §2.2).
        app.poll_todo();
        // Unlike the unconditional polls above, only while the respective
        // panel is open (`docs/features/tui-docker-and-kubernetes.md`
        // §2.4/§4) -- closing the panel mid-request just drops the
        // in-flight receiver, since a fresh fetch is cheap to re-request.
        app.poll_docker();
        app.poll_k8s();
        // Same reasoning, for external file-system changes (tree refresh,
        // a tab's file modified/deleted on disk) delivered by the
        // background file watcher (`docs/features/tui-file-watcher.md`
        // §2.2).
        app.poll_watcher();
        // Same reasoning, for the Claude chat panel's in-flight `claude
        // -p` request and every open Claude Terminal tab's PTY output
        // (`docs/features/tui-claude-panel.md` §4.2) -- unconditional so
        // a terminal tab's PTY channel never backs up while the panel is
        // closed (the exact DoS shape `ide-ui`'s own `hacker` pass found
        // and fixed for this same feature).
        app.poll_claude();
        // Same reasoning, for the debug session's DAP event stream
        // (`docs/features/tui-debugger.md` §2.3) -- unconditional so a
        // `Stopped`/`Terminated` event lands even while the Debug tool
        // window is closed.
        app.poll_debug();
        if crossterm::event::poll(Duration::from_millis(100))? {
            match crossterm::event::read()? {
                Event::Key(key) => {
                    if app.handle_key(key) == LoopSignal::Exit {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse, &hit_map),
                _ => {}
            }
        }
        terminal.draw(|frame| ui::render(frame, &app, &mut hit_map))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_root_prefers_an_explicit_path_over_everything_else() {
        let resolved = resolve_root(
            Some(PathBuf::from("/explicit")),
            Some(PathBuf::from(".")),
            PathBuf::from("/cwd"),
        );
        assert_eq!(resolved, PathBuf::from("/explicit"));
    }

    #[test]
    fn resolve_root_falls_back_to_a_remembered_path_that_still_exists() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_root(None, Some(dir.path().to_path_buf()), PathBuf::from("/cwd"));
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn resolve_root_skips_a_remembered_path_that_no_longer_exists() {
        let resolved = resolve_root(
            None,
            Some(PathBuf::from("/definitely/does/not/exist/ide-tui-test")),
            PathBuf::from("/cwd"),
        );
        assert_eq!(resolved, PathBuf::from("/cwd"));
    }

    #[test]
    fn resolve_root_falls_back_to_cwd_with_nothing_remembered() {
        let resolved = resolve_root(None, None, PathBuf::from("/cwd"));
        assert_eq!(resolved, PathBuf::from("/cwd"));
    }
}
