//! Pure rendering (`docs/features/tui-shell-and-editor.md` §2.6, extended
//! by `docs/features/tui-multi-buffer-tabs.md` §2.3 for the tab strip) --
//! reads `App`'s state only, mutates nothing. Not unit-tested per this
//! crate's own convention (`ratatui` widget-building calls, like `egui`'s
//! immediate-mode draw calls in `ide-ui`, aren't meaningfully testable
//! without a rendered terminal; the logic feeding them, in
//! `app.rs`/`tree.rs`/`editor.rs`/`highlight.rs`, is covered there
//! instead). `docs/features/tui-syntax-highlighting.md` §2.2's
//! `styled_line`/`style_for` are pure `TextBuffer`/`TokenKind` logic with
//! no rendering of their own -- they live in `highlight.rs`, not here, so
//! this file's line-coverage exemption stays about drawing code only,
//! not a mix of tested and untested lines in one file.

use std::ops::Range;

use ide_core::DiffLine;
use ide_lsp::DiagnosticSeverity;
use ratatui::layout::{Constraint, Direction as LayoutDirection, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::ai_panel::AiDisplayMessage;
use crate::app::{
    ActionFormField, App, AppScreen, BottomDockState, BottomDockTab, ChangesFocus, ClaudeView,
    DebugPanelFocus, FilterField, FinderRow, Focus, FormKind, GitPanelFocus, GitPanelState,
    GitPanelView, LeftDockState, LeftDockTab, SearchInPathField,
};
use crate::claude_panel::ClaudeMessage;
use crate::claude_terminal::{AnsiColor, Cell};
use crate::clone_panel::ClonePanelField;
use crate::commands::{commands, menu_groups, MenuEntry};
use crate::docker_panel::DockerTab;
use crate::editor::cursor_line_column;
use crate::folding::VisualLines;
use crate::git_panel::{assign_lanes, RemoteOpKind, WorktreeAddField};
use crate::highlight::{
    document_highlight_marks, inlay_hint_chips, semantic_token_marks, styled_line, LineOverlays,
};
use crate::k8s_panel::{K8sPicker, K8sTab};

/// Non-text rows around the editor's visible buffer content: the
/// permanent screen tab bar (1 row, `docs/features/
/// tui-screen-navigation.md` §2.3/§4, T44) plus the status
/// bar (`render`'s own vertical split, 1 row) plus the persistent
/// key-hint ribbon (1 row, `docs/features/tui-key-hint-ribbon.md` §2.4,
/// T48 -- the fourth and last row of `render`'s own outer split) plus
/// `render_editor`'s `Block`'s top/bottom borders (2 rows) plus the tab
/// strip (1 row) plus the breadcrumbs strip (1 row, `docs/features/
/// tui-file-structure-and-breadcrumbs.md` §3.4). This
/// crate has no scroll-follows-cursor logic inside this file (this file
/// mutates nothing, per its own doc comment above) -- `app.rs`'s
/// `handle_editor_key` needs to know how many text rows are actually
/// visible *before* a frame ever renders, so `main.rs` derives it from
/// this constant instead of a real `Layout` pass. If `render`/
/// `render_editor`'s layout ever changes shape, this constant has to
/// change with it -- there is no single source of truth to keep them in
/// sync automatically, so a `Layout` change here is also a reason to grep
/// for this constant's uses (`main.rs`) before merging (`docs/features/
/// tui-scroll-follows-cursor.md` §2.1). The breadcrumbs row (like the
/// screen tab bar) is *always* reserved, whether or not `App::
/// active_breadcrumbs()` is non-empty on any given frame -- a content-
/// conditional row would desync this precomputed count from
/// `render_editor`'s actual drawn layout on any frame where the caret
/// enters/leaves a symbol, since this constant is read before any
/// `Layout` pass runs (`tui-file-structure-and-breadcrumbs.md` §3.4
/// spells out why; the screen tab bar is unconditional by construction
/// so it never had this failure mode to begin with). The ribbon is the
/// same kind of always-reserved row (`tui-key-hint-ribbon.md` §3.1) --
/// unlike the `Bottom` dock tab, it is visible on every `AppScreen`, not
/// just `Editor`. The menu bar (`tui-menu-bar.md` §2.3, T54) is the same
/// kind of always-reserved row too, one above the screen tab bar.
pub const EDITOR_CHROME_ROWS: u16 = 8;

/// Right-margin guide column (`docs/features/right-margin-guide.md` §1) --
/// always this literal value in `ide-tui`, unlike `ide-ui` where it's
/// per-language configurable: this crate has no per-language settings
/// storage/UI to read an override from.
const RIGHT_MARGIN_COLUMN: u16 = 120;

/// Click/wheel hit-test targets from the most recently rendered frame
/// (`docs/features/tui-mouse-support.md` §2.2) -- rebuilt from scratch by
/// every [`render`] call, so a rect is `None`/absent whenever that panel
/// wasn't drawn this frame. `App::handle_mouse` reads whatever the
/// *previous* frame populated here (one-frame lag), the same latency this
/// crate's existing scroll-follow/resize handling already accepts.
#[derive(Default)]
pub struct HitMap {
    pub tree_area: Option<Rect>,
    pub editor_text_area: Option<Rect>,
    pub tab_strip: Vec<(Rect, usize)>,
    /// Screen tab bar click regions (`docs/features/
    /// tui-screen-navigation.md` §2.3, T44) -- mirrors `tab_strip`'s own
    /// `Vec<(Rect, _)>` shape, keyed by `AppScreen` instead of a buffer
    /// index.
    pub screen_tabs: Vec<(Rect, AppScreen)>,
    /// Left dock tab strip click regions (mouse-support revision note 3,
    /// §3.2.4) -- Files/Todos, same shape as `screen_tabs`.
    pub left_dock_tabs: Vec<(Rect, LeftDockTab)>,
    /// Bottom dock tab strip click regions (mouse-support revision note 3,
    /// §3.2.4) -- Docker/Kubernetes/Cargo/
    /// Custom Actions/Problems/Git Log, same shape as `screen_tabs`.
    pub bottom_dock_tabs: Vec<(Rect, BottomDockTab)>,
    /// `Top`-slot custom action click regions, right-aligned on the screen
    /// tab bar (`docs/features/tui-custom-actions-edge-slots.md` §3.1,
    /// T47) -- mouse-click-only, no keyboard focus target (see that doc's
    /// §3.1 for why).
    pub top_action_hits: Vec<(Rect, crate::custom_actions::CustomAction)>,
    /// `Outline`-slot custom action click regions, right-aligned on the
    /// breadcrumbs row (`docs/features/tui-custom-actions-edge-slots.md`
    /// §3.1, T47) -- same mouse-click-only scope as `top_action_hits`.
    pub outline_action_hits: Vec<(Rect, crate::custom_actions::CustomAction)>,
    /// `Ribbon`-slot custom action click regions, on the persistent
    /// key-hint ribbon (`docs/features/tui-key-hint-ribbon.md` §2.4,
    /// T48) -- same mouse-click-only scope as `top_action_hits`/
    /// `outline_action_hits`.
    pub ribbon_action_hits: Vec<(Rect, crate::custom_actions::CustomAction)>,
    /// The ribbon's own `[+]` affordance click region (`docs/features/
    /// tui-key-hint-ribbon.md` §2.4/§3.2, T48) -- a single optional
    /// `Rect`, not a `Vec`, since there is always exactly one `[+]`.
    pub ribbon_add_hit: Option<Rect>,
    /// The left dock's active-tab body area (`rows[1]` in
    /// `render_left_dock`, below its one-row tab strip) -- populated
    /// whenever the left dock renders at all, regardless of which tab is
    /// active (`docs/features/tui-panel-focus-and-scroll.md` §2.1, T51).
    pub left_dock_body: Option<Rect>,
    /// Mirrors `left_dock_body` for the bottom dock (`rows[1]` in
    /// `render_bottom_dock`).
    pub bottom_dock_body: Option<Rect>,
    /// The Git panel's commit graph list, populated by `render_git_left_
    /// column` whenever the Log view renders -- also reused verbatim by
    /// the bottom-dock Git Log tab's own graph pane, since both share that
    /// one render function and only one of the two is ever on screen in a
    /// given frame (`docs/features/tui-panel-pane-scroll.md` §2.1, T53).
    pub git_graph_area: Option<Rect>,
    /// The Git panel's Conflicts list, populated only while `!app.git.
    /// conflicts.is_empty()` (`tui-panel-pane-scroll.md` §2.1, T53). Not
    /// reused by the dock's Git Log tab -- `GitLogDockState` has no
    /// `conflicts_selected` field to scroll, so wheel-scroll over this
    /// area in that context is deliberately left unhandled.
    pub git_conflicts_area: Option<Rect>,
    /// The Git panel's Diff pane, populated only when a diff (not conflict
    /// resolution) is showing -- reused by the dock's Git Log tab like
    /// `git_graph_area` (`tui-panel-pane-scroll.md` §2.1, T53).
    pub git_diff_area: Option<Rect>,
    /// The Git panel's Changes-view Staged list (`tui-panel-pane-scroll.md`
    /// §2.1, T53).
    pub git_staged_area: Option<Rect>,
    /// The Git panel's Changes-view Unstaged list (`tui-panel-pane-scroll.
    /// md` §2.1, T53).
    pub git_unstaged_area: Option<Rect>,
    /// The bottom dock's "secondary" pane -- Docker/Kubernetes' logs
    /// column, Custom Actions' output row -- for whichever tab is active
    /// (`tui-panel-pane-scroll.md` §2.2, T53). One generic field, not one
    /// per tab, since only one bottom-dock tab is ever rendered at a time;
    /// dispatch reads `bottom_dock.as_ref().map(|d| d.tab)` to know which
    /// panel's scroll field to mutate.
    pub dock_secondary_area: Option<Rect>,
    /// Top menu bar label click regions (`docs/features/tui-menu-bar.md`
    /// §2.3, T54), keyed by index into `commands::menu_groups()` --
    /// mirrors `screen_tabs`'s own `Vec<(Rect, _)>` shape.
    pub menu_bar_labels: Vec<(Rect, usize)>,
    /// The open dropdown's row click regions, keyed by index into that
    /// group's own `entries` slice (`tui-menu-bar.md` §2.3, T54). Empty
    /// whenever no menu is open.
    pub menu_dropdown_items: Vec<(Rect, usize)>,
    /// The open flyout's row click regions, keyed by index into the
    /// selected `Submenu` entry's own id list (`tui-menu-bar.md` §2.3,
    /// T54). Empty whenever no flyout is open.
    pub menu_submenu_items: Vec<(Rect, usize)>,
}

/// Reads `App`'s state only, mutates nothing on `App` -- unchanged from
/// before mouse support; `hits` is a separate out-parameter (not a return
/// value, since `Terminal::draw`'s render closure's return value isn't
/// propagated to the caller), populated fresh every call.
pub fn render(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    *hits = HitMap::default();
    let size = frame.area();
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(size);
    let menu_bar_area = rows[0];
    let tab_bar_area = rows[1];
    let body = rows[2];
    let status_area = rows[3];
    let ribbon_area = rows[4];

    render_menu_bar(frame, app, menu_bar_area, hits);
    render_screen_tabs(frame, app, tab_bar_area, hits);

    match app.active_screen {
        AppScreen::Editor => {
            let left_width = app.left_dock.is_some().then_some(app.left_dock_width_pct);
            let columns = match left_width {
                Some(pct) => Layout::default()
                    .direction(LayoutDirection::Horizontal)
                    .constraints([
                        Constraint::Percentage(pct),
                        Constraint::Percentage(100 - pct),
                    ])
                    .split(body),
                None => Layout::default()
                    .constraints([Constraint::Percentage(0), Constraint::Percentage(100)])
                    .split(body),
            };
            if let Some(dock) = &app.left_dock {
                render_left_dock(frame, app, dock, columns[0], hits);
            }

            let right_column = columns[1];
            let bottom_height = app
                .bottom_dock
                .is_some()
                .then_some(app.bottom_dock_height_pct);
            let rows2 = match bottom_height {
                Some(pct) => Layout::default()
                    .direction(LayoutDirection::Vertical)
                    .constraints([
                        Constraint::Percentage(100 - pct),
                        Constraint::Percentage(pct),
                    ])
                    .split(right_column),
                None => Layout::default()
                    .constraints([Constraint::Percentage(100), Constraint::Percentage(0)])
                    .split(right_column),
            };
            render_editor(frame, app, rows2[0], hits);
            if let Some(dock) = &app.bottom_dock {
                render_bottom_dock(frame, app, dock, rows2[1], hits);
            }
        }
        AppScreen::Git => render_git_panel(frame, app, body, hits),
        AppScreen::Run => render_cargo_panel(frame, app, body),
        AppScreen::Keys => render_keys_screen(frame, app, body),
    }
    render_status(frame, app, status_area);
    render_key_hint_ribbon(frame, app, ribbon_area, hits);

    if let Some(open) = app.menu_bar.open {
        let dropdown_rect = render_menu_dropdown(frame, app, open, size, hits);
        if app.menu_bar.submenu_selected.is_some() {
            render_menu_submenu(frame, app, open, dropdown_rect, size, hits);
        }
    }

    if app.palette.is_some() {
        render_palette(frame, app, size);
    }
    if app.colon_command.is_some() {
        render_colon_command(frame, app, size);
    }
    if app.unified_finder.is_some() {
        render_unified_finder(frame, app, size);
    }
    if app.goto.is_some() {
        render_goto_popup(frame, app, size);
    }
    if app.notifications_open {
        render_notifications_panel(frame, app, size);
    }
    if app.hover_open {
        render_hover_popup(frame, app, size);
    }
    if app.pending_replace_in_path_preview.is_some() {
        render_replace_in_path_preview(frame, app, size);
    } else if app.search_open {
        render_search_panel(frame, app, size);
    }
    if app.code_actions.is_some() {
        render_code_actions_popup(frame, app, size);
    }
    if app.generate_menu.is_some() {
        render_generate_menu_popup(frame, app, size);
    }
    if app.rename_popup.is_some() {
        render_rename_popup(frame, app, size);
    }
    if app.pending_rename_preview.is_some() {
        render_rename_preview(frame, app, size);
    }
    if app.refactor_menu.is_some() {
        render_refactor_menu_popup(frame, app, size);
    }
    if app.pending_refactor_preview.is_some() {
        render_refactor_preview(frame, app, size);
    }
    if app.blame_popup.is_some() {
        render_blame_popup(frame, app, size);
    }
    if app.git_gutter_popup_line.is_some() {
        render_git_gutter_popup(frame, app, size);
    }
    if app.gutter_context_menu.is_some() {
        render_gutter_context_menu(frame, app, size);
    }
    if app.clone_panel_open {
        render_clone_panel(frame, app, size);
    }
    if app.go_to_file.is_some() {
        render_go_to_file_popup(frame, app, size);
    }
    if app.go_to_symbol.is_some() {
        render_go_to_symbol_popup(frame, app, size);
    }
    if app.file_structure.is_some() {
        render_file_structure_popup(frame, app, size);
    }
    if app.recent_files.is_some() {
        render_recent_files_popup(frame, app, size);
    }
    if app.bookmarks_popup.is_some() {
        render_bookmarks_popup(frame, app, size);
    }
    if app.keymap_popup.is_some() {
        render_keymap_popup(frame, app, size);
    }
    if app.theme_popup.is_some() {
        render_theme_popup(frame, app, size);
    }
    if app.manage_actions_popup.is_some() {
        render_manage_actions_popup(frame, app, size);
    }
    if app.new_scratch_file.is_some() {
        render_new_scratch_file_prompt(frame, app, size);
    }
    if app.scratch_files.is_some() {
        render_scratch_files_popup(frame, app, size);
    }
    if app.claude_panel_open {
        render_claude_panel(frame, app, size);
    }
    if app.new_claude_terminal.is_some() {
        render_new_claude_terminal_prompt(frame, app, size);
    }
    if app.debug.show_launch_popup {
        render_debug_launch_popup(frame, app, size);
    }
    if app.debug_adapter_config_popup.is_some() {
        render_debug_adapter_config_popup(frame, app, size);
    }
    if app.debug_panel_open {
        render_debug_panel(frame, app, size);
    }
}

/// The terminal-grid rows/cols implied by the current terminal
/// dimensions -- the single source of truth also used by `app.rs`'s
/// `sync_claude_terminal_size` (`docs/features/tui-claude-panel.md`
/// §3.3), so the stored grid size never drifts from what's actually
/// drawn. Mirrors `render_cargo_panel`'s own outer-popup sizing exactly
/// (same margin/minimums), then further accounts for this panel's own
/// chrome: the block's borders (2 rows, 2 cols) and the one-row tab strip
/// header above the grid.
pub(crate) fn claude_terminal_grid_size(term_width: u16, term_height: u16) -> (u16, u16) {
    let popup_width = term_width.saturating_sub(4).max(20);
    let popup_height = term_height.saturating_sub(4).max(3);
    let inner_width = popup_width.saturating_sub(2).max(1);
    let inner_height = popup_height.saturating_sub(2).max(1);
    let grid_rows = inner_height.saturating_sub(1).max(1);
    (grid_rows, inner_width)
}

/// Renders `items` as a scrollable list that keeps `selected` in view via
/// `ListState`, instead of `List`'s plain (stateless) rendering, which
/// always starts at row 0 and silently clips everything past the visible
/// height with no way to bring an off-screen selection back into view
/// (mouse-support revision note 3, §3.3 -- this bug was
/// present in every list-shaped popup in this file except
/// `render_palette`, which already used this exact `ListState` shape).
/// Every row's highlight style is already baked into its own `ListItem`
/// span (no `List::highlight_style`/`highlight_symbol` is set anywhere in
/// this file), so passing a selection here only affects scroll
/// positioning, never visual style -- safe to call even for a
/// placeholder-only single-row list with no real selection concept.
fn render_scrollable_list(
    frame: &mut Frame,
    items: Vec<ListItem>,
    block: Block,
    area: Rect,
    selected: usize,
) {
    let list = List::new(items).block(block);
    let mut state = ListState::default();
    state.select(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_tree(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let rows = app.tree_state.visible_rows(&app.tree);
    let selected_path = app.tree_state.selected_row(&rows).map(|r| r.path.clone());

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            let marker = if row.is_dir {
                if row.expanded {
                    "\u{25be} "
                } else {
                    "\u{25b8} "
                }
            } else {
                "  "
            };
            let name = row
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| row.path.display().to_string());
            let style = if selected_path.as_ref() == Some(&row.path) {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{indent}{marker}{name}"),
                style,
            )))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Project")
        .border_style(focus_style(app, Focus::LeftDock));
    hits.tree_area = Some(block.inner(area));
    render_scrollable_list(frame, items, block, area, app.tree_state.selected_index());
}

