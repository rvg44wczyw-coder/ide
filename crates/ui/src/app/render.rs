//! Rendering only — the `eframe::App::ui` body and its helpers. Exempt
//! from the coverage target (see `app`'s module docs): every decision here
//! delegates to a tested method on `IdeApp` from the parent module. As a
//! child module of `app`, this file can reach `IdeApp`'s private fields
//! directly (Rust visibility is scoped to a module and its descendants).

use super::{
    BottomView, ClaudeMessage, ClaudeView, ExternalChange, IdeApp, RestoreChoice,
    SearchEverywhereRow, SearchEverywhereTab, SmartModeState, ToolWindow, ViewMode,
};
use crate::command::{self, CommandAction};
use crate::editor::blame_gutter::relative_time;
use crate::editor::{cursor_line_column, CodeEditor};
use crate::file_structure;
use crate::theme::{self, Tokens};
use eframe::egui;
use ide_core::{ChangeKind, DiffLine, DiffSpan, DirEntry, DirEntryKind, FileDiff, StatusEntry};
use ide_lsp::{CodeAction, Diagnostic, Position};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Mirrors `ide_lsp::client`'s private `MAX_SYMBOLS_PER_MESSAGE` (not
/// re-exported -- `ide-ui` has no other way to know a `documentSymbol`/
/// `workspace/symbol` response was capped, per `docs/features/
/// search-everywhere.md` §4).
const MAX_SYMBOLS_PER_MESSAGE: usize = 500;

/// One recognizable glyph per tool-window stripe icon
/// (`intellij-shell-iconography.md` §2.3) -- real icon shapes, not the
/// abstract painter dots/diamonds/triangles B1/B2b used.
enum StripeIcon {
    Folder,
    Chat,
    Warning,
    Output,
    Loupe,
    Branch,
}

/// A diff cell, optionally on its own background tint and/or a solid
/// gutter/content change-bar on its left edge. **Every** cell of the grid
/// goes through this, tint or not: the frame's margin would otherwise
/// shift changed rows sideways relative to context rows, and a diff whose
/// columns don't line up is worse than one with no tint at all. Both cells
/// of a changed row get the fill, so the row reads as one band rather than
/// a coloured word (doc §3.3) -- `egui::Frame` sizes to its content, which
/// inside a `Grid` is the cell, and there is no row rect to paint directly.
/// `bar` paints on the content cell only (its left edge is exactly the
/// gutter/content boundary `docs/features/diff-viewer-enhancements.md`
/// §3.3 asks the bar to sit flush against) -- the gutter cell itself never
/// passes one.
const DIFF_CHANGE_BAR_WIDTH: f32 = 3.0;

fn diff_cell<R>(
    ui: &mut egui::Ui,
    tokens: &Tokens,
    fill: Option<egui::Color32>,
    bar: Option<egui::Color32>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut frame =
        egui::Frame::default().inner_margin(egui::Margin::symmetric(tokens.space.xs as i8, 0));
    if let Some(fill) = fill {
        frame = frame.fill(fill);
    }
    let response = frame.show(ui, add);
    if let Some(color) = bar {
        let rect = response.response.rect;
        let bar_rect =
            egui::Rect::from_min_size(rect.min, egui::vec2(DIFF_CHANGE_BAR_WIDTH, rect.height()));
        ui.painter().rect_filled(bar_rect, 0.0, color);
    }
    response.inner
}

/// Fixed diff-gutter width, sized for 5-digit line numbers
/// (`docs/features/diff-viewer-enhancements.md` §3.2) -- not per-file
/// dynamic; a file whose line numbers exceed this just clips.
const DIFF_GUTTER_WIDTH: f32 = 36.0;

fn diff_gutter_cell(ui: &mut egui::Ui, tokens: &Tokens, fill: Option<egui::Color32>, text: &str) {
    diff_cell(ui, tokens, fill, None, |ui| {
        ui.set_width(DIFF_GUTTER_WIDTH);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.colored_label(tokens.color.gutter_fg, text);
        });
    });
}

/// Splits `text` into `(is_highlighted, substring)` segments per `spans`,
/// in order, covering the whole string with no gaps
/// (`docs/features/diff-viewer-enhancements.md` §2.2/§5.2). `spans` is
/// trusted, already-validated data (sorted, non-overlapping, byte-boundary-
/// valid for `text`) -- every `DiffSpan` `ide-core` produces satisfies
/// this, so it isn't re-checked here.
fn split_line_by_spans<'a>(text: &'a str, spans: &[DiffSpan]) -> Vec<(bool, &'a str)> {
    let mut segments = Vec::new();
    let mut pos = 0;
    for span in spans {
        if span.start > pos {
            segments.push((false, &text[pos..span.start]));
        }
        if span.end > span.start {
            segments.push((true, &text[span.start..span.end]));
        }
        pos = span.end;
    }
    if pos < text.len() {
        segments.push((false, &text[pos..]));
    }
    segments
}

/// Renders one diff line's text, splitting it into an intraline-highlighted
/// box plus plain segments when `spans` is non-empty, or one plain label
/// when it's empty (`docs/features/diff-viewer-enhancements.md` §3.4).
fn diff_line_text(
    ui: &mut egui::Ui,
    tokens: &Tokens,
    fg: egui::Color32,
    text: &str,
    spans: &[DiffSpan],
) {
    if spans.is_empty() {
        ui.colored_label(fg, text);
        return;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (highlighted, segment) in split_line_by_spans(text, spans) {
            if highlighted {
                egui::Frame::default()
                    .fill(fg.gamma_multiply(0.85))
                    .show(ui, |ui| {
                        ui.colored_label(tokens.color.fg_primary, segment);
                    });
            } else {
                ui.colored_label(fg, segment);
            }
        }
    });
}

/// `"{command}"` when `args` is empty, `"{command} {args joined by a
/// space}"` otherwise -- shared by the Languages… settings list and the
/// language-suggestion popup (`docs/features/language-server-arguments.md`
/// §2.3).
fn command_line(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        command.to_string()
    } else {
        format!("{command} {}", args.join(" "))
    }
}

