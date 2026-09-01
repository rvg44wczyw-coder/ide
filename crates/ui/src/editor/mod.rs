//! The editor widget: an editable view of one `ide_core::Buffer`, rendered
//! line by line with viewport culling
//! (`docs/features/code-editor-widget.md`).
//!
//! Immediate-mode — built, shown and dropped every frame. Everything that
//! must survive a frame lives in `EditorState`, which a `Tab` owns.

pub mod blame_gutter;
pub(crate) mod double_tap;
pub mod folding;
mod geometry;
pub mod git_gutter;
mod input;
mod paint;

pub use blame_gutter::{annotations_from_blame, BlameAnnotation};
pub use folding::VisualLines;
pub use geometry::word_range_at;
pub use git_gutter::{marks_from_hunks, revert_hunk_change, GutterMark, GutterMarkKind};

use std::collections::BTreeSet;
use std::ops::Range;

use ide_core::{
    BracketPair, Buffer, FoldRange, IndentUnit, Selection, Selections, TextBuffer, MAX_OCCURRENCES,
};
use ide_lsp::{Diagnostic, InlayHint, Range as LspRange, SemanticToken};

use crate::theme::{Theme, Tokens};
use double_tap::DoubleTap;
use geometry::Metrics;
use input::{apply_intent, intent_for, Direction, Intent};
use paint::{
    diagnostic_marks, document_highlight_marks, merge_semantic_tokens, paint_code_action_marker,
    semantic_token_marks, tokens_in_range, LineCache, LineContext,
};

/// Everything the widget must remember between frames.
#[derive(Default)]
pub struct EditorState {
    /// Sticky x for vertical caret motion: `Up`/`Down` aim at the column the
    /// caret last moved to horizontally, not the one it happens to be in
    /// after passing through a short line.
    desired_column: Option<f32>,
    lines: LineCache,
    /// Set when the caret moves, consumed while painting to scroll it into
    /// view -- deferred rather than done mid-paint so a scroll request never
    /// races the viewport the same frame computed.
    pending_scroll: Option<usize>,
    /// `(line, time, count)` of the last click, for double/triple detection.
    last_click: Option<(usize, f64, u8)>,
    /// Widest line measured so far, never shrunk while the tab is open:
    /// recomputing it per frame would let egui clamp the horizontal offset
    /// and yank the view sideways when a long line scrolls off.
    content_width: f32,
    /// What the cached galleys were laid out under. Compared every frame, so
    /// a theme or font change invalidates the cache without anyone having to
    /// notify the editor.
    layout_key: Option<(egui::FontId, Theme)>,
    /// `⌥⌥` detector for Clone Caret (`multiple-cursors.md` §3.4).
    alt_tap: DoubleTap,
    /// `⌥`'s state last frame: `DoubleTap` is fed edges, and a modifier has
    /// no key event of its own to edge-detect from.
    alt_down: bool,
    /// Column Selection Mode (§3.5).
    column_mode: bool,
    /// `(line, column)` a column-mode drag started at -- the corner the
    /// rectangle is anchored to.
    column_anchor: Option<(usize, usize)>,
    /// The indent unit in force for this buffer. `IndentUnit::default()`
    /// throughout this set; A4b sets it from `.editorconfig` at tab open
    /// (`smart-editing.md` §2.7).
    indent: IndentUnit,
    /// The pair under the caret this frame, recomputed after input and
    /// painted by `paint_bracket_pair`. `None` when the caret is not on a
    /// bracket.
    bracket_pair: Option<BracketPair>,
    /// The primary caret `bracket_pair` was last computed for -- lets
    /// `update_bracket_pair` skip the scan on a frame where nothing moved
    /// it, matching doc §4's "recomputed only after input changed
    /// something" rather than rescanning on every idle frame.
    bracket_pair_caret: Option<usize>,
    /// Offsets of closing delimiters *this* auto-closer inserted, one per
    /// selection, as of the last keystroke. Type-over consults it and
    /// nothing else; every intent clears it before running, so it survives
    /// exactly one keystroke (§3.2).
    auto_closed: Vec<usize>,
    /// Ranges `⌥↑` grew through, newest last, so `⌥↓` can walk back down
    /// them (`line-commands-and-editorconfig.md` §2.6). Cleared by any edit
    /// and by any selection change that did not come from these two
    /// commands.
    shrink_stack: Vec<Selections>,
    /// `start_line` of every currently-collapsed fold, this tab's session-
    /// only view state -- reset by `EditorState::default()` on tab (re)open,
    /// same category `bracket_pair`/`column_mode` are already in
    /// (`code-folding.md` §2.3, §4.4).
    folded: BTreeSet<usize>,
}

impl EditorState {
    /// Whether Column Selection Mode is on -- read by the tab strip, which
    /// shows it, because a mode with no visible state is a trap (§3.5).
    pub fn column_mode(&self) -> bool {
        self.column_mode
    }

    /// Drops cached galleys and the content-width high-water mark.
    pub fn invalidate(&mut self) {
        self.lines.clear();
        self.content_width = 0.0;
    }

    pub fn indent(&self) -> IndentUnit {
        self.indent
    }

    /// Scrolls the given offset into view on the next frame, the same
    /// deferred mechanism `goto_offset` already drives internally --
    /// exposed so `IdeApp::find_next`/`find_previous`
    /// (`in-buffer-find-replace.md` §3.6) can scroll to a match range they
    /// place directly on the buffer's selections, without going through a
    /// `CodeEditor` builder call (they run in `handle_shortcuts`, before
    /// the widget renders this frame).
    pub fn request_scroll(&mut self, offset: usize) {
        self.pending_scroll = Some(offset);
    }

    /// Unused until A4b resolves this from `.editorconfig`; kept public now
    /// so that role's diff is confined to `crates/core/src/editorconfig.rs`
    /// and the one call site that reads the result.
    #[allow(dead_code)]
    pub fn set_indent(&mut self, unit: IndentUnit) {
        self.indent = unit;
    }

    pub fn is_folded(&self, start_line: usize) -> bool {
        self.folded.contains(&start_line)
    }

    /// Toggles one specific range by its `start_line` -- what a gutter-arrow
    /// or placeholder click uses, since both already know exactly which
    /// range was clicked (`code-folding.md` §3.6).
    pub fn toggle_fold(&mut self, start_line: usize) {
        if !self.folded.remove(&start_line) {
            self.folded.insert(start_line);
        }
    }

    /// `CollapseFold` (§2.4): the innermost range in `ranges` that contains
    /// `caret_line` and is not already collapsed. No-op if none does (§3.5).
    pub fn collapse_at_caret(&mut self, ranges: &[FoldRange], caret_line: usize) {
        let innermost = ranges
            .iter()
            .filter(|r| r.start_line <= caret_line && caret_line <= r.end_line)
            .filter(|r| !self.is_folded(r.start_line))
            .min_by_key(|r| r.end_line - r.start_line);
        if let Some(range) = innermost {
            self.folded.insert(range.start_line);
        }
    }

    /// `ExpandFold`: uncollapses the range whose `start_line` is
    /// `caret_line`, if one is currently collapsed there. No-op otherwise --
    /// in particular, this is the *only* shape a caret at a collapsed range
    /// can be in (§3.4), so "caret not on a collapsed start_line" and
    /// "nothing to expand" are the same condition.
    pub fn expand_at_caret(&mut self, caret_line: usize) {
        self.folded.remove(&caret_line);
    }

    pub fn collapse_all(&mut self, ranges: &[FoldRange]) {
        self.folded = ranges.iter().map(|r| r.start_line).collect();
    }

    pub fn expand_all(&mut self) {
        self.folded.clear();
    }

    /// Builds a `VisualLines` from this state's private `folded` set -- the
    /// one way anything outside `editor/**` (namely `app.rs`'s
    /// `run_command`) gets one, since `folded` itself stays private. Code
    /// inside `editor/**` may use this same method too, rather than
    /// reaching the private field directly, so there is exactly one way
    /// `VisualLines` gets constructed anywhere in the crate.
    pub fn visual_lines(&self, line_count: usize, ranges: &[FoldRange]) -> VisualLines {
        VisualLines::build(line_count, ranges, &self.folded)
    }

    /// Uncollapses every currently-collapsed range whose
    /// `start_line..=end_line` contains `line` -- used when a jump
    /// (`goto_offset`) targets a line otherwise hidden, so a
    /// search/diagnostic/usage/nav jump always reveals its target instead of
    /// silently landing on some unrelated fold's `start_line` (§3.4). Not
    /// recursive: `ranges` already lists every nested range up front, so one
    /// pass removing every containing `start_line` is enough.
    pub fn reveal_line(&mut self, ranges: &[FoldRange], line: usize) {
        for range in ranges {
            if range.start_line <= line && line <= range.end_line {
                self.folded.remove(&range.start_line);
            }
        }
    }
}

/// 0-based `(line, column)` for `offset` -- the same conversion the editor
/// widget uses for its own caret rendering, exposed so the status bar
/// (`fleet-shell.md` §3.6) can display the exact same position without a
/// second implementation.
pub fn cursor_line_column(buffer: &TextBuffer, offset: usize) -> (usize, usize) {
    (
        buffer.lines().line_at(offset),
        geometry::column_of(buffer, offset),
    )
}

pub struct EditorOutput {
    /// The primary caret after this frame's input.
    pub cursor_offset: usize,
    /// Whether the buffer's text changed this frame -- the gate for
    /// notifying the language server. The dirty flag is already set.
    pub changed: bool,
    /// The word under the pointer while `Cmd` is held.
    pub hovered_word: Option<Range<usize>>,
    /// Set on `Cmd+Click`, for the caller to turn into a Find Usages query.
    pub clicked_link: Option<Range<usize>>,
    /// Set when a git gutter mark's leading strip was clicked this frame
    /// (`docs/features/editor-git-gutter.md` §2.5) -- the buffer line the
    /// click landed on, for the caller to open the Revert Hunk/Show Diff
    /// popup on.
    pub git_gutter_clicked_line: Option<usize>,
    /// Set when the blame lane was clicked this frame on a line covered by
    /// an annotation (`docs/features/git-branches-and-blame.md` §2.2.3) --
    /// the buffer line the click landed on, for the caller to look up
    /// which annotation (and thus which `commit_id`) covers it and open
    /// the blame popup.
    pub blame_clicked_line: Option<usize>,
}