/// New in `docs/features/tui-tool-window-docking.md` (T33): renders a
/// one-row tab strip above `dock`'s content, then dispatches to whichever
/// existing per-tab render function matches `dock.tab` -- marks the active
/// tab with `[brackets]`, a convention distinct from both the editor tab
/// strip's reverse-video highlight (`render_tab_strip`) and
/// `render_debug_panel`'s `* `-prefix, chosen here because this strip
/// packs multiple short labels onto one line where a color-only cue would
/// be lost in a monochrome terminal.
fn render_left_dock(
    frame: &mut Frame,
    app: &App,
    dock: &LeftDockState,
    area: Rect,
    hits: &mut HitMap,
) {
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    render_dock_tab_strip(
        frame,
        rows[0],
        focus_style(app, Focus::LeftDock),
        &mut hits.left_dock_tabs,
        &[
            (LeftDockTab::Files, "Files"),
            (LeftDockTab::Todos, "Todos"),
            (LeftDockTab::Actions, "Actions"),
        ],
        dock.tab,
    );
    hits.left_dock_body = Some(rows[1]);

    match dock.tab {
        LeftDockTab::Files => render_tree(frame, app, rows[1], hits),
        LeftDockTab::Todos => render_todo_panel(frame, app, rows[1], dock.todos_selected),
        LeftDockTab::Actions => render_tree_actions_tab(frame, app, rows[1]),
    }
}

/// Shared by `render_left_dock`/`render_bottom_dock` (mouse-support
/// revision note 3, §3.2.4) -- renders a one-row `[active]`-
/// bracketed tab strip (same convention as before) and populates `hits`
/// with each label's click region, mirroring `render_screen_tabs`'
/// per-label `Rect` bookkeeping exactly.
fn render_dock_tab_strip<T: Copy + PartialEq>(
    frame: &mut Frame,
    area: Rect,
    style: Style,
    hits: &mut Vec<(Rect, T)>,
    tabs: &[(T, &'static str)],
    active: T,
) {
    let mut spans = Vec::new();
    let mut column = area.x;
    for (i, (tab, label)) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            column += 2;
        }
        let text = if *tab == active {
            format!("[{label}]")
        } else {
            (*label).to_string()
        };
        let width = Span::raw(text.as_str()).width() as u16;
        hits.push((
            Rect {
                x: column,
                y: area.y,
                width,
                height: 1,
            },
            *tab,
        ));
        column += width;
        spans.push(Span::raw(text));
    }
    frame.render_widget(Paragraph::new(Line::from(spans).style(style)), area);
}

/// Mirrors `render_left_dock` for the bottom dock's six tabs (`docs/
/// features/tui-tool-window-docking.md` §2.3, T33).
fn render_bottom_dock(
    frame: &mut Frame,
    app: &App,
    dock: &BottomDockState,
    area: Rect,
    hits: &mut HitMap,
) {
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    render_dock_tab_strip(
        frame,
        rows[0],
        focus_style(app, Focus::BottomDock),
        &mut hits.bottom_dock_tabs,
        &[
            (BottomDockTab::Docker, "Docker"),
            (BottomDockTab::Ai, "AI"),
            (BottomDockTab::Kubernetes, "Kubernetes"),
            (BottomDockTab::Cargo, "Cargo"),
            (BottomDockTab::CustomActions, "Custom Actions"),
            (BottomDockTab::Problems, "Problems"),
            (BottomDockTab::GitLog, "Git Log"),
        ],
        dock.tab,
    );
    hits.bottom_dock_body = Some(rows[1]);
    match dock.tab {
        BottomDockTab::Docker => render_docker_panel(frame, app, rows[1], hits),
        BottomDockTab::Ai => render_ai_panel(frame, app, rows[1]),
        BottomDockTab::Kubernetes => render_k8s_panel(frame, app, rows[1], hits),
        BottomDockTab::Cargo => render_cargo_panel(frame, app, rows[1]),
        BottomDockTab::CustomActions => render_custom_actions_panel(frame, app, rows[1], hits),
        BottomDockTab::Problems => {
            render_problems_panel(frame, app, rows[1], dock.problems_selected)
        }
        BottomDockTab::GitLog => {
            let state = GitPanelState {
                view: GitPanelView::Log,
                focus: app.git_log_dock.focus,
                graph_selected: app.git_log_dock.graph_selected,
                diff_scroll: app.git_log_dock.diff_scroll,
                ..GitPanelState::default()
            };
            render_git_log_view(frame, app, &state, rows[1], hits);
        }
    }
}

fn render_editor(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let title = app
        .active_buffer()
        .map(|b| b.path.display().to_string())
        .unwrap_or_else(|| "No file open".to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(focus_style(app, Focus::Editor));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // One row for the tab strip, one for breadcrumbs, the rest for the
    // buffer text (`docs/features/tui-multi-buffer-tabs.md` §2.3,
    // `tui-file-structure-and-breadcrumbs.md` §3.4) -- every cursor/
    // scroll computation below is relative to `text_area`, not `inner`,
    // since the strip/breadcrumbs now occupy `inner`'s first two rows.
    // The breadcrumbs row is unconditionally reserved even when empty --
    // see `EDITOR_CHROME_ROWS`'s own doc comment for why a content
    // -conditional row isn't safe here.
    let sections = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    let strip_area = sections[0];
    let breadcrumbs_area = sections[1];
    let text_area = sections[2];

    render_tab_strip(frame, app, strip_area, hits);
    render_breadcrumbs(frame, app, breadcrumbs_area, hits);
    hits.editor_text_area = Some(text_area);

    let Some(buf) = app.active_buffer() else {
        let paragraph = Paragraph::new("No file open -- select one from the tree");
        frame.render_widget(paragraph, text_area);
        return;
    };

    // Viewport-limited: only the lines actually visible in `text_area`
    // get a `styled_line` call, not the whole buffer, every frame
    // (`docs/features/tui-syntax-highlighting.md` §2.2/Revision notes) --
    // the slice below already *is* the visible window, so no `.scroll()`
    // call on the resulting `Paragraph` (that would double-skip).
    let text_buffer = buf.buffer.text_buffer();
    let total_lines = text_buffer.lines().line_count();
    // Rows, not raw buffer lines -- a collapsed fold's interior
    // contributes no row at all, so scrolling/painting/the caret
    // position below all operate in row space
    // (`docs/features/tui-code-folding.md` §3.4), fresh every frame like
    // every other per-frame overlay in this function.
    let fold_ranges = text_buffer.fold_ranges();
    let visual = VisualLines::build(total_lines, &fold_ranges, &buf.folded);
    let total_rows = visual.row_count();
    let visible_start = (buf.scroll as usize).min(total_rows);
    let visible_end = (visible_start + text_area.height as usize).min(total_rows);
    // Computed once per frame, not per visible line -- `styled_line`
    // slices these down to each line's overlapping entries internally
    // (`docs/features/tui-semantic-highlighting.md` §3.3/§4,
    // `tui-hover-and-inlay-hints.md` §2.3).
    let semantic_tokens = semantic_token_marks(buf.buffer.text(), app.active_semantic_tokens());
    let highlights = document_highlight_marks(buf.buffer.text(), &app.lsp.document_highlights);
    let inlay_hints = inlay_hint_chips(buf.buffer.text(), app.active_inlay_hints());
    // Cheap (bounded by `MAX_BRACKET_SCAN_BYTES`) and, unlike `ide-ui`'s
    // cached `EditorState::bracket_pair`, needs no separate invalidation
    // tracking here -- it just joins this same per-frame recomputation
    // group (`docs/features/tui-smart-editing.md` §2.4).
    let bracket_pair: Vec<Range<usize>> = text_buffer
        .matching_bracket(text_buffer.selections().primary().head)
        .map(|pair| vec![pair.open, pair.close])
        .unwrap_or_default();
    // Every non-empty selection washes its range, including the primary's
    // -- a bare caret (start()==head) contributes nothing
    // (`docs/features/tui-multiple-cursors.md` §2.3).
    let selections: Vec<Range<usize>> = text_buffer
        .selections()
        .all()
        .iter()
        .map(|s| s.range())
        .filter(|r| !r.is_empty())
        .collect();
    // `docs/features/tui-debugger.md` §2.4 -- `ide-tui` has no gutter to
    // paint a breakpoint marker into, so a breakpointed line washes its
    // whole background instead.
    let (breakpoints_verified, breakpoints_unverified) =
        app.breakpoint_line_ranges(&buf.path, text_buffer);
    let theme = app.theme.theme();
    let overlays = LineOverlays {
        semantic_tokens: &semantic_tokens,
        highlights: &highlights,
        inlay_hints: &inlay_hints,
        bracket_pair: &bracket_pair,
        selections: &selections,
        breakpoints_verified: &breakpoints_verified,
        breakpoints_unverified: &breakpoints_unverified,
    };
    // `docs/features/tui-blame.md` §2.4 -- computed once per frame, not
    // per visible line, same as every other overlay above.
    let blame_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let lines: Vec<Line> = (visible_start..visible_end)
        .map(|row| {
            let line = visual.buffer_line(row);
            let mut styled = styled_line(text_buffer, line, &overlays, buf.indent.width, theme);
            // The collapsed-fold placeholder (`tui-code-folding.md` §3.4)
            // -- appended here, not inside `styled_line`, so folding stays
            // a concern this file alone knows about. Checking
            // `fold_ranges` too, not just `buf.folded` membership, matters
            // for a stale entry (constraint 5: an edit removed the range
            // that used to start here) -- `VisualLines` already treats it
            // as inert and hides nothing for it, so the marker must not
            // render either, or the line would misleadingly look
            // collapsed while every line under it is already visible.
            if buf.folded.contains(&line) && fold_ranges.iter().any(|r| r.start_line == line) {
                styled.push_span(Span::styled(
                    " \u{22ef}",
                    Style::default().fg(theme.fold_marker_fg),
                ));
            }
            // Line-number lane (`docs/features/tui-gutter-line-numbers.md`
            // §2.9/§3.1, T50) -- prepended before the git-gutter lane so
            // the final left-to-right order (blame, git-gutter,
            // line-number, text) matches `editor_lane_width`'s summation
            // order and `ide-ui`'s own documented lane ordering
            // ("blame left of line numbers", `crates/ui/src/editor/
            // geometry.rs:78`).
            let line_number_lane_width = app.line_number_lane_width();
            if line_number_lane_width > 0 {
                let digits = line_number_lane_width as usize - 1;
                let number = format!("{:>digits$} ", line + 1, digits = digits);
                let mut spans = vec![Span::styled(number, Style::default().fg(theme.gutter_fg))];
                spans.extend(styled.spans);
                styled = Line::from(spans);
            }
            if app.git_gutter_lane_width() > 0 {
                let mark = app.git_gutter.iter().find(|m| m.line == line);
                let (glyph, color) = match mark.map(|m| m.kind) {
                    Some(crate::git_gutter::GutterMarkKind::Added) => ("+", theme.git_added),
                    Some(crate::git_gutter::GutterMarkKind::Modified) => ("~", theme.git_modified),
                    Some(crate::git_gutter::GutterMarkKind::Deleted) => ("-", theme.git_deleted),
                    None => (" ", theme.git_none),
                };
                let mut spans = vec![Span::styled(
                    format!("{glyph} "),
                    Style::default().fg(color),
                )];
                spans.extend(styled.spans);
                styled = Line::from(spans);
            }
            if let Some(annotations) = &buf.blame {
                let prefix = blame_lane_prefix(annotations, line, blame_now);
                let mut spans = vec![Span::styled(
                    prefix,
                    Style::default().fg(theme.blame_lane_fg),
                )];
                spans.extend(styled.spans);
                styled = Line::from(spans);
            }
            styled
        })
        .collect();
    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, text_area);

    // Right-margin guide (`docs/features/right-margin-guide.md` §1/§2.4):
    // always the fixed default column here -- this crate has no
    // per-language settings surface to read a `LanguageConfig` override
    // from. Tints the existing cell's background rather than replacing its
    // glyph, so whatever was already drawn there (text, or blank space on
    // an empty buffer) stays visible. Skipped entirely when the terminal
    // is too narrow to show column 120 -- there's no horizontal scroll in
    // this crate to bring it into view.
    if let Some(guide_x) = text_area.x.checked_add(RIGHT_MARGIN_COLUMN) {
        if guide_x < text_area.x + text_area.width {
            let buffer = frame.buffer_mut();
            for y in text_area.y..text_area.y + text_area.height {
                if let Some(cell) = buffer.cell_mut((guide_x, y)) {
                    cell.set_bg(theme.right_margin_guide_bg);
                }
            }
        }
    }

    if app.focus == Focus::Editor {
        let offset = buf.buffer.text_buffer().selections().primary().head;
        let (line, column) = cursor_line_column(buf.buffer.text_buffer(), offset);
        if let Some(screen_line) = visual.row_of(line).checked_sub(buf.scroll as usize) {
            if (screen_line as u16) < text_area.height {
                // `column` is a `char` count (`cursor_line_column`'s own
                // contract, unchanged -- editing/movement code elsewhere
                // relies on that). The screen column a line actually
                // renders at can be wider (a tab, or -- unlike
                // `IndentUnit::columns_of`, which only ever measures plain
                // whitespace -- a wide CJK character counting for 2
                // columns) -- re-derive it via the exact same
                // `expand_tabs` call `styled_line` renders this line with
                // above, so the caret lands on the character it's actually
                // next to rather than drifting from either one.
                let line_text = text_buffer.line_text(line).unwrap_or("");
                let byte_col = line_text
                    .char_indices()
                    .nth(column)
                    .map(|(i, _)| i)
                    .unwrap_or(line_text.len());
                let (_, screen_column) =
                    crate::highlight::expand_tabs(&line_text[..byte_col], 0, buf.indent.width);
                frame.set_cursor_position((
                    text_area.x + screen_column as u16 + app.editor_lane_width(),
                    text_area.y + screen_line as u16,
                ));
            }
        }
    }
}

/// `docs/features/tui-blame.md` §2.4 -- exactly `BLAME_LANE_WIDTH` chars
/// wide always (the labeled row and every blank row alike), so the
/// buffer's own text starts at a fixed column regardless of which rows
/// happen to carry a label.
fn blame_lane_prefix(
    annotations: &[crate::blame_gutter::BlameAnnotation],
    line: usize,
    now: i64,
) -> String {
    use crate::blame_gutter::{blame_annotation_at, truncate_display, BLAME_LANE_CHARS};
    let label = match blame_annotation_at(annotations, line) {
        Some(a) if a.line == line => truncate_display(
            &format!(
                "{} {}, {}",
                a.short_id,
                a.author,
                crate::blame_gutter::relative_time(a.timestamp, now)
            ),
            BLAME_LANE_CHARS,
        ),
        _ => String::new(),
    };
    format!("{label:<BLAME_LANE_CHARS$} ")
}

