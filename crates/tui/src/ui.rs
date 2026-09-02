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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, ClaudeView, Focus, GitPanelFocus};
use crate::claude_panel::ClaudeMessage;
use crate::claude_terminal::{AnsiColor, Cell};
use crate::docker_panel::DockerTab;
use crate::editor::cursor_line_column;
use crate::folding::VisualLines;
use crate::git_panel::assign_lanes;
use crate::highlight::{
    document_highlight_marks, inlay_hint_chips, semantic_token_marks, styled_line, LineOverlays,
};
use crate::k8s_panel::{K8sPicker, K8sTab};

/// Non-text rows around the editor's visible buffer content: the status
/// bar (`render`'s own vertical split, 1 row) plus `render_editor`'s
/// `Block`'s top/bottom borders (2 rows) plus the tab strip (1 row). This
/// crate has no scroll-follows-cursor logic inside this file (this file
/// mutates nothing, per its own doc comment above) -- `app.rs`'s
/// `handle_editor_key` needs to know how many text rows are actually
/// visible *before* a frame ever renders, so `main.rs` derives it from
/// this constant instead of a real `Layout` pass. If `render`/
/// `render_editor`'s layout ever changes shape, this constant has to
/// change with it -- there is no single source of truth to keep them in
/// sync automatically, so a `Layout` change here is also a reason to grep
/// for this constant's uses (`main.rs`) before merging (`docs/features/
/// tui-scroll-follows-cursor.md` §2.1).
pub const EDITOR_CHROME_ROWS: u16 = 4;

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
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(size);
    let body = rows[0];
    let status_area = rows[1];

    let columns = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(body);

    render_tree(frame, app, columns[0], hits);
    render_editor(frame, app, columns[1], hits);
    render_status(frame, app, status_area);

    if app.palette.is_some() {
        render_palette(frame, app, size);
    }
    if app.goto.is_some() {
        render_goto_popup(frame, app, size);
    }
    if app.notifications_open {
        render_notifications_panel(frame, app, size);
    }
    if app.problems.is_some() {
        render_problems_panel(frame, app, size);
    }
    if app.cargo_panel_open {
        render_cargo_panel(frame, app, size);
    }
    if app.hover_open {
        render_hover_popup(frame, app, size);
    }
    if app.search_open {
        render_search_panel(frame, app, size);
    }
    if app.code_actions.is_some() {
        render_code_actions_popup(frame, app, size);
    }
    if app.rename_popup.is_some() {
        render_rename_popup(frame, app, size);
    }
    if app.pending_rename_preview.is_some() {
        render_rename_preview(frame, app, size);
    }
    if app.git_panel.is_some() {
        render_git_panel(frame, app, size);
    }
    if app.docker_panel.is_some() {
        render_docker_panel(frame, app, size);
    }
    if app.k8s_panel.is_some() {
        render_k8s_panel(frame, app, size);
    }
    if app.go_to_file.is_some() {
        render_go_to_file_popup(frame, app, size);
    }
    if app.go_to_symbol.is_some() {
        render_go_to_symbol_popup(frame, app, size);
    }
    if app.recent_files.is_some() {
        render_recent_files_popup(frame, app, size);
    }
    if app.bookmarks_popup.is_some() {
        render_bookmarks_popup(frame, app, size);
    }
    if app.todo_panel.is_some() {
        render_todo_panel(frame, app, size);
    }
    if app.keymap_popup.is_some() {
        render_keymap_popup(frame, app, size);
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
        .border_style(focus_style(app, Focus::Tree));
    hits.tree_area = Some(block.inner(area));
    frame.render_widget(List::new(items).block(block), area);
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

    // One row for the tab strip, the rest for the buffer text
    // (`docs/features/tui-multi-buffer-tabs.md` §2.3) -- every cursor/
    // scroll computation below is relative to `text_area`, not `inner`,
    // since the strip now occupies `inner`'s first row.
    let sections = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    let strip_area = sections[0];
    let text_area = sections[1];

    render_tab_strip(frame, app, strip_area, hits);
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
    let overlays = LineOverlays {
        semantic_tokens: &semantic_tokens,
        highlights: &highlights,
        inlay_hints: &inlay_hints,
        bracket_pair: &bracket_pair,
        selections: &selections,
    };
    let lines: Vec<Line> = (visible_start..visible_end)
        .map(|row| {
            let line = visual.buffer_line(row);
            let mut styled = styled_line(text_buffer, line, &overlays, buf.indent.width);
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
                    Style::default().fg(Color::DarkGray),
                ));
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
                    cell.set_bg(Color::DarkGray);
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
                    text_area.x + screen_column as u16,
                    text_area.y + screen_line as u16,
                ));
            }
        }
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