pub struct CodeEditor<'a> {
    id: egui::Id,
    buffer: &'a mut Buffer,
    state: &'a mut EditorState,
    tokens: &'a Tokens,
    theme: Theme,
    diagnostics: &'a [Diagnostic],
    link: Option<&'a Range<usize>>,
    goto_offset: Option<usize>,
    search_matches: &'a [Range<usize>],
    current_match_index: Option<usize>,
    document_highlights: &'a [LspRange],
    inlay_hints: &'a [InlayHint],
    semantic_tokens: &'a [SemanticToken],
    code_action_line: Option<usize>,
    git_gutter_marks: &'a [GutterMark],
    blame_on: bool,
    blame_annotations: &'a [BlameAnnotation],
}

impl<'a> CodeEditor<'a> {
    /// Takes the whole `Buffer`, not its `TextBuffer`, so the dirty flag is
    /// set exactly when an edit happens rather than on every frame the
    /// editor is merely visible.
    pub fn new(
        id: egui::Id,
        buffer: &'a mut Buffer,
        state: &'a mut EditorState,
        tokens: &'a Tokens,
        theme: Theme,
    ) -> Self {
        Self {
            id,
            buffer,
            state,
            tokens,
            theme,
            diagnostics: &[],
            link: None,
            goto_offset: None,
            search_matches: &[],
            current_match_index: None,
            document_highlights: &[],
            inlay_hints: &[],
            semantic_tokens: &[],
            code_action_line: None,
            git_gutter_marks: &[],
            blame_on: false,
            blame_annotations: &[],
        }
    }

    pub fn diagnostics(mut self, diagnostics: &'a [Diagnostic]) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    pub fn link(mut self, link: Option<&'a Range<usize>>) -> Self {
        self.link = link;
        self
    }

    pub fn goto_offset(mut self, offset: Option<usize>) -> Self {
        self.goto_offset = offset;
        self
    }

    /// Every find/replace match to highlight, and which one (if any) is
    /// current -- painted as a background highlight behind the text,
    /// `search_match_current_bg` for `current` and `search_match_bg` for
    /// the rest (`in-buffer-find-replace.md` §3.6).
    pub fn search_matches(mut self, matches: &'a [Range<usize>], current: Option<usize>) -> Self {
        self.search_matches = matches;
        self.current_match_index = current;
        self
    }

    /// Every occurrence of the symbol at the caret, in this file -- painted
    /// as a background wash the same way `search_matches` already is
    /// (`docs/features/inlay-hints-and-hover.md` §3.4). Raw `ide_lsp::Range`s,
    /// not pre-converted byte offsets -- same convention `diagnostics`
    /// already uses; conversion happens inside the widget, not at the call
    /// site.
    pub fn document_highlights(mut self, ranges: &'a [LspRange]) -> Self {
        self.document_highlights = ranges;
        self
    }

    /// Inlay hints for this file -- painted as small muted chips inline,
    /// immediately after the buffer column each hint's `position` names
    /// (§3.5). Purely additive painting: never mutates the line's own
    /// `Galley`/`LayoutJob`, so buffer byte offsets and on-screen text stay
    /// in exact 1:1 correspondence everywhere cursor/selection math already
    /// depends on that (§4).
    pub fn inlay_hints(mut self, hints: &'a [InlayHint]) -> Self {
        self.inlay_hints = hints;
        self
    }

    /// Semantic tokens for this file, whole-buffer, still in raw
    /// `ide_lsp::SemanticToken` (`Position`-based) shape -- same convention
    /// `document_highlights`/`inlay_hints` already use: conversion to
    /// buffer byte ranges happens inside the widget (`semantic_token_marks`,
    /// called once in `show`), not at the call site
    /// (`docs/features/semantic-highlighting.md` §2.3, §3.4).
    pub fn semantic_tokens(mut self, tokens: &'a [SemanticToken]) -> Self {
        self.semantic_tokens = tokens;
        self
    }

    /// The buffer line to paint the gutter lightbulb on, if any code action
    /// is currently available there -- `None` when `code_actions` is empty
    /// or there's no target line yet. `ide-ui` computes this from
    /// `last_code_actions_target`'s position plus whether `lsp.code_actions`
    /// is non-empty; this widget does no LSP-aware reasoning of its own,
    /// the same division of labour `diagnostics`/`document_highlights`
    /// already keep (`docs/features/code-actions.md` §2.3).
    pub fn code_action_line(mut self, line: Option<usize>) -> Self {
        self.code_action_line = line;
        self
    }

    /// The active tab's git gutter marks (`docs/features/
    /// editor-git-gutter.md` §2.5) -- default `&[]` paints and hit-tests
    /// nothing, same as every other optional overlay here.
    pub fn git_gutter_marks(mut self, marks: &'a [GutterMark]) -> Self {
        self.git_gutter_marks = marks;
        self
    }

    /// The active tab's blame toggle state and annotations
    /// (`docs/features/git-branches-and-blame.md` §2.2.3). `on` is
    /// deliberately separate from `annotations.is_empty()` -- an
    /// untracked file toggled on legitimately blames to zero annotations
    /// (§3's edge case), but must still keep its (empty) gutter lane
    /// reserved rather than collapsing it, so `Metrics::new` keys its
    /// `blame_lane_width` off this flag, never off the slice's length.
    pub fn blame_annotations(mut self, on: bool, annotations: &'a [BlameAnnotation]) -> Self {
        self.blame_on = on;
        self.blame_annotations = annotations;
        self
    }

    pub fn show(self, ui: &mut egui::Ui) -> EditorOutput {
        let Self {
            id,
            buffer,
            state,
            tokens,
            theme,
            diagnostics,
            link,
            goto_offset,
            search_matches,
            current_match_index,
            document_highlights,
            inlay_hints,
            semantic_tokens,
            code_action_line,
            git_gutter_marks,
            blame_on,
            blame_annotations,
        } = self;

        let font_id = egui::TextStyle::Monospace.resolve(ui.style());
        let (row_height, char_width) = ui.fonts_mut(|f| {
            (
                f.row_height(&font_id),
                f.glyph_width(&font_id, ' ').max(1.0),
            )
        });
        if state.layout_key.as_ref() != Some(&(font_id.clone(), theme)) {
            state.invalidate();
            state.layout_key = Some((font_id.clone(), theme));
        }

        let line_count = buffer.text_buffer().lines().line_count();
        let page_rows = ((ui.available_height() / row_height).floor() as usize).max(1);
        let metrics = Metrics::new(
            row_height,
            char_width,
            geometry::digits_for(line_count),
            page_rows,
            blame_on,
            &tokens.space,
        );

        if let Some(offset) = goto_offset {
            let clamped = offset.min(buffer.text_buffer().len());
            let target_line = buffer.text_buffer().lines().line_at(clamped);
            let ranges = buffer.text_buffer().fold_ranges();
            state.reveal_line(&ranges, target_line);
            buffer
                .text_buffer_mut()
                .set_selections(Selections::single(Selection::caret(clamped)));
            state.desired_column = None;
            state.pending_scroll = Some(clamped);
        }

        let marks = diagnostic_marks(buffer.text_buffer().text(), diagnostics);
        let document_highlights =
            document_highlight_marks(buffer.text_buffer().text(), document_highlights);
        let semantic_tokens = semantic_token_marks(buffer.text_buffer().text(), semantic_tokens);
        let text_color = ui.visuals().text_color();

        let mut frame = Frame {
            id,
            buffer,
            state,
            tokens,
            metrics,
            font_id,
            text_color,
            marks,
            link,
            search_matches,
            current_match_index,
            document_highlights,
            inlay_hints,
            semantic_tokens,
            code_action_line,
            git_gutter_marks,
            blame_annotations,
            changed: false,
            hovered_word: None,
            clicked_link: None,
            git_gutter_clicked_line: None,
            blame_clicked_line: None,
            copy: None,
        };

        egui::ScrollArea::both()
            .id_salt(id)
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| frame.run(ui, viewport));

        if let Some(text) = frame.copy.take() {
            ui.ctx().copy_text(text);
        }

        EditorOutput {
            cursor_offset: frame.buffer.text_buffer().selections().primary().head,
            changed: frame.changed,
            hovered_word: frame.hovered_word,
            clicked_link: frame.clicked_link,
            git_gutter_clicked_line: frame.git_gutter_clicked_line,
            blame_clicked_line: frame.blame_clicked_line,
        }
    }
}

/// One frame's worth of borrows, so the scroll-area closure doesn't have to
/// capture eight separate locals.
struct Frame<'a> {
    id: egui::Id,
    buffer: &'a mut Buffer,
    state: &'a mut EditorState,
    tokens: &'a Tokens,
    metrics: Metrics,
    font_id: egui::FontId,
    text_color: egui::Color32,
    marks: Vec<(usize, usize, ide_lsp::DiagnosticSeverity)>,
    link: Option<&'a Range<usize>>,
    search_matches: &'a [Range<usize>],
    current_match_index: Option<usize>,
    document_highlights: Vec<Range<usize>>,
    inlay_hints: &'a [InlayHint],
    semantic_tokens: Vec<ide_core::Token>,
    code_action_line: Option<usize>,
    git_gutter_marks: &'a [GutterMark],
    blame_annotations: &'a [BlameAnnotation],
    changed: bool,
    hovered_word: Option<Range<usize>>,
    clicked_link: Option<Range<usize>>,
    git_gutter_clicked_line: Option<usize>,
    blame_clicked_line: Option<usize>,
    copy: Option<String>,
}