/// Appends `actions`' `[Name]` labels right-aligned within `area`, after
/// whatever's already in `spans` (which occupies `[area.x, column)`) --
/// shared by `render_screen_tabs`' `Top` slot and `render_breadcrumbs`'
/// `Outline` slot (`docs/features/tui-custom-actions-edge-slots.md` §3.1,
/// T47). A no-op (no padding spacer, no hit regions) when `actions` is
/// empty, so it never turns an otherwise-blank row non-blank.
fn append_right_aligned_actions(
    spans: &mut Vec<Span<'static>>,
    hit_regions: &mut Vec<(Rect, crate::custom_actions::CustomAction)>,
    actions: Vec<crate::custom_actions::CustomAction>,
    area: Rect,
    column: u16,
) {
    if actions.is_empty() {
        return;
    }
    let items: Vec<(crate::custom_actions::CustomAction, String, u16)> = actions
        .into_iter()
        .map(|a| {
            let label = format!("[{}]", a.name);
            let width = Span::raw(label.clone()).width() as u16;
            (a, label, width)
        })
        .collect();
    let total_width: u16 =
        items.iter().map(|(_, _, w)| *w).sum::<u16>() + 2 * items.len().saturating_sub(1) as u16;
    let start_x = (area.x + area.width)
        .saturating_sub(total_width)
        .max(column);
    if start_x > column {
        spans.push(Span::raw(" ".repeat((start_x - column) as usize)));
    }
    let mut x = start_x;
    for (i, (action, label, width)) in items.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            x += 2;
        }
        hit_regions.push((
            Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            action,
        ));
        x += width;
        spans.push(Span::raw(label));
    }
}

/// Permanent screen tab bar (`docs/features/tui-screen-navigation.md`
/// §2.3, T44) -- always rendered, the first row of every frame regardless
/// of `active_screen`, same "unconditional chrome row" precedent
/// `render_breadcrumbs`'s own reserved row already established (that one
/// renders blank when empty; this one always has all four labels).
fn render_screen_tabs(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let screens = [
        (AppScreen::Editor, "Editor"),
        (AppScreen::Git, "Git"),
        (AppScreen::Run, "Run"),
        (AppScreen::Keys, "Keys"),
    ];
    let mut spans = Vec::new();
    let mut column = area.x;
    for (i, (screen, label)) in screens.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            column += 2;
        }
        let style = if *screen == app.active_screen {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let width = Span::raw(*label).width() as u16;
        hits.screen_tabs.push((
            Rect {
                x: column,
                y: area.y,
                width,
                height: 1,
            },
            *screen,
        ));
        column += width;
        spans.push(Span::styled(*label, style));
    }

    let top_actions = app
        .custom_actions
        .actions_for_slot(crate::custom_actions::ActionSlot::Top);
    append_right_aligned_actions(
        &mut spans,
        &mut hits.top_action_hits,
        top_actions,
        area,
        column,
    );

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The always-visible top menu bar row, one above the screen tab bar
/// (`docs/features/tui-menu-bar.md` §2.3/§3.1, T54). The open menu (if
/// any) is highlighted the same `Modifier::REVERSED` way `render_screen_
/// tabs` highlights the active screen; each label's mnemonic character is
/// underlined via `Modifier::UNDERLINED` -- a UI navigation gesture, not a
/// `Command` keybinding (`tui-menu-bar.md` §2.1), so it's drawn here
/// rather than sourced from `keymap::label`.
fn render_menu_bar(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let mut spans: Vec<Span> = Vec::new();
    let mut column = area.x;
    for (i, group) in menu_groups().iter().enumerate() {
        let base_style = if app.menu_bar.open == Some(i) {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let width = group.title.chars().count() as u16 + 2;
        hits.menu_bar_labels.push((
            Rect {
                x: column,
                y: area.y,
                width,
                height: 1,
            },
            i,
        ));
        column += width;

        spans.push(Span::styled(" ", base_style));
        let mut mnemonic_drawn = false;
        for c in group.title.chars() {
            if !mnemonic_drawn && c.eq_ignore_ascii_case(&group.mnemonic) {
                spans.push(Span::styled(
                    c.to_string(),
                    base_style.add_modifier(Modifier::UNDERLINED),
                ));
                mnemonic_drawn = true;
            } else {
                spans.push(Span::styled(c.to_string(), base_style));
            }
        }
        spans.push(Span::styled(" ", base_style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One row's display label within an open dropdown/flyout (`tui-menu-bar
/// .md` §2.3, T54) -- an `Item` shows its command title plus its current
/// effective binding (mirrors `render_keymap_popup`'s own `cmd.title`/
/// `effective_binding` pairing exactly, so a rebind made in Keymap
/// Settings is reflected here too); a `Submenu` shows its own title plus a
/// flyout indicator, never a binding (it isn't a command, it has none).
fn menu_entry_label(app: &App, entry: &MenuEntry) -> String {
    match entry {
        MenuEntry::Item(id) => {
            let title = commands()
                .iter()
                .find(|command| command.id == *id)
                .map(|command| command.title)
                .unwrap_or(*id);
            match app.keymap.effective_binding(id) {
                Some(chord) => format!("{title}  {}", crate::keymap::label(chord)),
                None => title.to_string(),
            }
        }
        MenuEntry::Submenu(title, _) => format!("{title}  \u{25b8}"),
    }
}

/// The open top-level menu's dropdown (`tui-menu-bar.md` §2.3/§3.1, T54) --
/// anchored under its bar label, left-aligned, clamped to stay on screen.
/// Returns the drawn `Rect` so `render`'s caller can anchor a flyout off
/// its right edge without recomputing this geometry a second time.
fn render_menu_dropdown(
    frame: &mut Frame,
    app: &App,
    open: usize,
    area: Rect,
    hits: &mut HitMap,
) -> Rect {
    let group = &menu_groups()[open];
    let anchor_x = hits
        .menu_bar_labels
        .iter()
        .find(|(_, i)| *i == open)
        .map(|(rect, _)| rect.x)
        .unwrap_or(area.x);
    let labels: Vec<String> = group
        .entries
        .iter()
        .map(|entry| menu_entry_label(app, entry))
        .collect();
    let content_width = labels
        .iter()
        .map(|label| label.chars().count() as u16)
        .max()
        .unwrap_or(10);
    let width = (content_width + 4).clamp(16, area.width.saturating_sub(2).max(16));
    let height = (group.entries.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let x = anchor_x.min(area.width.saturating_sub(width));
    let popup = Rect {
        x,
        y: 1,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL).title(group.title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    for (i, label) in labels.into_iter().enumerate() {
        let row_y = inner.y + i as u16;
        if row_y >= inner.y + inner.height {
            break;
        }
        let row = Rect {
            x: inner.x,
            y: row_y,
            width: inner.width,
            height: 1,
        };
        hits.menu_dropdown_items.push((row, i));
        let style = if i == app.menu_bar.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(label, style))), row);
    }
    popup
}

/// The open flyout, one level deep off the dropdown's currently-selected
/// `Submenu` row (`tui-menu-bar.md` §2.3/§3.1, T54) -- anchored to the
/// right of `dropdown_rect` (the `Rect` `render_menu_dropdown` just
/// returned), clamped to stay on screen. A no-op if `menu_bar.selected`
/// doesn't currently point at a `Submenu` entry (defensive only --
/// `handle_menu_bar_key`/`handle_menu_bar_click` never set `submenu_
/// selected` to `Some` unless it does).
fn render_menu_submenu(
    frame: &mut Frame,
    app: &App,
    open: usize,
    dropdown_rect: Rect,
    area: Rect,
    hits: &mut HitMap,
) {
    let group = &menu_groups()[open];
    let Some(MenuEntry::Submenu(title, ids)) = group.entries.get(app.menu_bar.selected) else {
        return;
    };
    let Some(sub_selected) = app.menu_bar.submenu_selected else {
        return;
    };

    let labels: Vec<String> = ids
        .iter()
        .map(|id| menu_entry_label(app, &MenuEntry::Item(id)))
        .collect();
    let content_width = labels
        .iter()
        .map(|label| label.chars().count() as u16)
        .max()
        .unwrap_or(10);
    let width = (content_width + 4).clamp(16, area.width.saturating_sub(2).max(16));
    let height = (ids.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let x = (dropdown_rect.x + dropdown_rect.width).min(area.width.saturating_sub(width));
    // `tui-menu-bar.md` §2.3: anchors to the highlighted `Submenu` row's
    // own `y`, not the dropdown box's top border -- `hits.menu_dropdown_
    // items` was already populated by this frame's earlier `render_menu_
    // dropdown` call, keyed by entry index, so the row's real position is
    // just a lookup, not a recomputation. Falls back to `dropdown_rect.y`
    // defensively (should never miss: `app.menu_bar.selected` is always a
    // valid `entries` index while a flyout is open).
    let y = hits
        .menu_dropdown_items
        .iter()
        .find(|(_, i)| *i == app.menu_bar.selected)
        .map(|(rect, _)| rect.y)
        .unwrap_or(dropdown_rect.y);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL).title(*title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    for (i, label) in labels.into_iter().enumerate() {
        let row_y = inner.y + i as u16;
        if row_y >= inner.y + inner.height {
            break;
        }
        let row = Rect {
            x: inner.x,
            y: row_y,
            width: inner.width,
            height: 1,
        };
        hits.menu_submenu_items.push((row, i));
        let style = if i == sub_selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(label, style))), row);
    }
}

fn render_tab_strip(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let mut spans = Vec::new();
    let mut column = area.x;
    for (i, tab) in app.tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            column += 2;
        }
        let name = tab
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| tab.path.display().to_string());
        let dirty = if tab.buffer.is_dirty() { "*" } else { "" };
        let external = match tab.external_change {
            Some(crate::app::ExternalChange::Modified) => " [modified on disk]",
            Some(crate::app::ExternalChange::Deleted) => " [deleted on disk]",
            None => "",
        };
        let style = if Some(i) == app.active_tab {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let text = format!("{name}{dirty}{external}");
        let width = Span::raw(text.as_str()).width() as u16;
        hits.tab_strip.push((
            Rect {
                x: column,
                y: area.y,
                width,
                height: 1,
            },
            i,
        ));
        column += width;
        spans.push(Span::styled(text, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The always-reserved 1-row strip under the tab strip (`docs/features/
/// tui-file-structure-and-breadcrumbs.md` §3.2/§3.4) -- read-only, no
/// click-to-jump (§1.2's scope cut: the File Structure popup already
/// reaches every symbol a breadcrumb segment could, via the keyboard).
/// Renders a blank row (not a placeholder) when `active_breadcrumbs()` is
/// empty, matching `render_git_gutter`'s existing "nothing to show today"
/// convention.
fn render_breadcrumbs(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let crumbs = app.active_breadcrumbs();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut column = area.x;
    for (i, symbol) in crumbs.iter().enumerate() {
        if i > 0 {
            let sep = " \u{203a} ";
            column += Span::raw(sep).width() as u16;
            spans.push(Span::raw(sep));
        }
        let name = symbol.name.clone();
        column += Span::raw(name.as_str()).width() as u16;
        spans.push(Span::raw(name));
    }

    let outline_actions = app
        .custom_actions
        .actions_for_slot(crate::custom_actions::ActionSlot::Outline);
    append_right_aligned_actions(
        &mut spans,
        &mut hits.outline_action_hits,
        outline_actions,
        area,
        column,
    );

    if spans.is_empty() {
        return;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    // While the find bar is open it's the active modal context, so it
    // takes priority over `app.status()` -- the same priority the
    // palette's own overlay already gets over the status line
    // (`docs/features/tui-find.md` §2.4).
    let mut text = app
        .find
        .as_ref()
        .map(|f| f.status_text())
        .or_else(|| app.status().map(str::to_string))
        .unwrap_or_else(|| {
            app.active_buffer()
                .map(|b| {
                    let dirty = if b.buffer.is_dirty() { "*" } else { "" };
                    format!("{}{dirty}", b.path.display())
                })
                .unwrap_or_else(|| app.project_root().display().to_string())
        });
    // Unread-count badge (`docs/features/tui-goto-and-usages.md` §2.4) --
    // appended rather than replacing the line above, so it never hides
    // the find bar's own status text or an in-progress error.
    let unread = app.unread_notification_count();
    if unread > 0 {
        text.push_str(&format!("  [{unread} unread]"));
    }
    let problem_count = app.flattened_diagnostics().len();
    if problem_count > 0 {
        text.push_str(&format!("  [{problem_count} problems]"));
    }
    frame.render_widget(Paragraph::new(text), area);
}

/// Persistent bottom key-hint ribbon (`docs/features/
/// tui-key-hint-ribbon.md` §2.4/§3.2, T48) -- visible under every
/// `AppScreen`, unlike the `Bottom` dock tab which only exists inside
/// `Editor`. Left-aligned: `App::key_hint_rows`' fixed, curated hints.
/// Right-aligned: every `Ribbon`-slot custom action as a clickable
/// `[Name]` label (same shared right-alignment technique `T47`'s
/// `append_right_aligned_actions` established for `Top`/`Outline`,
/// generalized here to also place the non-`CustomAction` `[+]` marker at
/// the end of the same group), followed by the ribbon's own `[+]`
/// affordance.
fn render_key_hint_ribbon(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    use crate::custom_actions::ActionSlot;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut column = area.x;
    for (i, (title, binding)) in app.key_hint_rows().into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            column += 2;
        }
        let text = format!("{binding} {title}");
        column += Span::raw(text.as_str()).width() as u16;
        spans.push(Span::raw(text));
    }

    let ribbon_actions = app.custom_actions.actions_for_slot(ActionSlot::Ribbon);
    let plus_label = "[+]";
    let plus_width = Span::raw(plus_label).width() as u16;
    let mut right_items: Vec<(Option<crate::custom_actions::CustomAction>, String, u16)> =
        ribbon_actions
            .into_iter()
            .map(|a| {
                let label = format!("[{}]", a.name);
                let width = Span::raw(label.clone()).width() as u16;
                (Some(a), label, width)
            })
            .collect();
    right_items.push((None, plus_label.to_string(), plus_width));

    let total_width: u16 = right_items.iter().map(|(_, _, w)| *w).sum::<u16>()
        + 2 * right_items.len().saturating_sub(1) as u16;
    let start_x = (area.x + area.width)
        .saturating_sub(total_width)
        .max(column);
    if start_x > column {
        spans.push(Span::raw(" ".repeat((start_x - column) as usize)));
    }
    let mut x = start_x;
    for (i, (action, label, width)) in right_items.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            x += 2;
        }
        let rect = Rect {
            x,
            y: area.y,
            width,
            height: 1,
        };
        match action {
            Some(a) => hits.ribbon_action_hits.push((rect, a)),
            None => hits.ribbon_add_hit = Some(rect),
        }
        x += width;
        spans.push(Span::raw(label));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Fixed row budget for the popup body (excludes the top/bottom border),
/// independent of `palette.filtered.len()` -- with many matches the list
/// scrolls via `ListState` instead of growing the popup to fit them all.
const PALETTE_VISIBLE_ROWS: u16 = 12;

fn render_palette(frame: &mut Frame, app: &App, area: Rect) {
    let Some(palette) = app.palette.as_ref() else {
        return;
    };
    let width = area.width.clamp(20, 50);
    let height = (PALETTE_VISIBLE_ROWS + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = palette
        .filtered
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let style = if i == palette.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{}  ({})", cmd.title, cmd.id),
                style,
            )))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("Find Action: {}", palette.query));
    let list = List::new(items).block(block);
    let mut state = ListState::default();
    state.select(Some(palette.selected));
    frame.render_stateful_widget(list, popup, &mut state);
}

/// Smaller than [`PALETTE_VISIBLE_ROWS`] -- this is a quick single-command
/// line, not a way to browse the whole registry (`docs/features/
/// tui-colon-command.md` §2.4, T45).
const COLON_COMMAND_VISIBLE_ROWS: u16 = 6;

fn render_colon_command(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.colon_command.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(2);
    let height = (COLON_COMMAND_VISIBLE_ROWS + 2).clamp(3, area.height.saturating_sub(2).max(3));
    // Bottom-anchored, directly above the status bar row `render` reserves
    // as the last row of `area` -- unlike every other popup in this file,
    // which centers in `area`.
    let popup = Rect {
        x: area.x + 1,
        y: area.y + area.height.saturating_sub(1).saturating_sub(height),
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = state
        .filtered
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let style = if i == state.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{}  ({})", cmd.title, cmd.id),
                style,
            )))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(": {}", state.query));
    let list = List::new(items).block(block);
    let mut list_state = ListState::default();
    list_state.select(Some(state.selected));
    frame.render_stateful_widget(list, popup, &mut list_state);
}