fn render_palette(frame: &mut Frame, app: &App, area: Rect) {
    let Some(palette) = app.palette.as_ref() else {
        return;
    };
    let width = area.width.clamp(20, 50);
    let height = (palette.filtered.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
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
    frame.render_widget(List::new(items).block(block), popup);
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
    frame.render_widget(List::new(items).block(block), popup);
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
    frame.render_widget(List::new(items).block(block), popup);
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
    frame.render_widget(List::new(items).block(block), popup);
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
    frame.render_widget(List::new(items).block(block), popup);
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
    frame.render_widget(List::new(items).block(block), popup);
}

/// TODO panel's popup (`docs/features/tui-todo-panel.md` §2.3). Same
/// shape as `render_problems_panel`.
fn render_todo_panel(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.todo_panel.as_ref() else {
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
                    let style = if i == state.selected {
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
        .title("TODO  (Enter: jump, Esc: close)");
    frame.render_widget(List::new(items).block(block), popup);
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
    frame.render_widget(List::new(items).block(block), popup);
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
    frame.render_widget(List::new(items).block(block), popup);
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
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!(" {} ", tab.title), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// No scroll-back in v1, same precedent as `render_cargo_panel` (`docs/
/// features/tui-claude-panel.md` §1.1): only the tail of `history` that
/// fits `area` renders.
fn render_claude_chat(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let history_area = rows[0];
    let input_area = rows[1];

    let lines: Vec<Line> = app.claude.history.iter().map(claude_message_line).collect();
    let visible_rows = history_area.height as usize;
    let start = lines.len().saturating_sub(visible_rows);
    frame.render_widget(Paragraph::new(lines[start..].to_vec()), history_area);

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

fn claude_message_line(message: &ClaudeMessage) -> Line<'static> {
    match message {
        ClaudeMessage::User(text) => Line::from(format!("> {text}")),
        ClaudeMessage::Assistant(text) => Line::from(text.clone()),
        ClaudeMessage::Error(text) => Line::from(Span::styled(
            format!("error: {text}"),
            Style::default().fg(Color::Red),
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

fn render_problems_panel(frame: &mut Frame, app: &App, area: Rect) {
    let Some(problems) = app.problems.as_ref() else {
        return;
    };
    let rows = app.flattened_diagnostics();
    let width = area.width.clamp(40, 90);
    let height = (rows.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(Line::from("No problems."))]
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, (path, diag))| {
                let style = if i == problems.selected {
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
        .title("Problems  (Enter: open, Esc: close)");
    frame.render_widget(List::new(items).block(block), popup);
}

/// Deliberately not sized to its content the way every other overlay here
/// is -- build/test output can run to thousands of lines, so this popup
/// takes most of the screen and shows only the tail that fits, rather than
/// growing to `output.len()` the way `render_problems_panel`'s `height`
/// does (`docs/features/tui-cargo-panel.md` §4: no scroll-back in v1).
fn render_cargo_panel(frame: &mut Frame, app: &App, area: Rect) {
    if !app.cargo_panel_open {
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

    let visible_rows = height.saturating_sub(2) as usize;
    let output = &app.cargo.output;
    let start = output.len().saturating_sub(visible_rows);
    let items: Vec<ListItem> = if output.is_empty() {
        vec![ListItem::new(Line::from(
            "No output yet -- press b/r/t/c/l/f to run a command.",
        ))]
    } else {
        output[start..]
            .iter()
            .map(|line| ListItem::new(Line::from(line.as_str())))
            .collect()
    };

    let title = match app.cargo.running {
        Some(command) => format!("cargo {}  (running... Esc: close)", command.subcommand()),
        None => "Cargo  (b: build, r: run, t: test, c: check, l: clippy, f: fmt, Esc: close)"
            .to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(List::new(items).block(block), popup);
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

    let items: Vec<ListItem> = if app.search.searching {
        vec![ListItem::new(Line::from("Searching..."))]
    } else if let Some(results) = &app.search.results {
        if results.matches.is_empty() {
            vec![ListItem::new(Line::from("No results."))]
        } else {
            let mut rows: Vec<ListItem> = results
                .matches
                .iter()
                .enumerate()
                .map(|(i, m)| {
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
                })
                .collect();
            if results.truncated {
                rows.push(ListItem::new(Line::from(format!(
                    "results truncated -- showing the first {} matches",
                    ide_core::MAX_SEARCH_RESULTS
                ))));
            }
            rows
        }
    } else {
        vec![ListItem::new(Line::from(
            "Type a query and press Enter to search.",
        ))]
    };

    let title = format!(
        "Find in Path: {}  (Enter: search/open, Esc: close)",
        app.search_state.query
    );
    let block = Block::default().borders(Borders::ALL).title(title);
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
    frame.render_widget(List::new(items).block(block), popup);
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

/// The Git Panel overlay (`docs/features/tui-git-panel.md` §2.4/§3.2) --
/// near-fullscreen, same sizing convention `render_cargo_panel` uses. Left
/// column: branch header, the Conflicts list (only shown when non-empty),
/// and the commit graph (lane-indented per `assign_lanes`, no connector
/// lines -- §1's "no graph line-drawing" scope cut). Right column: either
/// the three-way conflict-resolution view (while `git.active_conflict`/
/// `binary_conflict` is `Some`) or the diff pane.
fn render_git_panel(frame: &mut Frame, app: &App, area: Rect) {
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

    let columns = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(popup);

    render_git_left_column(frame, app, state, columns[0]);
    if app.git.active_conflict.is_some() || app.git.binary_conflict.is_some() {
        render_git_conflict_resolution(frame, app, columns[1]);
    } else {
        render_git_diff(frame, app, state, columns[1]);
    }
}

fn render_git_left_column(
    frame: &mut Frame,
    app: &App,
    state: &crate::app::GitPanelState,
    area: Rect,
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

    let branch_line = match &app.git.current_branch {
        Some(branch) => format!("On branch: {branch}"),
        None => "On branch: (unknown)".to_string(),
    };
    frame.render_widget(Paragraph::new(branch_line), rows[0]);

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
}

/// Flattens every `FileDiff`'s hunks into styled lines, applying
/// `state.diff_scroll` (§3.2) -- a plain scroll offset, not a `ListState`,
/// since this content isn't a selectable list.
fn render_git_diff(frame: &mut Frame, app: &App, state: &crate::app::GitPanelState, area: Rect) {
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
                lines.push(diff_line_to_line(diff_line));
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

fn diff_line_to_line(diff_line: &DiffLine) -> Line<'static> {
    match diff_line {
        DiffLine::Context(text) => Line::from(format!("  {text}")),
        DiffLine::Removed(text, spans) => diff_spans_to_line("- ", text, spans, Color::Red),
        DiffLine::Added(text, spans) => diff_spans_to_line("+ ", text, spans, Color::Green),
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
        Style::default().fg(Color::Yellow)
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

/// The Docker Panel overlay (`docs/features/tui-docker-and-kubernetes.md`
/// §2.2/§2.6/§3.1/§3.3) -- near-fullscreen, same sizing convention
/// `render_cargo_panel`/`render_git_panel` use. Left column: the active
/// tab's list (containers or images). Right column: the selected
/// container's logs, once fetched, or the panel's current error. The
/// yes/no lifecycle confirm renders as a small centered modal on top, the
/// same shape `render_rename_popup` already establishes.
fn render_docker_panel(frame: &mut Frame, app: &App, area: Rect) {
    let Some(panel) = app.docker_panel.as_ref() else {
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

    let columns = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(popup);

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
        "Docker: {tab_label}  (Tab: switch, r: refresh, s/x/b/d: start/stop/restart/rm, Enter: logs, Esc: close)"
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
        let log_items: Vec<ListItem> = panel
            .logs
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

/// The Kubernetes Panel overlay (`docs/features/
/// tui-docker-and-kubernetes.md` §2.3/§2.6/§3.1/§3.4/§3.5) -- same
/// two-column shape as `render_docker_panel`. The right column shows
/// whichever of logs/describe output has content, or the panel's current
/// error. The typed-name confirm, the scale-replica-count prompt, and the
/// context/namespace picker each render as their own small centered modal
/// on top, checked in the same priority order `handle_k8s_panel_key`
/// uses.
fn render_k8s_panel(frame: &mut Frame, app: &App, area: Rect) {
    let Some(panel) = app.k8s_panel.as_ref() else {
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

    let columns = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(popup);

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
        "K8s [{context_label}/{namespace_label}]: {tab_label}  (Tab: switch, r: refresh, c: context, n: namespace, l: logs, d: delete, s: scale, Enter: describe, Esc: close)"
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
        let log_items: Vec<ListItem> = right_lines
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
