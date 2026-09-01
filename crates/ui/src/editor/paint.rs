//! Laying out and caching one line at a time. The three-dimension
//! composition -- syntax colour, diagnostic underline, `Cmd`-hover link
//! underline -- is the boundary-merge that used to run over the whole file
//! in `render.rs`; only its input span narrowed to a single line
//! (`docs/features/code-editor-widget.md` §3.2).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use ide_core::{Token, TokenKind};
use ide_lsp::{Diagnostic, DiagnosticSeverity, SemanticToken, SemanticTokenKind};

use crate::theme::{self, Tokens};

/// Diagnostic ranges as byte offsets, sorted and de-overlapped: a range that
/// starts before the previous kept one's end is dropped entirely rather than
/// clipped -- the same "first wins" precedence the whole-file version had.
pub fn diagnostic_marks(
    text: &str,
    diagnostics: &[Diagnostic],
) -> Vec<(usize, usize, DiagnosticSeverity)> {
    let mut marks: Vec<(usize, usize, DiagnosticSeverity)> = diagnostics
        .iter()
        .filter_map(|d| {
            let start = ide_lsp::position_to_byte_offset(text, d.range.start)?;
            let end = ide_lsp::position_to_byte_offset(text, d.range.end)?;
            (start < end && end <= text.len()).then_some((start, end, d.severity))
        })
        .collect();
    marks.sort_by_key(|(start, _, _)| *start);

    let mut kept = Vec::with_capacity(marks.len());
    let mut cursor = 0;
    for mark in marks {
        if mark.0 < cursor {
            continue;
        }
        cursor = mark.1;
        kept.push(mark);
    }
    kept
}

/// `DocumentHighlight` ranges as buffer byte ranges, same conversion
/// `diagnostic_marks` already does per entry (`ide_lsp::position_to_byte_
/// offset(text, range.start)`/`..end`) -- document-wide absolute offsets,
/// made row-relative later inside the widget the same two-step way §3.5
/// converts an `InlayHint.position` (`docs/features/inlay-hints-and-hover.md`
/// §3.4). Unlike `diagnostic_marks`, no overlap-dropping: overlapping
/// highlights are harmless to paint (same colour either way), so every
/// convertible entry is kept.
pub fn document_highlight_marks(text: &str, highlights: &[ide_lsp::Range]) -> Vec<Range<usize>> {
    highlights
        .iter()
        .filter_map(|r| {
            let start = ide_lsp::position_to_byte_offset(text, r.start)?;
            let end = ide_lsp::position_to_byte_offset(text, r.end)?;
            (start < end && end <= text.len()).then_some(start..end)
        })
        .collect()
}

/// A small lightbulb glyph in the gutter's marker lane, painted on the line
/// a code action is available at -- the same lane `paint_fold_arrow`
/// already draws into, mirroring how that marker is drawn
/// (`docs/features/code-actions.md` §2.3/§6). A filled circle (the bulb)
/// plus a short stroke underneath (the base), rather than a real bitmap
/// icon -- matches `paint_fold_arrow`'s own "a shape, not an asset" choice.
pub fn paint_code_action_marker(
    painter: &egui::Painter,
    marker_left: f32,
    top: f32,
    row_height: f32,
    char_width: f32,
    color: egui::Color32,
) {
    let cx = marker_left + super::geometry::MARKER_LANE_CHARS * char_width * 0.5;
    let cy = top + row_height * 0.5;
    let r = row_height * 0.22;
    painter.circle_filled(egui::pos2(cx, cy - r * 0.3), r, color);
    let base_half_width = r * 0.55;
    let base_y = cy + r * 0.9;
    painter.line_segment(
        [
            egui::pos2(cx - base_half_width, base_y),
            egui::pos2(cx + base_half_width, base_y),
        ],
        egui::Stroke::new(1.5, color),
    );
}