impl IdeApp {
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));

        // Palette-local list navigation: widget-internal, not a
        // registered/rebindable command (`command-palette.md` §4.5), same
        // exemption shape as the escape arbitration below.
        if self.command_palette_open {
            let (up, down, enter) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::Enter),
                )
            });
            if up {
                self.command_palette_move_selection(-1);
            }
            if down {
                self.command_palette_move_selection(1);
            }
            if enter {
                self.command_palette_confirm(ctx);
            }
        }

        // Branches popup-local list navigation, same exemption shape as
        // the command palette's above (`docs/features/
        // git-branches-and-blame.md` §2.2.2).
        if self.git.branches_popup.open {
            let (up, down, enter) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::Enter),
                )
            });
            if up {
                self.branches_popup_move_selection(-1);
            }
            if down {
                self.branches_popup_move_selection(1);
            }
            if enter {
                self.branches_popup_confirm();
            }
        }

        // Search Everywhere-local list navigation, same exemption shape as
        // the command palette's above (`docs/features/search-everywhere.md`
        // §3.2/§3.4).
        if self.search_everywhere_open {
            let (up, down, enter) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::Enter),
                )
            });
            if up {
                self.search_everywhere_move_selection(-1);
            }
            if down {
                self.search_everywhere_move_selection(1);
            }
            if enter {
                self.search_everywhere_confirm(ctx);
            }
        }
        if self.show_go_to_line && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            self.confirm_go_to_line();
        }

        // Recent Files-local list navigation, same exemption shape as
        // Search Everywhere's above (`docs/features/recent-files.md`
        // §2.4) -- clamped rather than wrapping, per that method's own
        // doc comment.
        if self.recent_files_open {
            let (up, down, enter) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::Enter),
                )
            });
            if up {
                self.recent_files_move_selection(-1);
            }
            if down {
                self.recent_files_move_selection(1);
            }
            if enter {
                self.recent_files_confirm();
            }
        }

        // Recent Locations-local list navigation, same shape as Recent
        // Files' above.
        if self.recent_locations_open {
            let (up, down, enter) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::Enter),
                )
            });
            if up {
                self.recent_locations_move_selection(-1);
            }
            if down {
                self.recent_locations_move_selection(1);
            }
            if enter {
                self.recent_locations_confirm();
            }
        }

        // File Structure-local list navigation, same exemption shape as
        // Search Everywhere's above (`file-structure-and-breadcrumbs.md`
        // §3.2).
        if self.file_structure_open {
            let (up, down, enter) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::Enter),
                )
            });
            if up {
                self.file_structure_move_selection(-1);
            }
            if down {
                self.file_structure_move_selection(1);
            }
            if enter {
                self.file_structure_confirm();
            }
        }

        // Registry-driven dispatch (`command-palette.md` §3.1, §B3;
        // `keymap.md` §3.1): suppressed while the palette, Search
        // Everywhere, or the Go to Line dialog own keyboard focus, except
        // `FindAction` itself, so a background `⌘S` can't fire while the
        // user is only trying to type into a query field --
        // `FindAction` staying reachable is what lets `⌘⇧A` reset an
        // already-open palette (§3.2) rather than doing nothing. Looks up
        // each command's binding through `self.keymap` (the user's
        // overlay), not `cmd.binding` (the compile-time default) directly,
        // and stops at the first enabled match rather than firing every
        // one -- `keymap.md` §3.1's new invariant, needed once a user
        // override can make two commands share a chord (impossible under
        // B3's fixed defaults alone).
        // Fourth condition (`docs/features/claude-terminal.md` §3.4): a
        // focused terminal tab needs every keystroke -- including chords
        // that would otherwise be a global command, most importantly
        // `Ctrl+C` (interrupt) -- so a background shortcut can't steal
        // input meant for the PTY.
        let terminal_tab_focused = self.claude_terminals.tabs().iter().any(|tab| {
            ctx.memory(|m| m.has_focus(crate::claude_terminal::terminal_tab_egui_id(tab.id)))
        });
        let suppress_dispatch = self.command_palette_open
            || self.search_everywhere_open
            || self.show_go_to_line
            || self.file_structure_open
            || self.recent_files_open
            || self.recent_locations_open
            || terminal_tab_focused;
        for cmd in command::commands() {
            if suppress_dispatch && cmd.action != CommandAction::FindAction {
                continue;
            }
            let Some(binding) = self.keymap.effective_binding(cmd.id) else {
                continue;
            };
            let pressed = ctx.input(|i| binding.for_platform().pressed(i));
            if pressed && self.is_command_enabled(cmd.action) {
                self.run_command(cmd.action, ctx);
                break;
            }
        }

        // `⇧⇧` Search Everywhere (`docs/features/search-everywhere.md`
        // §3.5): gated on `!ctx.text_edit_focused()` so typing two capital
        // letters in a row anywhere in the app (including this project's
        // own text fields, not just the editor) doesn't spuriously pop the
        // window open mid-sentence (§4).
        if !ctx.text_edit_focused() {
            let (now, shift) = ctx.input(|i| (i.time, i.modifiers.shift));
            if shift && !self.search_everywhere_shift_down {
                if self.search_everywhere_double_tap.press(now) {
                    self.open_search_everywhere(SearchEverywhereTab::Files, false);
                }
            } else if !shift {
                self.search_everywhere_double_tap.disarm();
            }
            self.search_everywhere_shift_down = shift;
        }

        // Escape arbitration: the palette -- the most recently opened
        // overlay -- wins first, then Search Everywhere / Go to Line
        // (mutually exclusive with the palette and each other by
        // construction, so their relative order here isn't observable),
        // then the find bar, then the Usages popup
        // (`command-palette.md` §3.5, `in-buffer-find-replace.md` §7,
        // `search-everywhere.md` §3.4). Each earlier link consumes the key
        // so a later one can't also react to the same press.
        let find_owns_escape = self
            .active_tab
            .is_some_and(|idx| self.tabs[idx].find.is_open());
        if escape && self.command_palette_owns_escape() {
            self.close_command_palette();
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        } else if escape && self.search_everywhere_owns_escape() {
            self.close_search_everywhere();
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        } else if escape && self.show_go_to_line {
            self.show_go_to_line = false;
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        } else if escape && self.file_structure_owns_escape() {
            self.close_file_structure();
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        } else if escape && self.recent_files_owns_escape() {
            self.close_recent_files();
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        } else if escape && self.recent_locations_owns_escape() {
            self.close_recent_locations();
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        } else if escape && find_owns_escape {
            self.close_find();
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        } else if (self.show_usages_popup
            || self.show_goto_popup
            || self.show_hover_popup
            || self.show_code_actions_popup
            || self.rename_popup.is_some()
            || self.pending_rename_preview.is_some()
            || self.show_refactor_menu_popup
            || self.pending_refactor_preview.is_some())
            && escape
            && !self.editor_owns_escape()
        {
            // With several cursors up, `Esc` belongs to the editor, which
            // collapses them. The editor consumes the key, but only inside
            // the central panel -- long after this runs -- so the popup has
            // to yield it here instead (`multiple-cursors.md` §3.6).
            self.show_usages_popup = false;
            self.show_goto_popup = false;
            self.show_hover_popup = false;
            self.show_code_actions_popup = false;
            self.rename_popup = None;
            self.pending_rename_preview = None;
            self.show_refactor_menu_popup = false;
            self.pending_refactor_preview = None;
        }
    }

    /// `pub(super)`: `IdeApp::run_command` (`app.rs`, B3's command
    /// registry dispatch) needs to call this from the parent module.
    pub(super) fn try_save_active(&mut self) {
        match self.save_active() {
            Some(Ok(())) => {
                self.error = None;
                self.maybe_trigger_format_on_save();
            }
            Some(Err(e)) => self.error = Some(e.to_string()),
            None => {
                if let Some(path) = rfd::FileDialog::new().save_file() {
                    if let Some(Err(e)) = self.save_active_as(&path) {
                        self.error = Some(e.to_string());
                    }
                }
            }
        }
    }

    /// Compact launcher (`docs/features/git-remote.md` §3.6): Open/Create/
    /// Clone laid out in a centered, fixed-width column instead of the
    /// window-width layout this replaced -- "compact" describes this
    /// layout, not the OS window's size (see the doc for why window
    /// sizing itself is out of scope).
    fn render_welcome(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.vertical_centered(|ui| {
            ui.set_max_width(420.0);
            ui.add_space(24.0);
            ui.heading("ide");
            ui.add_space(12.0);
            if let Some(parent) = self.pending_create_parent.clone() {
                ui.label(format!("New project inside: {}", parent.display()));
                ui.text_edit_singleline(&mut self.create_project_name);
                ui.horizontal(|ui| {
                    if ui.button("Create").clicked() && !self.create_project_name.is_empty() {
                        let path = parent.join(&self.create_project_name);
                        self.create_project(&path, &ctx);
                    }
                    if ui.button("Cancel").clicked() {
                        self.pending_create_parent = None;
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    if ui.button("Open Project").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.open_project(&dir, &ctx);
                        }
                    }
                    if ui.button("Create Project").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.pending_create_parent = Some(dir);
                        }
                    }
                });
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label("Clone Repository");
                ui.text_edit_singleline(&mut self.clone.url);
                ui.horizontal(|ui| {
                    if ui.button("Choose destination…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.clone.destination = Some(dir);
                        }
                    }
                    if let Some(dest) = &self.clone.destination {
                        ui.label(dest.display().to_string());
                    }
                });
                let cloning = self.clone.is_running();
                let can_clone = !cloning
                    && self.clone.destination.is_some()
                    && !self.clone.url.trim().is_empty();
                ui.add_enabled_ui(can_clone, |ui| {
                    if ui.button("Clone").clicked() {
                        if let Some(dest) = self.clone.destination.clone() {
                            self.clone.start(self.clone.url.clone(), dest);
                        }
                    }
                });
                if let Some(progress) = self.clone.progress {
                    if progress.total_objects > 0 {
                        ui.add(egui::ProgressBar::new(
                            progress.received_objects as f32 / progress.total_objects as f32,
                        ));
                    } else {
                        ui.spinner();
                    }
                    ui.label(format!(
                        "{}/{} objects",
                        progress.received_objects, progress.total_objects
                    ));
                } else if cloning {
                    ui.spinner();
                }
                if let Some(err) = &self.clone.error {
                    ui.colored_label(self.theme.tokens().color.danger, err);
                }
            }
            if let Some(err) = &self.error {
                ui.colored_label(self.theme.tokens().color.danger, err);
            }
        });
    }

    /// Slim tree row (`fleet-shell.md` §3.4): flat `ui.horizontal` entries
    /// with a small fixed indent-per-depth, replacing `CollapsingHeader`'s
    /// heavier frame/spacing. Expand/collapse state persists per path via
    /// `egui`'s own temporary widget memory (`Ui::data_mut`) rather than a
    /// new `IdeApp` field -- `CollapsingHeader` already worked the same
    /// way internally, so this doesn't add a new kind of state to persist.
    /// Icons are `egui::Painter`-drawn shapes in one existing generic theme
    /// color -- a filled circle for a directory, a small square outline for
    /// a file -- deliberately not a per-extension scheme (no such mapping
    /// exists anywhere in this codebase to reuse; see the doc's revision
    /// note 1).
    fn render_tree_entry(
        entry: &DirEntry,
        depth: usize,
        clicked: &mut Option<PathBuf>,
        ui: &mut egui::Ui,
        tokens: &Tokens,
    ) {
        const INDENT_PER_DEPTH: f32 = 14.0;
        const ICON_SIZE: f32 = 8.0;
        let indent = depth as f32 * INDENT_PER_DEPTH;

        match entry.kind {
            DirEntryKind::Dir => {
                let id = egui::Id::new(&entry.path);
                let mut open = ui
                    .ctx()
                    .data_mut(|d| *d.get_temp_mut_or_insert_with(id, || false));
                let row = ui.horizontal(|ui| {
                    ui.add_space(indent);
                    let triangle = if open { "\u{25be}" } else { "\u{25b8}" };
                    ui.label(triangle);
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ICON_SIZE, ICON_SIZE),
                        egui::Sense::hover(),
                    );
                    Self::paint_small_folder(ui.painter(), rect, tokens.color.fg_secondary);
                    ui.add(egui::Label::new(&entry.name).sense(egui::Sense::click()))
                });
                if row.inner.clicked() {
                    open = !open;
                    ui.ctx().data_mut(|d| d.insert_temp(id, open));
                }
                if open {
                    for child in &entry.children {
                        Self::render_tree_entry(child, depth + 1, clicked, ui, tokens);
                    }
                }
            }
            DirEntryKind::File => {
                ui.horizontal(|ui| {
                    ui.add_space(indent + INDENT_PER_DEPTH);
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ICON_SIZE, ICON_SIZE),
                        egui::Sense::hover(),
                    );
                    let dot_color = if entry.name.ends_with(".rs") {
                        tokens.color.accent
                    } else {
                        tokens.color.fg_secondary
                    };
                    ui.painter()
                        .circle_filled(rect.center(), ICON_SIZE / 2.0, dot_color);
                    if ui
                        .add(egui::Label::new(&entry.name).sense(egui::Sense::click()))
                        .clicked()
                    {
                        *clicked = Some(entry.path.clone());
                    }
                });
            }
        }
    }

    /// Small folder glyph for a Project-tree directory row
    /// (`intellij-shell-iconography.md` §2.5) -- a scaled-down version of
    /// the stripe icon's `StripeIcon::Folder` (tab + body), filled solid
    /// since tree rows have no open/closed distinction to communicate.
    fn paint_small_folder(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let c = rect.center();
        let r = rect.width() / 2.0;
        let body = egui::Rect::from_min_max(c + egui::vec2(-r, -r * 0.3), c + egui::vec2(r, r));
        let tab =
            egui::Rect::from_min_max(c + egui::vec2(-r, -r), c + egui::vec2(-r * 0.1, -r * 0.3));
        painter.rect_filled(tab, 0.5, color);
        painter.rect_filled(body, 0.5, color);
    }

    /// One recognizable glyph per tool-window stripe icon
    /// (`intellij-shell-iconography.md` §2.3) -- distinct enough to tell
    /// apart at a glance. `open` fills the shape (accent color) instead of
    /// just stroking it (`fg_secondary`), the painter equivalent of
    /// `selectable_label`'s highlight. `name` becomes a hover tooltip; the
    /// always-visible rotated label is a separate call, see
    /// `render_vertical_stripe_label`.
    fn render_stripe_icon(
        ui: &mut egui::Ui,
        tokens: &Tokens,
        shape: StripeIcon,
        name: &str,
        open: bool,
    ) -> egui::Response {
        const SIZE: f32 = 16.0;
        const R: f32 = 6.0;
        let (rect, response) = ui.allocate_exact_size(egui::vec2(SIZE, SIZE), egui::Sense::click());
        let color = if open {
            tokens.color.accent
        } else {
            tokens.color.fg_secondary
        };
        let c = rect.center();
        let painter = ui.painter();
        if response.hovered() && !open {
            painter.rect_filled(
                rect,
                egui::CornerRadius::same(tokens.radius.sm),
                tokens.color.bg_hover,
            );
        }
        match shape {
            StripeIcon::Folder => {
                let body =
                    egui::Rect::from_min_max(c + egui::vec2(-R, -R * 0.3), c + egui::vec2(R, R));
                let tab = egui::Rect::from_min_max(
                    c + egui::vec2(-R, -R),
                    c + egui::vec2(-R * 0.1, -R * 0.3),
                );
                if open {
                    painter.rect_filled(tab, 1.0, color);
                    painter.rect_filled(body, 1.0, color);
                } else {
                    painter.rect_stroke(
                        tab,
                        1.0,
                        egui::Stroke::new(1.0, color),
                        egui::StrokeKind::Middle,
                    );
                    painter.rect_stroke(
                        body,
                        1.0,
                        egui::Stroke::new(1.0, color),
                        egui::StrokeKind::Middle,
                    );
                }
            }
            StripeIcon::Chat => {
                let body = egui::Rect::from_center_size(
                    c - egui::vec2(0.0, R * 0.15),
                    egui::vec2(R * 1.8, R * 1.3),
                );
                let corner = egui::CornerRadius::same(2);
                if open {
                    painter.rect_filled(body, corner, color);
                } else {
                    painter.rect_stroke(
                        body,
                        corner,
                        egui::Stroke::new(1.0, color),
                        egui::StrokeKind::Middle,
                    );
                }
                let tail = vec![
                    body.left_bottom() + egui::vec2(2.0, -1.0),
                    body.left_bottom() + egui::vec2(2.0, 3.0),
                    body.left_bottom() + egui::vec2(6.0, -1.0),
                ];
                painter.add(egui::Shape::convex_polygon(tail, color, egui::Stroke::NONE));
            }
            StripeIcon::Warning => {
                let points = vec![
                    c + egui::vec2(0.0, -R),
                    c + egui::vec2(R, R),
                    c + egui::vec2(-R, R),
                ];
                painter.add(if open {
                    egui::Shape::convex_polygon(points, color, egui::Stroke::NONE)
                } else {
                    egui::Shape::closed_line(points, egui::Stroke::new(1.0, color))
                });
                painter.line_segment(
                    [c + egui::vec2(0.0, -1.0), c + egui::vec2(0.0, 2.0)],
                    egui::Stroke::new(1.2, color),
                );
                painter.circle_filled(c + egui::vec2(0.0, 3.6), 0.8, color);
            }
            StripeIcon::Output => {
                let body = egui::Rect::from_center_size(c, egui::vec2(R * 1.6, R * 1.6));
                let header =
                    egui::Rect::from_min_size(body.left_top(), egui::vec2(body.width(), R * 0.5));
                if open {
                    painter.rect_filled(body, 1.0, color);
                } else {
                    painter.rect_stroke(
                        body,
                        1.0,
                        egui::Stroke::new(1.0, color),
                        egui::StrokeKind::Middle,
                    );
                    painter.rect_filled(header, 0.0, color);
                }
            }
            StripeIcon::Loupe => Self::paint_loupe(painter, c, R, color),
            StripeIcon::Branch => Self::paint_branch(painter, c, R, color),
        }
        response.on_hover_text(name)
    }

    fn paint_loupe(painter: &egui::Painter, c: egui::Pos2, r: f32, color: egui::Color32) {
        painter.circle_stroke(
            c - egui::vec2(1.5, 1.5),
            r * 0.6,
            egui::Stroke::new(1.0, color),
        );
        painter.line_segment(
            [c + egui::vec2(1.0, 1.0), c + egui::vec2(r, r)],
            egui::Stroke::new(1.0, color),
        );
    }

    fn paint_branch(painter: &egui::Painter, c: egui::Pos2, r: f32, color: egui::Color32) {
        let top = c + egui::vec2(-r * 0.3, -r * 0.8);
        let bottom = c + egui::vec2(-r * 0.3, r * 0.8);
        let mid = c + egui::vec2(r * 0.5, 0.0);
        painter.line_segment([top, bottom], egui::Stroke::new(1.3, color));
        painter.line_segment(
            [c + egui::vec2(-r * 0.3, 0.0), mid],
            egui::Stroke::new(1.3, color),
        );
        for p in [top, bottom, mid] {
            painter.circle_filled(p, 1.6, color);
        }
    }

    /// Always-visible stripe label (`intellij-shell-iconography.md` §2.4),
    /// replacing B2b's tooltip-only name. `reads_upward` picks the rotation
    /// direction: left-stripe labels read bottom-to-top (tilt your head
    /// left), right-stripe labels read top-to-bottom -- the real IntelliJ
    /// convention for each side. `below` says whether the label sits below
    /// (`icon_rect.bottom()`) or above (`icon_rect.top()`) its icon --
    /// independent of `reads_upward`, since a bottom-anchored group in a
    /// `bottom_up` layout still reads the same direction as a top-anchored
    /// one on the same stripe. The edge of the rotated block nearest the
    /// icon is always the gap-adjacent one; the far edge is offset by the
    /// text's own (pre-rotation) width, since rotation swaps which axis
    /// that width lands on. `pos`/`angle` below are the doc's worked
    /// geometry for `TextShape`'s "clockwise around `pos`" rotation,
    /// verified during doc review; do not re-derive.
    fn render_vertical_stripe_label(
        ui: &mut egui::Ui,
        tokens: &Tokens,
        icon_rect: egui::Rect,
        text: &str,
        reads_upward: bool,
        below: bool,
    ) {
        let galley = ui.painter().layout_no_wrap(
            text.to_string(),
            egui::FontId::new(tokens.text.small, egui::FontFamily::Proportional),
            tokens.color.fg_secondary,
        );
        let (w, h) = (galley.size().x, galley.size().y);
        let gap = tokens.space.xs;
        // reads_upward: anchor.y is the label's BOTTOM edge (block extends
        // upward from it). reads_upward=false: anchor.y is the TOP edge
        // (block extends downward). `below`/`!below` picks which of those
        // edges sits next to the icon vs. `w` away from it.
        let anchor_y = match (below, reads_upward) {
            (true, true) => icon_rect.bottom() + gap + w,
            (true, false) => icon_rect.bottom() + gap,
            (false, true) => icon_rect.top() - gap,
            (false, false) => icon_rect.top() - gap - w,
        };
        let anchor = egui::pos2(icon_rect.center().x, anchor_y);
        let pos = if reads_upward {
            egui::pos2(anchor.x - h / 2.0, anchor.y)
        } else {
            egui::pos2(anchor.x + h / 2.0, anchor.y)
        };
        let mut shape = egui::epaint::TextShape::new(pos, galley, tokens.color.fg_secondary);
        shape.angle = if reads_upward {
            -std::f32::consts::FRAC_PI_2
        } else {
            std::f32::consts::FRAC_PI_2
        };
        ui.painter().add(shape);
        // reserve room so sibling widgets in the stripe's vertical layout
        // don't overlap the rotated label.
        ui.add_space(w + gap);
    }

    /// Boxed IntelliJ-style tab (`intellij-shell.md` §2.5), shared by the
    /// editor tab strip and the Bottom tool window's internal tabs. Fill
    /// and the top accent stroke communicate selection instead of egui's
    /// default `selectable_label` highlight.
    fn render_boxed_tab(
        ui: &mut egui::Ui,
        tokens: &Tokens,
        selected: bool,
        text: impl Into<egui::WidgetText>,
    ) -> egui::Response {
        let text: egui::WidgetText = text.into();
        let galley = text.into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Body,
        );
        let padding = egui::vec2(tokens.space.sm, tokens.space.sm * 0.5);
        let size = galley.size() + padding * 2.0;
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        let painter = ui.painter();
        if selected {
            painter.rect_filled(
                rect,
                egui::CornerRadius {
                    nw: tokens.radius.md,
                    ne: tokens.radius.md,
                    sw: 0,
                    se: 0,
                },
                tokens.color.bg_active,
            );
            painter.line_segment(
                [rect.left_top(), rect.right_top()],
                egui::Stroke::new(2.0, tokens.color.accent),
            );
        } else if response.hovered() {
            painter.rect_filled(
                rect,
                egui::CornerRadius {
                    nw: tokens.radius.md,
                    ne: tokens.radius.md,
                    sw: 0,
                    se: 0,
                },
                tokens.color.bg_hover,
            );
        }
        painter.galley(rect.left_top() + padding, galley, tokens.color.fg_primary);
        response
    }

    /// Editor tab strip's closable tab (`fleet-shell.md` §3.5). The close
    /// glyph's width is reserved unconditionally so the tab never resizes
    /// between hover states -- an earlier version added the "x" as a
    /// separate `ui.small_button` only while hovered, which shifted the
    /// tab's own width on the very frame the pointer arrived, so the glyph
    /// kept landing somewhere the pointer wasn't (it panned away from every
    /// click). One `allocate_exact_size`/`Sense::click()` call for the
    /// whole tab avoids that: hovering only changes whether the glyph is
    /// *painted*, never the tab's layout, and closing vs. selecting is
    /// decided by testing the click position against the glyph's rect
    /// rather than by a second widget.
    fn render_editor_tab(
        ui: &mut egui::Ui,
        tokens: &Tokens,
        selected: bool,
        text: impl Into<egui::WidgetText>,
    ) -> (egui::Response, bool) {
        let text: egui::WidgetText = text.into();
        let galley = text.into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Body,
        );
        let padding = egui::vec2(tokens.space.sm, tokens.space.sm * 0.5);
        let close_width = tokens.space.lg;
        let size = galley.size() + padding * 2.0 + egui::vec2(close_width, 0.0);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        let painter = ui.painter();
        if selected {
            painter.rect_filled(
                rect,
                egui::CornerRadius {
                    nw: tokens.radius.md,
                    ne: tokens.radius.md,
                    sw: 0,
                    se: 0,
                },
                tokens.color.bg_active,
            );
            painter.line_segment(
                [rect.left_top(), rect.right_top()],
                egui::Stroke::new(2.0, tokens.color.accent),
            );
        } else if response.hovered() {
            painter.rect_filled(
                rect,
                egui::CornerRadius {
                    nw: tokens.radius.md,
                    ne: tokens.radius.md,
                    sw: 0,
                    se: 0,
                },
                tokens.color.bg_hover,
            );
        }
        painter.galley(rect.left_top() + padding, galley, tokens.color.fg_primary);

        let close_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - close_width, rect.top()),
            egui::vec2(close_width, rect.height()),
        );
        if response.hovered() {
            painter.text(
                close_rect.center(),
                egui::Align2::CENTER_CENTER,
                "\u{2715}",
                egui::FontId::proportional(tokens.text.small),
                tokens.color.fg_secondary,
            );
        }
        let close_clicked = response.clicked()
            && response
                .interact_pointer_pos()
                .is_some_and(|pos| close_rect.contains(pos));
        (response, close_clicked)
    }

    /// Tool-window header row (`intellij-shell.md` §2.3): a title label plus
    /// a bottom border, the same seam treatment `render_top_bar`'s toolbar
    /// border uses. No close control -- the stripe icon that opened this
    /// window already closes it (`toggle_tool_window`'s open/close branch).
    fn render_tool_window_header(ui: &mut egui::Ui, tokens: &Tokens, title: &str) {
        let row = ui.horizontal(|ui| {
            ui.add_space(tokens.space.sm);
            ui.label(egui::RichText::new(title).strong());
        });
        ui.painter().hline(
            ui.max_rect().x_range(),
            row.response.rect.bottom(),
            egui::Stroke::new(1.0, tokens.color.border),
        );
    }

    fn render_tabs_and_editor(&mut self, ui: &mut egui::Ui) {
        let mut close_idx = None;
        let tokens = self.theme.tokens();
        ui.horizontal(|ui| {
            for (idx, tab) in self.tabs.iter().enumerate() {
                let label = if tab.buffer.is_dirty() {
                    format!("\u{25cf} {}", tab.title)
                } else {
                    tab.title.clone()
                };
                let (row, close_clicked) =
                    Self::render_editor_tab(ui, tokens, self.active_tab == Some(idx), label);
                if close_clicked {
                    close_idx = Some(idx);
                } else if row.clicked() {
                    self.active_tab = Some(idx);
                }
            }
            if ui.button("+ New").clicked() {
                self.new_untitled_tab();
            }
        });
        if let Some(idx) = close_idx {
            self.request_close_tab(idx);
        }

        self.render_breadcrumbs(ui);

        if let Some(idx) = self.active_tab {
            self.render_external_change_banner(idx, ui);
        }

        if let Some(idx) = self.active_tab {
            self.render_find_bar(idx, ui);
        }

        if let Some(idx) = self.active_tab {
            let theme = self.theme;
            let tokens = self.theme.tokens();
            let link = self.hover_link.clone();
            let goto = self.pending_cursor_offset.take();
            let diagnostics = std::mem::take(&mut self.tabs[idx].diagnostics);
            let search_matches = self.tabs[idx].find.matches().to_vec();
            let current_match_index = self.tabs[idx].find.current_index();
            let document_highlights = self.lsp.document_highlights.clone();
            let inlay_hints = self.active_inlay_hints().to_vec();
            let semantic_tokens = self.active_semantic_tokens().to_vec();
            let code_action_line = self.code_action_gutter_line();
            let git_gutter_marks = self.git_gutter.clone();
            let blame_on = self.tabs[idx].blame.is_some();
            let blame_annotations = self.tabs[idx].blame.clone().unwrap_or_default();
            let tab = &mut self.tabs[idx];
            let output = CodeEditor::new(
                egui::Id::new(("code_editor", idx)),
                &mut tab.buffer,
                &mut tab.editor,
                tokens,
                theme,
            )
            .diagnostics(&diagnostics)
            .link(link.as_ref())
            .goto_offset(goto)
            .search_matches(&search_matches, current_match_index)
            .document_highlights(&document_highlights)
            .inlay_hints(&inlay_hints)
            .semantic_tokens(&semantic_tokens)
            .code_action_line(code_action_line)
            .git_gutter_marks(&git_gutter_marks)
            .blame_annotations(blame_on, &blame_annotations)
            .show(ui);
            self.tabs[idx].diagnostics = diagnostics;
            self.handle_git_gutter_click(&output);
            self.handle_blame_click(idx, &output);

            self.active_cursor_offset = Some(output.cursor_offset);
            if output.changed {
                // The dirty flag is already set by the widget; this gate is
                // only about not flooding the language server from an idle
                // tab.
                self.notify_lsp_changed(idx);
                self.sync_inlay_hints(idx);
                self.sync_semantic_tokens(idx);
                self.sync_document_symbols(idx);
                // `in-buffer-find-replace.md` §3.6: an edit is one of the
                // explicit `FindBar::refresh` triggers, so an open bar's
                // matches never drift from what's actually in the buffer.
                if self.tabs[idx].find.is_open() {
                    let text = self.tabs[idx].buffer.text().to_string();
                    self.tabs[idx].find.refresh(&text, None);
                }
            }

            // The underline is painted from the *previous* frame's value, so
            // a change has to force the repaint that shows it -- pointer
            // motion alone doesn't cover the case where the modifier is
            // pressed with the pointer already parked on a symbol.
            if self.hover_link != output.hovered_word {
                ui.ctx().request_repaint();
                self.hover_link = output.hovered_word;
            }

            // Cmd+Click / Ctrl+Click follows that link: Go to Declaration,
            // same gesture Cmd+B triggers (`docs/features/
            // goto-definition.md` §3.1) -- find-usages doc §1/§3's original
            // "never go-to-definition" framing was superseded by this
            // phase's gesture-semantics fix (`roadmap.md` §5.3).
            if output.clicked_link.is_some() {
                self.trigger_go_to_declaration();
            }
        }
    }

    /// The caret's own symbol chain, outermost first
    /// (`file-structure-and-breadcrumbs.md` §2.3/§3.4). Renders nothing
    /// (no reserved height) when `active_breadcrumbs()` is empty -- no
    /// client running yet, the caret isn't inside any symbol, or the
    /// active file has no path. Clicking a segment jumps the caret to
    /// that symbol's own start, same `open_definition` call the File
    /// Structure popup's row-click uses.
    fn render_breadcrumbs(&mut self, ui: &mut egui::Ui) {
        let chain: Vec<(PathBuf, Position, String)> = self
            .active_breadcrumbs()
            .into_iter()
            .map(|s| {
                (
                    s.location.path.clone(),
                    s.location.range.start,
                    s.name.clone(),
                )
            })
            .collect();
        if chain.is_empty() {
            return;
        }
        let tokens = self.theme.tokens();
        let mut jump_to = None;
        ui.horizontal(|ui| {
            for (i, (path, position, name)) in chain.iter().enumerate() {
                if i > 0 {
                    ui.colored_label(tokens.color.fg_secondary, "\u{203a}");
                }
                if ui.link(name).clicked() {
                    jump_to = Some((path.clone(), *position));
                }
            }
        });
        if let Some((path, position)) = jump_to {
            self.open_definition(&path, position);
        }
    }

    /// `file-watcher.md` §2.2/§3.4/§3.5: a small banner above the editor
    /// when the active tab's `external_change` is set, offering
    /// Reload/Keep Mine for a content change or a plain dismiss for a
    /// deletion. Kept minimal -- not the focus of the doc.
    fn render_external_change_banner(&mut self, idx: usize, ui: &mut egui::Ui) {
        let Some(change) = self.tabs[idx].external_change else {
            return;
        };
        let tokens = self.theme.tokens();
        egui::Frame::default()
            .fill(tokens.color.bg_elevated)
            .inner_margin(egui::Margin::symmetric(
                tokens.space.sm as i8,
                tokens.space.xs as i8,
            ))
            .show(ui, |ui| {
                ui.horizontal(|ui| match change {
                    ExternalChange::Modified => {
                        ui.colored_label(tokens.color.warning, "File changed on disk.");
                        if ui.button("Reload").clicked() {
                            self.reload_active_from_disk();
                        }
                        if ui.button("Keep Mine").clicked() {
                            self.dismiss_external_change();
                        }
                    }
                    ExternalChange::Deleted => {
                        ui.colored_label(tokens.color.danger, "File deleted on disk.");
                        if ui.button("Dismiss").clicked() {
                            self.dismiss_external_change();
                        }
                    }
                });
            });
    }

    /// The find/replace panel, docked above the active tab's editor
    /// (`in-buffer-find-replace.md` §1, §3). Renders nothing when the tab's
    /// bar is closed.
    fn render_find_bar(&mut self, idx: usize, ui: &mut egui::Ui) {
        if !self.tabs[idx].find.is_open() {
            return;
        }
        let tokens = self.theme.tokens();
        let request_focus = std::mem::take(&mut self.pending_find_focus);
        let text = self.tabs[idx].buffer.text().to_string();
        let replace_open = self.tabs[idx].find.replace_open();

        let mut query = self.tabs[idx].find.query().to_string();
        let mut options = self.tabs[idx].find.options();
        let mut replacement = self.tabs[idx].find.replacement().to_string();
        let mut scoped = self.tabs[idx].find.is_scoped();

        let mut close = false;
        let mut reveal_replace = false;
        let mut go_next = false;
        let mut go_prev = false;
        let mut do_replace = false;
        let mut do_replace_all = false;

        egui::Frame::default()
            .fill(tokens.color.bg_elevated)
            .inner_margin(egui::Margin::symmetric(
                tokens.space.sm as i8,
                tokens.space.xs as i8,
            ))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let response = ui.text_edit_singleline(&mut query);
                    if request_focus {
                        response.request_focus();
                    }
                    if response.changed() {
                        self.tabs[idx].find.set_query(query.clone(), &text);
                    }
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        go_next = true;
                    }

                    match self.tabs[idx].find.error() {
                        Some(err) => {
                            ui.colored_label(tokens.color.danger, err.to_string());
                        }
                        None => {
                            let matches = self.tabs[idx].find.matches();
                            if !matches.is_empty() {
                                let total = if self.tabs[idx].find.truncated() {
                                    format!("{}+", ide_core::MAX_SEARCH_MATCHES)
                                } else {
                                    matches.len().to_string()
                                };
                                let current =
                                    self.tabs[idx].find.current_index().map_or(0, |i| i + 1);
                                ui.label(format!("{current} of {total}"));
                            } else if !query.is_empty() {
                                ui.label("No matches");
                            }
                        }
                    }

                    if ui.small_button("\u{2191}").clicked() {
                        go_prev = true;
                    }
                    if ui.small_button("\u{2193}").clicked() {
                        go_next = true;
                    }
                    if ui.checkbox(&mut options.case_sensitive, "Aa").changed() {
                        self.tabs[idx]
                            .find
                            .set_case_sensitive(options.case_sensitive, &text);
                    }
                    if ui.checkbox(&mut options.whole_word, "Word").changed() {
                        self.tabs[idx]
                            .find
                            .set_whole_word(options.whole_word, &text);
                    }
                    if ui.checkbox(&mut options.regex, ".*").changed() {
                        self.tabs[idx].find.set_regex(options.regex, &text);
                    }
                    if ui.checkbox(&mut scoped, "In Selection").changed() {
                        let scope = scoped
                            .then(|| self.tabs[idx].buffer.text_buffer().selections().primary())
                            .filter(|selection| !selection.is_empty())
                            .map(|selection| selection.range());
                        self.tabs[idx].find.set_scope(scope, &text);
                    }
                    if !replace_open && ui.button("Replace\u{2026}").clicked() {
                        reveal_replace = true;
                    }
                    if ui.small_button("\u{2715}").clicked() {
                        close = true;
                    }
                });

                if replace_open {
                    ui.horizontal(|ui| {
                        let response = ui.text_edit_singleline(&mut replacement);
                        if response.changed() {
                            self.tabs[idx].find.set_replacement(replacement.clone());
                        }
                        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            do_replace = true;
                        }
                        if ui.button("Replace").clicked() {
                            do_replace = true;
                        }
                        if ui.button("Replace All").clicked() {
                            do_replace_all = true;
                        }
                    });
                }
            });

        if close {
            self.close_find();
        }
        if reveal_replace {
            self.open_replace();
        }
        if go_next {
            self.find_next();
        }
        if go_prev {
            self.find_previous();
        }
        if do_replace {
            self.replace_current_match();
        }
        if do_replace_all {
            self.replace_all_matches();
        }
    }

    fn render_problems_panel(&mut self, ui: &mut egui::Ui) {
        let tokens = self.theme.tokens();
        let mut entries: Vec<(&PathBuf, &Vec<Diagnostic>)> = self.lsp.diagnostics.iter().collect();
        entries.sort_by_key(|(a, _)| *a);

        if entries.iter().all(|(_, diags)| diags.is_empty()) {
            ui.label("No problems.");
            return;
        }

        let mut clicked = None;
        egui::ScrollArea::vertical()
            .id_salt("problems_scroll")
            .show(ui, |ui| {
                for (path, diagnostics) in entries {
                    if diagnostics.is_empty() {
                        continue;
                    }
                    ui.heading(path.display().to_string());
                    for diag in diagnostics {
                        // Severity reads as a token-coloured dot rather than
                        // an emoji: the emoji carried a fixed colour of its
                        // own, so it was the one mark in the UI that ignored
                        // the palette (and the theme).
                        let mut label = egui::text::LayoutJob::default();
                        let font_id = egui::TextStyle::Body.resolve(ui.style());
                        label.append(
                            "\u{25cf} ",
                            0.0,
                            egui::TextFormat::simple(
                                font_id.clone(),
                                theme::severity_color(tokens, diag.severity),
                            ),
                        );
                        label.append(
                            &diag.message,
                            0.0,
                            egui::TextFormat::simple(font_id, ui.visuals().text_color()),
                        );
                        if ui.selectable_label(false, label).clicked() {
                            clicked = Some(((*path).clone(), diag.range.start));
                        }
                    }
                }
            });
        if let Some((path, start)) = clicked {
            self.open_diagnostic(&path, start);
        }
    }

    /// Usages panel (doc §3/§5): "Finding usages…" while a query is in
    /// flight, "No usages found." for an empty completed result, otherwise
    /// `LspBridge::references` grouped by file (in path order) and sorted
    /// by `range.start` within each file, labeled `line:column` 1-based
    /// (LSP positions are 0-based). No line-text preview, no
    /// de-duplication -- both explicitly deferred to a later version
    /// (doc §1).
    fn render_usages_panel(&mut self, ui: &mut egui::Ui) {
        if self.lsp.finding_references {
            ui.label("Finding usages…");
            return;
        }
        if self.lsp.references.is_empty() {
            ui.label("No usages found.");
            return;
        }

        let entries = self.sorted_references();

        let mut clicked = None;
        egui::ScrollArea::vertical()
            .id_salt("usages_scroll")
            .show(ui, |ui| {
                let mut current_path: Option<&PathBuf> = None;
                for loc in &entries {
                    if current_path != Some(&loc.path) {
                        ui.heading(loc.path.display().to_string());
                        current_path = Some(&loc.path);
                    }
                    let label = format!(
                        "{}:{}",
                        loc.range.start.line + 1,
                        loc.range.start.character + 1
                    );
                    if ui.selectable_label(false, label).clicked() {
                        clicked = Some((loc.path.clone(), loc.range.start));
                    }
                }
            });
        if let Some((path, position)) = clicked {
            self.open_usage(&path, position);
        }
    }

    /// Floating Usages window -- the `Cmd+B` / `Cmd+Click` destination
    /// (`richer-highlighting-and-usages-popup.md` §3). Same results and
    /// same ordering as the bottom panel's Usages view (both go through
    /// `sorted_references`), but labelled `file:line` on one line per
    /// usage rather than grouped under a path heading: the window is meant
    /// to be scanned and dismissed, not browsed. Clicking a row opens the
    /// file at that position and closes the window.
    fn render_usages_popup(&mut self, ctx: &egui::Context) {
        if !self.show_usages_popup {
            return;
        }

        let searching = self.lsp.finding_references;
        let entries: Vec<(String, PathBuf, Position)> = self
            .sorted_references()
            .into_iter()
            .map(|loc| {
                let label = format!(
                    "{}:{}",
                    self.display_path(&loc.path),
                    loc.range.start.line + 1
                );
                (label, loc.path, loc.range.start)
            })
            .collect();

        let mut open = true;
        let mut clicked = None;
        egui::Window::new("Usages")
            .open(&mut open)
            .collapsible(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                if searching {
                    ui.label("Finding usages…");
                    return;
                }
                if entries.is_empty() {
                    ui.label("No usages found.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("usages_popup_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (label, path, position) in &entries {
                            if ui.selectable_label(false, label).clicked() {
                                clicked = Some((path.clone(), *position));
                            }
                        }
                    });
            });

        if let Some((path, position)) = clicked {
            self.open_usage(&path, position);
            self.show_usages_popup = false;
        } else if !open {
            self.show_usages_popup = false;
        }
    }

    /// `render_usages_popup`'s structural twin for `Goto` results
    /// (`docs/features/goto-definition.md` §3.4): titled from
    /// `goto_action_label()` instead of the fixed `"Usages"`, sourced from
    /// `sorted_goto()`, empty-state text is `"No {label} found."`. Never
    /// shows a "finding…" state, unlike the Usages popup -- `handle_goto_
    /// response` only opens this once the answer is already known.
    fn render_goto_popup(&mut self, ctx: &egui::Context) {
        if !self.show_goto_popup {
            return;
        }

        let label = self.goto_action_label();
        let entries: Vec<(String, PathBuf, Position)> = self
            .sorted_goto()
            .into_iter()
            .map(|loc| {
                let label = format!(
                    "{}:{}",
                    self.display_path(&loc.path),
                    loc.range.start.line + 1
                );
                (label, loc.path, loc.range.start)
            })
            .collect();

        let mut open = true;
        let mut clicked = None;
        egui::Window::new(label)
            .open(&mut open)
            .collapsible(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                if entries.is_empty() {
                    ui.label(format!("No {label} found."));
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("goto_popup_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (row_label, path, position) in &entries {
                            if ui.selectable_label(false, row_label).clicked() {
                                clicked = Some((path.clone(), *position));
                            }
                        }
                    });
            });

        if let Some((path, position)) = clicked {
            self.open_definition(&path, position);
            self.show_goto_popup = false;
        } else if !open {
            self.show_goto_popup = false;
        }
    }

    /// `⌥↩`'s Intention Actions menu (`docs/features/code-actions.md`
    /// §2.3, §3.1): sourced from `self.lsp.code_actions` as-is -- no
    /// request is sent here, `sync_code_actions` keeps it fresh ambiently.
    /// Each row shows its `title`, with `kind` (if any) as a subtitle and
    /// `is_preferred` starred; a `disabled_reason` entry renders greyed
    /// out, not clickable, with the reason as a tooltip (per the
    /// `CodeAction::disabled_reason` doc comment). Clicking an enabled row
    /// calls `select_code_action(index)` and closes the popup.
    fn render_code_actions_popup(&mut self, ctx: &egui::Context) {
        if !self.show_code_actions_popup {
            return;
        }

        let actions = self.lsp.code_actions.clone();

        let mut open = true;
        let mut clicked = None;
        egui::Window::new("Intention Actions")
            .open(&mut open)
            .collapsible(false)
            .default_width(360.0)
            .show(ctx, |ui| {
                if actions.is_empty() {
                    ui.label("No code actions available.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("code_actions_popup_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for action in &actions {
                            let mut label = action.title.clone();
                            if action.is_preferred {
                                label = format!("★ {label}");
                            }
                            if let Some(kind) = &action.kind {
                                label = format!("{label}  ({kind})");
                            }
                            if let Some(reason) = &action.disabled_reason {
                                ui.add_enabled(false, egui::Label::new(&label))
                                    .on_disabled_hover_text(reason);
                            } else if ui.selectable_label(false, &label).clicked() {
                                clicked = Some(action.index);
                            }
                        }
                    });
            });

        if let Some(index) = clicked {
            self.select_code_action(index);
        } else if !open {
            self.show_code_actions_popup = false;
        }
    }

    /// `⌃T`'s popup (`docs/features/refactor-this.md` §3.1) -- same row
    /// shape `render_code_actions_popup` uses, filtered to `kind`s
    /// starting with `"refactor"`.
    fn render_refactor_menu_popup(&mut self, ctx: &egui::Context) {
        if !self.show_refactor_menu_popup {
            return;
        }

        let actions: Vec<CodeAction> = self
            .lsp
            .code_actions
            .iter()
            .filter(|a| a.kind.as_deref().is_some_and(|k| k.starts_with("refactor")))
            .cloned()
            .collect();

        let mut open = true;
        let mut clicked = None;
        egui::Window::new("Refactor This")
            .open(&mut open)
            .collapsible(false)
            .default_width(360.0)
            .show(ctx, |ui| {
                if actions.is_empty() {
                    ui.label("No refactoring available.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("refactor_menu_popup_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for action in &actions {
                            let mut label = action.title.clone();
                            if action.is_preferred {
                                label = format!("★ {label}");
                            }
                            if let Some(reason) = &action.disabled_reason {
                                ui.add_enabled(false, egui::Label::new(&label))
                                    .on_disabled_hover_text(reason);
                            } else if ui.selectable_label(false, &label).clicked() {
                                clicked = Some(action.index);
                            }
                        }
                    });
            });

        if let Some(index) = clicked {
            self.select_refactor_action(index);
        } else if !open {
            self.show_refactor_menu_popup = false;
        }
    }

    /// `⌘N`/`Alt+Insert`'s popup (`docs/features/code-generation.md`
    /// §2.2, §3.1) -- identical row rendering to `render_refactor_menu_
    /// popup`; the only differences are the filter predicate
    /// (`kind.as_deref() == Some("")` instead of `starts_with("refactor")`)
    /// and which trigger a row click calls.
    fn render_generate_menu_popup(&mut self, ctx: &egui::Context) {
        if !self.show_generate_menu_popup {
            return;
        }

        let actions: Vec<CodeAction> = self
            .lsp
            .code_actions
            .iter()
            .filter(|a| a.kind.as_deref() == Some(""))
            .cloned()
            .collect();

        let mut open = true;
        let mut clicked = None;
        egui::Window::new("Generate")
            .open(&mut open)
            .collapsible(false)
            .default_width(360.0)
            .show(ctx, |ui| {
                if actions.is_empty() {
                    ui.label("Nothing to generate here.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("generate_menu_popup_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for action in &actions {
                            let mut label = action.title.clone();
                            if action.is_preferred {
                                label = format!("★ {label}");
                            }
                            if let Some(reason) = &action.disabled_reason {
                                ui.add_enabled(false, egui::Label::new(&label))
                                    .on_disabled_hover_text(reason);
                            } else if ui.selectable_label(false, &label).clicked() {
                                clicked = Some(action.index);
                            }
                        }
                    });
            });

        if let Some(index) = clicked {
            self.select_generate_action(index);
        } else if !open {
            self.show_generate_menu_popup = false;
        }
    }

    /// The Refactor Preview dialog (`docs/features/refactor-this.md`
    /// §3.5) -- reuses `Self::render_diff` (the Source Control view's own
    /// diff renderer) per file, unlike `render_rename_preview`'s plain
    /// count list.
    fn render_refactor_preview(&mut self, ctx: &egui::Context) {
        if self.pending_refactor_preview.is_none() {
            return;
        }
        let tokens = self.theme.tokens();

        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new("Refactor Preview")
            .open(&mut open)
            .collapsible(false)
            .default_width(480.0)
            .show(ctx, |ui| {
                let preview = self
                    .pending_refactor_preview
                    .as_ref()
                    .expect("checked above");
                let file_count = preview.edit.edits.len();
                ui.label(format!(
                    "{}: {file_count} file{}",
                    preview.what,
                    if file_count == 1 { "" } else { "s" }
                ));
                egui::ScrollArea::vertical()
                    .id_salt("refactor_preview_scroll")
                    .max_height(360.0)
                    .show(ui, |ui| {
                        for (file_edit, diff) in preview.edit.edits.iter().zip(&preview.diffs) {
                            ui.heading(file_edit.path.display().to_string());
                            match diff {
                                Some(diff) => {
                                    Self::render_diff(ui, tokens, std::slice::from_ref(diff))
                                }
                                None => {
                                    ui.label("(diff unavailable — see file list above)");
                                }
                            }
                        }
                    });
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if apply {
            self.confirm_refactor_preview();
        } else if cancel || !open {
            self.cancel_refactor_preview();
        }
    }

    /// Replace in Path's own preview window (`search-in-path-v2.md`
    /// §2.2/§3.3) -- structurally the same window `render_refactor_preview`
    /// renders, as a new, separate method against
    /// `pending_replace_in_path_preview`/`ReplaceInPathPreview` rather than
    /// reusing `RefactorPreview` itself, since that struct's `edit` field
    /// is concretely typed `ide_lsp::WorkspaceEdit` and Replace in Path's
    /// edits never touch LSP at all. Title deliberately not "Refactor
    /// Preview", so a user doesn't read an LSP-refactor label on a plain-
    /// text bulk edit.
    fn render_replace_in_path_preview(&mut self, ctx: &egui::Context) {
        if self.pending_replace_in_path_preview.is_none() {
            return;
        }
        let tokens = self.theme.tokens();

        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new("Replace in Path Preview")
            .open(&mut open)
            .collapsible(false)
            .default_width(480.0)
            .show(ctx, |ui| {
                let preview = self
                    .pending_replace_in_path_preview
                    .as_ref()
                    .expect("checked above");
                let file_count = preview.edit.edits.len();
                ui.label(format!(
                    "Replace in Path: {file_count} file{}",
                    if file_count == 1 { "" } else { "s" }
                ));
                egui::ScrollArea::vertical()
                    .id_salt("replace_in_path_preview_scroll")
                    .max_height(360.0)
                    .show(ui, |ui| {
                        for (file_edit, diff) in preview.edit.edits.iter().zip(&preview.diffs) {
                            ui.heading(file_edit.path.display().to_string());
                            match diff {
                                Some(diff) => {
                                    Self::render_diff(ui, tokens, std::slice::from_ref(diff))
                                }
                                None => {
                                    ui.label("(diff unavailable — see file list above)");
                                }
                            }
                        }
                    });
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if apply {
            self.confirm_replace_in_path_preview();
        } else if cancel || !open {
            self.cancel_replace_in_path_preview();
        }
    }

    /// The gutter mark click popup (`docs/features/editor-git-gutter.md`
    /// §2.4, §3.2) -- same shape as `render_code_actions_popup`.
    fn render_git_gutter_popup(&mut self, ctx: &egui::Context) {
        if self.git_gutter_popup_line.is_none() {
            return;
        }

        let mut open = true;
        let mut revert = false;
        let mut show_diff = false;
        let mut toggle_blame = false;
        let blame_label = if self
            .active_tab
            .is_some_and(|idx| self.tabs[idx].blame.is_some())
        {
            "Close Annotations"
        } else {
            "Annotate with Blame"
        };
        egui::Window::new("Git Change")
            .open(&mut open)
            .collapsible(false)
            .default_width(220.0)
            .show(ctx, |ui| {
                if ui.button("Revert Hunk").clicked() {
                    revert = true;
                }
                if ui.button("Show Diff").clicked() {
                    show_diff = true;
                }
                if ui.button(blame_label).clicked() {
                    toggle_blame = true;
                }
            });

        if revert {
            self.trigger_revert_hunk();
        } else if show_diff {
            self.trigger_show_diff_for_gutter();
        } else if toggle_blame {
            self.toggle_blame_annotations();
            self.git_gutter_popup_line = None;
        } else if !open {
            self.git_gutter_popup_line = None;
        }
    }

    fn render_discard_confirm_popup(&mut self, ctx: &egui::Context) {
        let Some(path) = self.git.pending_discard.clone() else {
            return;
        };

        let mut open = true;
        let mut discard = false;
        let mut cancel = false;
        egui::Window::new("Discard changes?")
            .open(&mut open)
            .collapsible(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Discard changes to {}? This cannot be undone.",
                    path.display()
                ));
                ui.horizontal(|ui| {
                    discard = ui.button("Discard").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });

        if discard {
            if let Err(e) = self.git.confirm_discard() {
                self.error = Some(e);
            }
        } else if cancel || !open {
            self.git.cancel_discard();
        }
    }

    /// The branches popup (`docs/features/git-branches-and-blame.md`
    /// §2.2.2): filterable branch list, current branch marked, each
    /// non-current row offering Checkout/Merge Into Current/Delete, plus
    /// a "New Branch..." affordance. Every action's failure surfaces
    /// through the existing one-line `self.error` field, the same
    /// pattern `confirm_discard`/`commit` above already use rather than a
    /// dedicated error field on `BranchesPopupState`.
    fn render_branches_popup(&mut self, ctx: &egui::Context) {
        if !self.git.branches_popup.open {
            return;
        }
        let Some(root) = self.project.as_ref().map(|p| p.root().to_path_buf()) else {
            self.git.close_branches_popup();
            return;
        };

        let mut open = true;
        let mut checkout: Option<String> = None;
        let mut merge: Option<String> = None;
        let mut delete_requested: Option<String> = None;
        let mut delete_decision: Option<bool> = None;
        let mut delete_cancelled = false;
        let mut create = false;

        // Ephemeral, UI-only preference for the not-yet-open "New
        // Branch..." form -- kept in egui's own per-widget temp memory
        // rather than a `BranchesPopupState` field (the doc's own struct
        // listing has none), the same "pure rendering state doesn't need
        // app-state backing" carve-out the skill's Tests section already
        // makes for widget-only concerns.
        let checkout_new_id = egui::Id::new("git_branches_checkout_after_create");
        let mut checkout_new = ctx
            .data(|d| d.get_temp::<bool>(checkout_new_id))
            .unwrap_or(true);

        let rows = self.filtered_branch_rows();
        let mut newly_selected: Option<usize> = None;

        egui::Window::new("Git Branches")
            .open(&mut open)
            .collapsible(false)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.git.branches_popup.filter)
                        .hint_text("Filter branches"),
                );
                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for (index, (name, is_head)) in rows.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let label = if *is_head {
                                    format!("● {name}")
                                } else {
                                    name.clone()
                                };
                                if ui
                                    .selectable_label(
                                        index == self.git.branches_popup.selected,
                                        label,
                                    )
                                    .clicked()
                                {
                                    newly_selected = Some(index);
                                }
                                if *is_head {
                                    return;
                                }
                                if ui.small_button("Checkout").clicked() {
                                    checkout = Some(name.clone());
                                }
                                if ui.small_button("Merge Into Current").clicked() {
                                    merge = Some(name.clone());
                                }
                                if self.git.branches_popup.pending_delete.as_deref()
                                    == Some(name.as_str())
                                {
                                    if ui.small_button("Delete").clicked() {
                                        delete_decision = Some(false);
                                    }
                                    if ui.small_button("Force Delete").clicked() {
                                        delete_decision = Some(true);
                                    }
                                    if ui.small_button("Cancel").clicked() {
                                        delete_cancelled = true;
                                    }
                                } else if ui.small_button("Delete").clicked() {
                                    delete_requested = Some(name.clone());
                                }
                            });
                        }
                    });

                ui.separator();
                if ui
                    .button(if self.git.branches_popup.show_new_branch_input {
                        "Cancel New Branch"
                    } else {
                        "New Branch..."
                    })
                    .clicked()
                {
                    self.git.branches_popup.show_new_branch_input =
                        !self.git.branches_popup.show_new_branch_input;
                    if !self.git.branches_popup.show_new_branch_input {
                        self.git.branches_popup.new_branch_name.clear();
                    }
                }
                if self.git.branches_popup.show_new_branch_input {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.git.branches_popup.new_branch_name)
                            .hint_text("Branch name"),
                    );
                    ui.checkbox(&mut checkout_new, "Checkout after creating");
                    if ui.button("Create").clicked() {
                        create = true;
                    }
                }
            });

        ctx.data_mut(|d| d.insert_temp(checkout_new_id, checkout_new));

        if let Some(index) = newly_selected {
            self.git.branches_popup.selected = index;
        }
        if let Some(name) = checkout {
            match self.git.checkout_branch(&root, &name) {
                Ok(()) => self.error = None,
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(name) = merge {
            match self.git.merge_branch(&root, &name) {
                Ok(()) => {
                    self.error = None;
                    if self.git.merging {
                        self.view_mode = ViewMode::SourceControl;
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(name) = delete_requested {
            self.git.request_delete_branch(&name);
        }
        if let Some(force) = delete_decision {
            if let Err(e) = self.git.confirm_delete_branch(&root, force) {
                self.error = Some(e);
            } else {
                self.error = None;
            }
        }
        if delete_cancelled {
            self.git.cancel_delete_branch();
        }
        if create {
            let name = self.git.branches_popup.new_branch_name.trim().to_string();
            if name.is_empty() {
                self.error = Some("Branch name cannot be empty".to_string());
            } else {
                match self.git.create_branch(&root, &name, checkout_new) {
                    Ok(()) => self.error = None,
                    Err(e) => self.error = Some(e),
                }
            }
        }
        if !open {
            self.git.close_branches_popup();
        }
    }

    /// The worktrees popup (`docs/features/git-worktrees.md` §2.2.2): list
    /// of linked worktrees with Switch/Open-in-New-Window/Remove per row,
    /// plus an Add-worktree form.
    fn render_worktrees_popup(&mut self, ctx: &egui::Context) {
        if !self.git.worktrees_popup.open {
            return;
        }
        if self.project.is_none() {
            self.git.close_worktrees_popup();
            return;
        }

        let mut open = true;
        let mut switch_to: Option<PathBuf> = None;
        let mut open_new_window: Option<PathBuf> = None;
        let mut remove_decision: Option<(String, bool)> = None;
        let mut create = false;

        egui::Window::new("Git Worktrees")
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .show(ui, |ui| {
                        for worktree in &self.git.worktrees_popup.worktrees {
                            ui.horizontal(|ui| {
                                let branch = worktree
                                    .branch
                                    .clone()
                                    .unwrap_or_else(|| "(detached / unavailable)".to_string());
                                let lock_badge = if worktree.is_locked { " 🔒" } else { "" };
                                ui.label(format!(
                                    "{}{} — {} — {}",
                                    worktree.name,
                                    lock_badge,
                                    branch,
                                    worktree.path.display()
                                ));
                                if ui.small_button("Switch here").clicked() {
                                    switch_to = Some(worktree.path.clone());
                                }
                                if ui.small_button("Open in New Window").clicked() {
                                    open_new_window = Some(worktree.path.clone());
                                }
                                if self.git.worktrees_popup.pending_force_remove.as_deref()
                                    == Some(worktree.name.as_str())
                                {
                                    if ui.small_button("Remove Anyway").clicked() {
                                        remove_decision = Some((worktree.name.clone(), true));
                                    }
                                } else if ui.small_button("Remove").clicked() {
                                    remove_decision = Some((worktree.name.clone(), false));
                                }
                            });
                            if self.git.worktrees_popup.pending_force_remove.as_deref()
                                == Some(worktree.name.as_str())
                            {
                                ui.colored_label(
                                    self.theme.tokens().color.danger,
                                    "Has uncommitted changes or is locked — remove anyway?",
                                );
                            }
                        }
                    });

                ui.separator();
                ui.label("Add worktree");
                ui.add(
                    egui::TextEdit::singleline(&mut self.git.worktrees_popup.new_name)
                        .hint_text("Worktree name"),
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.git.worktrees_popup.new_path)
                            .hint_text("Destination path"),
                    );
                    if ui.button("Browse…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.git.worktrees_popup.new_path = dir.display().to_string();
                        }
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.git.worktrees_popup.new_branch)
                        .hint_text("leave empty to create a new branch named the worktree's name"),
                );
                if ui.button("Create").clicked() {
                    create = true;
                }

                if let Some(err) = &self.git.worktrees_popup.error {
                    ui.colored_label(self.theme.tokens().color.danger, err);
                }
            });

        if let Some(path) = switch_to {
            self.open_project(&path, ctx);
            self.git.close_worktrees_popup();
        }
        if let Some(path) = open_new_window {
            self.open_in_new_window(&path);
        }
        if let Some((name, force)) = remove_decision {
            self.git.remove_worktree(&name, force);
        }
        if create {
            self.git.create_worktree();
        }
        if !open {
            self.git.close_worktrees_popup();
        }
    }

    /// The blame gutter's click popup (`docs/features/
    /// git-branches-and-blame.md` §2.2.3): full `CommitDetail` for the
    /// clicked annotation's commit, looked up live each time it's open
    /// (`blame_popup_commit_id`'s own doc comment) rather than cached.
    fn render_blame_popup(&mut self, ctx: &egui::Context) {
        let Some(commit_id) = self.blame_popup_commit_id.clone() else {
            return;
        };

        let mut open = true;
        egui::Window::new("Commit Details")
            .open(&mut open)
            .collapsible(false)
            .default_width(360.0)
            .show(ctx, |ui| match self.git.commit_detail(&commit_id) {
                Ok(detail) => {
                    ui.label(format!("{}  {}", detail.short_id, detail.summary));
                    ui.label(format!("{} <{}>", detail.author, detail.email));
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    ui.label(relative_time(detail.timestamp, now));
                    if !detail.body.is_empty() {
                        ui.separator();
                        ui.label(detail.body);
                    }
                }
                Err(e) => {
                    ui.label(e);
                }
            });

        if !open {
            self.blame_popup_commit_id = None;
        }
    }

    /// `docs/features/language-auto-detect.md` §2.2/§3.4: always shows
    /// `pending_language_suggestions.first()`, if any. No-ops otherwise.
    fn render_language_suggestion_popup(&mut self, ctx: &egui::Context) {
        let Some(suggestion) = self.pending_language_suggestions.first().cloned() else {
            return;
        };

        let mut open = true;
        let mut enable = false;
        let mut dismiss = false;
        egui::Window::new("Language Detected")
            .open(&mut open)
            .collapsible(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} detected. Enable {} language support ({})?",
                    suggestion.marker_file,
                    suggestion.config.name,
                    command_line(&suggestion.config.command, &suggestion.config.args)
                ));
                ui.horizontal(|ui| {
                    enable = ui.button("Enable").clicked();
                    dismiss = ui.button("Dismiss").clicked();
                });
            });

        if enable {
            self.enable_language_suggestion(suggestion);
        } else if dismiss || !open {
            self.dismiss_language_suggestion(suggestion);
        }
    }

    /// `F1` / `Ctrl+Q`'s Quick Documentation popup (`docs/features/
    /// inlay-hints-and-hover.md` §2.2, §3.1, §3.3): fixed title
    /// `"Documentation"`, no rows to click -- just `self.lsp.hover`
    /// rendered as **plain text** via `ui.label`, wrapped in a
    /// `ScrollArea` for long answers. No markdown/HTML parsing anywhere in
    /// this path (§3.3/§4): `ui.label` draws the `String` as literal
    /// glyphs.
    fn render_hover_popup(&mut self, ctx: &egui::Context) {
        if !self.show_hover_popup {
            return;
        }

        let finding = self.lsp.finding_hover;
        let contents = self.lsp.hover.clone();

        let mut open = true;
        egui::Window::new("Documentation")
            .open(&mut open)
            .collapsible(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                if finding {
                    ui.label("Loading…");
                    return;
                }
                match &contents {
                    Some(text) => {
                        egui::ScrollArea::vertical()
                            .id_salt("hover_popup_scroll")
                            .max_height(320.0)
                            .show(ui, |ui| {
                                ui.label(text);
                            });
                    }
                    None => {
                        ui.label("No documentation available.");
                    }
                }
            });

        if !open {
            self.show_hover_popup = false;
        }
    }

    /// `⇧F6`'s popup (`docs/features/rename-refactoring.md` §1, §3.1,
    /// §3.3). Editable text field pre-filled with the symbol's current
    /// name, same "render mutates the field directly" convention the find
    /// bar's query already uses. Enter or the window's own "Rename" button
    /// confirms; Escape or the window's close button cancels -- both via
    /// `confirm_rename`/`cancel_rename`, which own the actual field
    /// mutation.
    fn render_rename_popup(&mut self, ctx: &egui::Context) {
        if self.rename_popup.is_none() {
            return;
        }

        let request_focus = std::mem::take(&mut self.pending_rename_focus);
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Rename")
            .open(&mut open)
            .collapsible(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                let Some(popup) = &mut self.rename_popup else {
                    return;
                };
                let response = ui.text_edit_singleline(&mut popup.input);
                if request_focus {
                    response.request_focus();
                }
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    confirm = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if confirm {
            self.confirm_rename();
        } else if cancel || !open {
            self.cancel_rename();
        }
    }

    /// The escalation path's preview window (§3.5): a summary line, one
    /// row per affected file, and an Apply/Cancel gate. No per-line
    /// diff/snippet view in v1 -- a file-and-count list is enough to
    /// answer "is this the rename I meant."
    fn render_rename_preview(&mut self, ctx: &egui::Context) {
        let Some((edit, new_name)) = self.pending_rename_preview.clone() else {
            return;
        };

        let occurrence_count: usize = edit.edits.iter().map(|f| f.text_edits.len()).sum();
        let file_count = edit.edits.len();

        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new("Rename Preview")
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Rename to `{new_name}`: {occurrence_count} occurrence{} across {file_count} file{}",
                    if occurrence_count == 1 { "" } else { "s" },
                    if file_count == 1 { "" } else { "s" },
                ));
                egui::ScrollArea::vertical()
                    .id_salt("rename_preview_scroll")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for file_edit in &edit.edits {
                            ui.label(format!(
                                "{} — {} occurrence{}",
                                file_edit.path.display(),
                                file_edit.text_edits.len(),
                                if file_edit.text_edits.len() == 1 { "" } else { "s" }
                            ));
                        }
                    });
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if apply {
            let what = format!("Rename to `{new_name}`");
            match self.apply_workspace_edit(edit, &what) {
                Ok(file_count) => {
                    self.error = Some(format!(
                        "{what}: applied to {file_count} file{}",
                        if file_count == 1 { "" } else { "s" }
                    ));
                }
                Err(e) => self.error = Some(e),
            }
            self.pending_rename_preview = None;
        } else if cancel || !open {
            self.pending_rename_preview = None;
        }
    }

    /// `⌘⇧A` ("Find Action", `command-palette.md` §3.2-§3.4). Same
    /// `egui::Window` popup convention as `render_usages_popup` above.
    fn render_command_palette(&mut self, ctx: &egui::Context) {
        if !self.command_palette_open {
            return;
        }

        let mac_style = cfg!(target_os = "macos");
        let request_focus = std::mem::take(&mut self.pending_command_palette_focus);
        let mut query = self.command_palette_query.clone();
        let selected = self.command_palette_selected;
        let commands = self.filtered_commands();

        let mut open = true;
        let mut query_changed = false;
        let mut clicked = None;
        egui::Window::new("Find Action")
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                let response = ui.text_edit_singleline(&mut query);
                if request_focus {
                    response.request_focus();
                }
                query_changed = response.changed();
                ui.separator();

                if commands.is_empty() {
                    ui.label("No matching actions.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("command_palette_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (index, cmd) in commands.iter().enumerate() {
                            let enabled = self.is_command_enabled(cmd.action);
                            // `cmd.id` (not `cmd.title`, which two commands
                            // could in principle share) is this row's
                            // stable egui identity across filter reorders.
                            ui.push_id(cmd.id, |ui| {
                                ui.add_enabled_ui(enabled, |ui| {
                                    ui.horizontal(|ui| {
                                        if ui
                                            .selectable_label(index == selected, cmd.title)
                                            .clicked()
                                        {
                                            clicked = Some(index);
                                        }
                                        if let Some(binding) = cmd.binding {
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.weak(
                                                        binding.for_platform().label(mac_style),
                                                    );
                                                },
                                            );
                                        }
                                    });
                                });
                            });
                        }
                    });
            });

        if query_changed {
            self.command_palette_query = query;
            let len = self.filtered_commands().len();
            self.command_palette_selected =
                self.command_palette_selected.min(len.saturating_sub(1));
        }
        if let Some(index) = clicked {
            self.command_palette_selected = index;
            self.command_palette_confirm(ctx);
        } else if !open {
            self.close_command_palette();
        }
    }

    /// `⇧⇧` / Go to File / Go to Class / Go to Symbol
    /// (`docs/features/search-everywhere.md` §3.4): `render_command_
    /// palette`'s `egui::Window` + text field + `ScrollArea` shape,
    /// extended with a tab strip and per-`SearchEverywhereRow`-variant row
    /// rendering.
    fn render_search_everywhere_popup(&mut self, ctx: &egui::Context) {
        if !self.search_everywhere_open {
            return;
        }

        let mac_style = cfg!(target_os = "macos");
        let request_focus = std::mem::take(&mut self.pending_search_everywhere_focus);
        let mut query = self.search_everywhere_query.clone();
        let selected = self.search_everywhere_selected;
        let current_tab = self.search_everywhere_tab;
        let rows = self.search_everywhere_rows();

        let mut open = true;
        let mut query_changed = false;
        let mut clicked = None;
        let mut switch_to = None;
        egui::Window::new("Search Everywhere")
            .open(&mut open)
            .collapsible(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (SearchEverywhereTab::Files, "Files"),
                        (SearchEverywhereTab::Symbols, "Symbols"),
                        (SearchEverywhereTab::Actions, "Actions"),
                        (SearchEverywhereTab::Text, "Text"),
                    ] {
                        if ui.selectable_label(tab == current_tab, label).clicked() {
                            switch_to = Some(tab);
                        }
                    }
                });
                ui.separator();

                let response = ui.text_edit_singleline(&mut query);
                if request_focus {
                    response.request_focus();
                }
                query_changed = response.changed();
                ui.separator();

                if rows.is_empty() {
                    ui.label("No results.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("search_everywhere_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (index, row) in rows.iter().enumerate() {
                            let response = match row {
                                SearchEverywhereRow::File(m) => {
                                    ui.selectable_label(index == selected, &m.relative)
                                }
                                SearchEverywhereRow::Symbol(s) => ui.selectable_label(
                                    index == selected,
                                    match &s.container_name {
                                        Some(container) => {
                                            format!("{}  ({container})", s.name)
                                        }
                                        None => s.name.clone(),
                                    },
                                ),
                                SearchEverywhereRow::Action(cmd) => {
                                    let enabled = self.is_command_enabled(cmd.action);
                                    let label = match cmd.binding {
                                        Some(binding) => format!(
                                            "{}  ({})  {}",
                                            cmd.title,
                                            cmd.category,
                                            binding.for_platform().label(mac_style)
                                        ),
                                        None => format!("{}  ({})", cmd.title, cmd.category),
                                    };
                                    ui.add_enabled_ui(enabled, |ui| {
                                        ui.selectable_label(index == selected, label)
                                    })
                                    .inner
                                }
                                SearchEverywhereRow::Text(m) => ui.selectable_label(
                                    index == selected,
                                    format!("{}:{}  {}", m.line + 1, m.column + 1, m.line_text),
                                ),
                            };
                            if response.clicked() {
                                clicked = Some(index);
                            }
                        }
                    });

                let truncated = match current_tab {
                    SearchEverywhereTab::Files => self
                        .search_everywhere_files
                        .results
                        .as_ref()
                        .is_some_and(|r| r.truncated),
                    // Deliberately the *unfiltered* symbol count, not
                    // `rows.len()` -- `search_everywhere_class_filter` (Go
                    // to Class) can shrink the visible row count well
                    // below the cap even when the underlying response was
                    // genuinely truncated at the cap.
                    SearchEverywhereTab::Symbols => {
                        let unfiltered_len = if self.search_everywhere_query.is_empty() {
                            self.lsp.document_symbols.len()
                        } else {
                            self.lsp.workspace_symbols.len()
                        };
                        unfiltered_len >= MAX_SYMBOLS_PER_MESSAGE
                    }
                    SearchEverywhereTab::Text => self
                        .search_everywhere_text
                        .results
                        .as_ref()
                        .is_some_and(|r| r.truncated),
                    SearchEverywhereTab::Actions => false,
                };
                if truncated {
                    ui.label("+N more, refine your search");
                }
            });

        if let Some(tab) = switch_to {
            const TABS: [SearchEverywhereTab; 4] = [
                SearchEverywhereTab::Files,
                SearchEverywhereTab::Symbols,
                SearchEverywhereTab::Actions,
                SearchEverywhereTab::Text,
            ];
            let current = TABS.iter().position(|t| *t == current_tab).unwrap_or(0) as isize;
            let target = TABS.iter().position(|t| *t == tab).unwrap_or(0) as isize;
            self.search_everywhere_switch_tab(target - current);
        }
        if query_changed {
            self.search_everywhere_query = query;
        }
        if let Some(index) = clicked {
            self.search_everywhere_selected = index;
            self.search_everywhere_confirm(ctx);
        } else if !open {
            self.close_search_everywhere();
        }
    }

    /// `⌘F12` (`file-structure-and-breadcrumbs.md` §2.3): the active
    /// file's own outline, indented by nesting depth, filterable by
    /// typing -- a flat indented list rather than a real collapsible tree
    /// widget (§3.5's own explicit v1 scope cut).
    fn render_file_structure_popup(&mut self, ctx: &egui::Context) {
        if !self.file_structure_open {
            return;
        }

        const INDENT_PX: f32 = 14.0;

        let request_focus = std::mem::take(&mut self.pending_file_structure_focus);
        let mut query = self.file_structure_query.clone();
        let selected = self.file_structure_selected;
        let symbols = self.active_document_symbols().to_vec();
        let rows = file_structure::visible_rows(&symbols, &query);

        let mut open = true;
        let mut query_changed = false;
        let mut clicked = None;
        egui::Window::new("File Structure")
            .open(&mut open)
            .collapsible(false)
            .default_width(360.0)
            .show(ctx, |ui| {
                let response = ui.text_edit_singleline(&mut query);
                if request_focus {
                    response.request_focus();
                }
                query_changed = response.changed();
                ui.separator();

                if rows.is_empty() {
                    ui.label("No symbols.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("file_structure_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (index, row) in rows.iter().enumerate() {
                            let symbol = &symbols[row.symbol_index];
                            let response = ui.horizontal(|ui| {
                                ui.add_space(row.depth as f32 * INDENT_PX);
                                ui.selectable_label(index == selected, &symbol.name)
                            });
                            if response.inner.clicked() {
                                clicked = Some(index);
                            }
                        }
                    });
            });

        if query_changed {
            self.file_structure_query = query;
            self.file_structure_selected = 0;
        }
        if let Some(index) = clicked {
            self.file_structure_selected = index;
            self.file_structure_confirm();
        } else if !open {
            self.close_file_structure();
        }
    }

    /// `⌘E` (`docs/features/recent-files.md` §2.6) -- same text-input +
    /// list shape as `render_file_structure_popup`, minus the nesting
    /// depth. Rows show the path relative to the project root, matching
    /// `recent_files_rows`'s own filtering display (§2.4's temp-directory
    /// -spoofing rationale).
    fn render_recent_files_popup(&mut self, ctx: &egui::Context) {
        if !self.recent_files_open {
            return;
        }

        let request_focus = std::mem::take(&mut self.pending_recent_files_focus);
        let mut query = self.recent_files_query.clone();
        let selected = self.recent_files_selected;
        let rows = self.recent_files_rows();
        let root = self.project.as_ref().map(|p| p.root().to_path_buf());

        let mut open = true;
        let mut query_changed = false;
        let mut clicked = None;
        egui::Window::new("Recent Files")
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                let response = ui.text_edit_singleline(&mut query);
                if request_focus {
                    response.request_focus();
                }
                query_changed = response.changed();
                ui.separator();

                if rows.is_empty() {
                    ui.label("No recent files.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("recent_files_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (index, path) in rows.iter().enumerate() {
                            let display = match &root {
                                Some(root) => path
                                    .strip_prefix(root)
                                    .unwrap_or(path)
                                    .display()
                                    .to_string(),
                                None => path.display().to_string(),
                            };
                            if ui.selectable_label(index == selected, display).clicked() {
                                clicked = Some(index);
                            }
                        }
                    });
            });

        if query_changed {
            self.recent_files_query = query;
            self.recent_files_selected = 0;
        }
        if let Some(index) = clicked {
            self.recent_files_selected = index;
            self.recent_files_confirm();
        } else if !open {
            self.close_recent_files();
        }
    }

    /// `⌘⇧E` (`docs/features/recent-files.md` §2.6) -- list-only, no text
    /// input, same shape as `render_generate_menu_popup`. Each row is
    /// `path:line  preview`, or just `path` (no `:line`) with
    /// `"(unavailable)"` in place of a preview when `recent_locations_
    /// rows` couldn't read the file/line.
    fn render_recent_locations_popup(&mut self, ctx: &egui::Context) {
        if !self.recent_locations_open {
            return;
        }

        let selected = self.recent_locations_selected;
        let rows = self.recent_locations_rows();
        let root = self.project.as_ref().map(|p| p.root().to_path_buf());

        let mut open = true;
        let mut clicked = None;
        egui::Window::new("Recent Locations")
            .open(&mut open)
            .collapsible(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                if rows.is_empty() {
                    ui.label("No recent locations.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("recent_locations_scroll")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (index, (location, line, preview)) in rows.iter().enumerate() {
                            let display_path = match &root {
                                Some(root) => location
                                    .path
                                    .strip_prefix(root)
                                    .unwrap_or(&location.path)
                                    .display()
                                    .to_string(),
                                None => location.path.display().to_string(),
                            };
                            let label = match (line, preview) {
                                (Some(line), Some(preview)) => {
                                    format!("{display_path}:{line}  {preview}")
                                }
                                _ => format!("{display_path}  (unavailable)"),
                            };
                            if ui.selectable_label(index == selected, label).clicked() {
                                clicked = Some(index);
                            }
                        }
                    });
            });

        if let Some(index) = clicked {
            self.recent_locations_selected = index;
            self.recent_locations_confirm();
        } else if !open {
            self.close_recent_locations();
        }
    }

    /// `⌘L` / `Ctrl+G` (`docs/features/search-everywhere.md` §3.4/§3.6):
    /// same small-dialog shape as `render_confirm_modal`, one text field.
    fn render_go_to_line_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_go_to_line {
            return;
        }

        let request_focus = std::mem::take(&mut self.pending_go_to_line_focus);
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Go to Line")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                let response = ui.text_edit_singleline(&mut self.go_to_line_input);
                if request_focus {
                    response.request_focus();
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if confirm {
            self.confirm_go_to_line();
        } else if cancel {
            self.show_go_to_line = false;
        }
    }

    /// Search panel (`search-in-path-v2.md` §2.2/§3): a query field (Enter
    /// submits) with case/whole-word/regex checkboxes (same `"Aa"`/`"Word"`/
    /// `".*"` convention `render_find_bar` already uses), Include/Exclude
    /// glob fields and a "Respect .gitignore" checkbox, and -- only while
    /// `search_replace_open` -- a replacement field plus "Preview" button
    /// (§2.3/§3.3). Below that: "Searching…" while in flight, the last
    /// `PathSearchError`'s message in place of the results list (§3.5,
    /// same "error replaces content" convention `render_find_bar` uses),
    /// "No results." for an empty completed result, otherwise one heading
    /// per file (click toggles expand/collapse, §3.4) followed by its
    /// matched lines -- `{line+1}:{column+1}  {line_text}` -- only while
    /// expanded, and a trailing truncation note if `results.truncated`.
    fn render_search_panel(&mut self, ui: &mut egui::Ui) {
        let tokens = self.theme.tokens();

        ui.horizontal(|ui| {
            let response = ui.text_edit_singleline(&mut self.search_query);
            let submitted = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Search").clicked() || submitted {
                self.run_search();
            }
            ui.checkbox(&mut self.search_options.search.case_sensitive, "Aa");
            ui.checkbox(&mut self.search_options.search.whole_word, "Word");
            ui.checkbox(&mut self.search_options.search.regex, ".*");
        });
        ui.horizontal(|ui| {
            // Bound directly to the raw text fields, not derived from
            // `search_options.include`/`exclude` every frame -- those are
            // only ever written by `sync_search_glob_options` at submit
            // time (`run_search`/`run_replace_preview`). Re-deriving the
            // text field from the parsed `Vec<String>` every frame would
            // silently eat a trailing comma/separator the user just typed,
            // before the next keystroke lands (rev finding, fix round 1).
            ui.label("Include:");
            ui.text_edit_singleline(&mut self.search_include_text);
            ui.label("Exclude:");
            ui.text_edit_singleline(&mut self.search_exclude_text);
            ui.checkbox(
                &mut self.search_options.respect_gitignore,
                "Respect .gitignore",
            );
        });

        if self.search_replace_open {
            ui.horizontal(|ui| {
                let response = ui.text_edit_singleline(&mut self.search_replacement);
                let submitted =
                    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Preview").clicked() || submitted {
                    self.run_replace_preview();
                }
                if self.search.replacing {
                    ui.label("Computing preview…");
                }
            });
            if let Some(err) = &self.search.replace_error {
                ui.colored_label(tokens.color.danger, err.to_string());
            }
        }

        ui.separator();

        if self.search.searching {
            ui.label("Searching…");
            return;
        }
        if let Some(err) = &self.search.error {
            ui.colored_label(tokens.color.danger, err.to_string());
            return;
        }
        let Some(results) = &self.search.results else {
            return;
        };
        if results.matches.is_empty() {
            ui.label("No results.");
            return;
        }

        let mut clicked = None;
        let mut toggled = None;
        egui::ScrollArea::vertical()
            .id_salt("search_scroll")
            .show(ui, |ui| {
                let mut current_path: Option<&PathBuf> = None;
                let mut current_expanded = true;
                for m in &results.matches {
                    if current_path != Some(&m.path) {
                        current_expanded = !self.search.expanded.contains(&m.path);
                        if ui
                            .selectable_label(false, m.path.display().to_string())
                            .clicked()
                        {
                            toggled = Some(m.path.clone());
                        }
                        current_path = Some(&m.path);
                    }
                    if !current_expanded {
                        continue;
                    }
                    let label = format!("{}:{}  {}", m.line + 1, m.column + 1, m.line_text);
                    if ui.selectable_label(false, label).clicked() {
                        clicked = Some((m.path.clone(), m.byte_offset));
                    }
                }
                if results.truncated {
                    ui.label(format!(
                        "results truncated — showing the first {} matches",
                        ide_core::MAX_SEARCH_RESULTS
                    ));
                }
            });
        if let Some(path) = toggled {
            self.search.toggle_expanded(&path);
        }
        if let Some((path, byte_offset)) = clicked {
            self.open_search_result(&path, byte_offset);
        }
    }

    /// "Languages…" settings window (doc §3): a fixed, non-interactive row
    /// for the built-in Rust config, each `custom_languages` entry with a
    /// "Remove" button, a three-field add-form, and any
    /// `language_settings_error` in red.
    fn render_language_settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_language_settings {
            return;
        }
        let danger = self.theme.tokens().color.danger;
        let mut open = true;
        egui::Window::new("Languages…")
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Rust (.rs) — rust-analyzer (built-in)");
                ui.separator();

                let mut remove = None;
                for (idx, lang) in self.custom_languages.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{} (.{}) — {} [{}]",
                            lang.name,
                            lang.extension,
                            command_line(&lang.command, &lang.args),
                            if self.lsp.is_running_for_extension(&lang.extension) {
                                "running"
                            } else {
                                "stopped"
                            }
                        ));
                        if ui.button("Remove").clicked() {
                            remove = Some(idx);
                        }
                    });
                }
                if let Some(idx) = remove {
                    self.remove_custom_language(idx);
                }

                ui.separator();
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.new_language_name);
                    ui.text_edit_singleline(&mut self.new_language_extension);
                    ui.text_edit_singleline(&mut self.new_language_command);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_language_args)
                            .hint_text("Arguments")
                            .desired_width(160.0),
                    )
                    .on_hover_text("Not for secrets — visible to other local processes (ps).");
                    if ui.button("Add").clicked() {
                        self.add_custom_language();
                    }
                });
                if let Some(err) = &self.language_settings_error {
                    ui.colored_label(danger, err);
                }
                if ui.button("Close").clicked() {
                    self.show_language_settings = false;
                }
            });
        if !open {
            self.show_language_settings = false;
        }
    }

    /// "Keymap…" settings window (`keymap.md` §3.5): scheme picker,
    /// search field, one row per `keymap_filtered_ids()` id (effective
    /// binding label, Edit/Reset), every `keymap::gestures()` entry
    /// (display-only, `keymap.md` §6), Reset All / Export… / Import….
    fn render_keymap_settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_keymap_settings {
            return;
        }
        self.poll_keymap_capture(ctx);
        let danger = self.theme.tokens().color.danger;
        let mac_style = cfg!(target_os = "macos");
        let mut open = true;
        egui::Window::new("Keymap…")
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Scheme:");
                    egui::ComboBox::from_id_salt("keymap_scheme")
                        .selected_text(self.keymap.scheme.label())
                        .show_ui(ui, |ui| {
                            for scheme in crate::keymap::KeymapScheme::ALL {
                                if ui
                                    .selectable_label(self.keymap.scheme == scheme, scheme.label())
                                    .clicked()
                                {
                                    self.keymap.scheme = scheme;
                                }
                            }
                        });
                });
                ui.text_edit_singleline(&mut self.keymap_search);
                ui.separator();

                let mut reset_clicked = None;
                let mut edit_clicked = None;
                for id in self.keymap_filtered_ids() {
                    let cmd = command::commands().iter().find(|c| c.id == id).unwrap();
                    let label = self
                        .keymap
                        .effective_binding(cmd.id)
                        .map(|b| b.for_platform().label(mac_style))
                        .unwrap_or_else(|| "—".to_string());
                    let capturing = self.keymap_capture_target == Some(cmd.id);
                    let pending = self.keymap_capture_pending.clone();
                    ui.push_id(cmd.id, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!("{} ({})", cmd.title, cmd.category));
                            match (capturing, pending) {
                                (true, None) => {
                                    ui.label("Press a shortcut…");
                                }
                                (true, Some((chord, conflicts))) => {
                                    ui.label(chord.label(mac_style));
                                    if !conflicts.is_empty() {
                                        ui.colored_label(
                                            danger,
                                            format!("Conflicts with: {}", conflicts.join(", ")),
                                        );
                                    }
                                    if ui.button("Confirm").clicked() {
                                        self.confirm_keymap_capture();
                                    }
                                    if ui.button("Cancel").clicked() {
                                        self.cancel_keymap_capture();
                                    }
                                }
                                (false, _) => {
                                    ui.label(label);
                                    if ui.button("Edit").clicked() {
                                        edit_clicked = Some(cmd.id);
                                    }
                                    ui.add_enabled_ui(self.keymap.is_customized(cmd.id), |ui| {
                                        if ui.button("Reset").clicked() {
                                            reset_clicked = Some(cmd.id);
                                        }
                                    });
                                }
                            }
                        });
                    });
                }
                if let Some(id) = edit_clicked {
                    self.start_keymap_capture(id);
                }
                if let Some(id) = reset_clicked {
                    self.reset_keymap_binding(id);
                }

                ui.separator();
                for gesture in crate::keymap::gestures() {
                    ui.push_id(gesture.id, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!(
                                "{} ({}) — gesture",
                                gesture.title, gesture.category
                            ));
                            ui.label(gesture.default.label(mac_style));
                        });
                    });
                }

                ui.separator();
                if ui.button("Reset All").clicked() {
                    self.keymap.reset_all();
                }
                if ui.button("Export…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().save_file() {
                        if let Err(e) = self.export_keymap_to(&path) {
                            self.keymap_import_error = Some(e.to_string());
                        }
                    }
                }
                if ui.button("Import…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_file() {
                        match self.import_keymap_from(&path) {
                            Ok(report) => {
                                self.keymap_import_error = None;
                                if !report.skipped_unknown_ids.is_empty() {
                                    self.error = Some(format!(
                                        "Keymap import: unknown commands skipped: {}",
                                        report.skipped_unknown_ids.join(", ")
                                    ));
                                }
                            }
                            Err(e) => self.keymap_import_error = Some(e),
                        }
                    }
                }
                if let Some(err) = &self.keymap_import_error {
                    ui.colored_label(danger, err);
                }
                if ui.button("Close").clicked() {
                    self.show_keymap_settings = false;
                }
            });
        if !open {
            self.show_keymap_settings = false;
        }
    }

    fn render_cargo_output(&mut self, ui: &mut egui::Ui) {
        let running = self.cargo.running.is_some();
        egui::ScrollArea::vertical()
            .id_salt("cargo_output_scroll")
            .stick_to_bottom(running)
            .show(ui, |ui| {
                for line in &self.cargo.output {
                    ui.label(line);
                }
            });
    }

    fn render_claude_panel(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        self.render_claude_tab_strip(ui);
        match self.claude_view {
            ClaudeView::Chat => self.render_claude_chat(ctx, ui),
            ClaudeView::Terminal(_) => self.render_claude_terminal_tab(ctx, ui),
        }
    }

    /// Picks a directory for a new terminal tab the same way `render_welcome`
    /// picks a project directory (`docs/features/claude-terminal.md` §3.1).
    fn select_claude_terminal_tab(&mut self, index: usize) {
        self.claude_terminals.active = Some(index);
        self.claude_view = ClaudeView::Terminal(index);
    }

    fn render_claude_tab_strip(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.claude_view == ClaudeView::Chat, "Chat")
                .clicked()
            {
                self.claude_view = ClaudeView::Chat;
            }

            let mut clicked_index = None;
            let mut close_index = None;
            for (i, tab) in self.claude_terminals.tabs().iter().enumerate() {
                let selected = matches!(self.claude_view, ClaudeView::Terminal(idx) if idx == i);
                let label = if tab.exited {
                    format!("{} (exited)", tab.title)
                } else {
                    tab.title.clone()
                };
                let response = ui
                    .selectable_label(selected, label)
                    .on_hover_text(tab.cwd.display().to_string());
                if response.clicked() {
                    clicked_index = Some(i);
                }
                if ui.small_button("\u{2715}").clicked() {
                    close_index = Some(i);
                }
            }
            if ui.button("+").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let (rows, cols) = Self::claude_terminal_char_grid(ui);
                    self.claude_terminals.open_tab(dir, rows, cols);
                    let new_index = self.claude_terminals.tabs().len() - 1;
                    self.select_claude_terminal_tab(new_index);
                }
            }

            if let Some(i) = clicked_index {
                self.select_claude_terminal_tab(i);
            }
            if let Some(i) = close_index {
                self.claude_terminals.close_tab(i);
                self.claude_view = match self.claude_terminals.active {
                    Some(idx) => ClaudeView::Terminal(idx),
                    None => ClaudeView::Chat,
                };
            }
        });
    }

    fn render_claude_chat(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let danger = self.theme.tokens().color.danger;
        if self.claude.poll() {
            ctx.request_repaint();
        }
        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 40.0)
            .show(ui, |ui| {
                for msg in &self.claude.history {
                    match msg {
                        ClaudeMessage::User(text) => {
                            ui.label(format!("you: {text}"));
                        }
                        ClaudeMessage::Assistant(text) => {
                            ui.label(format!("claude: {text}"));
                        }
                        ClaudeMessage::Error(text) => {
                            ui.colored_label(danger, text);
                        }
                    }
                }
            });
        ui.horizontal(|ui| {
            let response = ui.text_edit_singleline(&mut self.claude.input);
            let submitted = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Send").clicked() || submitted) && !self.claude.input.trim().is_empty() {
                let prompt = std::mem::take(&mut self.claude.input);
                self.claude.submit(prompt);
            }
        });
        if self.claude.is_in_flight() {
            ctx.request_repaint();
        }
    }

    /// Char-cell sizing for the terminal grid, mirroring `editor/mod.rs`'s
    /// monospace font-metrics approach (`docs/features/claude-terminal.md`
    /// §3.3's last paragraph).
    fn claude_terminal_char_grid(ui: &egui::Ui) -> (u16, u16) {
        let font_id = egui::TextStyle::Monospace.resolve(ui.style());
        let (row_height, char_width) = ui.fonts_mut(|f| {
            (
                f.row_height(&font_id),
                f.glyph_width(&font_id, ' ').max(1.0),
            )
        });
        let available = ui.available_size();
        let rows = ((available.y / row_height).floor() as i64).clamp(1, u16::MAX as i64) as u16;
        let cols = ((available.x / char_width).floor() as i64).clamp(1, u16::MAX as i64) as u16;
        (rows, cols)
    }

    /// Renders one `TerminalGrid` row, coalescing adjacent same-styled
    /// cells into one `RichText` span (§3.3's layout-cost note). `cursor_col`
    /// -- `Some` only for the visible row the cursor is actually on -- gets
    /// its cell drawn with fg/bg swapped, a block-cursor indicator that
    /// needs no new color (`TerminalGrid::cursor`'s doc guarantees it's
    /// always in-bounds).
    fn render_claude_terminal_row(
        ui: &mut egui::Ui,
        row: &[crate::claude_terminal::Cell],
        fg_default: egui::Color32,
        bg_default: egui::Color32,
        cursor_col: Option<usize>,
    ) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let mut run = String::new();
            let mut run_style: Option<(egui::Color32, egui::Color32, bool)> = None;
            let flush =
                |ui: &mut egui::Ui,
                 run: &mut String,
                 run_style: &mut Option<(egui::Color32, egui::Color32, bool)>| {
                    if run.is_empty() {
                        return;
                    }
                    let (fg, bg, bold) = run_style.unwrap_or((fg_default, bg_default, false));
                    let mut text = egui::RichText::new(std::mem::take(run)).color(fg);
                    if bg != bg_default {
                        text = text.background_color(bg);
                    }
                    if bold {
                        text = text.strong();
                    }
                    ui.label(text);
                    *run_style = None;
                };
            for (col_idx, cell) in row.iter().enumerate() {
                let mut fg = cell.fg.xterm_rgb().unwrap_or(fg_default);
                let mut bg = cell.bg.xterm_rgb().unwrap_or(bg_default);
                if cursor_col == Some(col_idx) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let style = (fg, bg, cell.bold);
                if run_style.is_some_and(|s| s != style) {
                    flush(ui, &mut run, &mut run_style);
                }
                run_style = Some(style);
                run.push(cell.ch);
            }
            flush(ui, &mut run, &mut run_style);
        });
    }

    fn render_claude_terminal_tab(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let tokens = self.theme.tokens();
        let fg_default = tokens.color.fg_primary;
        let bg_default = tokens.color.bg_base;
        let font_id = egui::TextStyle::Monospace.resolve(ui.style());

        let (rows, cols) = Self::claude_terminal_char_grid(ui);
        let widget_id = {
            let Some(tab) = self.claude_terminals.active_tab_mut() else {
                self.claude_view = ClaudeView::Chat;
                return;
            };
            if tab.grid().rows() != rows as usize || tab.grid().cols() != cols as usize {
                tab.resize(rows, cols);
            }
            crate::claude_terminal::terminal_tab_egui_id(tab.id)
        };

        ui.horizontal(|ui| {
            if ui.button("Copy").clicked() {
                if let Some(tab) = self.claude_terminals.active_tab() {
                    ctx.copy_text(tab.grid().plain_text());
                }
            }
        });

        let outer_rect = ui.available_rect_before_wrap();
        let focus_response = ui.interact(outer_rect, widget_id, egui::Sense::click());
        if focus_response.clicked() {
            focus_response.request_focus();
        }
        let has_focus = focus_response.has_focus();

        egui::ScrollArea::vertical()
            .id_salt(widget_id)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.style_mut().override_font_id = Some(font_id);
                let Some(tab) = self.claude_terminals.active_tab() else {
                    return;
                };
                let grid = tab.grid();
                let cursor = grid.cursor();
                // Scrollback first (oldest-first, per `scrollback_rows`'s
                // own doc), then the visible viewport, matching
                // `plain_text`'s "scrollback + visible viewport" framing
                // (`claude-terminal.md` §3.3) -- without this, the
                // ScrollArea has nothing to scroll into, since the
                // viewport is already sized to exactly fill it.
                for row in grid.scrollback_rows() {
                    Self::render_claude_terminal_row(ui, row, fg_default, bg_default, None);
                }
                for (row_idx, row) in grid.visible_rows().iter().enumerate() {
                    let cursor_col = (row_idx == cursor.0).then_some(cursor.1);
                    Self::render_claude_terminal_row(ui, row, fg_default, bg_default, cursor_col);
                }
            });

        if has_focus {
            let events = ctx.input(|i| i.events.clone());
            for event in &events {
                if let Some(bytes) = crate::claude_terminal::key_event_to_bytes(event) {
                    if let Some(tab) = self.claude_terminals.active_tab_mut() {
                        let _ = tab.write(&bytes);
                    }
                }
            }
        }

        if self
            .claude_terminals
            .active_tab()
            .is_some_and(|t| !t.exited)
        {
            ctx.request_repaint();
        }
    }

    fn render_source_control(&mut self, ui: &mut egui::Ui) {
        let tokens = self.theme.tokens();
        if !self.git.is_repo() {
            ui.label("Not a git repository");
            return;
        }
        self.git.sync_status();

        if !self.git.conflicts.is_empty() {
            ui.heading("Conflicts");
            let mut clicked = None;
            for path in &self.git.conflicts {
                if ui
                    .selectable_label(false, path.display().to_string())
                    .clicked()
                {
                    clicked = Some(path.clone());
                }
            }
            if let Some(path) = clicked {
                self.git.select_conflict(&path);
            }
            ui.separator();
        }

        if let Some(path) = &self.git.binary_conflict {
            ui.colored_label(
                tokens.color.warning,
                format!(
                    "{}: binary conflict — resolve outside the app",
                    path.display()
                ),
            );
            ui.separator();
        }

        let (mut accept_ours, mut accept_theirs, mut mark_resolved) = (false, false, false);
        if let Some(state) = &mut self.git.active_conflict {
            ui.heading(state.path.display().to_string());
            ui.columns(3, |cols| {
                cols[0].label("Base");
                cols[0].label(state.sides.base.as_deref().unwrap_or("(deleted)"));
                cols[1].label("Ours");
                cols[1].label(state.sides.ours.as_deref().unwrap_or("(deleted)"));
                cols[2].label("Theirs");
                cols[2].label(state.sides.theirs.as_deref().unwrap_or("(deleted)"));
            });
            ui.label("Result");
            ui.add(egui::TextEdit::multiline(&mut state.result).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                accept_ours = ui.button("Accept Ours").clicked();
                accept_theirs = ui.button("Accept Theirs").clicked();
                mark_resolved = ui.button("Mark Resolved").clicked();
            });
            ui.label("Mark Resolved stages the file; commit to finish the merge.");
            ui.separator();
        }
        if accept_ours {
            self.git.accept_ours();
        }
        if accept_theirs {
            self.git.accept_theirs();
        }
        if mark_resolved {
            match self.git.mark_resolved() {
                Ok(()) => self.error = None,
                Err(e) => self.error = Some(e),
            }
        }

        self.render_changes_section(ui);
        ui.separator();

        self.render_log_filter_bar(ui);

        ui.heading("Commits");
        let lanes = crate::git_panel::assign_lanes(&self.git.graph);
        let mut clicked_commit = None;
        egui::ScrollArea::vertical()
            .id_salt("commit_graph_scroll")
            .max_height(200.0)
            .show(ui, |ui| {
                for node in &self.git.graph {
                    let lane = lanes.get(&node.id).copied().unwrap_or(0);
                    let selected = self.git.selected_commit.as_deref() == Some(node.id.as_str());
                    ui.horizontal(|ui| {
                        ui.add_space(lane as f32 * 16.0);
                        if ui
                            .selectable_label(
                                selected,
                                format!("{} {}", node.short_id, node.summary),
                            )
                            .clicked()
                        {
                            clicked_commit = Some(node.id.clone());
                        }
                    });
                }
            });
        if let Some(id) = clicked_commit {
            self.git.select_commit(&id);
        }

        ui.separator();
        if self.git.selected_commit.is_none() {
            let active_path = self
                .active_tab
                .and_then(|idx| self.tabs[idx].buffer.path())
                .map(|p| p.to_path_buf());
            match active_path {
                Some(path) => self.git.show_working_tree_diff(&path),
                None => self.git.diff = None,
            }
        }
        match &self.git.diff {
            Some(diffs) if !diffs.is_empty() => Self::render_diff(ui, tokens, diffs),
            Some(_) => {
                ui.label("No changes.");
            }
            None => {
                ui.label("No diff to show — open a tracked file or select a commit.");
            }
        }
    }

    fn render_changes_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Changes");
        ui.add(
            egui::TextEdit::multiline(&mut self.git.commit_message)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("Commit message"),
        );
        ui.checkbox(&mut self.git.amend, "Amend");
        let commit_enabled = !self.git.commit_message.trim().is_empty() || self.git.amend;
        if ui
            .add_enabled(commit_enabled, egui::Button::new("Commit"))
            .clicked()
        {
            match self.git.commit() {
                Ok(()) => self.error = None,
                Err(e) => self.error = Some(e),
            }
        }

        let (mut to_stage, mut to_unstage, mut to_discard) = (None, None, None);
        egui::ScrollArea::vertical()
            .id_salt("staged_changes_scroll")
            .max_height(120.0)
            .show(ui, |ui| {
                ui.label("Staged Changes");
                for entry in &self.git.status.staged {
                    Self::render_change_row(ui, entry, false, &mut to_unstage, &mut to_discard);
                }
            });
        egui::ScrollArea::vertical()
            .id_salt("unstaged_changes_scroll")
            .max_height(120.0)
            .show(ui, |ui| {
                ui.label("Changes");
                for entry in &self.git.status.unstaged {
                    Self::render_change_row(ui, entry, true, &mut to_stage, &mut to_discard);
                }
            });

        if let Some(path) = to_stage {
            if let Err(e) = self.git.stage(&path) {
                self.error = Some(e);
            }
        }
        if let Some(path) = to_unstage {
            if let Err(e) = self.git.unstage(&path) {
                self.error = Some(e);
            }
        }
        if let Some(path) = to_discard {
            self.git.request_discard(&path);
        }
    }

    /// The Log tab's filter bar (`git-log-viewer.md` §2.2/§3.2), or --
    /// while `log_filter.viewing_file_history` is set -- a "← Back to Log"
    /// affordance in its place (§3.3: the two are mutually exclusive
    /// modes of the same graph list, never shown active at once).
    fn render_log_filter_bar(&mut self, ui: &mut egui::Ui) {
        let tokens = self.theme.tokens();
        if let Some(path) = self.git.log_filter.viewing_file_history.clone() {
            ui.horizontal(|ui| {
                ui.label(format!("History of {}", path.display()));
                if ui.button("← Back to Log").clicked() {
                    self.git.back_to_log();
                }
            });
            ui.separator();
            return;
        }

        let mut submitted = false;
        let mut clear_clicked = false;
        // `default_open(true)` -- this is the primary way to narrow a
        // large log, not a rarely-used option; collapsed by default would
        // hide the whole feature behind an undiscovered extra click.
        egui::CollapsingHeader::new("Filter")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("log_filter_grid")
                    .num_columns(2)
                    .show(ui, |ui| {
                        for (label, field) in [
                            ("Branch", &mut self.git.log_filter.branch),
                            ("Author", &mut self.git.log_filter.author),
                            ("Path", &mut self.git.log_filter.path),
                            ("Since (YYYY-MM-DD)", &mut self.git.log_filter.since),
                            ("Until (YYYY-MM-DD)", &mut self.git.log_filter.until),
                            ("Message contains", &mut self.git.log_filter.query),
                        ] {
                            ui.label(label);
                            let response = ui.text_edit_singleline(field);
                            submitted |= response.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            ui.end_row();
                        }
                    });
                ui.horizontal(|ui| {
                    submitted |= ui.button("Apply").clicked();
                    clear_clicked = ui.button("Clear Filter").clicked();
                });
                if let Some(error) = &self.git.log_filter.error {
                    ui.colored_label(tokens.color.warning, error);
                }
            });
        if clear_clicked {
            self.git.clear_log_filter();
        } else if submitted {
            self.git.apply_log_filter();
        }
        ui.separator();
    }

    fn render_change_row(
        ui: &mut egui::Ui,
        entry: &StatusEntry,
        unstaged: bool,
        toggle_target: &mut Option<PathBuf>,
        discard_target: &mut Option<PathBuf>,
    ) {
        ui.horizontal(|ui| {
            let badge = match entry.kind {
                ChangeKind::Added => "A",
                ChangeKind::Modified => "M",
                ChangeKind::Deleted => "D",
                ChangeKind::Untracked => "U",
                ChangeKind::Conflicted => "C",
            };
            ui.label(badge);
            ui.label(entry.path.display().to_string());
            let toggle_label = if unstaged { "Stage" } else { "Unstage" };
            if ui.button(toggle_label).clicked() {
                *toggle_target = Some(entry.path.clone());
            }
            if unstaged && ui.button("Discard").clicked() {
                *discard_target = Some(entry.path.clone());
            }
        });
    }

    fn render_diff(ui: &mut egui::Ui, tokens: &Tokens, diffs: &[FileDiff]) {
        for (i, file) in diffs.iter().enumerate() {
            let label = file
                .new_path
                .as_ref()
                .or(file.old_path.as_ref())
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".to_string());
            ui.heading(label);
            egui::ScrollArea::both()
                .id_salt(("diff_scroll", i))
                .max_height(300.0)
                .show(ui, |ui| {
                    egui::Grid::new(("diff_grid", i))
                        .num_columns(4)
                        .show(ui, |ui| {
                            for hunk in &file.hunks {
                                let mut old_line = hunk.old_start;
                                let mut new_line = hunk.new_start;
                                for line in &hunk.lines {
                                    match line {
                                        DiffLine::Context(text) => {
                                            diff_gutter_cell(
                                                ui,
                                                tokens,
                                                None,
                                                &old_line.to_string(),
                                            );
                                            diff_cell(ui, tokens, None, None, |ui| {
                                                ui.label(text);
                                            });
                                            diff_gutter_cell(
                                                ui,
                                                tokens,
                                                None,
                                                &new_line.to_string(),
                                            );
                                            diff_cell(ui, tokens, None, None, |ui| {
                                                ui.label(text);
                                            });
                                            old_line += 1;
                                            new_line += 1;
                                        }
                                        DiffLine::Removed(text, spans) => {
                                            let tint =
                                                tokens.color.diff_removed_fg.gamma_multiply(0.5);
                                            diff_gutter_cell(
                                                ui,
                                                tokens,
                                                Some(tint),
                                                &old_line.to_string(),
                                            );
                                            diff_cell(
                                                ui,
                                                tokens,
                                                Some(tint),
                                                Some(tokens.color.diff_removed_fg),
                                                |ui| {
                                                    diff_line_text(
                                                        ui,
                                                        tokens,
                                                        tokens.color.diff_removed_fg,
                                                        text,
                                                        spans,
                                                    );
                                                },
                                            );
                                            diff_gutter_cell(ui, tokens, None, "");
                                            diff_cell(ui, tokens, None, None, |ui| {
                                                ui.label("");
                                            });
                                            old_line += 1;
                                        }
                                        DiffLine::Added(text, spans) => {
                                            let tint =
                                                tokens.color.diff_added_fg.gamma_multiply(0.5);
                                            diff_gutter_cell(ui, tokens, None, "");
                                            diff_cell(ui, tokens, None, None, |ui| {
                                                ui.label("");
                                            });
                                            diff_gutter_cell(
                                                ui,
                                                tokens,
                                                Some(tint),
                                                &new_line.to_string(),
                                            );
                                            diff_cell(
                                                ui,
                                                tokens,
                                                Some(tint),
                                                Some(tokens.color.diff_added_fg),
                                                |ui| {
                                                    diff_line_text(
                                                        ui,
                                                        tokens,
                                                        tokens.color.diff_added_fg,
                                                        text,
                                                        spans,
                                                    );
                                                },
                                            );
                                            new_line += 1;
                                        }
                                    }
                                    ui.end_row();
                                }
                            }
                        });
                });
            if file.truncated {
                ui.colored_label(
                    tokens.color.warning,
                    "diff truncated — file too large to show in full",
                );
            }
            ui.separator();
        }
    }

    /// `docs/features/git-worktrees.md` §2.2.3: shown only when the
    /// open-projects registry had 2+ entries at startup and there was no
    /// explicit `initial_project` -- a blocking modal (no close button; the
    /// user must pick one of the three options, each of which clears
    /// `startup_restore_prompt` via `resolve_startup_restore`).
    fn render_startup_restore_prompt(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &self.startup_restore_prompt else {
            return;
        };
        let candidates = prompt.candidates.clone();
        let mut choice = None;
        egui::Window::new(format!("{} projects were open last time", candidates.len()))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                if ui.button("Restore All").clicked() {
                    choice = Some(RestoreChoice::All);
                }
                ui.separator();
                ui.label("Or open just one:");
                for (i, path) in candidates.iter().enumerate() {
                    if ui.button(path.display().to_string()).clicked() {
                        choice = Some(RestoreChoice::One(i));
                    }
                }
                ui.separator();
                if ui.button("Open None").clicked() {
                    choice = Some(RestoreChoice::None);
                }
            });
        if let Some(choice) = choice {
            self.resolve_startup_restore(choice, ctx);
        }
    }

    fn render_confirm_modal(&mut self, ctx: &egui::Context) {
        if self.pending_confirm.is_none() {
            return;
        }
        egui::Window::new("Discard unsaved changes?")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("This tab has unsaved changes. Discard them?");
                ui.horizontal(|ui| {
                    if ui.button("Discard").clicked() {
                        self.confirm_discard();
                    }
                    if ui.button("Cancel").clicked() {
                        self.cancel_confirm();
                    }
                });
            });
    }

    /// The main toolbar (`intellij-shell.md` §2.1): left = workspace name +
    /// back/forward, center = context line (click opens the command
    /// palette, an explicit interim stand-in for §4.7's Search Everywhere
    /// entry point until C2 exists), right = branch label. Problems count
    /// and Smart Mode moved to `render_status_bar` -- keeping them here too
    /// duplicated both indicators across two bars. `ui.columns(3, ..)` is
    /// the same "equal thirds" idiom `render_source_control`'s Base/Ours/
    /// Theirs grid already uses. No framed buttons for the indicators
    /// (§4.1) -- a clickable `egui::Label`, not `ui.button`.
    fn render_top_bar(&mut self, ui: &mut egui::Ui) {
        let tokens = self.theme.tokens();
        let space_xl = tokens.space.xl;
        let border = tokens.color.border;
        let frame = egui::Frame::side_top_panel(ui.style())
            .inner_margin(egui::Margin::symmetric(8, space_xl as i8));
        egui::Panel::top("top_bar").frame(frame).show(ui, |ui| {
            ui.painter().hline(
                ui.max_rect().x_range(),
                ui.max_rect().bottom(),
                egui::Stroke::new(1.0, border),
            );
            ui.columns(3, |columns| {
                columns[0].horizontal(|ui| {
                    if let Some(project) = &self.project {
                        let name = project
                            .root()
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        ui.label(name);
                    }
                    if ui
                        .add_enabled(self.nav.can_go_back(), egui::Button::new("\u{2190}"))
                        .clicked()
                    {
                        self.nav_back();
                    }
                    if ui
                        .add_enabled(self.nav.can_go_forward(), egui::Button::new("\u{2192}"))
                        .clicked()
                    {
                        self.nav_forward();
                    }
                });

                columns[1].with_layout(
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| {
                        let context_line = self
                            .active_tab
                            .and_then(|idx| self.tabs[idx].buffer.path())
                            .map(|p| self.display_path(p))
                            .unwrap_or_else(|| "No file open".to_string());
                        let pill = egui::Frame::new()
                            .corner_radius(egui::CornerRadius::same(tokens.radius.lg))
                            .fill(tokens.color.bg_hover)
                            .inner_margin(egui::Margin::symmetric(
                                tokens.space.md as i8,
                                tokens.space.sm as i8,
                            ));
                        let response = pill
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(12.0, 12.0),
                                        egui::Sense::hover(),
                                    );
                                    Self::paint_loupe(
                                        ui.painter(),
                                        rect.center(),
                                        4.5,
                                        tokens.color.fg_secondary,
                                    );
                                    ui.label(context_line);
                                });
                            })
                            .response
                            .interact(egui::Sense::click());
                        if response.clicked() {
                            self.open_command_palette();
                        }
                    },
                );

                columns[2].with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.git.is_repo() {
                        if let Some(branch) = self.git.current_branch.clone() {
                            let label_response =
                                ui.add(egui::Label::new(branch).sense(egui::Sense::click()));
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                            Self::paint_branch(
                                ui.painter(),
                                rect.center(),
                                4.5,
                                tokens.color.fg_secondary,
                            );
                            if label_response.clicked() {
                                if let Some(root) =
                                    self.project.as_ref().map(|p| p.root().to_path_buf())
                                {
                                    self.git.open_branches_popup(&root);
                                }
                            }
                        }
                    }
                });
            });
        });
    }

    /// Left edge (§3.7): a thin, always-visible rail before the actual
    /// Project tool window (the slim tree) -- the tree itself is skipped
    /// entirely, not just hidden, when `show_project_tool_window` is
    /// `false` (the same `if !self.show_x { return; }` shape
    /// `render_language_settings_window` already uses for a window,
    /// applied here to a permanent panel).
    fn render_project_rail(&mut self, ui: &mut egui::Ui) {
        let open = self.is_tool_window_open(ToolWindow::Project);
        let showing_run = self.is_tool_window_open(ToolWindow::Bottom)
            && self.bottom_view == BottomView::CargoOutput;
        let showing_problems = self.is_tool_window_open(ToolWindow::Bottom)
            && self.bottom_view == BottomView::Problems;
        let tokens = self.theme.tokens();
        let rail_frame = egui::Frame::side_top_panel(ui.style()).inner_margin(
            egui::Margin::symmetric(tokens.space.xs as i8, tokens.space.sm as i8),
        );
        egui::Panel::left("project_rail")
            .frame(rail_frame)
            .exact_size(16.0 + tokens.space.xs * 2.0)
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    let r =
                        Self::render_stripe_icon(ui, tokens, StripeIcon::Folder, "Project", open);
                    Self::render_vertical_stripe_label(ui, tokens, r.rect, "Project", true, true);
                    if r.clicked() {
                        self.toggle_tool_window(ToolWindow::Project);
                    }
                });
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    let r = Self::render_stripe_icon(
                        ui,
                        tokens,
                        StripeIcon::Warning,
                        "Problems",
                        showing_problems,
                    );
                    Self::render_vertical_stripe_label(ui, tokens, r.rect, "Problems", true, false);
                    if r.clicked() {
                        self.toggle_bottom_tool_window(BottomView::Problems);
                    }
                    let r = Self::render_stripe_icon(
                        ui,
                        tokens,
                        StripeIcon::Output,
                        "Run",
                        showing_run,
                    );
                    Self::render_vertical_stripe_label(ui, tokens, r.rect, "Run", true, false);
                    if r.clicked() {
                        self.toggle_bottom_tool_window(BottomView::CargoOutput);
                    }
                });
            });
        if !open {
            return;
        }
        let mut clicked_path = None;
        if let Some(tree) = self.tree.clone() {
            egui::Panel::left("tree_panel").show(ui, |ui| {
                let tokens = self.theme.tokens();
                Self::render_tool_window_header(ui, tokens, "Project");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for child in &tree.children {
                        Self::render_tree_entry(child, 0, &mut clicked_path, ui, tokens);
                    }
                });
            });
        } else if self.project.is_some() && self.tree_scan.is_scanning() {
            egui::Panel::left("tree_panel").show(ui, |ui| {
                let tokens = self.theme.tokens();
                Self::render_tool_window_header(ui, tokens, "Project");
                ui.label(egui::RichText::new("Scanning project…").color(tokens.color.fg_secondary));
            });
        }
        if let Some(path) = clicked_path {
            self.open_file(&path);
            self.push_nav_location();
        }
    }

    /// Right edge (§3.7): rail before the Claude tool window.
    fn render_claude_rail(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let open = self.is_tool_window_open(ToolWindow::Claude);
        let showing_find =
            self.is_tool_window_open(ToolWindow::Bottom) && self.bottom_view == BottomView::Search;
        let showing_vcs = self.view_mode == ViewMode::SourceControl;
        let tokens = self.theme.tokens();
        let rail_frame = egui::Frame::side_top_panel(ui.style()).inner_margin(
            egui::Margin::symmetric(tokens.space.xs as i8, tokens.space.sm as i8),
        );
        egui::Panel::right("claude_rail")
            .frame(rail_frame)
            .exact_size(16.0 + tokens.space.xs * 2.0)
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    let r = Self::render_stripe_icon(ui, tokens, StripeIcon::Chat, "Claude", open);
                    Self::render_vertical_stripe_label(ui, tokens, r.rect, "Claude", false, true);
                    if r.clicked() {
                        self.toggle_tool_window(ToolWindow::Claude);
                    }
                });
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    let r = Self::render_stripe_icon(
                        ui,
                        tokens,
                        StripeIcon::Branch,
                        "Version Control",
                        showing_vcs,
                    );
                    Self::render_vertical_stripe_label(
                        ui,
                        tokens,
                        r.rect,
                        "Version Control",
                        false,
                        false,
                    );
                    if r.clicked() {
                        self.toggle_view_mode();
                    }
                    let r = Self::render_stripe_icon(
                        ui,
                        tokens,
                        StripeIcon::Loupe,
                        "Find",
                        showing_find,
                    );
                    Self::render_vertical_stripe_label(ui, tokens, r.rect, "Find", false, false);
                    if r.clicked() {
                        self.toggle_bottom_tool_window(BottomView::Search);
                    }
                });
            });
        if !open {
            return;
        }
        egui::Panel::right("claude_panel").show(ui, |ui| {
            let tokens = self.theme.tokens();
            Self::render_tool_window_header(ui, tokens, "Claude");
            self.render_claude_panel(ctx, ui);
        });
    }

    /// Bottom tool window body (§2.2): tabs + content only. The
    /// activation icons that used to live in a horizontal `bottom_rail`
    /// strip above this panel now live at the bottom of the left/right
    /// stripes (`render_project_rail`/`render_claude_rail`) instead, per
    /// real IntelliJ's side-anchored convention.
    fn render_bottom_panel(&mut self, ui: &mut egui::Ui) {
        let open = self.is_tool_window_open(ToolWindow::Bottom);
        if !open {
            return;
        }
        let tokens = self.theme.tokens();
        egui::Panel::bottom("bottom_panel")
            .resizable(true)
            .default_size(160.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if Self::render_boxed_tab(
                        ui,
                        tokens,
                        self.bottom_view == BottomView::Problems,
                        "Problems",
                    )
                    .clicked()
                    {
                        self.bottom_view = BottomView::Problems;
                    }
                    if Self::render_boxed_tab(
                        ui,
                        tokens,
                        self.bottom_view == BottomView::CargoOutput,
                        "Cargo Output",
                    )
                    .clicked()
                    {
                        self.bottom_view = BottomView::CargoOutput;
                    }
                    if Self::render_boxed_tab(
                        ui,
                        tokens,
                        self.bottom_view == BottomView::Usages,
                        "Usages",
                    )
                    .clicked()
                    {
                        self.bottom_view = BottomView::Usages;
                    }
                    if Self::render_boxed_tab(
                        ui,
                        tokens,
                        self.bottom_view == BottomView::Search,
                        "Search",
                    )
                    .clicked()
                    {
                        self.bottom_view = BottomView::Search;
                    }
                });
                ui.separator();
                match self.bottom_view {
                    BottomView::Problems => self.render_problems_panel(ui),
                    BottomView::CargoOutput => self.render_cargo_output(ui),
                    BottomView::Usages => self.render_usages_panel(ui),
                    BottomView::Search => self.render_search_panel(ui),
                }
            });
    }

    /// New bottom-of-window status bar (§3.6), rendered before
    /// `render_project_rail`/`render_claude_rail` claim the side strips
    /// (§2.1) so it spans the window's full width rather than being
    /// squeezed to the space between them. Blank/hidden fields (no active
    /// tab, no project) render nothing rather than a placeholder -- same
    /// convention the rest of the shell already follows.
    fn render_status_bar(&mut self, ui: &mut egui::Ui) {
        let space_xl = self.theme.tokens().space.xl;
        let border = self.theme.tokens().color.border;
        let frame = egui::Frame::side_top_panel(ui.style())
            .inner_margin(egui::Margin::symmetric(8, space_xl as i8));
        egui::Panel::bottom("status_bar")
            .frame(frame)
            .show(ui, |ui| {
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    ui.max_rect().top(),
                    egui::Stroke::new(1.0, border),
                );
                ui.horizontal(|ui| {
                    if let Some(idx) = self.active_tab {
                        let offset = self.active_cursor_offset.unwrap_or(0);
                        let (line, column) =
                            cursor_line_column(self.tabs[idx].buffer.text_buffer(), offset);
                        ui.label(format!("{}:{}", line + 1, column + 1));

                        if self.tabs[idx].editor.column_mode() {
                            let tokens = self.theme.tokens();
                            ui.label(
                                egui::RichText::new("COLUMN")
                                    .small()
                                    .color(tokens.color.accent),
                            );
                        }

                        ui.separator();
                        ui.label(super::charset_label(self.tabs[idx].config.charset));
                        ui.separator();
                        ui.label(super::end_of_line_label(self.tabs[idx].config.end_of_line));
                        ui.separator();
                        ui.label(super::indent_label(self.tabs[idx].editor.indent()));
                    }

                    let (errors, warnings) = self.problems_count();
                    if ui
                        .add(
                            egui::Label::new(format!("Errors: {errors}  Warnings: {warnings}"))
                                .sense(egui::Sense::click()),
                        )
                        .clicked()
                    {
                        self.toggle_bottom_tool_window(BottomView::Problems);
                    }

                    if self.git.is_repo() {
                        if let Some(branch) = self.git.current_branch.clone() {
                            ui.separator();
                            let label_response =
                                ui.add(egui::Label::new(branch).sense(egui::Sense::click()));
                            if label_response.clicked() {
                                if let Some(root) =
                                    self.project.as_ref().map(|p| p.root().to_path_buf())
                                {
                                    self.git.open_branches_popup(&root);
                                }
                            }
                        }
                    }

                    ui.separator();
                    let state = self.smart_mode_state();
                    let tokens = self.theme.tokens();
                    let (label, color) = match state {
                        SmartModeState::Off => ("Smart Mode: Off", tokens.color.fg_muted),
                        SmartModeState::On => ("Smart Mode: On", tokens.color.success),
                        SmartModeState::Error => ("Smart Mode: Error", tokens.color.danger),
                    };
                    if ui
                        .add(
                            egui::Label::new(egui::RichText::new(label).color(color))
                                .sense(egui::Sense::click()),
                        )
                        .clicked()
                    {
                        self.toggle_smart_mode();
                    }
                });
            });
    }
}