/// `⇧⇧`'s popup (`docs/features/tui-unified-finder.md` §3.4). Same
/// near-fullscreen-minus-margin geometry `render_go_to_file_popup` uses
/// (not the small fixed-height box `render_palette`/`render_colon_command`
/// use) -- a merged three-source list needs the room a single-category one
/// doesn't. Reuses `render_scrollable_list` verbatim; no new list-
/// rendering logic.
fn render_unified_finder(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.unified_finder.as_ref() else {
        return;
    };
    let rows = app.unified_finder_rows();
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if state.query.trim().is_empty() {
        vec![ListItem::new(Line::from(
            "Type to search files, symbols, and actions.",
        ))]
    } else if rows.is_empty() {
        vec![ListItem::new(Line::from("No results."))]
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, row)| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let text = match row {
                    FinderRow::File(m) => m.relative.clone(),
                    FinderRow::Symbol(s) => {
                        let container = s
                            .container_name
                            .as_deref()
                            .map(|c| format!(" -- {c}"))
                            .unwrap_or_default();
                        format!("{} ({:?}){container}", s.name, s.kind)
                    }
                    FinderRow::Command(cmd) => format!("{}  ({})", cmd.title, cmd.id),
                };
                ListItem::new(Line::from(Span::styled(text, style)))
            })
            .collect()
    };

    let title = format!(
        "Search Everywhere: {}  (Enter: open, Esc: close)",
        state.query
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    render_scrollable_list(frame, items, block, popup, state.selected);
}

fn render_goto_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(goto) = app.goto.as_ref() else {
        return;
    };
    let width = area.width.clamp(30, 70);
    let height = (goto.results.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = goto
        .results
        .iter()
        .enumerate()
        .map(|(i, location)| {
            let style = if i == goto.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let line = location.range.start.line + 1;
            ListItem::new(Line::from(Span::styled(
                format!("{}:{line}", location.path.display()),
                style,
            )))
        })
        .collect();

    let block = Block::default().borders(Borders::ALL).title(goto.title);
    render_scrollable_list(frame, items, block, popup, goto.selected);
}

/// `Ctrl+Shift+N`'s popup (`docs/features/tui-go-to-file-and-symbol.md`
/// §3.3) -- same centered-`List` shape as `render_goto_popup`. No bolded
/// match-character highlighting -- this crate's existing goto/search
/// popups are plain text too, so this doesn't introduce an
/// inconsistency.
fn render_go_to_file_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.go_to_file.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if app.files_search.searching {
        vec![ListItem::new(Line::from("Searching..."))]
    } else if let Some(results) = &app.files_search.results {
        if results.matches.is_empty() {
            vec![ListItem::new(Line::from("No results."))]
        } else {
            let mut rows: Vec<ListItem> = results
                .matches
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let style = if i == state.selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Line::from(Span::styled(m.relative.clone(), style)))
                })
                .collect();
            if results.truncated {
                rows.push(ListItem::new(Line::from(format!(
                    "+ more, refine your search -- showing the first {} matches",
                    ide_core::MAX_FUZZY_FILE_RESULTS
                ))));
            }
            rows
        }
    } else {
        vec![ListItem::new(Line::from("Type to fuzzy-match a file."))]
    };

    let title = format!("Go to File: {}  (Enter: open, Esc: close)", state.query);
    let block = Block::default().borders(Borders::ALL).title(title);
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// `Ctrl+Alt+Shift+N`'s popup (`docs/features/tui-go-to-file-and-symbol.md`
/// §3.3). Rows show `name` plus `kind`/`container_name` as trailing
/// context -- this crate's popups are single-line-per-row throughout
/// (e.g. `render_notifications_panel`), so there's no separate subtitle
/// line the way an `egui` window could lay out.
fn render_go_to_symbol_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.go_to_symbol.as_ref() else {
        return;
    };
    let rows = app.go_to_symbol_rows();
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(Line::from("No symbols."))]
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, symbol)| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let container = symbol
                    .container_name
                    .as_deref()
                    .map(|c| format!(" -- {c}"))
                    .unwrap_or_default();
                ListItem::new(Line::from(Span::styled(
                    format!("{} ({:?}){container}", symbol.name, symbol.kind),
                    style,
                )))
            })
            .collect()
    };

    let title = format!("Go to Symbol: {}  (Enter: jump, Esc: close)", state.query);
    let block = Block::default().borders(Borders::ALL).title(title);
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// `F12`'s popup (`docs/features/tui-file-structure-and-breadcrumbs.md`
/// §2.5/§3.1). Same centered-`Rect`-plus-bordered-`List` shape as
/// `render_go_to_symbol_popup`, one row per `App::file_structure_rows`
/// entry, indented by `depth * 2` spaces (no real tree lines, per that
/// doc's §1.2/§3.1 scope cuts).
fn render_file_structure_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.file_structure.as_ref() else {
        return;
    };
    let rows = app.file_structure_rows();
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(Line::from("No symbols."))]
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, (symbol, depth))| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let indent = "  ".repeat(*depth);
                ListItem::new(Line::from(Span::styled(
                    format!("{indent}{} ({:?})", symbol.name, symbol.kind),
                    style,
                )))
            })
            .collect()
    };

    let title = format!("File Structure: {}  (Enter: jump, Esc: close)", state.query);
    let block = Block::default().borders(Borders::ALL).title(title);
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// `Ctrl+E`'s popup (`docs/features/tui-recent-files-and-bookmarks.md`
/// §3.3). Same shape as `render_go_to_file_popup`.
fn render_recent_files_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.recent_files.as_ref() else {
        return;
    };
    let rows = app.recent_files_rows();
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(Line::from("No recent files."))]
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, path)| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(path.display().to_string(), style)))
            })
            .collect()
    };

    let title = format!("Recent Files: {}  (Enter: open, Esc: close)", state.query);
    let block = Block::default().borders(Borders::ALL).title(title);
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// `Ctrl+F3`'s popup (`docs/features/tui-recent-files-and-bookmarks.md`
/// §3.3). Rows are `path:line` (1-based) in insertion order -- no line-
/// text preview, matching this crate's single-line-per-row popup
/// convention (§1.1's explicit scope cut).
fn render_bookmarks_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.bookmarks_popup.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if app.nav_state.bookmarks.is_empty() {
        vec![ListItem::new(Line::from(
            "No bookmarks. Press F3 on a line to add one.",
        ))]
    } else {
        app.nav_state
            .bookmarks
            .iter()
            .enumerate()
            .map(|(i, bookmark)| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{}:{}", bookmark.path.display(), bookmark.line + 1),
                    style,
                )))
            })
            .collect()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Show Bookmarks  (Enter: jump, Esc: close)");
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// TODO panel's popup (`docs/features/tui-todo-panel.md` §2.3). Same
/// shape as `render_problems_panel`.
/// `LeftDockTab::Todos` (`docs/features/tui-tool-window-docking.md` §2.3,
/// T33) -- `selected` is `LeftDockState::todos_selected`, now that Todos'
/// cursor lives on the dock's own state instead of a standalone
/// `TodoPanelState` popup. No more centered-popup math/`Clear`: this draws
/// directly into `area`, the dock's own content rect below its tab strip.
fn render_todo_panel(frame: &mut Frame, app: &App, area: Rect, selected: usize) {
    let items: Vec<ListItem> = if app.todo.searching {
        vec![ListItem::new(Line::from("Scanning..."))]
    } else if let Some(results) = &app.todo.results {
        if results.matches.is_empty() {
            vec![ListItem::new(Line::from("No TODOs/FIXMEs/HACKs found."))]
        } else {
            let mut rows: Vec<ListItem> = results
                .matches
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let style = if i == selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Line::from(Span::styled(
                        format!(
                            "{}: {}:{}: {}",
                            m.pattern,
                            m.inner.path.display(),
                            m.inner.line + 1,
                            m.inner.line_text.trim()
                        ),
                        style,
                    )))
                })
                .collect();
            if results.truncated {
                rows.push(ListItem::new(Line::from(format!(
                    "results truncated -- showing the first {} matches per pattern",
                    ide_core::MAX_SEARCH_RESULTS
                ))));
            }
            rows
        }
    } else {
        vec![ListItem::new(Line::from("Scanning..."))]
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title("TODO  (Enter: jump)");
    render_scrollable_list(frame, items, block, area, selected);
}

/// Keymap popup (`docs/features/tui-keymap.md` §2.5). Same shape as
/// `render_todo_panel`; a customized row gets a `*` suffix (mirrors the
/// dirty-tab `*` in `render_tab_strip`) so `is_customized` has a real,
/// non-test call site.
fn render_keymap_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.keymap_popup.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let rows = app.keymap_popup_rows();
    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(Line::from("No matching commands."))]
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, cmd)| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let binding = app
                    .keymap
                    .effective_binding(cmd.id)
                    .map(crate::keymap::label)
                    .unwrap_or_else(|| "\u{2014}".to_string());
                let customized = if app.keymap.is_customized(cmd.id) {
                    "*"
                } else {
                    ""
                };
                let text = if Some(cmd.id) == state.capturing {
                    format!("{}  [Press a key... Esc to cancel]", cmd.title)
                } else {
                    format!("{}{customized}  {binding}", cmd.title)
                };
                ListItem::new(Line::from(Span::styled(text, style)))
            })
            .collect()
    };

    let block = Block::default().borders(Borders::ALL).title(format!(
        "Keymap: {}  (Enter: rebind, Delete: reset, Esc: close)",
        state.query
    ));
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// Keys screen (`docs/features/tui-screen-navigation.md` §2.3, T44) --
/// full-screen, read-only reference reusing `keymap_popup_rows`'s content
/// (which tolerates `app.keymap_popup` being `None`, returning every
/// command unfiltered). No selection/rebind UI here -- that's the existing
/// Keymap Settings popup's job (`render_keymap_popup` above); this screen
/// is a plain reference list, editing arrives in a later T-run (T47).
fn render_keys_screen(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.keymap_popup_rows();
    let start = (app.keys_screen_scroll as usize).min(rows.len());
    let items: Vec<ListItem> = rows[start..]
        .iter()
        .map(|cmd| {
            let binding = app
                .keymap
                .effective_binding(cmd.id)
                .map(crate::keymap::label)
                .unwrap_or_else(|| "\u{2014}".to_string());
            ListItem::new(Line::from(format!("{}  {binding}", cmd.title)))
        })
        .collect();

    let block = Block::default().borders(Borders::ALL).title(
        "Keys  (reference only -- use the Keymap command to rebind; Up/Down/PgUp/PgDn: scroll)",
    );
    frame.render_widget(List::new(items).block(block), area);
}

/// Theme Settings popup (`docs/features/tui-theme.md` §2.3/`T41`) --
/// mirrors `render_keymap_popup`'s centered-popup/`Clear`/`List` shape.
fn render_theme_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.theme_popup.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let rows = app.theme_popup_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            let style = if i == state.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let current = if *kind == app.theme {
                "  (current)"
            } else {
                ""
            };
            let text = format!("{}{current}", kind.label());
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Theme  (Enter: apply, Esc: close)");
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// "New Scratch File" prompt (`docs/features/tui-scratch-files.md`
/// §2.3) -- this crate's first single-line *text-entry* popup that isn't
/// the Find/Replace bar's status-line field or a list's own search box;
/// reuses `List` with one `ListItem` rather than inventing a new widget.
fn render_new_scratch_file_prompt(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.new_scratch_file.as_ref() else {
        return;
    };
    let width = area.width.clamp(30, 70);
    let height = 3u16.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items = vec![ListItem::new(Line::from(state.name.clone()))];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("New Scratch File (name with extension, Enter to create, Esc to cancel):");
    frame.render_widget(List::new(items).block(block), popup);
}