impl Frame<'_> {
    fn run(&mut self, ui: &mut egui::Ui, viewport: egui::Rect) {
        let line_count = self.buffer.text_buffer().lines().line_count();
        let ranges = self.buffer.text_buffer().fold_ranges();
        let visual = self.state.visual_lines(line_count, &ranges);
        let visible =
            geometry::visible_lines(viewport, self.metrics.row_height, visual.row_count());

        ui.set_height(self.metrics.row_height * visual.row_count() as f32);
        ui.set_width(self.state.content_width.max(ui.available_width()));

        let origin = ui.min_rect().min;
        let response = ui.interact(ui.max_rect(), self.id, egui::Sense::click_and_drag());

        self.handle_mouse(ui, &response, origin, viewport, &visual);
        if response.clicked() {
            ui.memory_mut(|m| m.request_focus(self.id));
        }
        if ui.memory(|m| m.has_focus(self.id)) {
            // Without this, egui's own focus machinery still claims bare
            // arrows and `Tab` and hands focus to a neighbouring widget --
            // the editor would stop receiving the very keys
            // `code-editor-widget.md` §3.5 binds.
            //
            // `escape` is locked only while there is more than one cursor:
            // egui drops focus on `Esc` in `Focus::begin_pass`, before this
            // widget's frame runs, so an unlocked `Esc` never reaches
            // `handle_keys` at all and could not collapse anything. With one
            // cursor it stays unlocked, and `Esc` releases the editor as it
            // did before (`multiple-cursors.md` §3.6).
            let collapsible = self.buffer.text_buffer().selections().is_multiple();
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    self.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: collapsible,
                    },
                );
            });
            self.handle_keys(ui, &visual);
        }

        self.update_bracket_pair();
        self.paint(ui, origin, visible.clone(), viewport, &visual, &ranges);
        // `LineCache` is keyed by buffer line, but `visible` is now a row
        // range -- convert back through `visual` rather than retaining by
        // row, so a line hidden by an unrelated fold still gets evicted
        // (and a line just revealed by scrolling still gets kept).
        let slack_start = visual.buffer_line(visible.start.saturating_sub(visible.len()));
        let slack_end =
            visual.buffer_line(visible.end.saturating_add(visible.len()).saturating_sub(1)) + 1;
        self.state.lines.retain(slack_start..slack_end);
    }

    /// The primary caret's pair -- never per-cursor, which is what makes N
    /// carets on N different brackets paint one highlight, not N (doc
    /// §3.4). Skips the scan when the primary caret hasn't moved and the
    /// buffer hasn't changed since the last frame that computed it (doc §4:
    /// "recomputed only after input changed something").
    fn update_bracket_pair(&mut self) {
        let head = self.buffer.text_buffer().selections().primary().head;
        if !self.changed && self.state.bracket_pair_caret == Some(head) {
            return;
        }
        self.state.bracket_pair = self.buffer.text_buffer().matching_bracket(head);
        self.state.bracket_pair_caret = Some(head);
    }

    fn galley(&mut self, ui: &egui::Ui, line: usize) -> std::sync::Arc<egui::Galley> {
        let buffer = self.buffer.text_buffer();
        let range = buffer
            .lines()
            .line_range(line, buffer.text())
            .unwrap_or(0..0);
        let text = buffer.text();
        let line_text = &text[range.clone()];
        let regex_syntax = buffer.tokens_in_lines(line..line + 1);
        let semantic_syntax = tokens_in_range(&self.semantic_tokens, range.clone());
        let merged_syntax = merge_semantic_tokens(regex_syntax, semantic_syntax);
        let context = LineContext {
            font_id: self.font_id.clone(),
            text_color: self.text_color,
            tokens: self.tokens,
            syntax: &merged_syntax,
            marks: &self.marks,
            link: self.link,
        };
        let job = paint::line_layout_job(text, range, &context);
        let painter = ui.painter();
        self.state
            .lines
            .galley(line, line_text, || painter.layout_job(job))
    }

    fn paint(
        &mut self,
        ui: &mut egui::Ui,
        origin: egui::Pos2,
        visible_rows: Range<usize>,
        viewport: egui::Rect,
        visual: &VisualLines,
        ranges: &[FoldRange],
    ) {
        let selections: Vec<Selection> = self.buffer.text_buffer().selections().all().to_vec();
        let primary = self.buffer.text_buffer().selections().primary();
        let primary_line = self.buffer.text_buffer().lines().line_at(primary.head);
        let primary_row = visual.row_of(primary_line);
        // Every line that is a `start_line` of at least one *current* range
        // -- a stale `EditorState::folded` entry with no matching range here
        // draws neither an arrow nor a placeholder, matching §4 constraint
        // 5's "renders as an ordinary visible line again".
        let fold_start_lines: BTreeSet<usize> = ranges.iter().map(|r| r.start_line).collect();

        let text_left = origin.x + self.metrics.text_left;
        let width = ui.max_rect().width();

        // Current-line band first: everything else draws over it. Only under
        // a bare caret -- a band under a selection fights with its colour.
        // Keyed by row, not line: a caret sitting on a collapsed fold's
        // `start_line` still gets a band on that row even though its own
        // buffer line is what the caret's offset resolves to (§3.4/§3.7).
        if primary.is_empty() && visible_rows.contains(&primary_row) {
            let y = origin.y + geometry::line_top(primary_row, self.metrics.row_height);
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(origin.x, y),
                    egui::vec2(width, self.metrics.row_height),
                ),
                0.0,
                self.tokens.color.current_line_bg,
            );
        }

        let mut widest: f32 = 0.0;
        for row in visible_rows.clone() {
            let line = visual.buffer_line(row);
            let galley = self.galley(ui, line);
            widest = widest.max(galley.rect.width());
            let top = origin.y + geometry::line_top(row, self.metrics.row_height);
            let pos = egui::pos2(text_left, top);

            self.paint_selections(ui, &selections, line, pos, &galley);
            self.paint_search_matches(ui, line, pos, &galley);
            self.paint_document_highlights(ui, line, pos, &galley);
            self.paint_bracket_pair(ui, line, pos, &galley);
            ui.painter().galley(pos, galley.clone(), self.text_color);
            self.paint_carets(ui, &selections, line, pos, &galley);
            if fold_start_lines.contains(&line) && self.state.is_folded(line) {
                self.paint_fold_placeholder(ui, &galley, pos);
            }
            self.paint_inlay_hints(ui, line, pos, &galley);
        }
        self.state.content_width = self
            .state
            .content_width
            .max(widest + self.metrics.text_left + self.metrics.char_width);

        self.paint_gutter(
            ui,
            origin,
            visible_rows,
            viewport,
            visual,
            &fold_start_lines,
        );
        self.paint_search_marker_strip(ui, origin, viewport, visual);
        self.scroll_to_pending(ui, origin, visual);
    }

    /// The three characters `" ⋯"` appended right after a collapsed range's
    /// `start_line` text -- a muted marker, not a rewrite of the line's own
    /// highlighting (§3.6). Clicking it is handled in `handle_mouse`
    /// alongside the gutter arrow.
    fn paint_fold_placeholder(&self, ui: &egui::Ui, galley: &egui::Galley, pos: egui::Pos2) {
        let x = pos.x + galley.rect.width();
        ui.painter().text(
            egui::pos2(x, pos.y),
            egui::Align2::LEFT_TOP,
            " \u{22ef}",
            self.font_id.clone(),
            self.tokens.color.fg_muted,
        );
    }

    fn line_bounds(&self, line: usize) -> Range<usize> {
        let buffer = self.buffer.text_buffer();
        buffer
            .lines()
            .line_range(line, buffer.text())
            .unwrap_or(0..0)
    }

    fn x_of(&self, galley: &egui::Galley, line_text: &str, offset_in_line: usize) -> f32 {
        let column = geometry::char_index_in_line(line_text, offset_in_line);
        galley
            .pos_from_cursor(egui::text::CCursor::new(column))
            .min
            .x
    }

    fn paint_selections(
        &self,
        ui: &egui::Ui,
        selections: &[Selection],
        line: usize,
        pos: egui::Pos2,
        galley: &egui::Galley,
    ) {
        let bounds = self.line_bounds(line);
        let text = self.buffer.text_buffer().text();
        let line_text = &text[bounds.clone()];
        for selection in selections.iter().filter(|s| !s.is_empty()) {
            let start = selection.start().max(bounds.start);
            let end = selection.end().min(bounds.end);
            if selection.start() > bounds.end || selection.end() < bounds.start {
                continue;
            }
            let x0 = pos.x + self.x_of(galley, line_text, start.saturating_sub(bounds.start));
            let mut x1 = pos.x + self.x_of(galley, line_text, end.saturating_sub(bounds.start));
            // A selection continuing onto the next line shows the newline it
            // swallowed as half a character of trailing highlight.
            if selection.end() > bounds.end {
                x1 += self.metrics.char_width * 0.5;
            }
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x0, pos.y),
                    egui::vec2((x1 - x0).max(1.0), self.metrics.row_height),
                ),
                0.0,
                self.tokens.color.selection_bg,
            );
        }
    }

    /// Both halves of the matched pair, on whichever visible line each
    /// falls on. Independent rects: the two brackets are rarely on the same
    /// line, and never assumed to be.
    fn paint_bracket_pair(
        &self,
        ui: &egui::Ui,
        line: usize,
        pos: egui::Pos2,
        galley: &egui::Galley,
    ) {
        let Some(pair) = &self.state.bracket_pair else {
            return;
        };
        let bounds = self.line_bounds(line);
        let text = self.buffer.text_buffer().text();
        let line_text = &text[bounds.clone()];
        for range in [&pair.open, &pair.close] {
            if range.start < bounds.start || range.end > bounds.end {
                continue;
            }
            let x0 = pos.x + self.x_of(galley, line_text, range.start - bounds.start);
            let x1 = pos.x + self.x_of(galley, line_text, range.end - bounds.start);
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x0, pos.y),
                    egui::vec2((x1 - x0).max(1.0), self.metrics.row_height),
                ),
                0.0,
                self.tokens.color.bracket_match_bg,
            );
        }
    }

    /// Every `search_matches` range touching `line`, `search_match_bg`
    /// except `current_match_index`'s, which gets the more prominent
    /// `search_match_current_bg` -- the same "own token distinct from
    /// `selection_bg`" shape `bracket_match_bg` already uses
    /// (`in-buffer-find-replace.md` §3.6).
    fn paint_search_matches(
        &self,
        ui: &egui::Ui,
        line: usize,
        pos: egui::Pos2,
        galley: &egui::Galley,
    ) {
        if self.search_matches.is_empty() {
            return;
        }
        let bounds = self.line_bounds(line);
        let text = self.buffer.text_buffer().text();
        let line_text = &text[bounds.clone()];
        for (index, range) in self.search_matches.iter().enumerate() {
            if range.start > bounds.end || range.end < bounds.start {
                continue;
            }
            let start = range.start.max(bounds.start);
            let end = range.end.min(bounds.end);
            let x0 = pos.x + self.x_of(galley, line_text, start.saturating_sub(bounds.start));
            let x1 = pos.x + self.x_of(galley, line_text, end.saturating_sub(bounds.start));
            let color = if self.current_match_index == Some(index) {
                self.tokens.color.search_match_current_bg
            } else {
                self.tokens.color.search_match_bg
            };
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x0, pos.y),
                    egui::vec2((x1 - x0).max(1.0), self.metrics.row_height),
                ),
                0.0,
                color,
            );
        }
    }

    /// Every occurrence of the symbol at the caret, in `symbol_highlight_bg`
    /// -- structurally identical to `paint_search_matches` (same
    /// `x_of`-derived left/right bounds, same per-row loop position), a
    /// genuinely new token rather than reusing `search_match_bg`: symbol
    /// highlighting is semantically distinct from find/replace matches
    /// (`docs/features/inlay-hints-and-hover.md` §3.4).
    fn paint_document_highlights(
        &self,
        ui: &egui::Ui,
        line: usize,
        pos: egui::Pos2,
        galley: &egui::Galley,
    ) {
        if self.document_highlights.is_empty() {
            return;
        }
        let bounds = self.line_bounds(line);
        let text = self.buffer.text_buffer().text();
        let line_text = &text[bounds.clone()];
        for range in &self.document_highlights {
            if range.start > bounds.end || range.end < bounds.start {
                continue;
            }
            let start = range.start.max(bounds.start);
            let end = range.end.min(bounds.end);
            let x0 = pos.x + self.x_of(galley, line_text, start.saturating_sub(bounds.start));
            let x1 = pos.x + self.x_of(galley, line_text, end.saturating_sub(bounds.start));
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x0, pos.y),
                    egui::vec2((x1 - x0).max(1.0), self.metrics.row_height),
                ),
                0.0,
                self.tokens.color.symbol_highlight_bg,
            );
        }
    }

    /// Every hint whose `position.line` falls on `line`, painted as a
    /// muted chip immediately after its column -- the same paint-time-
    /// overlay shape `paint_fold_placeholder` already establishes for
    /// "extra text after a line's real content that isn't part of the
    /// buffer", extended to mid-line insertion points and to N chips per
    /// line (`docs/features/inlay-hints-and-hover.md` §3.5). **Never
    /// touches `galley`, the line's `LayoutJob`, or its underlying text
    /// string** -- a separate `painter.text` call over the already-laid-out,
    /// unmodified line (§4).
    fn paint_inlay_hints(
        &self,
        ui: &egui::Ui,
        line: usize,
        pos: egui::Pos2,
        galley: &egui::Galley,
    ) {
        if self.inlay_hints.is_empty() {
            return;
        }
        let bounds = self.line_bounds(line);
        let text = self.buffer.text_buffer().text();
        let line_text = &text[bounds.clone()];
        for hint in self.inlay_hints {
            if hint.position.line as usize != line {
                continue;
            }
            let Some(absolute) = ide_lsp::position_to_byte_offset(text, hint.position) else {
                continue;
            };
            let offset_in_line = absolute.saturating_sub(bounds.start);
            let x = pos.x + self.x_of(galley, line_text, offset_in_line);
            let label = match (hint.padding_left, hint.padding_right) {
                (true, true) => format!(" {} ", hint.label),
                (true, false) => format!(" {}", hint.label),
                (false, true) => format!("{} ", hint.label),
                (false, false) => hint.label.clone(),
            };
            ui.painter().text(
                egui::pos2(x, pos.y),
                egui::Align2::LEFT_TOP,
                label,
                self.font_id.clone(),
                self.tokens.color.fg_muted,
            );
        }
    }

    /// A thin tick per match along the viewport's right edge, proportional
    /// to the match's line within the file -- not egui's own scrollbar
    /// (stock egui exposes no hook to draw inside it), a separate overlay
    /// column this widget paints itself (`in-buffer-find-replace.md` §3.6).
    /// The current match's tick is drawn last, in `search_match_current_bg`,
    /// so it's never hidden under an ordinary tick at the same proportional
    /// position.
    fn paint_search_marker_strip(
        &self,
        ui: &egui::Ui,
        origin: egui::Pos2,
        viewport: egui::Rect,
        visual: &VisualLines,
    ) {
        if self.search_matches.is_empty() || visual.row_count() == 0 {
            return;
        }
        const MARKER_WIDTH: f32 = 4.0;
        const MARKER_HEIGHT: f32 = 2.0;
        let x = origin.x + viewport.max.x - MARKER_WIDTH;
        let painter = ui.painter();
        let total_rows = visual.row_count() as f32;
        let tick = |range: &Range<usize>| {
            let line = self.buffer.text_buffer().lines().line_at(range.start);
            let row = visual.row_of(line) as f32;
            origin.y + viewport.min.y + (row / total_rows) * viewport.height()
        };
        for (index, range) in self.search_matches.iter().enumerate() {
            if self.current_match_index == Some(index) {
                continue;
            }
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x, tick(range)),
                    egui::vec2(MARKER_WIDTH, MARKER_HEIGHT),
                ),
                0.0,
                self.tokens.color.search_match_bg,
            );
        }
        if let Some(range) = self
            .current_match_index
            .and_then(|index| self.search_matches.get(index))
        {
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x, tick(range)),
                    egui::vec2(MARKER_WIDTH, MARKER_HEIGHT),
                ),
                0.0,
                self.tokens.color.search_match_current_bg,
            );
        }
    }

    fn paint_carets(
        &self,
        ui: &egui::Ui,
        selections: &[Selection],
        line: usize,
        pos: egui::Pos2,
        galley: &egui::Galley,
    ) {
        let bounds = self.line_bounds(line);
        let text = self.buffer.text_buffer().text();
        let line_text = &text[bounds.clone()];
        for selection in selections {
            if selection.head < bounds.start || selection.head > bounds.end {
                continue;
            }
            let x = pos.x + self.x_of(galley, line_text, selection.head - bounds.start);
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x, pos.y),
                    egui::vec2(2.0, self.metrics.row_height),
                ),
                0.0,
                self.tokens.color.caret,
            );
        }
    }

    fn paint_gutter(
        &mut self,
        ui: &mut egui::Ui,
        origin: egui::Pos2,
        visible_rows: Range<usize>,
        viewport: egui::Rect,
        visual: &VisualLines,
        fold_start_lines: &BTreeSet<usize>,
    ) {
        let primary = self.buffer.text_buffer().selections().primary();
        let primary_line = self.buffer.text_buffer().lines().line_at(primary.head);
        let primary_row = visual.row_of(primary_line);
        // Painted last and pinned to the viewport's left edge, so horizontal
        // scrolling slides the text under a gutter that stays put.
        let left = origin.x + viewport.min.x;
        let rect = egui::Rect::from_min_size(
            egui::pos2(left, origin.y + viewport.min.y),
            egui::vec2(self.metrics.gutter_width, viewport.height()),
        );
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, self.tokens.color.bg_editor);
        painter.vline(
            rect.max.x,
            rect.y_range(),
            egui::Stroke::new(1.0, self.tokens.color.border),
        );

        let number_right = rect.max.x - self.tokens.space.sm;
        let marker_left = rect.min.x + self.metrics.blame_lane_width + self.tokens.space.sm;
        let blame_now = blame_now();
        for row in visible_rows {
            let line = visual.buffer_line(row);
            let top = origin.y + geometry::line_top(row, self.metrics.row_height);
            let color = if row == primary_row {
                self.tokens.color.gutter_fg_active
            } else {
                self.tokens.color.gutter_fg
            };
            painter.text(
                egui::pos2(number_right, top),
                egui::Align2::RIGHT_TOP,
                line + 1,
                self.font_id.clone(),
                color,
            );
            if fold_start_lines.contains(&line) {
                self.paint_fold_arrow(painter, marker_left, top, self.state.is_folded(line), color);
            } else if self.code_action_line == Some(line) {
                paint_code_action_marker(
                    painter,
                    marker_left,
                    top,
                    self.metrics.row_height,
                    self.metrics.char_width,
                    self.tokens.color.warning,
                );
            }
            if let Some(kind) = self.git_gutter_mark_at(line) {
                self.paint_git_gutter_mark(painter, rect.min.x, marker_left, top, kind);
            }
            if self.metrics.blame_lane_width > 0.0 {
                if let Some(annotation) = self.blame_annotation_at(line) {
                    if annotation.line == line {
                        self.paint_blame_label(painter, rect.min.x, top, annotation, blame_now);
                    }
                }
            }
        }
    }

    /// `blame_annotations` is sorted ascending by `line`, each entry
    /// covering `line..line + run_len` -- a `partition_point` binary
    /// search over the run-start line, then a range check against that
    /// candidate's `run_len`, is sound (mirrors `git_gutter_mark_at`'s own
    /// binary search one level down, adapted for a run instead of a
    /// single-line match).
    fn blame_annotation_at(&self, line: usize) -> Option<&BlameAnnotation> {
        let idx = self.blame_annotations.partition_point(|a| a.line <= line);
        if idx == 0 {
            return None;
        }
        let candidate = &self.blame_annotations[idx - 1];
        (line < candidate.line + candidate.run_len).then_some(candidate)
    }

    /// The blame lane's own label -- rendered only on an annotation's
    /// first line (`BlameAnnotation::run_len`'s own doc comment), manually
    /// truncated to `BLAME_LANE_CHARS` since the lane's width must not
    /// vary with the annotation text (`geometry::Metrics::new`'s doc
    /// comment).
    fn paint_blame_label(
        &self,
        painter: &egui::Painter,
        gutter_left: f32,
        top: f32,
        annotation: &BlameAnnotation,
        now: i64,
    ) {
        let text = format!(
            "{}, {}",
            annotation.author,
            blame_gutter::relative_time(annotation.timestamp, now)
        );
        let truncated = blame_gutter::truncate_display(&text, geometry::BLAME_LANE_CHARS as usize);
        painter.text(
            egui::pos2(gutter_left + self.tokens.space.xs, top),
            egui::Align2::LEFT_TOP,
            truncated,
            self.font_id.clone(),
            self.tokens.color.gutter_fg,
        );
    }

    /// `git_gutter_marks` is sorted ascending by `line` with at most one
    /// entry per line by construction (`marks_from_hunks`'s segments are
    /// always separated by at least one `Context` line) -- a binary
    /// search is sound.
    fn git_gutter_mark_at(&self, line: usize) -> Option<GutterMarkKind> {
        self.git_gutter_marks
            .binary_search_by_key(&line, |m| m.line)
            .ok()
            .map(|i| self.git_gutter_marks[i].kind)
    }

    /// A colored strip in the gutter's *leading* padding (`gutter_left` ..
    /// `marker_left`, the `space.sm` gap before the fold-arrow/code-action
    /// marker lane starts) -- a distinct lane from those, so nothing ever
    /// collides on the same row (`docs/features/editor-git-gutter.md`
    /// §2.5). `Added`/`Modified` paint a full-height vertical bar;
    /// `Deleted` a short horizontal notch at the row's top edge, matching
    /// every JetBrains IDE's own "N lines removed here" convention.
    fn paint_git_gutter_mark(
        &self,
        painter: &egui::Painter,
        gutter_left: f32,
        marker_left: f32,
        top: f32,
        kind: GutterMarkKind,
    ) {
        const BAR_WIDTH: f32 = 2.0;
        let bar_left = gutter_left + 1.0;
        match kind {
            GutterMarkKind::Added | GutterMarkKind::Modified => {
                let color = if kind == GutterMarkKind::Added {
                    self.tokens.color.diff_added_fg
                } else {
                    self.tokens.color.diff_modified_fg
                };
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(bar_left, top),
                        egui::vec2(BAR_WIDTH, self.metrics.row_height),
                    ),
                    0.0,
                    color,
                );
            }
            GutterMarkKind::Deleted => {
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(bar_left, top - BAR_WIDTH * 0.5),
                        egui::vec2(marker_left - bar_left, BAR_WIDTH),
                    ),
                    0.0,
                    self.tokens.color.diff_removed_fg,
                );
            }
        }
    }

    /// A small triangle in the gutter's marker lane -- pointing right when
    /// collapsed, down when expanded, the same shape convention the slim
    /// tree's directory rows use (`fleet-shell.md` §3.4, `code-folding.md`
    /// §3.6).
    fn paint_fold_arrow(
        &self,
        painter: &egui::Painter,
        marker_left: f32,
        top: f32,
        collapsed: bool,
        color: egui::Color32,
    ) {
        let size = self.metrics.row_height * 0.35;
        let cx = marker_left + geometry::MARKER_LANE_CHARS * self.metrics.char_width * 0.5;
        let cy = top + self.metrics.row_height * 0.5;
        let points = if collapsed {
            vec![
                egui::pos2(cx - size * 0.5, cy - size),
                egui::pos2(cx - size * 0.5, cy + size),
                egui::pos2(cx + size * 0.7, cy),
            ]
        } else {
            vec![
                egui::pos2(cx - size, cy - size * 0.5),
                egui::pos2(cx + size, cy - size * 0.5),
                egui::pos2(cx, cy + size * 0.7),
            ]
        };
        painter.add(egui::Shape::convex_polygon(
            points,
            color,
            egui::Stroke::NONE,
        ));
    }

    fn scroll_to_pending(&mut self, ui: &mut egui::Ui, origin: egui::Pos2, visual: &VisualLines) {
        let Some(offset) = self.state.pending_scroll.take() else {
            return;
        };
        let line = self.buffer.text_buffer().lines().line_at(offset);
        let row = visual.row_of(line);
        let top = origin.y + geometry::line_top(row, self.metrics.row_height);
        ui.scroll_to_rect(
            egui::Rect::from_min_size(
                egui::pos2(origin.x, top),
                egui::vec2(1.0, self.metrics.row_height),
            ),
            Some(egui::Align::Center),
        );
    }

    fn handle_keys(&mut self, ui: &egui::Ui, visual: &VisualLines) {
        let (events, now, alt) = ui.input(|i| (i.events.clone(), i.time, i.modifiers.alt));

        if alt && !self.state.alt_down {
            self.state.alt_tap.press(now);
        } else if !alt {
            // Releasing the modifier ends the gesture but keeps the press
            // that ended it, which is the first half of the next one.
            self.state.alt_tap.disarm();
        }
        self.state.alt_down = alt;

        for event in &events {
            let Some(intent) = intent_for(event) else {
                continue;
            };
            let intent = self.rewrite(ui, intent, now);
            let Some(intent) = intent else {
                continue;
            };
            let before = self.buffer.text_buffer().selections().primary().head;
            let applied = apply_intent(self.buffer, self.state, &self.metrics, visual, intent);
            self.changed |= applied.changed;
            if let Some(text) = applied.copy {
                self.copy = Some(text);
            }
            let after = self.buffer.text_buffer().selections().primary().head;
            if after != before || applied.changed {
                self.state.pending_scroll = Some(after);
            }
        }
    }

    /// The two intents `intent_for` cannot decide on its own, because both
    /// depend on state it deliberately cannot see (doc §3.4, §3.6).
    fn rewrite(&mut self, ui: &egui::Ui, intent: Intent, now: f64) -> Option<Intent> {
        match intent {
            Intent::Move {
                direction: direction @ (Direction::Up | Direction::Down),
                ..
            } if self.state.alt_tap.is_armed(now) => {
                self.state.alt_tap.disarm();
                Some(Intent::CloneCaret(direction))
            }
            // Only reachable when the arm above didn't fire, i.e. `⌥⌥` isn't
            // armed -- an unarmed `⌥↑`/`⌥↓` extends/shrinks instead (doc
            // §1.2 collision 1, §2.5). `intent_for` cannot make this call
            // itself: the `alt` bit is indistinguishable from a bare arrow
            // press once collapsed into `Intent::Move`'s fields, so this
            // reads the live modifiers the same way `handle_keys` already
            // does for the `alt_tap` feed.
            Intent::Move {
                direction: direction @ (Direction::Up | Direction::Down),
                extend: false,
                ..
            } if ui.input(|i| i.modifiers.alt && !i.modifiers.command) => {
                Some(if direction == Direction::Up {
                    Intent::ExtendSelection
                } else {
                    Intent::ShrinkSelection
                })
            }
            Intent::CollapseSelections => {
                if !self.buffer.text_buffer().selections().is_multiple() {
                    return None;
                }
                // The one event this widget consumes: without it egui would
                // also release focus on the same `Esc` that collapsed the
                // cursors. The Usages popup reads its own `Esc` earlier in
                // the frame, so `handle_shortcuts` gates that half itself.
                ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                Some(intent)
            }
            // `intent_for` cannot see the selection, so it always names this
            // A2's literal tab; here is where it becomes one indent level or
            // a real indent command (doc §2.6, §3.5).
            Intent::Insert(ref text) if text == "\t" => {
                if self.selection_worth_indenting() {
                    Some(Intent::Indent)
                } else {
                    Some(Intent::Insert(self.state.indent().one().into_owned()))
                }
            }
            other => Some(other),
        }
    }

    /// Any selection non-empty -- the doc §3.5 condition under which `Tab`
    /// indents instead of inserting. A non-empty selection already covers
    /// both cases the doc names (single-line and multi-line): an *empty*
    /// selection is a single point, so it can never itself span a line
    /// boundary.
    fn selection_worth_indenting(&self) -> bool {
        self.buffer
            .text_buffer()
            .selections()
            .all()
            .iter()
            .any(|selection| !selection.is_empty())
    }

    fn handle_mouse(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        origin: egui::Pos2,
        viewport: egui::Rect,
        visual: &VisualLines,
    ) {
        let (command, alt) = ui.input(|i| (i.modifiers.command, i.modifiers.alt));

        if let Some(pointer) = response.hover_pos() {
            let offset = self.offset_at(ui, pointer, origin, visual);
            let word = command
                .then(|| word_range_at(self.buffer.text_buffer().text(), offset))
                .flatten();
            if word.is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            } else {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
            }
            self.hovered_word = word;
        }

        if response.drag_started() || response.clicked() {
            if let Some(pointer) = response.interact_pointer_pos() {
                if response.clicked() {
                    if let Some(start_line) =
                        self.fold_click_target(ui, pointer, origin, viewport, visual)
                    {
                        // The caret is anywhere in the buffer when a distant
                        // gutter arrow is clicked, including inside the
                        // range that click just collapsed (§3.4/§3.6).
                        let was_folded = self.state.is_folded(start_line);
                        self.state.toggle_fold(start_line);
                        if !was_folded {
                            folding::reveal_caret_after_collapse(self.buffer, self.state);
                        }
                        return;
                    }
                    if let Some(line) =
                        self.git_gutter_click_target(pointer, origin, viewport, visual)
                    {
                        self.git_gutter_clicked_line = Some(line);
                        return;
                    }
                    if let Some(line) = self.blame_click_target(pointer, origin, viewport, visual) {
                        self.blame_clicked_line = Some(line);
                        return;
                    }
                }
                let offset = self.offset_at(ui, pointer, origin, visual);
                if command && response.clicked() {
                    self.clicked_link = word_range_at(self.buffer.text_buffer().text(), offset);
                }
                if alt && response.clicked() {
                    self.toggle_caret(offset);
                } else if self.state.column_mode {
                    self.state.column_anchor = Some(self.line_column(offset));
                    self.select_for_click(ui, offset);
                } else {
                    self.select_for_click(ui, offset);
                }
            }
        } else if response.dragged() {
            if let Some(pointer) = response.interact_pointer_pos() {
                let offset = self.offset_at(ui, pointer, origin, visual);
                match self.state.column_anchor.filter(|_| self.state.column_mode) {
                    Some(anchor) => self.select_column(anchor, offset),
                    None => {
                        let anchor = self.buffer.text_buffer().selections().primary().anchor;
                        self.state.shrink_stack.clear();
                        self.buffer
                            .text_buffer_mut()
                            .set_selections(Selections::single(Selection::new(anchor, offset)));
                    }
                }
            }
        } else if response.drag_stopped() {
            self.state.column_anchor = None;
        }
    }

    fn line_column(&self, offset: usize) -> (usize, usize) {
        cursor_line_column(self.buffer.text_buffer(), offset)
    }

    /// `⌥Click` is a toggle resolved before anything is added: off a bare
    /// caret it removes that cursor, inside a selection the user made on
    /// purpose it does nothing, and anywhere else it adds a caret
    /// (`multiple-cursors.md` §3.1).
    fn toggle_caret(&mut self, offset: usize) {
        let selections = self.buffer.text_buffer().selections();
        let mut updated = selections.clone();
        match selections.index_at(offset) {
            Some(index) if selections.all()[index].is_empty() => {
                if !updated.remove_at(index) {
                    return;
                }
            }
            Some(_) => return,
            None => {
                updated.push_primary(Selection::caret(offset));
            }
        }
        self.buffer.text_buffer_mut().set_selections(updated);
        self.state.desired_column = None;
        self.state.shrink_stack.clear();
    }

    fn select_column(&mut self, anchor: (usize, usize), offset: usize) {
        let (line, column) = self.line_column(offset);
        let line = clamped_span(anchor.0, line);
        let buffer = self.buffer.text_buffer();
        let ranges = geometry::column_selections(buffer, anchor.0..line, anchor.1..column);
        if ranges.is_empty() {
            return;
        }
        self.state.shrink_stack.clear();
        // The line the pointer is on stays primary, so the caret the user is
        // steering is the one the view follows.
        let primary = ranges
            .iter()
            .position(|s| buffer.lines().line_at(s.head) == line)
            .unwrap_or(0);
        self.buffer
            .text_buffer_mut()
            .set_selections(Selections::new(ranges, primary));
        self.state.desired_column = None;
    }

    /// Click, double-click and triple-click, distinguished by how close in
    /// time and place the previous click was.
    fn select_for_click(&mut self, ui: &egui::Ui, offset: usize) {
        let now = ui.input(|i| i.time);
        // `offset` always comes from `offset_at`, already resolved through a
        // visible row, so it's already a valid, visible buffer line -- no
        // extra clamp needed here the way an unclamped raw `line_count`
        // check would have required.
        let line = self.buffer.text_buffer().lines().line_at(offset);
        let count = match self.state.last_click {
            Some((last_line, at, count)) if last_line == line && now - at < 0.4 => {
                (count + 1).min(3)
            }
            _ => 1,
        };
        self.state.last_click = Some((line, now, count));
        self.state.shrink_stack.clear();
        self.state.desired_column = None;

        let selection = match count {
            2 => word_range_at(self.buffer.text_buffer().text(), offset)
                .map(|r| Selection::new(r.start, r.end))
                .unwrap_or(Selection::caret(offset)),
            3 => {
                let bounds = self.line_bounds(line);
                Selection::new(bounds.start, bounds.end)
            }
            _ => Selection::caret(offset),
        };
        self.buffer
            .text_buffer_mut()
            .set_selections(Selections::single(selection));
    }

    fn offset_at(
        &mut self,
        ui: &egui::Ui,
        pointer: egui::Pos2,
        origin: egui::Pos2,
        visual: &VisualLines,
    ) -> usize {
        let row = geometry::line_at_y(
            pointer.y - origin.y,
            self.metrics.row_height,
            visual.row_count(),
        );
        let line = visual.buffer_line(row);
        let galley = self.galley(ui, line);
        geometry::offset_at_pos(
            self.buffer.text_buffer(),
            &galley,
            line,
            pointer.x - origin.x - self.metrics.text_left,
        )
    }

    /// The `start_line` a click at `pointer` should toggle, if it landed on
    /// that row's gutter arrow or, if the row is currently collapsed, its
    /// placeholder marker -- checked before the general click-to-select
    /// handling in `handle_mouse` so neither accidentally moves the caret
    /// or starts a selection (`code-folding.md` §3.6).
    fn fold_click_target(
        &mut self,
        ui: &egui::Ui,
        pointer: egui::Pos2,
        origin: egui::Pos2,
        viewport: egui::Rect,
        visual: &VisualLines,
    ) -> Option<usize> {
        let row = geometry::line_at_y(
            pointer.y - origin.y,
            self.metrics.row_height,
            visual.row_count(),
        );
        let line = visual.buffer_line(row);
        let ranges = self.buffer.text_buffer().fold_ranges();
        if !ranges.iter().any(|r| r.start_line == line) {
            return None;
        }

        // Pinned to the viewport's left edge, same as `paint_gutter`'s own
        // rect -- horizontal scrolling must not move the click target out
        // from under where the arrow is actually drawn.
        let marker_left = origin.x + viewport.min.x + self.tokens.space.sm;
        let marker_right = marker_left + geometry::MARKER_LANE_CHARS * self.metrics.char_width;
        if pointer.x >= marker_left && pointer.x <= marker_right {
            return Some(line);
        }

        if self.state.is_folded(line) {
            let galley = self.galley(ui, line);
            let placeholder_start = origin.x + self.metrics.text_left + galley.rect.width();
            let placeholder_end = placeholder_start + 2.0 * self.metrics.char_width;
            if pointer.x >= placeholder_start && pointer.x <= placeholder_end {
                return Some(line);
            }
        }

        None
    }

    /// The buffer line a click landed on, if `pointer` is within the
    /// gutter's *leading* strip (`gutter_left` .. `marker_left`, the
    /// `space.sm` gap `paint_git_gutter_mark` draws into) on a line that
    /// actually has a mark -- checked alongside `fold_click_target`, a
    /// distinct lane so the two never fight over the same click
    /// (`docs/features/editor-git-gutter.md` §2.5).
    fn git_gutter_click_target(
        &self,
        pointer: egui::Pos2,
        origin: egui::Pos2,
        viewport: egui::Rect,
        visual: &VisualLines,
    ) -> Option<usize> {
        let gutter_left = origin.x + viewport.min.x;
        let marker_left = gutter_left + self.tokens.space.sm;
        if pointer.x < gutter_left || pointer.x > marker_left {
            return None;
        }
        let row = geometry::line_at_y(
            pointer.y - origin.y,
            self.metrics.row_height,
            visual.row_count(),
        );
        let line = visual.buffer_line(row);
        self.git_gutter_mark_at(line).map(|_| line)
    }

    /// The buffer line a click landed on, if `pointer` is within the
    /// blame lane and that line is covered by an annotation -- `None`
    /// whenever blame is off (`blame_lane_width` is `0.0`, so the range
    /// check below never matches).
    fn blame_click_target(
        &self,
        pointer: egui::Pos2,
        origin: egui::Pos2,
        viewport: egui::Rect,
        visual: &VisualLines,
    ) -> Option<usize> {
        if self.metrics.blame_lane_width <= 0.0 {
            return None;
        }
        let gutter_left = origin.x + viewport.min.x;
        let blame_right = gutter_left + self.metrics.blame_lane_width;
        if pointer.x < gutter_left || pointer.x > blame_right {
            return None;
        }
        let row = geometry::line_at_y(
            pointer.y - origin.y,
            self.metrics.row_height,
            visual.row_count(),
        );
        let line = visual.buffer_line(row);
        self.blame_annotation_at(line).map(|_| line)
    }
}