/// Buffer-byte-range `Token`s decoded from `tokens`, converting `ide_lsp::
/// SemanticToken`'s `Position`+UTF-16-`length` shape the same way
/// `document_highlight_marks` converts `document_highlights` -- an entry
/// whose start or end position doesn't convert (buggy/malicious server, or
/// a transient client/server text desync) is skipped, not inserted
/// (`docs/features/semantic-highlighting.md` §3.2). `length`/`character`
/// are added with `saturating_add`, not `+`: a language server is
/// untrusted input, and a `length`/`position.character` pair chosen to
/// overflow a plain `u32` addition must degrade to "this token converts to
/// an out-of-range position, gets skipped" rather than panic (same
/// defensive discipline `ide-lsp`'s own delta-decode already applies, see
/// `docs/security-findings/rust-lsp-dev-semantic-highlighting-2026-08-25.md`).
/// Sorted by `range.start` on return -- not assumed pre-sorted, since
/// `merge_semantic_tokens`/`tokens_in_range`'s binary search both depend on
/// that postcondition.
pub fn semantic_token_marks(text: &str, tokens: &[SemanticToken]) -> Vec<Token> {
    let mut marks: Vec<Token> = tokens
        .iter()
        .filter_map(|t| {
            let start = ide_lsp::position_to_byte_offset(text, t.position)?;
            let end_position = ide_lsp::Position {
                line: t.position.line,
                character: t.position.character.saturating_add(t.length),
            };
            let end = ide_lsp::position_to_byte_offset(text, end_position)?;
            (start < end && end <= text.len()).then_some(Token {
                range: start..end,
                kind: map_semantic_token_kind(t.kind),
            })
        })
        .collect();
    marks.sort_by_key(|t| t.range.start);
    marks
}

/// `docs/features/semantic-highlighting.md` §3.2's exact table: every
/// `SemanticTokenKind` maps 1:1 onto the `ide_core::TokenKind` of the same
/// name.
fn map_semantic_token_kind(kind: SemanticTokenKind) -> TokenKind {
    match kind {
        SemanticTokenKind::Type => TokenKind::Type,
        SemanticTokenKind::Function => TokenKind::Function,
        SemanticTokenKind::Macro => TokenKind::Macro,
        SemanticTokenKind::Keyword => TokenKind::Keyword,
        SemanticTokenKind::String => TokenKind::String,
        SemanticTokenKind::Number => TokenKind::Number,
        SemanticTokenKind::Comment => TokenKind::Comment,
        SemanticTokenKind::Operator => TokenKind::Operator,
        SemanticTokenKind::Variable => TokenKind::Variable,
    }
}

/// The same `partition_point` binary search `TextBuffer::tokens_in_lines`
/// already uses internally, applied to an arbitrary token slice sorted by
/// `range.start` instead of the buffer's own regex-tokenizer output --
/// slices a whole-buffer semantic-token list down to one row's byte range
/// the same way (`docs/features/semantic-highlighting.md` §3.4).
pub fn tokens_in_range(tokens: &[Token], range: Range<usize>) -> &[Token] {
    let first = tokens.partition_point(|t| t.range.end <= range.start);
    let last = tokens.partition_point(|t| t.range.start < range.end);
    &tokens[first..last.max(first)]
}

/// Merges `semantic` over `regex` for one row's worth of tokens: keeps
/// every `regex` token that doesn't overlap any `semantic` token, appends
/// every `semantic` token verbatim, sorts the result by `range.start`.
///
/// Guarantees no two tokens in the returned `Vec` overlap. This is
/// load-bearing: `line_layout_job`'s boundary walk resolves a sub-range's
/// colour via the *first* token in iteration order whose range contains
/// the point, with no explicit priority field to break a tie otherwise --
/// any future change to how the merged slice is built must preserve this
/// postcondition or colour resolution becomes order-dependent by accident
/// instead of correct by construction (`docs/features/
/// semantic-highlighting.md` §2.3, §4).
///
/// A semantic token's span doesn't always land on the same boundary the
/// regex tokenizer's own heuristics picked, so overlap (not an exact-range
/// match) is what triggers dropping the regex token -- an exact-match-only
/// rule would silently fail to override in every case the two tokenizers
/// disagree about span boundaries, precisely the cases where the semantic
/// answer is most worth having (§3.4).
pub fn merge_semantic_tokens(regex: &[Token], semantic: &[Token]) -> Vec<Token> {
    fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
        a.start < b.end && b.start < a.end
    }
    let mut merged: Vec<Token> = regex
        .iter()
        .filter(|r| !semantic.iter().any(|s| overlaps(&r.range, &s.range)))
        .cloned()
        .collect();
    merged.extend(semantic.iter().cloned());
    merged.sort_by_key(|t| t.range.start);
    merged
}