/// Scratch Files browse popup -- same shape as `render_recent_files_popup`.
fn render_scratch_files_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.scratch_files.as_ref() else {
        return;
    };
    let rows = app.scratch_files_rows();
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(Line::from("No scratch files yet."))]
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, path)| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                ListItem::new(Line::from(Span::styled(name, style)))
            })
            .collect()
    };

    let title = format!("Scratch Files: {}  (Enter: open, Esc: close)", state.query);
    let block = Block::default().borders(Borders::ALL).title(title);
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// Claude chat + terminal panel (`docs/features/tui-claude-panel.md`
/// §3.1-§3.3). Same outer popup sizing as `render_cargo_panel`/
/// `render_scratch_files_popup`; see `claude_terminal_grid_size`'s own
/// doc comment for how the terminal grid's content area is derived from
/// this same rect (must stay in sync with that function's arithmetic).
fn render_claude_panel(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let title = match app.claude_view {
        ClaudeView::Chat => {
            "Claude  (Tab: switch view, Ctrl+N: new terminal, Esc: close)".to_string()
        }
        ClaudeView::Terminal(_) => {
            let cwd = app
                .claude_terminals
                .active_tab()
                .map(|tab| tab.cwd.display().to_string())
                .unwrap_or_default();
            format!(
                "Claude -- {cwd}  (Enter: focus terminal, Shift+Esc: leave focus, \
                 Tab: switch view, Ctrl+N: new terminal, Ctrl+W: close terminal, Esc: close)"
            )
        }
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    render_claude_tab_strip(frame, app, rows[0]);
    match app.claude_view {
        ClaudeView::Chat => render_claude_chat(frame, app, rows[1]),
        ClaudeView::Terminal(_) => render_claude_terminal(frame, app, rows[1]),
    }
}

fn render_claude_tab_strip(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme.theme();
    let chat_style = if matches!(app.claude_view, ClaudeView::Chat) {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    let mut spans = vec![Span::styled(" Chat ", chat_style)];
    for (i, tab) in app.claude_terminals.tabs().iter().enumerate() {
        let is_active = matches!(app.claude_view, ClaudeView::Terminal(idx) if idx == i);
        let style = if is_active {
            Style::default().add_modifier(Modifier::REVERSED)
        } else if tab.exited {
            Style::default().fg(theme.fold_marker_fg)
        } else {
            Style::default()
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!(" {} ", tab.title), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Scroll-back via `claude.history_scroll` (`docs/features/
/// tui-panel-history-scroll.md` §2.3/§3.1, T52 -- revises this function's
/// previous "no scroll-back in v1" cut, `tui-claude-panel.md` §1.1).
fn render_claude_chat(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let history_area = rows[0];
    let input_area = rows[1];

    let theme = app.theme.theme();
    let lines: Vec<Line> = app
        .claude
        .history
        .iter()
        .map(|m| claude_message_line(m, theme))
        .collect();
    let visible_rows = history_area.height as usize;
    frame.render_widget(
        Paragraph::new(tail_window(&lines, visible_rows, app.claude.history_scroll).to_vec()),
        history_area,
    );

    let prefix = if app.claude.is_in_flight() {
        "(running) > "
    } else {
        "> "
    };
    frame.render_widget(
        Paragraph::new(format!("{prefix}{}", app.claude.input)),
        input_area,
    );
}

/// The AI dock tab (`docs/features/tui-ai-hybrid-fallback.md` §2.4): one
/// status line (last serving provider + whether the outgoing payload was
/// masked), tail-only history, and the single input line -- the same
/// no-scroll-back shape `render_claude_chat` uses (§1.1 precedent).
fn render_ai_panel(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let theme = app.theme.theme();

    let mut status = String::new();
    if let Some(provider) = &app.ai.provider {
        status.push_str(&format!("serving {provider}"));
    }
    if app.ai.sanitized {
        if !status.is_empty() {
            status.push_str("  |  ");
        }
        status.push_str("payload masked");
    }
    if status.is_empty() {
        status = "idle".to_string();
    }
    let status_style = if app.ai.is_in_flight() {
        Style::default().fg(theme.chip_fg)
    } else {
        Style::default().fg(theme.gutter_fg)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(status, status_style))),
        rows[0],
    );

    let lines: Vec<Line> = app
        .ai
        .history
        .iter()
        .map(|m| ai_message_line(m, theme))
        .collect();
    let visible_rows = rows[1].height as usize;
    frame.render_widget(
        Paragraph::new(tail_window(&lines, visible_rows, app.ai.history_scroll).to_vec()),
        rows[1],
    );

    let prefix = if app.ai.is_in_flight() {
        "(streaming) > "
    } else {
        "> "
    };
    frame.render_widget(Paragraph::new(format!("{prefix}{}", app.ai.input)), rows[2]);
}

fn ai_message_line(message: &AiDisplayMessage, theme: &crate::theme::Theme) -> Line<'static> {
    match message {
        AiDisplayMessage::User(t) => Line::from(format!("> {t}")),
        AiDisplayMessage::Assistant(t) => Line::from(t.clone()),
        AiDisplayMessage::StreamingDelta(d) => Line::from(d.clone()),
        AiDisplayMessage::ProviderServing(p) => Line::from(Span::styled(
            format!("-- {p} --"),
            Style::default().fg(theme.gutter_fg),
        )),
        AiDisplayMessage::Error(t) => Line::from(Span::styled(
            format!("error: {t}"),
            Style::default().fg(theme.error_text),
        )),
    }
}

fn claude_message_line(message: &ClaudeMessage, theme: &crate::theme::Theme) -> Line<'static> {
    match message {
        ClaudeMessage::User(text) => Line::from(format!("> {text}")),
        ClaudeMessage::Assistant(text) => Line::from(text.clone()),
        ClaudeMessage::Error(text) => Line::from(Span::styled(
            format!("error: {text}"),
            Style::default().fg(theme.error_text),
        )),
    }
}

/// No scroll-back in v1 (`docs/features/tui-claude-panel.md` §1.1): only
/// `grid.visible_rows()` renders, sized by `claude_terminal_grid_size`.
/// Adjacent cells sharing the same rendered style are coalesced into one
/// `Span` (`docs/features/tui-claude-panel.md` §3.3).
fn render_claude_terminal(frame: &mut Frame, app: &App, area: Rect) {
    let Some(tab) = app.claude_terminals.active_tab() else {
        frame.render_widget(
            Paragraph::new("No terminal tabs open. Ctrl+N to create one."),
            area,
        );
        return;
    };
    let grid = tab.grid();
    let cursor = grid.cursor();
    let show_cursor = app.claude_terminal_focus;
    let lines: Vec<Line> = grid
        .visible_rows()
        .iter()
        .enumerate()
        .map(|(row_idx, row)| claude_row_to_line(row, row_idx, cursor, show_cursor))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn claude_row_to_line(
    row: &[Cell],
    row_idx: usize,
    cursor: (usize, usize),
    show_cursor: bool,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run: Option<(AnsiColor, AnsiColor, bool, bool, String)> = None;
    for (col_idx, cell) in row.iter().enumerate() {
        let is_cursor = show_cursor && row_idx == cursor.0 && col_idx == cursor.1;
        match &mut run {
            Some((rfg, rbg, rbold, rrev, text))
                if *rfg == cell.fg
                    && *rbg == cell.bg
                    && *rbold == cell.bold
                    && *rrev == is_cursor =>
            {
                text.push(cell.ch);
            }
            _ => {
                if let Some((rfg, rbg, rbold, rrev, text)) = run.take() {
                    spans.push(claude_cell_span(rfg, rbg, rbold, rrev, text));
                }
                run = Some((cell.fg, cell.bg, cell.bold, is_cursor, cell.ch.to_string()));
            }
        }
    }
    if let Some((rfg, rbg, rbold, rrev, text)) = run.take() {
        spans.push(claude_cell_span(rfg, rbg, rbold, rrev, text));
    }
    Line::from(spans)
}

fn claude_cell_span(
    fg: AnsiColor,
    bg: AnsiColor,
    bold: bool,
    reversed: bool,
    text: String,
) -> Span<'static> {
    let mut style = Style::default();
    if let Some(color) = fg.xterm_rgb() {
        style = style.fg(color);
    }
    if let Some(color) = bg.xterm_rgb() {
        style = style.bg(color);
    }
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if reversed {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Span::styled(text, style)
}

fn render_new_claude_terminal_prompt(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.new_claude_terminal.as_ref() else {
        return;
    };
    let width = area.width.clamp(30, 70);
    let height = 3u16.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items = vec![ListItem::new(Line::from(state.name.clone()))];
    let block = Block::default().borders(Borders::ALL).title(
        "New Claude Terminal (directory, blank = project root, Enter to create, Esc to cancel):",
    );
    frame.render_widget(List::new(items).block(block), popup);
}

/// The "Debug" launch popup (`docs/features/tui-debugger.md` §2.5) --
/// shows the resolved adapter command (read-only) and a single-line raw-
/// JSON field for launch arguments. `Enter` parses and launches
/// (`confirm_debug_launch`), `Esc` closes without launching.
fn render_debug_launch_popup(frame: &mut Frame, app: &App, area: Rect) {
    if !app.debug.show_launch_popup {
        return;
    }
    let theme = app.theme.theme();
    let width = area.width.clamp(30, 70);
    let height = if app.debug.error.is_some() { 5 } else { 4 }.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let command_line = app
        .language
        .as_ref()
        .and_then(|c| c.debug_adapter())
        .map(|(command, args)| {
            if args.is_empty() {
                command.to_string()
            } else {
                format!("{command} {}", args.join(" "))
            }
        })
        .unwrap_or_else(|| "(no debug adapter configured)".to_string());
    let mut items = vec![
        ListItem::new(Line::from(format!("Command: {command_line}"))),
        ListItem::new(Line::from(format!(
            "Launch args (JSON): {}",
            app.debug.launch_args_draft
        ))),
    ];
    if let Some(error) = &app.debug.error {
        items.push(ListItem::new(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme.error_text),
        ))));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Debug  (Enter: launch, Esc: cancel)");
    frame.render_widget(List::new(items).block(block), popup);
}

/// The "Configure Debug Adapter" popup (`docs/features/tui-debugger.md`
/// §2.5) -- `ide-tui`'s only way to set a debug adapter command, since it
/// has no Languages… settings window the way `ide-ui` does. `Tab`/
/// `Shift+Tab` switches the focused field; the focused field's row is
/// marked with a leading `>`.
fn render_debug_adapter_config_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.debug_adapter_config_popup.as_ref() else {
        return;
    };
    let width = area.width.clamp(30, 70);
    let height = 4u16.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let field_marker = |field: crate::app::DebugConfigField| {
        if state.field == field {
            ">"
        } else {
            " "
        }
    };
    let items = vec![
        ListItem::new(Line::from(format!(
            "{} Command: {}",
            field_marker(crate::app::DebugConfigField::Command),
            state.command
        ))),
        ListItem::new(Line::from(format!(
            "{} Args: {}",
            field_marker(crate::app::DebugConfigField::Args),
            state.args
        ))),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Configure Debug Adapter  (Tab: switch field, Enter: save, Esc: cancel)");
    frame.render_widget(List::new(items).block(block), popup);
}

/// The Debug tool window (`docs/features/tui-debugger.md` §2.6): Threads/
/// Stack/Output sections, `Tab`-cycled focus (the focused section's title
/// is marked `*`). Single-key shortcuts (c/o/i/u/p/x) run regardless of
/// which section has focus, listed in the outer block's title.
fn render_debug_panel(frame: &mut Frame, app: &App, area: Rect) {
    if !app.debug_panel_open {
        return;
    }
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let outer = Block::default().borders(Borders::ALL).title(
        "Debug  (c: continue, o: step over, i: step into, u: step out, p: pause, x: stop, Esc: close)",
    );
    let inner = outer.inner(popup);
    frame.render_widget(outer, popup);

    let sections = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
        ])
        .split(inner);

    let section_title = |base: &str, focus: DebugPanelFocus| {
        if app.debug_panel.focus == focus {
            format!("* {base}")
        } else {
            base.to_string()
        }
    };

    let thread_items: Vec<ListItem> = if app.debug.threads.is_empty() {
        vec![ListItem::new(Line::from("No threads."))]
    } else {
        app.debug
            .threads
            .iter()
            .enumerate()
            .map(|(i, thread)| {
                let style = if i == app.debug_panel.thread_selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{}: {}", thread.id, thread.name),
                    style,
                )))
            })
            .collect()
    };
    render_scrollable_list(
        frame,
        thread_items,
        Block::default()
            .borders(Borders::ALL)
            .title(section_title("Threads", DebugPanelFocus::Threads)),
        sections[0],
        app.debug_panel.thread_selected,
    );

    let stack_items: Vec<ListItem> = if app.debug.stack.is_empty() {
        vec![ListItem::new(Line::from("No stack frames."))]
    } else {
        app.debug
            .stack
            .iter()
            .enumerate()
            .map(|(i, frame_info)| {
                let style = if i == app.debug_panel.stack_selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let location = frame_info
                    .source
                    .as_ref()
                    .map(|p| format!("{}:{}", p.display(), frame_info.line))
                    .unwrap_or_else(|| "<no source>".to_string());
                ListItem::new(Line::from(Span::styled(
                    format!("{} ({location})", frame_info.name),
                    style,
                )))
            })
            .collect()
    };
    render_scrollable_list(
        frame,
        stack_items,
        Block::default()
            .borders(Borders::ALL)
            .title(section_title("Stack", DebugPanelFocus::Stack)),
        sections[1],
        app.debug_panel.stack_selected,
    );

    let visible_rows = sections[2].height.saturating_sub(2) as usize;
    let output = &app.debug.output;
    let scroll = app.debug_panel.output_scroll as usize;
    let start = output
        .len()
        .saturating_sub(visible_rows)
        .saturating_sub(scroll);
    let end = output.len().saturating_sub(scroll.min(output.len()));
    let output_items: Vec<ListItem> = if output.is_empty() {
        vec![ListItem::new(Line::from("No output yet."))]
    } else {
        output
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|(_, line)| ListItem::new(Line::from(line.as_str())))
            .collect()
    };
    frame.render_widget(
        List::new(output_items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(section_title("Output", DebugPanelFocus::Output)),
        ),
        sections[2],
    );
}

fn render_notifications_panel(frame: &mut Frame, app: &App, area: Rect) {
    if !app.notifications_open {
        return;
    }
    let width = area.width.clamp(30, 70);
    let height =
        (app.notifications.len() as u16 + 3).clamp(4, area.height.saturating_sub(2).max(4));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if app.notifications.is_empty() {
        vec![ListItem::new(Line::from("No notifications."))]
    } else {
        app.notifications
            .iter()
            .rev()
            .map(|n| {
                let marker = if n.read { "  " } else { "* " };
                ListItem::new(Line::from(format!("{marker}{}", n.message)))
            })
            .collect()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Notifications  (c: clear, r: mark all read, Esc: close)");
    frame.render_widget(List::new(items).block(block), popup);
}

/// `BottomDockTab::Problems` (`docs/features/tui-tool-window-docking.md`
/// §2.3, T33) -- `selected` is `BottomDockState::problems_selected`. No
/// more centered-popup math/`Clear`: draws directly into `area`.
fn render_problems_panel(frame: &mut Frame, app: &App, area: Rect, selected: usize) {
    let rows = app.flattened_diagnostics();
    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(Line::from("No problems."))]
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, (path, diag))| {
                let style = if i == selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let marker = match diag.severity {
                    DiagnosticSeverity::Error => "E",
                    DiagnosticSeverity::Warning => "W",
                    DiagnosticSeverity::Information => "I",
                    DiagnosticSeverity::Hint => "H",
                };
                let line = diag.range.start.line + 1;
                ListItem::new(Line::from(Span::styled(
                    format!("{marker} {}:{line}: {}", path.display(), diag.message),
                    style,
                )))
            })
            .collect()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Problems  (Enter: open)");
    render_scrollable_list(frame, items, block, area, selected);
}

/// `BottomDockTab::Cargo` (`docs/features/tui-tool-window-docking.md` §2.3,
/// T33) -- no more centered-popup math/`Clear`: draws directly into `area`.
/// Deliberately not sized to its content the way `render_problems_panel` is
/// -- build/test output can run to thousands of lines, so this tab shows
/// only the tail that fits, rather than growing to `output.len()`
/// (`docs/features/tui-cargo-panel.md` §4: no scroll-back in v1).
/// Windows `items` to the last `visible_rows` entries, offset back by
/// `scroll` from the tail (`docs/features/tui-panel-history-scroll.md`
/// §2.1/§3.1, T52) -- shared by every "live-tailing log" panel (Cargo
/// output, Claude chat, AI chat). `scroll == 0` is the tail exactly as
/// every one of these three rendered unconditionally before this
/// feature; increasing it slides the window backward through history.
/// Never panics: `scroll` is clamped to `items.len()` before use.
fn tail_window<T>(items: &[T], visible_rows: usize, scroll: u16) -> &[T] {
    let max_scroll = items.len().saturating_sub(visible_rows);
    let scroll = (scroll as usize).min(max_scroll);
    let end = items.len() - scroll;
    let start = end.saturating_sub(visible_rows);
    &items[start..end]
}

fn render_cargo_panel(frame: &mut Frame, app: &App, area: Rect) {
    let visible_rows = area.height.saturating_sub(2) as usize;
    let output = &app.cargo.output;
    let items: Vec<ListItem> = if output.is_empty() {
        vec![ListItem::new(Line::from(
            "No output yet -- press b/r/t/c/l/f to run a command.",
        ))]
    } else {
        tail_window(output, visible_rows, app.cargo.output_scroll)
            .iter()
            .map(|line| ListItem::new(Line::from(line.as_str())))
            .collect()
    };

    let title = match app.cargo.running {
        Some(command) => format!("cargo {}  (running...)", command.subcommand()),
        None => "Cargo  (b: build, r: run, t: test, c: check, l: clippy, f: fmt)".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(List::new(items).block(block), area);
}

/// A `CustomAction`'s one-line list label -- `External` keeps `T42`'s own
/// `name  --  command args` shape; `Builtin` shows the bound `Command::id`
/// in brackets instead of a program name, so the two kinds are visually
/// distinguishable at a glance (`docs/features/tui-custom-actions-edge-
/// slots.md` §2.1/§3.1, T47).
fn custom_action_label(action: &crate::custom_actions::CustomAction) -> String {
    match &action.kind {
        crate::custom_actions::CustomActionKind::External { command, args } => {
            let args = args.join(" ");
            if args.is_empty() {
                format!("{}  --  {}", action.name, command)
            } else {
                format!("{}  --  {} {}", action.name, command, args)
            }
        }
        crate::custom_actions::CustomActionKind::Builtin { command_id } => {
            format!("{}  --  [{}]", action.name, command_id)
        }
    }
}

/// `LeftDockTab::Actions` -- the `Tree` slot (`docs/features/
/// tui-custom-actions-edge-slots.md` §3.1) -- a plain scrollable list, no
/// output pane (that's `External`-run-shaped state the `Bottom` slot's own
/// tab already owns; a `Tree`-bound action's visible effect, `External` or
/// `Builtin` alike, shows up wherever it always would -- the Bottom dock's
/// Output pane for a subprocess, or the rest of the UI for a `Builtin`).
fn render_tree_actions_tab(frame: &mut Frame, app: &App, area: Rect) {
    use crate::custom_actions::ActionSlot;
    let slot_actions = app.custom_actions.actions_for_slot(ActionSlot::Tree);
    let selected = app.custom_actions.selected(ActionSlot::Tree);
    let items: Vec<ListItem> = if slot_actions.is_empty() {
        vec![ListItem::new(Line::from(
            "No custom actions declared -- open the command palette and run \
             \"Custom Actions: Manage\" to add one.",
        ))]
    } else {
        slot_actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let style = if i == selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(custom_action_label(action), style)))
            })
            .collect()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Actions  (Enter: run)");
    render_scrollable_list(frame, items, block, area, selected);
}

/// Custom Actions dock tab -- the `Bottom` slot (`docs/features/
/// tui-custom-actions-edge-slots.md` §3.1) -- mirrors `render_docker_
/// panel`'s list-plus-output-area shape rather than `render_cargo_panel`'s
/// single list, since here the declared-action list and the running
/// output are two logically distinct things (Cargo only ever has its six
/// fixed built-in subcommands, never a user-declared list to select from).
fn render_custom_actions_panel(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    use crate::custom_actions::ActionSlot;
    let theme = app.theme.theme();
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(0)])
        .split(area);

    let slot_actions = app.custom_actions.actions_for_slot(ActionSlot::Bottom);
    let selected = app.custom_actions.selected(ActionSlot::Bottom);
    let items: Vec<ListItem> = if slot_actions.is_empty() {
        vec![ListItem::new(Line::from(
            "No custom actions declared -- open the command palette and run \
             \"Custom Actions: Manage\" to add one.",
        ))]
    } else {
        slot_actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let style = if i == selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(custom_action_label(action), style)))
            })
            .collect()
    };
    let list_block = Block::default()
        .borders(Borders::ALL)
        .title("Custom Actions  (Enter: run)");
    render_scrollable_list(frame, items, list_block, rows[0], selected);
    hits.dock_secondary_area = Some(rows[1]);

    let visible_rows = rows[1].height.saturating_sub(2) as usize;
    let output = &app.custom_actions.output;
    let windowed = tail_window(output, visible_rows, app.custom_actions.output_scroll);
    let output_items: Vec<ListItem> = if output.is_empty() {
        vec![ListItem::new(Line::from("No output yet."))]
    } else {
        windowed
            .iter()
            .map(|line| {
                // `subprocess::run_and_stream`'s two spawn-failure message
                // shapes ("{program} not found on PATH" / "failed to run
                // {program}: {e}") -- flagged in `error_text` so a mistyped
                // custom-action command is visually distinct from ordinary
                // stdout/stderr output (`docs/features/tui-custom-
                // actions.md` §2.3's `ui.rs` bullet).
                let style =
                    if line.ends_with("not found on PATH") || line.starts_with("failed to run ") {
                        Style::default().fg(theme.error_text)
                    } else {
                        Style::default()
                    };
                ListItem::new(Line::from(Span::styled(line.as_str(), style)))
            })
            .collect()
    };
    let output_title = match app.custom_actions.running.as_ref() {
        Some(action) => format!("{}  (running...)", action.name),
        None => "Output".to_string(),
    };
    let output_block = Block::default().borders(Borders::ALL).title(output_title);
    frame.render_widget(List::new(output_items).block(output_block), rows[1]);
}