impl eframe::App for IdeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if ctx.input(|i| i.viewport().close_requested())
            && self.pending_confirm.is_none()
            && !self.request_quit()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.should_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        self.handle_shortcuts(&ctx);

        if self.lsp.poll() {
            ctx.request_repaint();
        }
        if self.poll_menu_events(&ctx) {
            ctx.request_repaint();
        }
        self.sync_document_highlights();
        self.handle_goto_response();
        self.handle_interface_check_response();
        self.sync_code_actions();
        self.sync_git_gutter();
        self.handle_workspace_edit_ready();
        self.handle_format_ready();
        self.handle_prepare_rename_ready();
        self.handle_rename_ready();
        self.sync_tab_diagnostics();
        self.sync_search_everywhere();
        if self.cargo.poll() {
            ctx.request_repaint();
        }
        // Drained every frame regardless of whether the Claude rail is
        // open (`docs/security-findings/rust-ui-dev-claude-terminal-
        // 2026-08-25.md` finding 3): a terminal tab's PTY keeps producing
        // output while the panel is collapsed, and the reader thread's
        // channel has no cap of its own -- gating this on panel
        // visibility let it grow unboundedly for as long as the panel
        // stayed closed.
        if self.claude_terminals.poll() {
            ctx.request_repaint();
        }
        if self.search.poll() {
            ctx.request_repaint();
        }
        if self.search.poll_replace() {
            ctx.request_repaint();
            if let Some(result) = self.search.replace_preview.take() {
                self.show_replace_in_path_preview(result.edit);
            }
        }
        if self.search_everywhere_files.poll() {
            ctx.request_repaint();
        }
        if self.search_everywhere_text.poll() {
            ctx.request_repaint();
        }
        if self.poll_watcher() {
            ctx.request_repaint();
        }
        match self.clone.poll() {
            Some(crate::clone_panel::ClonePollResult::Succeeded(path)) => {
                self.open_project(&path, &ctx);
                ctx.request_repaint();
            }
            Some(_) => ctx.request_repaint(),
            None => {}
        }
        if self.poll_tree_scan() {
            ctx.request_repaint();
        }

        // Zen Mode (§3.8): hides the top bar, all three rails/tool windows,
        // and the status bar entirely -- a display-only override, the
        // underlying `show_*_tool_window` flags are left untouched so
        // turning it back off restores exactly what was visible before.
        if !self.zen_mode {
            self.render_top_bar(ui);
        }
        self.render_language_settings_window(&ctx);
        self.render_keymap_settings_window(&ctx);

        if self.project.is_none() {
            egui::CentralPanel::default().show(ui, |ui| self.render_welcome(ui));
            self.render_confirm_modal(&ctx);
            return;
        }

        if !self.zen_mode {
            self.render_status_bar(ui);
            self.render_project_rail(ui);
            self.render_claude_rail(&ctx, ui);
            self.render_bottom_panel(ui);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(err) = &self.error {
                ui.colored_label(self.theme.tokens().color.danger, err);
            }
            match self.view_mode {
                ViewMode::Editor => self.render_tabs_and_editor(ui),
                ViewMode::SourceControl => {
                    egui::ScrollArea::vertical().show(ui, |ui| self.render_source_control(ui));
                }
            }
        });

        self.render_usages_popup(&ctx);
        self.render_goto_popup(&ctx);
        self.render_hover_popup(&ctx);
        self.render_code_actions_popup(&ctx);
        self.render_refactor_menu_popup(&ctx);
        self.render_generate_menu_popup(&ctx);
        self.render_refactor_preview(&ctx);
        self.render_replace_in_path_preview(&ctx);
        self.render_git_gutter_popup(&ctx);
        self.render_discard_confirm_popup(&ctx);
        self.render_branches_popup(&ctx);
        self.render_worktrees_popup(&ctx);
        self.render_blame_popup(&ctx);
        self.render_language_suggestion_popup(&ctx);
        self.render_rename_popup(&ctx);
        self.render_rename_preview(&ctx);
        self.render_command_palette(&ctx);
        self.render_search_everywhere_popup(&ctx);
        self.render_file_structure_popup(&ctx);
        self.render_recent_files_popup(&ctx);
        self.render_recent_locations_popup(&ctx);
        self.render_go_to_line_dialog(&ctx);
        self.render_confirm_modal(&ctx);
        self.render_startup_restore_prompt(&ctx);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        // Theme/custom_languages/keymap/format_on_save moved to
        // `.ide/preferences.json` (`project-settings.md` §4) -- flushed on
        // every project switch already; flush again here too, since app
        // exit is also a point where the in-memory state could otherwise
        // be lost if it changed since the last switch. The open-projects
        // registry (`OPEN_PROJECTS_STORAGE_KEY`) is its own file, kept up
        // to date by `register_open_project`/`deregister_open_project` on
        // every load/switch/exit rather than here -- nothing left for this
        // method to write through `eframe::Storage`.
        if let Some(project) = &self.project {
            let root = project.root().to_path_buf();
            self.flush_project_settings(&root);
        }
    }

    fn on_exit(&mut self) {
        // Best-effort (`docs/features/git-worktrees.md` §2.2.3/§3): a
        // crash or force-quit skips this, and the registry self-corrects
        // on the next read rather than needing it to be reliable.
        if let Some(project) = &self.project {
            self.deregister_open_project(project.root());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;
    use ide_core::{DiffHunk, DiffSpan};

    #[test]
    fn command_line_with_no_args_is_just_the_command() {
        assert_eq!(command_line("gopls", &[]), "gopls");
    }

    #[test]
    fn command_line_with_args_joins_them_with_spaces() {
        assert_eq!(
            command_line(
                "typescript-language-server",
                &["--stdio".to_string(), "--log-level=verbose".to_string()]
            ),
            "typescript-language-server --stdio --log-level=verbose"
        );
    }

    #[test]
    fn split_line_by_spans_matches_the_doc_worked_example() {
        // docs/features/diff-viewer-enhancements.md §5.2.
        let segments = split_line_by_spans(
            "let x = compute_result();",
            &[DiffSpan { start: 16, end: 22 }],
        );
        assert_eq!(
            segments,
            vec![
                (false, "let x = compute_"),
                (true, "result"),
                (false, "();"),
            ]
        );
    }

    #[test]
    fn split_line_by_spans_with_no_spans_is_one_plain_segment() {
        let segments = split_line_by_spans("unchanged", &[]);
        assert_eq!(segments, vec![(false, "unchanged")]);
    }

    #[test]
    fn split_line_by_spans_span_at_the_very_start_has_no_leading_segment() {
        let segments = split_line_by_spans("abcdef", &[DiffSpan { start: 0, end: 3 }]);
        assert_eq!(segments, vec![(true, "abc"), (false, "def")]);
    }

    #[test]
    fn split_line_by_spans_span_covering_the_whole_string_is_one_segment() {
        let segments = split_line_by_spans("abc", &[DiffSpan { start: 0, end: 3 }]);
        assert_eq!(segments, vec![(true, "abc")]);
    }

    #[test]
    fn split_line_by_spans_empty_text_and_no_spans_is_empty() {
        assert_eq!(split_line_by_spans("", &[]), Vec::<(bool, &str)>::new());
    }

    fn diff_harness(diffs: Vec<FileDiff>) -> Harness<'static, Vec<FileDiff>> {
        let mut harness = Harness::new_ui_state(
            |ui, diffs: &mut Vec<FileDiff>| {
                let tokens = theme::Theme::Dark.tokens();
                IdeApp::render_diff(ui, tokens, diffs);
            },
            diffs,
        );
        harness.run();
        harness
    }

    fn sample_diff() -> FileDiff {
        // Same text as docs/features/diff-viewer-enhancements.md §5.1's
        // worked example, so the expected spans are already known-correct.
        FileDiff {
            old_path: Some(PathBuf::from("f.rs")),
            new_path: Some(PathBuf::from("f.rs")),
            truncated: false,
            hunks: vec![DiffHunk {
                old_start: 10,
                new_start: 50,
                lines: vec![
                    DiffLine::Context("shared context line".to_string()),
                    DiffLine::Removed(
                        "let x = compute_value();".to_string(),
                        vec![DiffSpan { start: 16, end: 21 }],
                    ),
                    DiffLine::Added(
                        "let x = compute_result();".to_string(),
                        vec![DiffSpan { start: 16, end: 22 }],
                    ),
                ],
            }],
        }
    }

    #[test]
    fn gutter_numbers_follow_the_per_line_counter_rules() {
        // §3.2: Context shows and increments both counters; Removed shows/
        // increments only the old counter; Added shows/increments only the
        // new counter. old_start=10/new_start=50 keep every rendered number
        // unique across the whole hunk, so a single `get_by_label` per
        // number is unambiguous.
        let harness = diff_harness(vec![sample_diff()]);
        assert!(harness.query_by_label("10").is_some(), "context old line");
        assert!(harness.query_by_label("50").is_some(), "context new line");
        assert!(harness.query_by_label("11").is_some(), "removed old line");
        assert!(harness.query_by_label("51").is_some(), "added new line");
        // Never incremented/rendered: the old side stopped at 11, the new
        // side at 51.
        assert!(harness.query_by_label("12").is_none());
        assert!(harness.query_by_label("52").is_none());
    }

    #[test]
    fn intraline_highlighted_lines_render_as_split_text_runs() {
        // §3.4: a non-empty span vector splits the line into separate
        // label nodes instead of one -- the full unsplit string must NOT
        // appear as its own node, and the exact segments must.
        let harness = diff_harness(vec![sample_diff()]);
        assert!(
            harness.query_by_label("let x = compute_value();").is_none(),
            "removed line should be split, not rendered whole"
        );
        assert!(
            harness
                .query_by_label("let x = compute_result();")
                .is_none(),
            "added line should be split, not rendered whole"
        );
        assert!(harness.query_by_label("value").is_some());
        assert!(harness.query_by_label("result").is_some());
        // Both lines share the exact prefix "let x = compute_", so a
        // correctly-split render produces *two* separate nodes with that
        // label -- one per line, not one merged node.
        assert_eq!(
            harness.get_all_by_label("let x = compute_").count(),
            2,
            "prefix segment should be its own node on both the removed and added line"
        );
    }

    #[test]
    fn context_line_renders_unsplit_on_both_sides() {
        let harness = diff_harness(vec![sample_diff()]);
        let count = harness.get_all_by_label("shared context line").count();
        assert_eq!(count, 2, "context text appears once per side, unsplit");
    }
}