pub struct LineContext<'a> {
    pub font_id: egui::FontId,
    pub text_color: egui::Color32,
    pub tokens: &'a Tokens,
    pub syntax: &'a [Token],
    pub marks: &'a [(usize, usize, DiagnosticSeverity)],
    pub link: Option<&'a Range<usize>>,
}

/// Builds the `LayoutJob` for one line. `line` is the line's byte range in
/// the buffer; every overlay range is absolute and gets clipped to it here,
/// so callers hand over whole-buffer data and this decides what lands.
pub fn line_layout_job(
    text: &str,
    line: Range<usize>,
    context: &LineContext<'_>,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    // No wrapping: one line is exactly one row, which is what makes the
    // viewport arithmetic in `geometry` exact (doc §1.1).
    job.wrap.max_width = f32::INFINITY;
    if line.start >= line.end {
        return job;
    }

    let mut boundaries: Vec<usize> = vec![line.start, line.end];
    for token in context.syntax {
        boundaries.push(token.range.start);
        boundaries.push(token.range.end);
    }
    for (start, end, _) in context.marks {
        boundaries.push(*start);
        boundaries.push(*end);
    }
    if let Some(range) = context.link {
        boundaries.push(range.start);
        boundaries.push(range.end);
    }
    boundaries.retain(|b| line.contains(b) || *b == line.end);
    boundaries.sort_unstable();
    boundaries.dedup();

    for pair in boundaries.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        if from >= to {
            continue;
        }
        let color = context
            .syntax
            .iter()
            .find(|t| t.range.start <= from && from < t.range.end)
            .map(|t| context.tokens.syntax.of(t.kind, context.text_color))
            .unwrap_or(context.text_color);
        let underline = context
            .marks
            .iter()
            .find(|(start, end, _)| *start <= from && from < *end)
            .map(|(_, _, severity)| theme::severity_color(context.tokens, *severity))
            // A hovered link is underlined in its own colour, so it reads as
            // a link without a second colour competing with the palette.
            .or_else(|| {
                context
                    .link
                    .filter(|range| range.contains(&from))
                    .map(|_| color)
            });

        let mut format = egui::TextFormat::simple(context.font_id.clone(), color);
        if let Some(underline_color) = underline {
            format.underline = egui::Stroke::new(1.5, underline_color);
        }
        job.append(&text[from..to], 0.0, format);
    }
    job
}

/// Galleys for the lines on screen. An entry is reused only when the line's
/// text is byte-for-byte what it was laid out from -- cheaper than laying it
/// out again, and unlike a revision counter it cannot go stale if anything
/// ever edits the buffer from outside the widget (doc §4.2).
#[derive(Default)]
pub struct LineCache {
    entries: HashMap<usize, (String, Arc<egui::Galley>)>,
}

impl LineCache {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn galley(
        &mut self,
        line: usize,
        text: &str,
        build: impl FnOnce() -> Arc<egui::Galley>,
    ) -> Arc<egui::Galley> {
        if let Some((cached_text, galley)) = self.entries.get(&line) {
            if cached_text == text {
                return galley.clone();
            }
        }
        let galley = build();
        self.entries
            .insert(line, (text.to_string(), galley.clone()));
        galley
    }