/// The Manage Custom Actions popup (`docs/features/tui-custom-actions.md`
/// §3.2) -- mirrors `render_git_worktrees_popup`'s list/add-form split
/// exactly, including the field-marker (`>`) convention for the form.
fn render_manage_actions_popup(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.clamp(30, 70).min(area.width);
    let height = area.height.clamp(6, 16).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let Some(state) = app.manage_actions_popup.as_ref() else {
        return;
    };

    if state.adding {
        let field_marker = |field: ActionFormField| {
            if state.add_field == field {
                ">"
            } else {
                " "
            }
        };
        let mut items = vec![
            ListItem::new(Line::from(format!(
                "{} Name: {}",
                field_marker(ActionFormField::Name),
                state.new_name
            ))),
            ListItem::new(Line::from(format!(
                "{} Kind: {:?}  (Space to toggle)",
                field_marker(ActionFormField::Kind),
                state.form_kind
            ))),
            ListItem::new(Line::from(format!(
                "{} {}: {}",
                field_marker(ActionFormField::Command),
                match state.form_kind {
                    FormKind::External => "Command",
                    FormKind::Builtin => "Command id",
                },
                state.new_command
            ))),
        ];
        if state.form_kind == FormKind::External {
            items.push(ListItem::new(Line::from(format!(
                "{} Args: {}",
                field_marker(ActionFormField::Args),
                state.new_args
            ))));
        }
        items.push(ListItem::new(Line::from(format!(
            "{} Slot: {:?}  (Space to cycle)",
            field_marker(ActionFormField::Slot),
            state.form_slot
        ))));
        let title = if state.editing_index.is_some() {
            "Edit Custom Action  (Tab: next field, Enter: save, Esc: cancel)"
        } else {
            "New Custom Action  (Tab: next field, Enter: save, Esc: cancel)"
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        frame.render_widget(List::new(items).block(block), popup);
        return;
    }

    let selected = state.selected;
    let mut items: Vec<ListItem> = app
        .custom_actions
        .actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            let style = if i == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let label = format!("[{:?}] {}", action.slot, custom_action_label(action));
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();
    if items.is_empty() {
        items.push(ListItem::new(Line::from("No custom actions.")));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Manage Custom Actions  (n: add, Enter: edit, d: delete, Esc: close)");
    render_scrollable_list(frame, items, block, popup, selected);
}

/// `F1`'s popup (`docs/features/tui-hover-and-inlay-hints.md` §3.1) --
/// unlike every other overlay in this file, its content is prose (a
/// language server's hover text, possibly multi-line), so it's a
/// `Paragraph` with word wrap rather than a `List` of discrete rows; there
/// is nothing to select or navigate, only `Esc` to close (`handle_hover_
/// key`). Shows a loading state while `lsp.finding_hover` is true --
/// there's always exactly one answer to show, never zero-or-many, so
/// unlike the Goto/Find Usages popups there's no jump-vs-list branch here.
fn render_hover_popup(frame: &mut Frame, app: &App, area: Rect) {
    if !app.hover_open {
        return;
    }
    let width = area.width.clamp(30, 80);
    let body = if app.lsp.finding_hover {
        "Loading..."
    } else {
        app.lsp
            .hover
            .as_deref()
            .unwrap_or("No documentation available.")
    };
    let line_count = body.lines().count().max(1) as u16;
    let height = (line_count + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Documentation  (Esc: close)");
    let paragraph = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup);
}

/// `Ctrl+Shift+F`'s panel (`docs/features/tui-find-in-path.md` §2.4). The
/// typed query lives in the title (live, as the user types) rather than
/// a separate unselectable list row -- the same place `render_cargo_
/// panel`'s title already shows transient state (the running command),
/// so there's no new "non-selectable header row" concept to invent for a
/// `List` widget that doesn't otherwise have one.
fn render_search_panel(frame: &mut Frame, app: &App, area: Rect) {
    if !app.search_open {
        return;
    }
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let field_style = |field: SearchInPathField| {
        if app.search_state.field == field {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        }
    };
    let checkbox = |on: bool| if on { "[x]" } else { "[ ]" };

    let mut header = vec![
        Line::from(Span::styled(
            format!("Query: {}", app.search_state.query),
            field_style(SearchInPathField::Query),
        )),
        Line::from(Span::styled(
            format!("Include: {}", app.search_state.include),
            field_style(SearchInPathField::Include),
        )),
        Line::from(Span::styled(
            format!("Exclude: {}", app.search_state.exclude),
            field_style(SearchInPathField::Exclude),
        )),
    ];
    if app.search_replace_open {
        header.push(Line::from(Span::styled(
            format!("Replace with: {}", app.search_state.replacement),
            field_style(SearchInPathField::Replacement),
        )));
    }
    header.push(Line::from(Span::styled(
        format!(
            "{} Case sensitive  {} Whole word  {} Regex  {} Respect .gitignore",
            checkbox(app.search_options.search.case_sensitive),
            checkbox(app.search_options.search.whole_word),
            checkbox(app.search_options.search.regex),
            checkbox(app.search_options.respect_gitignore),
        ),
        Style::default(),
    )));

    let mut items: Vec<ListItem> = header.into_iter().map(ListItem::new).collect();
    let header_len = items.len();
    // Only the matches branch below has a real, keyboard-navigable
    // selection (`app.search_state.selected`) -- every other branch is a
    // single informational row with nothing to scroll-follow, so `selected`
    // stays at the header's own top rather than pointing at a stale index
    // from a previous search.
    let mut selected = 0;

    if app.search.searching {
        items.push(ListItem::new(Line::from("Searching...")));
    } else if let Some(err) = &app.search.error {
        items.push(ListItem::new(Line::from(err.to_string())));
    } else if let Some(results) = &app.search.results {
        if results.matches.is_empty() {
            items.push(ListItem::new(Line::from("No results.")));
        } else {
            selected = header_len + app.search_state.selected.min(results.matches.len() - 1);
            items.extend(results.matches.iter().enumerate().map(|(i, m)| {
                let style = if i == app.search_state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(
                    format!(
                        "{}:{}:{}  {}",
                        m.path.display(),
                        m.line + 1,
                        m.column + 1,
                        m.line_text
                    ),
                    style,
                )))
            }));
            if results.truncated {
                items.push(ListItem::new(Line::from(format!(
                    "results truncated -- showing the first {} matches",
                    ide_core::MAX_SEARCH_RESULTS
                ))));
            }
        }
    } else {
        items.push(ListItem::new(Line::from(
            "Type a query and press Enter to search.",
        )));
    }

    let title = if app.search_replace_open {
        "Replace in Path  (Tab: next field, Space: toggle, Enter: search/replace/open, Esc: close)"
    } else {
        "Find in Path  (Tab: next field, Space: toggle, Enter: search/open, Esc: close)"
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    render_scrollable_list(frame, items, block, popup, selected);
}

/// The Replace in Path preview (`docs/features/
/// tui-search-and-replace-in-path.md` §2.4/§3.3) -- a per-file occurrence-
/// count summary, deliberately not a line-level diff, mirroring `render_
/// rename_preview`'s exact shape (§1.2's scope cut).
fn render_replace_in_path_preview(frame: &mut Frame, app: &App, area: Rect) {
    let Some(preview) = app.pending_replace_in_path_preview.as_ref() else {
        return;
    };
    let edit = &preview.edit;
    let width = area.width.clamp(40, 90);
    let height = (edit.edits.len() as u16 + 3).clamp(4, area.height.saturating_sub(2).max(4));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let occurrence_count: usize = edit
        .edits
        .iter()
        .map(|f| f.transaction.changes().len())
        .sum();
    let file_count = edit.edits.len();
    let mut items = vec![ListItem::new(Line::from(format!(
        "Replace in Path: {occurrence_count} occurrence{} across {file_count} file{}{}",
        if occurrence_count == 1 { "" } else { "s" },
        if file_count == 1 { "" } else { "s" },
        if preview.truncated {
            " (truncated)"
        } else {
            ""
        },
    )))];
    items.extend(edit.edits.iter().map(|file_edit| {
        let n = file_edit.transaction.changes().len();
        ListItem::new(Line::from(format!(
            "{} -- {n} occurrence{}",
            file_edit.path.display(),
            if n == 1 { "" } else { "s" },
        )))
    }));

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Replace in Path Preview  (Enter: apply, Esc: cancel)");
    frame.render_widget(List::new(items).block(block), popup);
}

/// `Alt+Enter`'s popup (`docs/features/tui-code-actions-and-rename.md`
/// §2.4/§3.1) -- a `List`, same shape as `render_goto_popup`/`render_
/// problems_panel`. A `disabled_reason: Some` entry is still shown (marked
/// `(disabled)`), not filtered out -- the server's full menu is always
/// visible, only *selecting* one distinguishes supported from not (§3.1).
fn render_code_actions_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.code_actions.as_ref() else {
        return;
    };
    let actions = &app.lsp.code_actions;
    let width = area.width.clamp(30, 70);
    let height = (actions.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if actions.is_empty() {
        vec![ListItem::new(Line::from("No actions available."))]
    } else {
        actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let label = if action.disabled_reason.is_some() {
                    format!("{} (disabled)", action.title)
                } else {
                    action.title.clone()
                };
                ListItem::new(Line::from(Span::styled(label, style)))
            })
            .collect()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Show Intention Actions  (Enter: apply, Esc: close)");
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// `Alt+Insert`'s popup (`docs/features/tui-code-generation.md` §2.3) --
/// mirrors `render_code_actions_popup`'s exact shape, sourcing rows from
/// the filtered `generate_menu_actions()` view instead of `lsp.code_
/// actions` wholesale.
fn render_generate_menu_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.generate_menu.as_ref() else {
        return;
    };
    let actions = app.generate_menu_actions();
    let width = area.width.clamp(30, 70);
    let height = (actions.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if actions.is_empty() {
        vec![ListItem::new(Line::from("Nothing to generate here."))]
    } else {
        actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let label = if action.disabled_reason.is_some() {
                    format!("{} (disabled)", action.title)
                } else {
                    action.title.clone()
                };
                ListItem::new(Line::from(Span::styled(label, style)))
            })
            .collect()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Generate  (Enter: apply, Esc: close)");
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// `Shift+F6`'s popup (`docs/features/tui-code-actions-and-rename.md`
/// §2.4/§3.2) -- unlike every list-shaped overlay in this file, this is an
/// editable single-line `Paragraph`, the same "prose, not discrete rows"
/// shape `render_hover_popup` already establishes for a different reason.
fn render_rename_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(popup_state) = app.rename_popup.as_ref() else {
        return;
    };
    let width = area.width.clamp(30, 60);
    let height = 3;
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Rename  (Enter: confirm, Esc: cancel)");
    frame.render_widget(
        Paragraph::new(Line::from(popup_state.input.as_str())).block(block),
        popup,
    );
}

/// The cross-file rename preview (`docs/features/
/// tui-code-actions-and-rename.md` §2.4/§3.3) -- a `List`: a summary row
/// plus one row per affected file, the same content `rename-refactoring
/// .md` §3.5 specifies for `ide-ui`'s own preview window.
fn render_rename_preview(frame: &mut Frame, app: &App, area: Rect) {
    let Some((edit, new_name)) = app.pending_rename_preview.as_ref() else {
        return;
    };
    let width = area.width.clamp(40, 90);
    let height = (edit.edits.len() as u16 + 3).clamp(4, area.height.saturating_sub(2).max(4));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let occurrence_count: usize = edit.edits.iter().map(|f| f.text_edits.len()).sum();
    let file_count = edit.edits.len();
    let mut items = vec![ListItem::new(Line::from(format!(
        "Rename to `{new_name}`: {occurrence_count} occurrence{} across {file_count} file{}",
        if occurrence_count == 1 { "" } else { "s" },
        if file_count == 1 { "" } else { "s" },
    )))];
    items.extend(edit.edits.iter().map(|file_edit| {
        let n = file_edit.text_edits.len();
        ListItem::new(Line::from(format!(
            "{} -- {n} occurrence{}",
            file_edit.path.display(),
            if n == 1 { "" } else { "s" },
        )))
    }));

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Rename Preview  (Enter: apply, Esc: cancel)");
    frame.render_widget(List::new(items).block(block), popup);
}

/// `⌃T`'s popup (`docs/features/tui-refactor-this.md` §2.3/§3.1) --
/// mirrors `render_generate_menu_popup`'s exact shape, sourcing rows from
/// the filtered `refactor_menu_actions()` view instead of `lsp.code_
/// actions` wholesale.
fn render_refactor_menu_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.refactor_menu.as_ref() else {
        return;
    };
    let actions = app.refactor_menu_actions();
    let width = area.width.clamp(30, 70);
    let height = (actions.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if actions.is_empty() {
        vec![ListItem::new(Line::from("No refactoring available here."))]
    } else {
        actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let style = if i == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let label = if action.disabled_reason.is_some() {
                    format!("{} (disabled)", action.title)
                } else {
                    action.title.clone()
                };
                ListItem::new(Line::from(Span::styled(label, style)))
            })
            .collect()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Refactor This  (Enter: apply, Esc: close)");
    render_scrollable_list(frame, items, block, popup, state.selected);
}

/// The Refactor Preview popup (`docs/features/tui-refactor-this.md`
/// §2.3/§3.5) -- near-fullscreen like `render_git_panel`/`render_cargo_
/// panel`, not the small centered-box `render_rename_preview` uses above,
/// since this one shows real multi-file diff content. Flattens each
/// file's diff into `Line`s the same way `render_git_diff` does, reusing
/// `diff_line_to_line` verbatim.
fn render_refactor_preview(frame: &mut Frame, app: &App, area: Rect) {
    let Some(preview) = app.pending_refactor_preview.as_ref() else {
        return;
    };
    let theme = app.theme.theme();
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Refactor Preview  (Enter: apply, Esc: cancel, \u{2191}\u{2193}/PgUp/PgDn: scroll)");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let file_count = preview.edit.edits.len();
    let mut lines: Vec<Line> = vec![Line::from(format!(
        "{}: {file_count} file{}",
        preview.what,
        if file_count == 1 { "" } else { "s" }
    ))];
    for (file_edit, diff) in preview.edit.edits.iter().zip(preview.diffs.iter()) {
        lines.push(Line::from(Span::styled(
            file_edit.path.display().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        match diff {
            Some(file_diff) => {
                for hunk in &file_diff.hunks {
                    for diff_line in &hunk.lines {
                        lines.push(diff_line_to_line(diff_line, theme));
                    }
                }
            }
            None => lines.push(Line::from("(diff unavailable)")),
        }
    }

    let start = (preview.scroll as usize).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[start..].to_vec()).wrap(Wrap { trim: false }),
        inner,
    );
}

/// The Git Panel overlay (`docs/features/tui-git-panel.md` §2.4/§3.2) --
/// near-fullscreen, same sizing convention `render_cargo_panel` uses. Left
/// column: branch header, the Conflicts list (only shown when non-empty),
/// and the commit graph (lane-indented per `assign_lanes`, no connector
/// lines -- §1's "no graph line-drawing" scope cut). Right column: either
/// the three-way conflict-resolution view (while `git.active_conflict`/
/// `binary_conflict` is `Some`) or the diff pane.
fn render_git_panel(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some(state) = app.git_panel.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    if !app.git.is_repo() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Git  (Esc: close)");
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(Paragraph::new("Not a git repository."), inner);
        return;
    }

    let content_area = if app.git.remote_op.is_running() {
        let rows = Layout::default()
            .direction(LayoutDirection::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(3)])
            .split(popup);
        render_remote_op_status_line(frame, app, rows[0]);
        rows[1]
    } else {
        popup
    };

    match state.view {
        GitPanelView::Log => render_git_log_view(frame, app, state, content_area, hits),
        GitPanelView::Changes => render_git_changes(frame, app, state, content_area, hits),
    }

    if app.git.branches_popup.open {
        render_git_branches_popup(frame, app, popup);
    }
    if app.git.worktrees_popup.open {
        render_git_worktrees_popup(frame, app, popup);
    }
}