/// Wall-clock "now" for `blame_gutter::relative_time`'s labels -- read once
/// per `paint_gutter` call rather than per row, since the visible rows all
/// share the same "now" within a single frame. `unwrap_or(0)` treats a
/// pre-1970 system clock (never realistically hit) as maximally stale
/// rather than panicking on it.
fn blame_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The far end a column drag may reach from its anchor line. Same ceiling
/// `multiple-cursors.md` §4.8 puts on `⌃⌘G`, for the same reason: the
/// painter walks every selection for every visible line, so an unbounded
/// rectangle is a frozen frame rather than a big selection.
fn clamped_span(anchor: usize, line: usize) -> usize {
    let reach = MAX_OCCURRENCES - 1;
    if line >= anchor {
        line.min(anchor + reach)
    } else {
        line.max(anchor.saturating_sub(reach))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use geometry::Metrics;
    use ide_core::RUST;

    /// The harness' state: the widget's two owned pieces, plus the geometry
    /// the closure resolved this frame -- a click position can only be
    /// computed from the metrics the widget itself laid out under.
    struct Fixture {
        buffer: Buffer,
        editor: EditorState,
        origin: egui::Pos2,
        metrics: Option<Metrics>,
        /// Puts a focusable widget below the editor, which is what makes a
        /// focus-stealing arrow key observable at all.
        neighbour: bool,
        git_gutter_marks: Vec<GutterMark>,
        /// Captured from `EditorOutput` each frame -- the widget itself
        /// has no other observable side effect from a gutter-mark click
        /// (no caret move, no buffer change), so the test harness needs
        /// this to assert the click was actually recognised.
        git_gutter_clicked_line: Option<usize>,
        blame_on: bool,
        blame_annotations: Vec<BlameAnnotation>,
        blame_clicked_line: Option<usize>,
    }

    const EDITOR_ID: &str = "test-editor";

    fn editor_id() -> egui::Id {
        egui::Id::new(EDITOR_ID)
    }

    fn show(ui: &mut egui::Ui, state: &mut Fixture) {
        let tokens = Theme::Dark.tokens();
        let font_id = egui::TextStyle::Monospace.resolve(ui.style());
        let (row_height, char_width) = ui.fonts_mut(|f| {
            (
                f.row_height(&font_id),
                f.glyph_width(&font_id, ' ').max(1.0),
            )
        });
        let line_count = state.buffer.text_buffer().lines().line_count();
        state.origin = ui.cursor().min;
        state.metrics = Some(Metrics::new(
            row_height,
            char_width,
            geometry::digits_for(line_count),
            10,
            state.blame_on,
            &tokens.space,
        ));
        let marks = state.git_gutter_marks.clone();
        let annotations = state.blame_annotations.clone();
        let output = CodeEditor::new(
            editor_id(),
            &mut state.buffer,
            &mut state.editor,
            tokens,
            Theme::Dark,
        )
        .git_gutter_marks(&marks)
        .blame_annotations(state.blame_on, &annotations)
        .show(ui);
        state.git_gutter_clicked_line = output.git_gutter_clicked_line;
        state.blame_clicked_line = output.blame_clicked_line;
    }

    fn harness(text: &str) -> Harness<'static, Fixture> {
        harness_with(text, false)
    }

    fn harness_with(text: &str, neighbour: bool) -> Harness<'static, Fixture> {
        harness_with_marks(text, neighbour, Vec::new())
    }

    fn harness_with_marks(
        text: &str,
        neighbour: bool,
        git_gutter_marks: Vec<GutterMark>,
    ) -> Harness<'static, Fixture> {
        let mut buffer = Buffer::untitled();
        buffer.insert(0, text);
        Harness::builder()
            .with_size(egui::vec2(600.0, 400.0))
            // Default is a quarter-second per frame, which would put two
            // clicks further apart than the double-click window.
            .with_step_dt(0.01)
            .build_ui_state(
                |ui, state: &mut Fixture| {
                    if state.neighbour {
                        let size = egui::vec2(ui.available_width(), ui.available_height() - 60.0);
                        ui.allocate_ui(size, |ui| show(ui, state));
                        let _ = ui.button("below the editor");
                    } else {
                        show(ui, state);
                    }
                },
                Fixture {
                    buffer,
                    editor: EditorState::default(),
                    origin: egui::Pos2::ZERO,
                    metrics: None,
                    neighbour,
                    git_gutter_marks,
                    git_gutter_clicked_line: None,
                    blame_on: false,
                    blame_annotations: Vec::new(),
                    blame_clicked_line: None,
                },
            )
    }

    fn harness_with_blame(
        text: &str,
        annotations: Vec<BlameAnnotation>,
    ) -> Harness<'static, Fixture> {
        let mut harness = harness_with_marks(text, false, Vec::new());
        harness.state_mut().blame_on = true;
        harness.state_mut().blame_annotations = annotations;
        harness
    }

    /// Content-relative position of a column on a line, in screen space.
    fn at(harness: &Harness<'static, Fixture>, line: usize, column: f32) -> egui::Pos2 {
        let metrics = harness.state().metrics.expect("a frame has run");
        let origin = harness.state().origin;
        egui::pos2(
            origin.x + metrics.text_left + column * metrics.char_width,
            origin.y + (line as f32 + 0.5) * metrics.row_height,
        )
    }

    /// A point inside the gutter's leading strip (`paint_git_gutter_mark`'s
    /// own lane) on `line`, in screen space.
    fn gutter_leading_pos(harness: &Harness<'static, Fixture>, line: usize) -> egui::Pos2 {
        let metrics = harness.state().metrics.expect("a frame has run");
        let origin = harness.state().origin;
        let space = Theme::Dark.tokens().space.sm;
        egui::pos2(
            origin.x + space * 0.5,
            origin.y + (line as f32 + 0.5) * metrics.row_height,
        )
    }

    /// A point inside the blame lane (leftmost of the whole gutter when
    /// blame is on) on `line`, in screen space.
    fn blame_lane_pos(harness: &Harness<'static, Fixture>, line: usize) -> egui::Pos2 {
        let metrics = harness.state().metrics.expect("a frame has run");
        let origin = harness.state().origin;
        egui::pos2(
            origin.x + metrics.blame_lane_width * 0.5,
            origin.y + (line as f32 + 0.5) * metrics.row_height,
        )
    }

    fn click_times(harness: &mut Harness<'static, Fixture>, pos: egui::Pos2, times: usize) {
        harness.event(egui::Event::PointerMoved(pos));
        for _ in 0..times {
            for pressed in [true, false] {
                harness.event(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
        }
        harness.step();
    }

    fn click(harness: &mut Harness<'static, Fixture>, pos: egui::Pos2) {
        click_times(harness, pos, 1);
    }

    fn alt_click(harness: &mut Harness<'static, Fixture>, pos: egui::Pos2) {
        let alt = egui::Modifiers {
            alt: true,
            ..Default::default()
        };
        // `InputState::modifiers` only follows `ModifiersChanged`; the
        // modifiers on a pointer event never reach it, which is exactly how
        // a real backend reports a held ⌥.
        harness.event(egui::Event::ModifiersChanged(alt));
        harness.event(egui::Event::PointerMoved(pos));
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: alt,
            });
        }
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
        harness.step();
    }

    fn selection(harness: &Harness<'static, Fixture>) -> Selection {
        harness.state().buffer.text_buffer().selections().primary()
    }

    #[test]
    fn the_widget_renders_a_buffer_and_takes_click_and_keyboard_input() {
        let mut harness = harness("fn main() {}\nlet x = 1;\n");
        harness.step();
        harness.step();

        assert!(
            harness.state().editor.lines.len() >= 2,
            "every visible line should have been laid out"
        );

        // Third column of the second line: past the third glyph's start, so
        // the nearest boundary is unambiguous.
        let pos = at(&harness, 1, 3.2);
        click(&mut harness, pos);

        let head = selection(&harness).head;
        assert_eq!(head, 16, "caret should land where the click did");
        assert!(harness.state().buffer.text()[..head].ends_with("let"));

        harness.event(egui::Event::Text("Z".to_owned()));
        harness.step();
        assert_eq!(harness.state().buffer.text(), "fn main() {}\nletZ x = 1;\n");
    }

    #[test]
    fn a_double_click_selects_the_word_and_a_triple_click_the_line() {
        let mut harness = harness("one two\nthree four\n");
        harness.step();
        harness.step();

        let pos = at(&harness, 1, 7.0);
        click_times(&mut harness, pos, 2);
        let selected = selection(&harness).range();
        assert_eq!(
            &harness.state().buffer.text()[selected],
            "four",
            "a double click selects the word under the pointer"
        );

        click_times(&mut harness, pos, 1);
        let selected = selection(&harness).range();
        assert_eq!(
            &harness.state().buffer.text()[selected],
            "three four",
            "a third click within the window extends to the whole line"
        );
    }

    #[test]
    fn clicking_a_git_gutter_mark_reports_its_line_and_does_not_move_the_caret() {
        let marks = vec![GutterMark {
            line: 1,
            kind: GutterMarkKind::Modified,
        }];
        let mut harness = harness_with_marks("one\ntwo\nthree\n", false, marks);
        harness.step();
        harness.step();

        let before = selection(&harness).head;
        let pos = gutter_leading_pos(&harness, 1);
        click(&mut harness, pos);

        assert_eq!(harness.state().git_gutter_clicked_line, Some(1));
        assert_eq!(
            selection(&harness).head,
            before,
            "a gutter-mark click must not move the caret"
        );
    }

    #[test]
    fn clicking_the_leading_strip_on_an_unmarked_line_falls_through_to_a_normal_click() {
        let mut harness = harness_with_marks("one\ntwo\nthree\n", false, Vec::new());
        harness.step();
        harness.step();

        let pos = gutter_leading_pos(&harness, 1);
        click(&mut harness, pos);

        assert!(harness.state().git_gutter_clicked_line.is_none());
    }

    fn blame_annotation(line: usize, run_len: usize) -> BlameAnnotation {
        BlameAnnotation {
            line,
            run_len,
            commit_id: "abc123".to_string(),
            short_id: "abc123".to_string(),
            author: "Test".to_string(),
            timestamp: 0,
            summary: "a commit".to_string(),
        }
    }

    #[test]
    fn clicking_the_blame_lane_on_an_annotated_line_reports_it_and_does_not_move_the_caret() {
        let mut harness = harness_with_blame("one\ntwo\nthree\n", vec![blame_annotation(0, 3)]);
        harness.step();
        harness.step();
        let before = selection(&harness).head;

        let pos = blame_lane_pos(&harness, 1);
        click(&mut harness, pos);

        assert_eq!(harness.state().blame_clicked_line, Some(1));
        assert_eq!(
            selection(&harness).head,
            before,
            "a blame-lane click must not move the caret"
        );
    }

    #[test]
    fn clicking_the_blame_lane_when_blame_is_off_falls_through_to_a_normal_click() {
        let mut harness = harness_with_marks("one\ntwo\nthree\n", false, Vec::new());
        harness.step();
        harness.step();

        // Blame is off, so `metrics.blame_lane_width` is `0.0` -- this
        // point lands in what would be the blame lane if it were on, but
        // there is no lane to click at all.
        let pos = gutter_leading_pos(&harness, 1);
        click(&mut harness, pos);

        assert!(harness.state().blame_clicked_line.is_none());
    }

    #[test]
    fn blame_toggled_off_reserves_no_gutter_width() {
        let mut harness = harness_with_marks("one\ntwo\n", false, Vec::new());
        harness.step();
        assert_eq!(harness.state().metrics.unwrap().blame_lane_width, 0.0);
    }

    #[test]
    fn blame_toggled_on_reserves_gutter_width_even_with_no_annotations() {
        let mut harness = harness_with_blame("one\ntwo\n", Vec::new());
        harness.step();
        assert!(harness.state().metrics.unwrap().blame_lane_width > 0.0);
    }

    #[test]
    fn arrow_keys_do_not_hand_focus_to_a_neighbouring_widget() {
        let mut harness = harness_with("one\ntwo\nthree\n", true);
        harness.step();
        harness.step();

        let pos = at(&harness, 0, 0.0);
        click(&mut harness, pos);
        assert_eq!(selection(&harness).head, 0);
        // `set_focus_lock_filter` only takes effect once the widget has held
        // focus for a whole frame -- egui's own `TextEdit` has the same
        // one-frame window, and a real key press is never in the same frame
        // as the click that focused the editor.
        harness.step();

        // Without a focus lock, egui turns the first `ArrowDown` into a
        // focus move to the button below and the second one never reaches
        // the editor (doc §3.8).
        for _ in 0..2 {
            harness.key_press(egui::Key::ArrowDown);
            harness.step();
        }
        assert!(
            harness.ctx.memory(|m| m.has_focus(editor_id())),
            "the editor must keep focus through vertical movement"
        );
        assert_eq!(
            harness
                .state()
                .buffer
                .text_buffer()
                .lines()
                .line_at(selection(&harness).head),
            2,
            "both arrow presses should have reached the editor"
        );

        // `Tab` is text input here, not focus navigation -- and, since this
        // set, one indent unit rather than a literal tab (doc §1.2).
        harness.key_press(egui::Key::Tab);
        harness.step();
        assert!(harness
            .state()
            .buffer
            .text()
            .starts_with("one\ntwo\n    three"));
        assert!(harness.ctx.memory(|m| m.has_focus(editor_id())));
    }

    #[test]
    fn a_column_drag_reaches_at_most_max_occurrences_lines() {
        // Downwards, upwards, and the short drags that are not clamped at all.
        assert_eq!(clamped_span(0, 10), 10);
        assert_eq!(clamped_span(10, 0), 0);
        assert_eq!(clamped_span(0, MAX_OCCURRENCES * 5), MAX_OCCURRENCES - 1);
        assert_eq!(
            clamped_span(MAX_OCCURRENCES * 5, 0),
            MAX_OCCURRENCES * 4 + 1
        );
        assert_eq!(clamped_span(7, 7), 7);
    }

    #[test]
    fn alt_click_adds_a_caret_that_edits_alongside_the_first() {
        let mut harness = harness("one\ntwo\n");
        harness.step();
        harness.step();

        let first = at(&harness, 0, 0.0);
        click(&mut harness, first);
        let second = at(&harness, 1, 0.0);
        alt_click(&mut harness, second);
        assert_eq!(
            harness.state().buffer.text_buffer().selections().len(),
            2,
            "the second caret should have been added, not moved to"
        );

        harness.event(egui::Event::Text("#".to_owned()));
        harness.step();
        assert_eq!(harness.state().buffer.text(), "#one\n#two\n");

        // One transaction, so one undo step puts both back.
        harness.state_mut().buffer.text_buffer_mut().undo();
        assert_eq!(harness.state().buffer.text(), "one\ntwo\n");

        // A second ⌥Click on that caret removes it again.
        let second = at(&harness, 1, 0.0);
        alt_click(&mut harness, second);
        assert_eq!(harness.state().buffer.text_buffer().selections().len(), 1);
    }

    #[test]
    fn escape_collapses_the_cursors() {
        let mut harness = harness("one\ntwo\n");
        harness.step();
        harness.step();

        let first = at(&harness, 0, 0.0);
        click(&mut harness, first);
        let second = at(&harness, 1, 0.0);
        alt_click(&mut harness, second);
        assert_eq!(harness.state().buffer.text_buffer().selections().len(), 2);

        harness.step();
        harness.key_press(egui::Key::Escape);
        harness.step();
        assert_eq!(harness.state().buffer.text_buffer().selections().len(), 1);
    }

    #[test]
    fn tab_with_a_selection_indents_every_touched_line() {
        let mut harness = harness("a\nb\n");
        harness.step();
        harness.step();
        let pos = at(&harness, 0, 0.0);
        click(&mut harness, pos);
        harness.step();

        harness
            .state_mut()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 3)));
        harness.key_press(egui::Key::Tab);
        harness.step();
        assert_eq!(harness.state().buffer.text(), "    a\n    b\n");
    }

    #[test]
    fn enter_between_a_brace_pair_expands_to_three_lines_in_one_undo_step() {
        let mut harness = harness("fn main() {}");
        harness.state_mut().buffer.set_syntax(Some(&RUST));
        harness.step();
        harness.step();
        let pos = at(&harness, 0, 0.0);
        click(&mut harness, pos);
        harness.step();

        let brace = harness.state().buffer.text().find('{').unwrap() + 1;
        harness
            .state_mut()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(brace)));
        harness.key_press(egui::Key::Enter);
        harness.step();
        assert_eq!(harness.state().buffer.text(), "fn main() {\n    \n}");

        assert!(harness.state_mut().buffer.text_buffer_mut().undo());
        assert_eq!(harness.state().buffer.text(), "fn main() {}");
    }

    #[test]
    fn typing_an_opening_delimiter_with_a_selection_surrounds_it() {
        let mut harness = harness("word");
        harness.state_mut().buffer.set_syntax(Some(&RUST));
        harness.step();
        harness.step();
        let pos = at(&harness, 0, 0.0);
        click(&mut harness, pos);
        harness.step();

        harness
            .state_mut()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 4)));
        harness.event(egui::Event::Text("(".to_owned()));
        harness.step();
        assert_eq!(harness.state().buffer.text(), "(word)");
    }

    #[test]
    fn clicking_a_gutter_arrow_toggles_the_folds_collapsed_state() {
        let mut harness = harness("fn f() {\n    a();\n    b();\n}\nfn g() {}\n");
        harness.state_mut().buffer.set_syntax(Some(&RUST));
        harness.step();
        harness.step();

        let origin = harness.state().origin;
        let metrics = harness.state().metrics.expect("a frame has run");
        let tokens = Theme::Dark.tokens();
        let arrow = egui::pos2(
            origin.x + tokens.space.sm + geometry::MARKER_LANE_CHARS * metrics.char_width * 0.5,
            origin.y + 0.5 * metrics.row_height,
        );

        assert!(!harness.state().editor.is_folded(0));
        click(&mut harness, arrow);
        harness.step();
        assert!(harness.state().editor.is_folded(0));

        click(&mut harness, arrow);
        harness.step();
        assert!(!harness.state().editor.is_folded(0));
    }

    // ---- A4b: `rewrite`'s Extend/Shrink Selection vs. Clone Caret, and the
    // shrink stack's mouse-side clearing ----

    #[test]
    fn an_armed_double_tap_alt_still_clones_a_caret_up() {
        let mut harness = harness("abc\ndef\n");
        harness.step();
        harness.step();
        // `handle_keys` only runs while the widget has focus, which is only
        // granted on a click (`Frame::run`) -- every keyboard-driving test
        // needs one first.
        let pos = at(&harness, 0, 0.0);
        click(&mut harness, pos);
        harness
            .state_mut()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(4)));
        let alt = egui::Modifiers {
            alt: true,
            ..Default::default()
        };

        // Two rising edges of `⌥` inside the double-tap window arm the
        // gesture, exactly as a real ⌥⌥ press would.
        harness.event(egui::Event::ModifiersChanged(alt));
        harness.step();
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
        harness.step();
        harness.event(egui::Event::ModifiersChanged(alt));
        harness.step();

        harness.event(egui::Event::Key {
            key: egui::Key::ArrowUp,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: alt,
        });
        harness.step();

        assert!(harness
            .state()
            .buffer
            .text_buffer()
            .selections()
            .is_multiple());
    }

    #[test]
    fn an_unarmed_alt_up_extends_the_selection_instead_of_cloning() {
        let mut harness = harness("let a = 1;");
        harness.step();
        harness.step();
        let pos = at(&harness, 0, 0.0);
        click(&mut harness, pos);
        harness
            .state_mut()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(4))); // inside "a"
        let alt = egui::Modifiers {
            alt: true,
            ..Default::default()
        };

        harness.event(egui::Event::ModifiersChanged(alt));
        harness.event(egui::Event::Key {
            key: egui::Key::ArrowUp,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: alt,
        });
        harness.step();

        assert!(!harness
            .state()
            .buffer
            .text_buffer()
            .selections()
            .is_multiple());
        assert_eq!(
            harness.state().buffer.text_buffer().selections().primary(),
            Selection::new(4, 5)
        );
    }

    #[test]
    fn a_plain_click_clears_the_shrink_stack() {
        let mut harness = harness("abc\ndef\n");
        harness.step();
        harness.step();
        harness
            .state_mut()
            .editor
            .shrink_stack
            .push(Selections::single(Selection::caret(0)));

        let pos = at(&harness, 1, 1.0);
        click(&mut harness, pos);
        assert!(harness.state().editor.shrink_stack.is_empty());
    }

    #[test]
    fn a_plain_drag_clears_the_shrink_stack() {
        let mut harness = harness("abc\ndef\n");
        harness.step();
        harness.step();
        harness
            .state_mut()
            .editor
            .shrink_stack
            .push(Selections::single(Selection::caret(0)));

        let start = at(&harness, 0, 0.0);
        let end = at(&harness, 1, 2.0);
        harness.event(egui::Event::PointerMoved(start));
        harness.event(egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        harness.event(egui::Event::PointerMoved(end));
        harness.step();

        assert!(harness.state().editor.shrink_stack.is_empty());
    }
}