    /// Drops everything outside `keep`, so the cache stays bounded by the
    /// window height rather than growing with the file.
    pub fn retain(&mut self, keep: Range<usize>) {
        self.entries.retain(|line, _| keep.contains(line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::TokenKind;
    use ide_lsp::{Position, Range as LspRange};

    /// Even a test must not name a colour: the palette is the only place
    /// colours come from (`fleet-look-foundation.md` §4.1).
    fn fg() -> egui::Color32 {
        crate::theme::Theme::Dark.tokens().color.fg_primary
    }

    /// A bare `Context` has no font atlas until it has run a frame, and the
    /// cache is what these tests are about, not layout.
    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        ctx
    }

    fn galley(ctx: &egui::Context, text: &str) -> Arc<egui::Galley> {
        ctx.fonts_mut(|f| f.layout_no_wrap(text.to_string(), egui::FontId::monospace(12.0), fg()))
    }

    fn diagnostic(line: u32, from: u32, to: u32, severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: LspRange {
                start: Position {
                    line,
                    character: from,
                },
                end: Position {
                    line,
                    character: to,
                },
            },
            severity,
            message: String::new(),
        }
    }

    #[test]
    fn marks_are_sorted_and_overlaps_dropped_whole() {
        let text = "abcdefghij";
        let marks = diagnostic_marks(
            text,
            &[
                diagnostic(0, 4, 8, DiagnosticSeverity::Warning),
                diagnostic(0, 0, 5, DiagnosticSeverity::Error),
            ],
        );
        assert_eq!(marks.len(), 1);
        assert_eq!((marks[0].0, marks[0].1), (0, 5));
    }

    #[test]
    fn marks_outside_the_text_are_dropped() {
        let marks = diagnostic_marks("ab", &[diagnostic(9, 0, 1, DiagnosticSeverity::Error)]);
        assert!(marks.is_empty());
    }

    fn lsp_range(line: u32, from: u32, to: u32) -> LspRange {
        LspRange {
            start: Position {
                line,
                character: from,
            },
            end: Position {
                line,
                character: to,
            },
        }
    }

    #[test]
    fn document_highlight_marks_converts_positions_to_byte_ranges() {
        let text = "let a = 1;\nlet b = 2;";
        let marks = document_highlight_marks(text, &[lsp_range(0, 4, 5), lsp_range(1, 4, 5)]);
        assert_eq!(marks, vec![4..5, 15..16]);
    }

    #[test]
    fn document_highlight_marks_keeps_overlapping_entries_unlike_diagnostic_marks() {
        let text = "abcdefghij";
        let marks = document_highlight_marks(text, &[lsp_range(0, 0, 5), lsp_range(0, 4, 8)]);
        assert_eq!(marks, vec![0..5, 4..8]);
    }

    #[test]
    fn document_highlight_marks_outside_the_text_are_dropped() {
        let marks = document_highlight_marks("ab", &[lsp_range(9, 0, 1)]);
        assert!(marks.is_empty());
    }

    #[test]
    fn document_highlight_marks_on_an_empty_range_is_dropped() {
        let marks = document_highlight_marks("abc", &[lsp_range(0, 2, 2)]);
        assert!(marks.is_empty());
    }

    fn semantic_token(
        line: u32,
        character: u32,
        length: u32,
        kind: SemanticTokenKind,
    ) -> SemanticToken {
        SemanticToken {
            position: Position { line, character },
            length,
            kind,
        }
    }

    #[test]
    fn semantic_token_marks_converts_position_and_length_to_a_byte_range() {
        let text = "let foo = 1;";
        let marks = semantic_token_marks(
            text,
            &[semantic_token(0, 4, 3, SemanticTokenKind::Variable)],
        );
        assert_eq!(
            marks,
            vec![Token {
                range: 4..7,
                kind: TokenKind::Variable
            }]
        );
    }

    #[test]
    fn semantic_token_marks_maps_every_kind_by_name() {
        let cases = [
            (SemanticTokenKind::Type, TokenKind::Type),
            (SemanticTokenKind::Function, TokenKind::Function),
            (SemanticTokenKind::Macro, TokenKind::Macro),
            (SemanticTokenKind::Keyword, TokenKind::Keyword),
            (SemanticTokenKind::String, TokenKind::String),
            (SemanticTokenKind::Number, TokenKind::Number),
            (SemanticTokenKind::Comment, TokenKind::Comment),
            (SemanticTokenKind::Operator, TokenKind::Operator),
            (SemanticTokenKind::Variable, TokenKind::Variable),
        ];
        let text = "xxxxxxxxxx";
        for (semantic, expected) in cases {
            let marks = semantic_token_marks(text, &[semantic_token(0, 0, 1, semantic)]);
            assert_eq!(marks[0].kind, expected, "mismatch for {semantic:?}");
        }
    }

    #[test]
    fn semantic_token_marks_skips_entries_that_dont_convert() {
        let marks = semantic_token_marks("ab", &[semantic_token(9, 0, 1, SemanticTokenKind::Type)]);
        assert!(marks.is_empty());
    }

    #[test]
    fn semantic_token_marks_sorts_by_range_start_even_if_input_is_unsorted() {
        let text = "aaaa bbbb cccc";
        let marks = semantic_token_marks(
            text,
            &[
                semantic_token(0, 10, 4, SemanticTokenKind::Type),
                semantic_token(0, 0, 4, SemanticTokenKind::Keyword),
            ],
        );
        assert_eq!(marks[0].range, 0..4);
        assert_eq!(marks[1].range, 10..14);
    }

    #[test]
    fn semantic_token_marks_saturates_instead_of_overflowing_on_an_extreme_length() {
        let marks = semantic_token_marks(
            "ab",
            &[semantic_token(0, 0, u32::MAX, SemanticTokenKind::Type)],
        );
        // `character` saturates far past the text's length --
        // `position_to_byte_offset` then correctly rejects it as
        // out-of-range, so the entry is skipped rather than panicking.
        assert!(marks.is_empty());
    }

    #[test]
    fn tokens_in_range_slices_to_tokens_overlapping_the_given_byte_range() {
        let tokens = vec![
            Token {
                range: 0..3,
                kind: TokenKind::Keyword,
            },
            Token {
                range: 5..8,
                kind: TokenKind::Type,
            },
            Token {
                range: 10..13,
                kind: TokenKind::Function,
            },
        ];
        let slice = tokens_in_range(&tokens, 4..9);
        assert_eq!(
            slice,
            &[Token {
                range: 5..8,
                kind: TokenKind::Type
            }]
        );
    }

    #[test]
    fn tokens_in_range_empty_slice_for_an_empty_range() {
        let tokens = vec![Token {
            range: 0..3,
            kind: TokenKind::Keyword,
        }];
        assert!(tokens_in_range(&tokens, 10..10).is_empty());
    }

    #[test]
    fn merge_semantic_tokens_with_no_semantic_input_degrades_to_regex_untouched() {
        let regex = vec![Token {
            range: 0..3,
            kind: TokenKind::Keyword,
        }];
        let merged = merge_semantic_tokens(&regex, &[]);
        assert_eq!(merged, regex);
    }

    #[test]
    fn merge_semantic_tokens_with_no_regex_input_still_returns_semantic_tokens() {
        let semantic = vec![Token {
            range: 0..3,
            kind: TokenKind::Variable,
        }];
        let merged = merge_semantic_tokens(&[], &semantic);
        assert_eq!(merged, semantic);
    }

    #[test]
    fn merge_semantic_tokens_drops_an_overlapping_regex_token_and_keeps_the_semantic_one() {
        let regex = vec![Token {
            range: 0..3,
            kind: TokenKind::Type,
        }];
        let semantic = vec![Token {
            range: 1..4,
            kind: TokenKind::Variable,
        }];
        let merged = merge_semantic_tokens(&regex, &semantic);
        assert_eq!(
            merged,
            vec![Token {
                range: 1..4,
                kind: TokenKind::Variable
            }]
        );
    }

    #[test]
    fn merge_semantic_tokens_keeps_a_non_overlapping_regex_token_alongside_a_semantic_one() {
        let regex = vec![Token {
            range: 0..3,
            kind: TokenKind::Keyword,
        }];
        let semantic = vec![Token {
            range: 5..8,
            kind: TokenKind::Variable,
        }];
        let merged = merge_semantic_tokens(&regex, &semantic);
        assert_eq!(
            merged,
            vec![
                Token {
                    range: 0..3,
                    kind: TokenKind::Keyword
                },
                Token {
                    range: 5..8,
                    kind: TokenKind::Variable
                },
            ]
        );
    }

    #[test]
    fn merge_semantic_tokens_output_never_overlaps_even_with_multiple_touching_regex_tokens() {
        let regex = vec![
            Token {
                range: 0..5,
                kind: TokenKind::Type,
            },
            Token {
                range: 3..8,
                kind: TokenKind::Function,
            },
            Token {
                range: 20..25,
                kind: TokenKind::Keyword,
            },
        ];
        let semantic = vec![Token {
            range: 4..6,
            kind: TokenKind::Variable,
        }];
        let merged = merge_semantic_tokens(&regex, &semantic);
        for pair in merged.windows(2) {
            assert!(
                pair[0].range.end <= pair[1].range.start,
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(merged
            .iter()
            .any(|t| t.range == (4..6) && t.kind == TokenKind::Variable));
        assert!(merged.iter().any(|t| t.range == (20..25)));
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn an_empty_line_lays_out_to_nothing() {
        let tokens = crate::theme::Theme::Dark.tokens();
        let context = LineContext {
            font_id: egui::FontId::monospace(12.0),
            text_color: fg(),
            tokens,
            syntax: &[],
            marks: &[],
            link: None,
        };
        let job = line_layout_job("a\n", 2..2, &context);
        assert!(job.sections.is_empty());
        assert!(job.text.is_empty());
    }

    #[test]
    fn overlays_split_a_line_into_sections_and_stay_inside_it() {
        let text = "let a = 1;\nlet b = 2;";
        let tokens = crate::theme::Theme::Dark.tokens();
        let syntax = vec![
            Token {
                range: 0..3,
                kind: TokenKind::Keyword,
            },
            // On the *next* line: must not leak into this one's job.
            Token {
                range: 11..14,
                kind: TokenKind::Keyword,
            },
        ];
        let marks = vec![(4, 5, DiagnosticSeverity::Error)];
        let context = LineContext {
            font_id: egui::FontId::monospace(12.0),
            text_color: fg(),
            tokens,
            syntax: &syntax,
            marks: &marks,
            link: None,
        };
        let job = line_layout_job(text, 0..10, &context);
        assert_eq!(job.text, "let a = 1;");
        assert!(job.sections.len() >= 3);
        assert_eq!(
            job.sections[0].format.color,
            tokens.syntax.of(TokenKind::Keyword, fg())
        );
        assert!(job.sections.iter().any(|s| s.format.underline.width > 0.0));
    }

    #[test]
    fn a_link_underlines_in_the_span_own_color() {
        let text = "name";
        let tokens = crate::theme::Theme::Dark.tokens();
        let link = 0..4;
        let context = LineContext {
            font_id: egui::FontId::monospace(12.0),
            text_color: fg(),
            tokens,
            syntax: &[],
            marks: &[],
            link: Some(&link),
        };
        let job = line_layout_job(text, 0..4, &context);
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.underline.color, fg());
    }

    #[test]
    fn the_cache_reuses_an_untouched_line_and_rebuilds_an_edited_one() {
        let ctx = context();
        let mut cache = LineCache::default();

        let first = cache.galley(0, "abc", || galley(&ctx, "abc"));
        let again = cache.galley(0, "abc", || panic!("must not rebuild an untouched line"));
        assert!(Arc::ptr_eq(&first, &again));

        let edited = cache.galley(0, "abcd", || galley(&ctx, "abcd"));
        assert!(!Arc::ptr_eq(&first, &edited));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn the_cache_evicts_lines_outside_the_viewport() {
        let ctx = context();
        let mut cache = LineCache::default();
        for line in 0..10 {
            cache.galley(line, "x", || galley(&ctx, "x"));
        }
        assert_eq!(cache.len(), 10);
        cache.retain(3..6);
        assert_eq!(cache.len(), 3);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }
}