/// A one-line status strip carved off the top of the Git Panel while a
/// Fetch/Pull/Push is running (`docs/features/git-fetch-pull-push.md`
/// §2.3) -- this crate's stand-in for `ide-ui`'s toolbar spinner/progress
/// bar, since the Git Panel has no toolbar row of its own. Completion
/// (success or error) is reported via `notify()` instead (`App::
/// poll_remote_op`), never here -- this line only ever renders while
/// `remote_op.is_running()` is true, so it never needs its own dismissal.
fn render_remote_op_status_line(frame: &mut Frame, app: &App, area: Rect) {
    let Some(kind) = app.git.remote_op.kind else {
        return;
    };
    let verb = match kind {
        RemoteOpKind::Fetch => "Fetching…",
        RemoteOpKind::Pull => "Pulling…",
        RemoteOpKind::Push => "Pushing…",
    };
    let text = match app.git.remote_op.progress {
        Some(p) if p.total > 0 => format!("{verb} ({}/{} objects)", p.current, p.total),
        _ => verb.to_string(),
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn render_git_log_view(
    frame: &mut Frame,
    app: &App,
    state: &crate::app::GitPanelState,
    area: Rect,
    hits: &mut HitMap,
) {
    let columns = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    render_git_left_column(frame, app, state, columns[0], hits);
    if app.git.active_conflict.is_some() || app.git.binary_conflict.is_some() {
        render_git_conflict_resolution(frame, app, columns[1]);
    } else {
        render_git_diff(frame, app, state, columns[1]);
        hits.git_diff_area = Some(columns[1]);
    }
}

/// Two `List`s (Staged, Unstaged) plus a boxed commit-message text area
/// (`docs/features/tui-git-staging-branches-and-log-filters.md` §2.4,
/// `git-commit-and-staging.md` §2.3's egui rendering content translated to
/// plain list rows).
fn render_git_changes(
    frame: &mut Frame,
    app: &App,
    state: &crate::app::GitPanelState,
    area: Rect,
    hits: &mut HitMap,
) {
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(35),
            Constraint::Min(3),
        ])
        .split(area);

    let status_row =
        |title: &str, entries: &[ide_core::StatusEntry], focused: bool, selected: usize| {
            let items: Vec<ListItem> = if entries.is_empty() {
                vec![ListItem::new(Line::from("(none)"))]
            } else {
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| {
                        let style = if focused && i == selected {
                            Style::default().add_modifier(Modifier::REVERSED)
                        } else {
                            Style::default()
                        };
                        let badge = change_kind_badge(entry.kind);
                        ListItem::new(Line::from(Span::styled(
                            format!("{badge} {}", entry.path.display()),
                            style,
                        )))
                    })
                    .collect()
            };
            (title.to_string(), items)
        };

    let (staged_title, staged_items) = status_row(
        "Staged",
        &app.git.status.staged,
        state.changes_focus == ChangesFocus::Staged,
        state.staged_selected,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{staged_title}  (Enter: unstage)"));
    render_scrollable_list(frame, staged_items, block, rows[0], state.staged_selected);
    hits.git_staged_area = Some(rows[0]);

    let (unstaged_title, unstaged_items) = status_row(
        "Unstaged",
        &app.git.status.unstaged,
        state.changes_focus == ChangesFocus::Unstaged,
        state.unstaged_selected,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{unstaged_title}  (Enter: stage, x: discard)"));
    render_scrollable_list(
        frame,
        unstaged_items,
        block,
        rows[1],
        state.unstaged_selected,
    );
    hits.git_unstaged_area = Some(rows[1]);

    if let Some(path) = app.git.pending_discard.as_ref() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!("Discard changes to {}? (y/n)", path.display()));
        frame.render_widget(Paragraph::new(""), block.inner(rows[1]));
        frame.render_widget(block, rows[1]);
    }

    let amend_marker = if app.git.amend { " [amend]" } else { "" };
    let message_style = if state.changes_focus == ChangesFocus::Message {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    let block = Block::default().borders(Borders::ALL).title(format!(
        "Message{amend_marker}  (a: toggle amend, Enter: commit)"
    ));
    let inner = block.inner(rows[2]);
    frame.render_widget(block, rows[2]);
    frame.render_widget(
        Paragraph::new(Span::styled(app.git.commit_message.clone(), message_style))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn change_kind_badge(kind: ide_core::ChangeKind) -> &'static str {
    match kind {
        ide_core::ChangeKind::Added => "A",
        ide_core::ChangeKind::Modified => "M",
        ide_core::ChangeKind::Deleted => "D",
        ide_core::ChangeKind::Conflicted => "U",
        ide_core::ChangeKind::Untracked => "?",
    }
}

/// The branches popup (`docs/features/
/// tui-git-staging-branches-and-log-filters.md` §2.4/§3.4): centered over
/// whichever view is active underneath, matching the existing conflict-
/// resolution popup's own "draw over the current view" precedent.
/// How many rows `text` occupies once word-wrapped to `width` columns --
/// used only for popup height sizing, so an approximation matching
/// `ratatui`'s own `Wrap` behaviour closely enough is fine; it doesn't
/// need to be pixel-perfect, only big enough that `blame_popup_scroll`
/// only ever has to cover genuine overflow, not a systematic undercount.
fn wrapped_line_count(text: &str, width: u16) -> usize {
    let width = (width.max(1)) as usize;
    text.lines()
        .map(|line| {
            let chars = line.chars().count();
            if chars == 0 {
                1
            } else {
                chars.div_ceil(width)
            }
        })
        .sum::<usize>()
        .max(1)
}

/// The Commit Details popup (`docs/features/tui-blame.md` §2.4/§3.3) --
/// unlike `render_git_branches_popup`'s fixed small-box shape, a commit
/// body has no length cap, so height grows with the wrapped body line
/// count up to a terminal-bounded max; `blame_popup_scroll` covers
/// whatever still doesn't fit. Always goes through `app.git.commit_detail`
/// (the sanitizing `GitPanel` wrapper), never `ide_core::GitRepo::
/// commit_detail` directly -- `docs/security-findings/
/// git-branches-and-blame-ui-2026-09-01.md` findings 1/2.
fn render_blame_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(id) = app.blame_popup.as_ref() else {
        return;
    };
    let width = area.width.clamp(40, 90);
    let inner_width = width.saturating_sub(2);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let body_text = match app.git.commit_detail(id) {
        Ok(detail) => {
            let mut text = format!(
                "{}  {}\n{} <{}>\n{}\n",
                detail.short_id,
                detail.summary,
                detail.author,
                detail.email,
                crate::blame_gutter::relative_time(detail.timestamp, now),
            );
            if !detail.body.is_empty() {
                text.push('\n');
                text.push_str(&detail.body);
            }
            text
        }
        Err(e) => format!("Failed to load commit details: {e}"),
    };

    let wrapped = wrapped_line_count(&body_text, inner_width);
    let height = (wrapped as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Commit Details  (Esc: close)");
    let paragraph = Paragraph::new(body_text)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.blame_popup_scroll, 0));
    frame.render_widget(paragraph, popup);
}

/// Small fixed-size popup (same shape `render_git_branches_popup` uses,
/// not the growing shape `render_blame_popup` needs -- this popup's
/// content is two fixed action lines, never variable-length prose)
/// listing the git-gutter click popup's two actions (`docs/features/
/// tui-git-gutter.md` §2.4).
fn render_git_gutter_popup(frame: &mut Frame, _app: &App, area: Rect) {
    let width = area.width.clamp(20, 40).min(area.width);
    let height = 4u16.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Git Gutter  (r: Revert Hunk, d: Show Diff, Esc: close)");
    let body = "r  Revert Hunk\nd  Show Diff";
    frame.render_widget(Paragraph::new(body).block(block), popup);
}

/// The line-number gutter's right-click menu (`docs/features/
/// tui-gutter-line-numbers.md` §2.9, T50) -- same small fixed-size popup
/// shape `render_git_gutter_popup` uses, but list-selectable via
/// `render_scrollable_list` since it has four items, not two mnemonic
/// single keys -- mirrors `render_bookmarks_popup`'s exact
/// list-with-`REVERSED`-highlight shape.
fn render_gutter_context_menu(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.gutter_context_menu.as_ref() else {
        return;
    };
    let width = area.width.clamp(28, 40).min(area.width);
    let height = 6u16.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    const LABELS: [&str; 4] = [
        "Toggle Line Breakpoint",
        "Toggle Bookmark",
        "Show Bookmarks",
        "Toggle Blame Annotations",
    ];
    let items: Vec<ListItem> = LABELS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let style = if i == state.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(*label, style)))
        })
        .collect();

    let block = Block::default().borders(Borders::ALL).title(format!(
        "Line {}  (\u{2191}/\u{2193} select, Enter: run, Esc: close)",
        state.line + 1
    ));
    render_scrollable_list(frame, items, block, popup, state.selected);
}

fn render_git_branches_popup(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.clamp(30, 60).min(area.width);
    let height = area.height.clamp(6, 16).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let rows = app.filtered_branch_rows();
    let selected = app.git.branches_popup.selected;
    let mut items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, (name, is_head))| {
            let style = if i == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let marker = if *is_head { "* " } else { "  " };
            let mut label = format!("{marker}{name}");
            if app.git.branches_popup.pending_delete.as_deref() == Some(name.as_str()) {
                label.push_str("  (not fully merged -- press d again to force delete)");
            }
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();
    if items.is_empty() {
        items.push(ListItem::new(Line::from("No branches match.")));
    }

    let title = if app.git.branches_popup.show_new_branch_input {
        format!("New branch: {}", app.git.branches_popup.new_branch_name)
    } else if app.git.branches_popup.typing_filter {
        format!(
            "Branches  /{}  (Enter: checkout, Esc: stop filtering)",
            app.git.branches_popup.filter
        )
    } else if !app.git.branches_popup.filter.is_empty() {
        format!(
            "Branches  /{}  (Enter: checkout, m: merge, n: new, d: delete, /: filter)",
            app.git.branches_popup.filter
        )
    } else {
        "Branches  (Enter: checkout, m: merge, n: new, d: delete, /: filter, Esc: close)"
            .to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(List::new(items).block(block), popup);
}

/// `docs/features/tui-git-worktrees.md` §2.4.
fn render_git_worktrees_popup(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme.theme();
    let width = area.width.clamp(30, 70).min(area.width);
    let height = area.height.clamp(6, 16).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let state = &app.git.worktrees_popup;

    if state.adding {
        let field_marker = |field: WorktreeAddField| {
            if state.add_field == field {
                ">"
            } else {
                " "
            }
        };
        let branch_value = if state.new_branch.is_empty() && !state.new_name.is_empty() {
            format!("(new branch named {})", state.new_name)
        } else {
            state.new_branch.clone()
        };
        let mut items = vec![
            ListItem::new(Line::from(format!(
                "{} Name: {}",
                field_marker(WorktreeAddField::Name),
                state.new_name
            ))),
            ListItem::new(Line::from(format!(
                "{} Path: {}",
                field_marker(WorktreeAddField::Path),
                state.new_path
            ))),
            ListItem::new(Line::from(format!(
                "{} Branch: {}",
                field_marker(WorktreeAddField::Branch),
                branch_value
            ))),
        ];
        if let Some(error) = state.error.as_ref() {
            items.push(ListItem::new(Line::from(Span::styled(
                error.clone(),
                Style::default().fg(theme.error_text),
            ))));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Add Worktree  (Tab: next field, Enter: create, Esc: cancel)");
        frame.render_widget(List::new(items).block(block), popup);
        return;
    }

    let selected = state.selected;
    let mut items: Vec<ListItem> = state
        .worktrees
        .iter()
        .enumerate()
        .map(|(i, wt)| {
            let style = if i == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let branch = wt.branch.as_deref().unwrap_or("(detached / unavailable)");
            let mut label = format!("{}  {}  {}", wt.name, branch, wt.path.display());
            if wt.is_locked {
                label.push_str("  [locked]");
            }
            if state.pending_force_remove.as_deref() == Some(wt.name.as_str()) {
                label.push_str(
                    "  (has uncommitted changes or is locked -- press r again to force remove)",
                );
            }
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();
    if items.is_empty() {
        items.push(ListItem::new(Line::from("No worktrees.")));
    }
    if let Some(error) = state.error.as_ref() {
        items.push(ListItem::new(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme.error_text),
        ))));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Worktrees  (r: remove, n: add, Esc: close)");
    frame.render_widget(List::new(items).block(block), popup);
}

/// The standalone Clone Repository popup (`docs/features/tui-git-clone.md`
/// §2.3/§3.1, T34) -- same `Clear`-plus-centered-`Rect` construction as
/// `render_git_worktrees_popup` above, but not nested inside the full Git
/// Panel: `app.clone_panel_open` gates this independently of
/// `app.git_panel`.
fn render_clone_panel(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme.theme();
    let width = area.width.clamp(40, 70).min(area.width);
    let height = area.height.clamp(6, 8).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let state = &app.clone;
    let field_marker = |field: ClonePanelField| {
        if state.field == field {
            ">"
        } else {
            " "
        }
    };
    let mut items = vec![
        ListItem::new(Line::from(format!(
            "{} URL: {}",
            field_marker(ClonePanelField::Url),
            state.url
        ))),
        ListItem::new(Line::from(format!(
            "{} Destination: {}",
            field_marker(ClonePanelField::Destination),
            state.destination
        ))),
    ];
    if let Some(error) = state.error.as_ref() {
        items.push(ListItem::new(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme.error_text),
        ))));
    } else if let Some(progress) = state.progress {
        let text = if progress.total_objects > 0 {
            format!(
                "{}/{} objects",
                progress.received_objects, progress.total_objects
            )
        } else {
            "Cloning…".to_string()
        };
        items.push(ListItem::new(Line::from(text)));
    } else if let Some(path) = state.done.as_ref() {
        items.push(ListItem::new(Line::from(format!(
            "Cloned to {}",
            path.display()
        ))));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Clone Repository  (Tab: next field, Enter: clone, Esc: close)");
    frame.render_widget(List::new(items).block(block), popup);
}

/// The `Log` view's top row (`docs/features/
/// tui-git-staging-branches-and-log-filters.md` §2.4): the plain "On
/// branch: ..." line, replaced by the six filter fields laid out inline
/// (focused one reverse-styled) while `focus == Filter`, or by a
/// "← Esc: Back to Log" line while viewing a file's history.
fn render_git_branch_line_row(
    frame: &mut Frame,
    app: &App,
    state: &crate::app::GitPanelState,
    area: Rect,
) {
    if let Some(path) = app.git.log_filter.viewing_file_history.as_ref() {
        frame.render_widget(
            Paragraph::new(format!("History: {}  (← Esc: Back to Log)", path.display())),
            area,
        );
        return;
    }
    let theme = app.theme.theme();

    if state.focus == GitPanelFocus::Filter {
        let filter = &app.git.log_filter;
        let field = |label: &'static str, value: &str, this: FilterField| {
            let style = if state.filter_field == this {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            (label, value.to_string(), style)
        };
        let fields = [
            field("Br", &filter.branch, FilterField::Branch),
            field("Au", &filter.author, FilterField::Author),
            field("Pa", &filter.path, FilterField::Path),
            field("Since", &filter.since, FilterField::Since),
            field("Until", &filter.until, FilterField::Until),
            field("Q", &filter.query, FilterField::Query),
        ];
        let mut spans = Vec::new();
        for (i, (label, value, style)) in fields.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(format!("{label}:{value}"), *style));
        }
        if let Some(error) = filter.error.as_ref() {
            spans.push(Span::styled(
                format!("  ({error})"),
                Style::default().fg(theme.error_text),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let branch_line = match &app.git.current_branch {
        Some(branch) => format!("On branch: {branch}"),
        None => "On branch: (unknown)".to_string(),
    };
    frame.render_widget(Paragraph::new(branch_line), area);
}

fn render_git_left_column(
    frame: &mut Frame,
    app: &App,
    state: &crate::app::GitPanelState,
    area: Rect,
    hits: &mut HitMap,
) {
    let has_conflicts = !app.git.conflicts.is_empty();
    let rows = if has_conflicts {
        Layout::default()
            .direction(LayoutDirection::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length((app.git.conflicts.len() as u16 + 2).min(8)),
                Constraint::Min(3),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(LayoutDirection::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(3)])
            .split(area)
    };

    render_git_branch_line_row(frame, app, state, rows[0]);

    let graph_area = if has_conflicts {
        let items: Vec<ListItem> = app
            .git
            .conflicts
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let style =
                    if state.focus == GitPanelFocus::Conflicts && i == state.conflicts_selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                ListItem::new(Line::from(Span::styled(path.display().to_string(), style)))
            })
            .collect();
        let block = Block::default().borders(Borders::ALL).title("Conflicts");
        frame.render_widget(List::new(items).block(block), rows[1]);
        hits.git_conflicts_area = Some(rows[1]);
        rows[2]
    } else {
        rows[1]
    };

    let lanes = assign_lanes(&app.git.graph);
    let items: Vec<ListItem> = if app.git.graph.is_empty() {
        vec![ListItem::new(Line::from("No commits."))]
    } else {
        app.git
            .graph
            .iter()
            .enumerate()
            .map(|(i, commit)| {
                let style = if state.focus == GitPanelFocus::Graph && i == state.graph_selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let lane = lanes.get(&commit.id).copied().unwrap_or(0);
                let indent = "  ".repeat(lane);
                ListItem::new(Line::from(Span::styled(
                    format!("{indent}* {} {}", commit.short_id, commit.summary),
                    style,
                )))
            })
            .collect()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Commits  (Tab: switch focus, Enter: view diff)");
    frame.render_widget(List::new(items).block(block), graph_area);
    hits.git_graph_area = Some(graph_area);
}

/// Flattens every `FileDiff`'s hunks into styled lines, applying
/// `state.diff_scroll` (§3.2) -- a plain scroll offset, not a `ListState`,
/// since this content isn't a selectable list.
fn render_git_diff(frame: &mut Frame, app: &App, state: &crate::app::GitPanelState, area: Rect) {
    let theme = app.theme.theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Diff  (Up/Down: scroll, PageUp/PageDown: page)");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(diffs) = app.git.diff.as_ref() else {
        frame.render_widget(Paragraph::new("No diff to show."), inner);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    for file_diff in diffs {
        let path = file_diff
            .new_path
            .as_deref()
            .or(file_diff.old_path.as_deref())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            path,
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for hunk in &file_diff.hunks {
            for diff_line in &hunk.lines {
                lines.push(diff_line_to_line(diff_line, theme));
            }
        }
        if file_diff.truncated {
            lines.push(Line::from(
                "... diff truncated -- file too large to show in full",
            ));
        }
    }

    let start = (state.diff_scroll as usize).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[start..].to_vec()).wrap(Wrap { trim: false }),
        inner,
    );
}

fn diff_line_to_line(diff_line: &DiffLine, theme: &crate::theme::Theme) -> Line<'static> {
    match diff_line {
        DiffLine::Context(text) => Line::from(format!("  {text}")),
        DiffLine::Removed(text, spans) => diff_spans_to_line("- ", text, spans, theme.diff_removed),
        DiffLine::Added(text, spans) => diff_spans_to_line("+ ", text, spans, theme.diff_added),
    }
}

/// Terminal analogue of `diff-viewer-enhancements.md` §3.4's intraline
/// highlight box: a `DiffSpan` gets `Modifier::REVERSED` on top of the
/// row's own fg color, rather than a stacked alpha-blended `Frame` --
/// ratatui has no alpha compositing to layer a stronger box over a softer
/// row tint the way `egui`'s `gamma_multiply` does (`docs/features/
/// tui-git-panel.md` §1).
fn diff_spans_to_line(
    prefix: &str,
    text: &str,
    spans: &[ide_core::DiffSpan],
    color: Color,
) -> Line<'static> {
    let base = Style::default().fg(color);
    let mut out = vec![Span::styled(prefix.to_string(), base)];
    let mut pos = 0;
    for span in spans {
        if span.start > pos {
            out.push(Span::styled(text[pos..span.start].to_string(), base));
        }
        out.push(Span::styled(
            text[span.start..span.end].to_string(),
            base.add_modifier(Modifier::REVERSED),
        ));
        pos = span.end;
    }
    if pos < text.len() {
        out.push(Span::styled(text[pos..].to_string(), base));
    }
    Line::from(out)
}

/// The three-way conflict view (`docs/features/tui-git-panel.md` §3.2):
/// Base/Ours/Theirs side by side (read-only), plus the current `result`
/// below. `o`/`t`/`Enter`/`Esc` are `handle_git_panel_key`'s job, not
/// rendered as buttons -- the title line spells out the keys instead.
fn render_git_conflict_resolution(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(path) = app.git.binary_conflict.as_ref() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Conflict  (Esc: back)");
        frame.render_widget(
            Paragraph::new(format!(
                "{}: binary conflict -- resolve outside the app",
                path.display()
            ))
            .wrap(Wrap { trim: false }),
            block.inner(area),
        );
        frame.render_widget(block, area);
        return;
    }
    let Some(conflict) = app.git.active_conflict.as_ref() else {
        return;
    };

    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    let columns = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(rows[0]);

    render_conflict_side(frame, "Base", conflict.sides.base.as_deref(), columns[0]);
    render_conflict_side(frame, "Ours", conflict.sides.ours.as_deref(), columns[1]);
    render_conflict_side(
        frame,
        "Theirs",
        conflict.sides.theirs.as_deref(),
        columns[2],
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Result  (o: accept ours, t: accept theirs, Enter: mark resolved, Esc: back)");
    frame.render_widget(
        Paragraph::new(conflict.result.as_str()).wrap(Wrap { trim: false }),
        block.inner(rows[1]),
    );
    frame.render_widget(block, rows[1]);
}

fn render_conflict_side(frame: &mut Frame, title: &str, content: Option<&str>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    let text = content.unwrap_or("(deleted)");
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }),
        block.inner(area),
    );
    frame.render_widget(block, area);
}

fn focus_style(app: &App, focus: Focus) -> Style {
    if app.focus == focus {
        Style::default().fg(app.theme.theme().focus_indicator)
    } else {
        Style::default()
    }
}

fn selection_style(is_selected: bool) -> Style {
    if is_selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    }
}

/// `BottomDockTab::Docker` (`docs/features/tui-docker-and-kubernetes.md`
/// §2.2/§2.6/§3.1/§3.3, adapted for the dock by `docs/features/
/// tui-tool-window-docking.md` §2.3, T33) -- no more centered-popup math/
/// `Clear`: draws directly into `area`, reading `app.docker` (always alive)
/// instead of an `Option`. Left column: the active tab's list (containers
/// or images). Right column: the selected container's logs, once fetched,
/// or the panel's current error. The yes/no lifecycle confirm still renders
/// as its own small centered modal on top of `area`.
fn render_docker_panel(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let panel = &app.docker;
    let columns = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    hits.dock_secondary_area = Some(columns[1]);

    let tab_label = match panel.tab {
        DockerTab::Containers => "Containers",
        DockerTab::Images => "Images",
    };
    let mut items: Vec<ListItem> = match panel.tab {
        DockerTab::Containers => panel
            .containers
            .iter()
            .enumerate()
            .map(|(i, c)| {
                ListItem::new(Line::from(format!(
                    "{}  {}  {}",
                    c.names, c.image, c.status
                )))
                .style(selection_style(i == panel.selected))
            })
            .collect(),
        DockerTab::Images => panel
            .images
            .iter()
            .enumerate()
            .map(|(i, img)| {
                ListItem::new(Line::from(format!(
                    "{}:{}  {}",
                    img.repository, img.tag, img.size
                )))
                .style(selection_style(i == panel.selected))
            })
            .collect(),
    };
    if items.is_empty() {
        items.push(ListItem::new(Line::from("(none)")));
    }
    if panel.truncated {
        items.push(ListItem::new(Line::from(format!(
            "showing first {} of possibly more",
            crate::docker_panel::MAX_DOCKER_LIST_ITEMS
        ))));
    }
    let left_title = format!(
        "Docker: {tab_label}  ([/]: switch, r: refresh, s/x/b/d: start/stop/restart/rm, Enter: logs)"
    );
    let left_block = Block::default().borders(Borders::ALL).title(left_title);
    frame.render_widget(List::new(items).block(left_block), columns[0]);

    let right_title = match &panel.logs_for {
        Some(id) => format!("Logs: {id}"),
        None => "Logs".to_string(),
    };
    let right_block = Block::default().borders(Borders::ALL).title(right_title);
    if let Some(error) = &panel.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .wrap(Wrap { trim: false })
                .block(right_block),
            columns[1],
        );
    } else if panel.logs.is_empty() {
        frame.render_widget(
            Paragraph::new("(select a container and press Enter to fetch logs)")
                .wrap(Wrap { trim: false })
                .block(right_block),
            columns[1],
        );
    } else {
        let visible_rows = columns[1].height.saturating_sub(2) as usize;
        let windowed = tail_window(&panel.logs, visible_rows, panel.logs_scroll);
        let log_items: Vec<ListItem> = windowed
            .iter()
            .map(|line| ListItem::new(Line::from(line.as_str())))
            .collect();
        frame.render_widget(List::new(log_items).block(right_block), columns[1]);
    }

    if let Some(confirm) = &panel.confirm {
        render_docker_confirm_popup(frame, confirm, area);
    }
}

fn render_docker_confirm_popup(
    frame: &mut Frame,
    confirm: &crate::docker_panel::DockerConfirm,
    area: Rect,
) {
    let width = area.width.clamp(30, 70);
    let height = 3;
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL).title(format!(
        "{} {}?  (y/n)",
        confirm.action.label(),
        confirm.container_name
    ));
    frame.render_widget(Paragraph::new(""), block.inner(popup));
    frame.render_widget(block, popup);
}

/// `BottomDockTab::Kubernetes` (`docs/features/
/// tui-docker-and-kubernetes.md` §2.3/§2.6/§3.1/§3.4/§3.5, adapted for the
/// dock by `docs/features/tui-tool-window-docking.md` §2.3, T33) -- same
/// two-column shape as `render_docker_panel`, reading `app.k8s` (always
/// alive) instead of an `Option`, no more centered-popup math/`Clear`. The
/// right column shows whichever of logs/describe output has content, or
/// the panel's current error. The typed-name confirm, the scale-replica-
/// count prompt, and the context/namespace picker each still render as
/// their own small centered modal on top of `area`, checked in the same
/// priority order `handle_k8s_panel_key` uses.
fn render_k8s_panel(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let panel = &app.k8s;
    let columns = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    hits.dock_secondary_area = Some(columns[1]);

    let tab_label = match panel.tab {
        K8sTab::Pods => "Pods",
        K8sTab::Deployments => "Deployments",
        K8sTab::Services => "Services",
    };
    let mut items: Vec<ListItem> = match panel.tab {
        K8sTab::Pods => panel
            .pods
            .iter()
            .enumerate()
            .map(|(i, p)| {
                ListItem::new(Line::from(format!(
                    "{}  {}  {}  restarts={}",
                    p.name, p.phase, p.ready, p.restarts
                )))
                .style(selection_style(i == panel.selected))
            })
            .collect(),
        K8sTab::Deployments => panel
            .deployments
            .iter()
            .enumerate()
            .map(|(i, d)| {
                ListItem::new(Line::from(format!(
                    "{}  {}/{}",
                    d.name, d.ready, d.replicas
                )))
                .style(selection_style(i == panel.selected))
            })
            .collect(),
        K8sTab::Services => panel
            .services
            .iter()
            .enumerate()
            .map(|(i, s)| {
                ListItem::new(Line::from(format!(
                    "{}  {}  {}",
                    s.name, s.service_type, s.cluster_ip
                )))
                .style(selection_style(i == panel.selected))
            })
            .collect(),
    };
    if items.is_empty() {
        items.push(ListItem::new(Line::from("(none)")));
    }
    if panel.truncated {
        items.push(ListItem::new(Line::from(format!(
            "showing first {} of possibly more",
            crate::k8s_panel::MAX_K8S_LIST_ITEMS
        ))));
    }
    let context_label = panel.context.as_deref().unwrap_or("(default)");
    let namespace_label = panel.namespace.as_deref().unwrap_or("(default)");
    let left_title = format!(
        "K8s [{context_label}/{namespace_label}]: {tab_label}  ([/]: switch, r: refresh, c: context, n: namespace, l: logs, d: delete, s: scale, Enter: describe)"
    );
    let left_block = Block::default().borders(Borders::ALL).title(left_title);
    frame.render_widget(List::new(items).block(left_block), columns[0]);

    let right_title;
    let mut right_error: Option<&str> = None;
    let mut right_lines: &[String] = &[];
    if let Some(error) = &panel.error {
        right_title = "Error".to_string();
        right_error = Some(error.as_str());
    } else if panel.logs_for.is_some() || !panel.logs.is_empty() {
        right_title = match &panel.logs_for {
            Some(id) => format!("Logs: {id}"),
            None => "Logs".to_string(),
        };
        right_lines = &panel.logs;
    } else if panel.describe_for.is_some() || !panel.describe_output.is_empty() {
        right_title = match &panel.describe_for {
            Some(id) => format!("Describe: {id}"),
            None => "Describe".to_string(),
        };
        right_lines = &panel.describe_output;
    } else {
        right_title = "Details".to_string();
    }
    let right_block = Block::default().borders(Borders::ALL).title(right_title);
    if let Some(error) = right_error {
        frame.render_widget(
            Paragraph::new(error)
                .wrap(Wrap { trim: false })
                .block(right_block),
            columns[1],
        );
    } else {
        let visible_rows = columns[1].height.saturating_sub(2) as usize;
        let windowed = tail_window(right_lines, visible_rows, panel.output_scroll);
        let log_items: Vec<ListItem> = windowed
            .iter()
            .map(|line| ListItem::new(Line::from(line.as_str())))
            .collect();
        frame.render_widget(List::new(log_items).block(right_block), columns[1]);
    }

    if let Some(confirm) = &panel.confirm {
        render_k8s_confirm_popup(frame, confirm, area);
    } else if let Some(input) = &panel.scale_input {
        render_k8s_scale_input_popup(frame, input, area);
    } else if let Some(picker) = panel.picker {
        render_k8s_picker_popup(frame, panel, picker, area);
    }
}

fn render_k8s_confirm_popup(frame: &mut Frame, confirm: &crate::k8s_panel::K8sConfirm, area: Rect) {
    let width = area.width.clamp(30, 70);
    let height = 3;
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let action_label = match &confirm.action {
        crate::k8s_panel::K8sDestructive::DeletePod { name } => format!("Delete pod {name}"),
        crate::k8s_panel::K8sDestructive::ScaleDeployment { name, replicas } => {
            format!("Scale {name} to {replicas} replicas")
        }
    };
    let block = Block::default().borders(Borders::ALL).title(format!(
        "{action_label} -- type `{}` to confirm, Esc to cancel",
        confirm.target_name
    ));
    frame.render_widget(Paragraph::new(confirm.typed.as_str()).block(block), popup);
}

fn render_k8s_scale_input_popup(frame: &mut Frame, input: &str, area: Rect) {
    let width = area.width.clamp(30, 60);
    let height = 3;
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Scale to how many replicas?  (Enter: confirm, Esc: cancel)");
    frame.render_widget(Paragraph::new(input).block(block), popup);
}

fn render_k8s_picker_popup(
    frame: &mut Frame,
    panel: &crate::k8s_panel::K8sPanel,
    picker: K8sPicker,
    area: Rect,
) {
    let width = area.width.clamp(30, 60);
    let height = area.height.clamp(6, 20);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let (title, items): (&str, Vec<ListItem>) = match picker {
        K8sPicker::Context => (
            "Select context  (Enter: choose, Esc: cancel)",
            panel
                .available_contexts
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    ListItem::new(Line::from(c.as_str()))
                        .style(selection_style(i == panel.selected))
                })
                .collect(),
        ),
        K8sPicker::Namespace => {
            let mut items = vec![ListItem::new(Line::from("(no namespace filter)"))
                .style(selection_style(panel.selected == 0))];
            items.extend(panel.available_namespaces.iter().enumerate().map(|(i, n)| {
                ListItem::new(Line::from(n.as_str()))
                    .style(selection_style(i + 1 == panel.selected))
            }));
            ("Select namespace  (Enter: choose, Esc: cancel)", items)
        }
    };

    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(List::new(items).block(block), popup);
}

#[cfg(test)]
mod tests {
    use super::tail_window;

    #[test]
    fn tail_window_with_zero_scroll_shows_the_tail() {
        let items: Vec<i32> = (0..10).collect();
        assert_eq!(tail_window(&items, 3, 0), &[7, 8, 9]);
    }

    #[test]
    fn tail_window_with_nonzero_scroll_slides_back_from_the_tail() {
        let items: Vec<i32> = (0..10).collect();
        assert_eq!(tail_window(&items, 3, 2), &[5, 6, 7]);
    }

    #[test]
    fn tail_window_scroll_past_the_start_clamps_at_the_first_item() {
        let items: Vec<i32> = (0..10).collect();
        assert_eq!(tail_window(&items, 3, 100), &[0, 1, 2]);
    }

    #[test]
    fn tail_window_on_empty_items_never_panics() {
        let items: Vec<i32> = Vec::new();
        assert_eq!(tail_window(&items, 5, 3), &[] as &[i32]);
        assert_eq!(tail_window(&items, 5, u16::MAX), &[] as &[i32]);
    }

    #[test]
    fn tail_window_visible_rows_larger_than_items_shows_everything() {
        let items: Vec<i32> = vec![1, 2, 3];
        assert_eq!(tail_window(&items, 10, 0), &[1, 2, 3]);
    }

    #[test]
    fn tail_window_new_items_slide_the_window_forward_while_scrolled_back() {
        // The auto-follow property `docs/features/
        // tui-panel-history-scroll.md` §3.1 relies on: a fixed `scroll`
        // depth stays that many items behind the *current* tail, not
        // pinned to the same absolute indices, as more items arrive.
        let mut items: Vec<i32> = (0..10).collect();
        assert_eq!(tail_window(&items, 3, 2), &[5, 6, 7]);
        items.push(10);
        items.push(11);
        assert_eq!(tail_window(&items, 3, 2), &[7, 8, 9]);
    }
}
