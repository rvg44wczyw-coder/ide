//! Maps `ide_core`'s regex tokenizer output to `ratatui` styles
//! (`docs/features/tui-syntax-highlighting.md` §2.2), extended by
//! `docs/features/tui-semantic-highlighting.md` (`T14`) to merge LSP
//! semantic tokens in over that regex output, wherever the server has an
//! opinion. Pure functions of a `&TextBuffer`/line index, a `TokenKind`,
//! or a token slice -- no rendering, no `App` dependency; `ui.rs` calls
//! `styled_line` per line inside `render_editor`, after computing the
//! active buffer's semantic tokens once per frame via
//! `semantic_token_marks`.

use std::ops::Range;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use ide_core::{TextBuffer, Token, TokenKind};
use ide_lsp::{SemanticToken, SemanticTokenKind};

/// Mirrors `crates/ui/src/theme/mod.rs`'s `SyntaxColors::of` shape: ten
/// distinctly-colored variants, `Punctuation`/`Variable` both left at the
/// plain-text default (brackets are structure, not logic; `Variable` is a
/// semantic-highlighting target the regex tokenizer never produces on its
/// own -- see that variant's own doc comment in `crates/core/src/syntax.rs`).
pub fn style_for(kind: TokenKind) -> Style {
    let color = match kind {
        TokenKind::Keyword => Color::Magenta,
        TokenKind::String => Color::Green,
        TokenKind::Number => Color::LightYellow,
        TokenKind::Comment => Color::DarkGray,
        TokenKind::Key => Color::Blue,
        TokenKind::Function => Color::Cyan,
        TokenKind::Type => Color::LightCyan,
        TokenKind::Macro => Color::LightMagenta,
        TokenKind::Constant => Color::LightRed,
        TokenKind::Operator => Color::Red,
        TokenKind::Punctuation | TokenKind::Variable => return Style::default(),
    };
    Style::default().fg(color)
}

/// Converts raw, `Position`-based semantic tokens to absolute-byte-range
/// `ide_core::Token`s against `text`, mapping `SemanticTokenKind` to
/// `TokenKind` via `map_semantic_token_kind`'s exact table
/// (`docs/features/tui-semantic-highlighting.md` §2.3, ported from
/// `crates/ui/src/editor/paint.rs::semantic_token_marks`). A token whose
/// start or end position doesn't resolve to a valid byte offset
/// (`ide_lsp::position_to_byte_offset` returns `None`) is dropped, not
/// inserted -- tolerates a buggy or malicious server response the same
/// way the rest of this bridge already does. Uses `saturating_add` when
/// combining a `Position`'s `character` with `length`, since both are
/// untrusted `u32` values sourced directly from the language server.
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
/// `range.start` instead of the buffer's own regex-tokenizer storage --
/// slices a whole-buffer semantic-token list down to the entries
/// overlapping `range`.
pub fn tokens_in_range(tokens: &[Token], range: Range<usize>) -> &[Token] {
    let first = tokens.partition_point(|t| t.range.end <= range.start);
    let last = tokens.partition_point(|t| t.range.start < range.end);
    &tokens[first..last.max(first)]
}

/// Merges `semantic` over `regex` for one line's worth of tokens: keeps
/// every `regex` token that doesn't overlap any `semantic` token, appends
/// every `semantic` token verbatim, sorts the result by `range.start`.
///
/// Guarantees no two tokens in the returned `Vec` overlap -- load-bearing,
/// same reason `crates/ui/src/editor/paint.rs::merge_semantic_tokens`'s
/// own doc comment gives: `styled_line`'s span-building loop below
/// resolves each stretch of text to one style by walking tokens in order,
/// with no priority field to break a tie otherwise. Overlap (not an
/// exact-range match) is what triggers dropping the regex token, since a
/// semantic token's span doesn't always land on the same boundary the
/// regex tokenizer's own heuristics picked.
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

/// Converts each `ide_lsp::Range` (an LSP `DocumentHighlight` answer) to an
/// absolute byte `Range<usize>` against `text`; drops any entry whose start
/// or end doesn't resolve to a valid byte offset, or whose start doesn't
/// precede its end (`docs/features/tui-hover-and-inlay-hints.md` §2.3) --
/// same drop-on-invalid-conversion tolerance `semantic_token_marks` already
/// established. No `kind` mapping, unlike semantic tokens: a document
/// highlight is a plain background wash, not a colored token. Sorted by
/// start.
pub fn document_highlight_marks(text: &str, ranges: &[ide_lsp::Range]) -> Vec<Range<usize>> {
    let mut marks: Vec<Range<usize>> = ranges
        .iter()
        .filter_map(|r| {
            let start = ide_lsp::position_to_byte_offset(text, r.start)?;
            let end = ide_lsp::position_to_byte_offset(text, r.end)?;
            (start < end && end <= text.len()).then_some(start..end)
        })
        .collect();
    marks.sort_by_key(|r| r.start);
    marks
}

/// Converts each hint's `position` to a byte offset via
/// `ide_lsp::position_to_byte_offset`, dropping entries that don't
/// convert, and bakes `padding_left`/`padding_right` into the returned
/// label as a literal leading/trailing space (`docs/features/
/// tui-hover-and-inlay-hints.md` §2.3) -- `ide-ui` pads at paint time
/// instead; here it happens once at conversion time, since there's no
/// separate paint-time padding decision to make in a plain-text renderer.
/// Sorted by offset.
pub fn inlay_hint_chips(text: &str, hints: &[ide_lsp::InlayHint]) -> Vec<(usize, String)> {
    let mut chips: Vec<(usize, String)> = hints
        .iter()
        .filter_map(|h| {
            let offset = ide_lsp::position_to_byte_offset(text, h.position)?;
            let mut label = h.label.clone();
            if h.padding_right {
                label.push(' ');
            }
            if h.padding_left {
                label.insert(0, ' ');
            }
            Some((offset, label))
        })
        .collect();
    chips.sort_by_key(|(offset, _)| *offset);
    chips
}

/// Bundles `styled_line`'s four independent per-line overlay sources --
/// mirrors `ide-ui`'s own `paint.rs::LineContext` shape, needed now that
/// there are four (fg-coloring tokens, background-wash highlight ranges,
/// point-insertion inlay-hint chips, the matching-bracket-pair background
/// wash `docs/features/tui-smart-editing.md` §2.3/§3.3 adds) instead of
/// `tui-semantic-highlighting.md`'s single `semantic_tokens` parameter
/// (`docs/features/tui-hover-and-inlay-hints.md` §2.3). Every field is
/// whole-buffer data, converted once per frame by the caller (`ui.rs`'s
/// `render_editor`), not recomputed per line.
pub struct LineOverlays<'a> {
    pub semantic_tokens: &'a [Token],
    pub highlights: &'a [Range<usize>],
    pub inlay_hints: &'a [(usize, String)],
    /// The two (open, close) byte ranges of the bracket pair the caret
    /// currently touches, if any -- computed fresh each frame from
    /// `TextBuffer::matching_bracket`, same as every other overlay here
    /// (`tui-smart-editing.md` §3.3). Styled distinctly from `highlights`
    /// (`Color::Blue` rather than `Color::DarkGray`) so a matched bracket
    /// doesn't read as a plain document highlight.
    pub bracket_pair: &'a [Range<usize>],
    /// Every selection's range with `start() < end()` -- **including** the
    /// primary's, for visual consistency (`docs/features/
    /// tui-multiple-cursors.md` §2.3). A bare caret contributes nothing (an
    /// empty range clamps to nothing below, same as `highlights`/
    /// `bracket_pair` already handle). Distinct background (`Color::Yellow`)
    /// from `highlights` (`Color::DarkGray`) and `bracket_pair`
    /// (`Color::Blue`).
    pub selections: &'a [Range<usize>],
}

/// Builds `line`'s styled `Line` from `text_buffer.line_text(line)`,
/// `text_buffer.tokens_in_lines(line..line + 1)` (the regex tokenizer's
/// output for this line) merged with `overlays.semantic_tokens`' entries
/// overlapping this line (`tokens_in_range`, then `merge_semantic_tokens`
/// -- `docs/features/tui-semantic-highlighting.md` §2.3/§3.3), further
/// combined with `overlays.highlights` (background wash) and
/// `overlays.inlay_hints` (point-insertion chips) via a full boundary-list
/// walk (`docs/features/tui-hover-and-inlay-hints.md` §3.3): collect every
/// line bound, merged-token bound, and clamped-highlight bound into one
/// sorted, deduped list of byte offsets, then style each consecutive pair
/// as one contiguous span -- guarantees the no-gap/no-overlap covering of
/// `[line_start, line_end)` this function has always needed (see the
/// clamping rationale below, unchanged from the previous single-
/// `semantic_tokens`-parameter version), while still letting a highlighted
/// span and a colored token boundary land on different splits. Chip
/// offsets are folded into the same boundary list (a refinement over the
/// doc's literal §3.3 wording, which lists only token/highlight bounds --
/// needed so a chip renders at its exact position even when that position
/// isn't already a token or highlight edge; every consecutive pair a chip
/// offset creates is still a valid, correctly-styled split, so this never
/// weakens the no-overlap/no-gap invariant).
///
/// A token's/highlight's absolute byte range is **clamped** to this line's
/// own `[line_start, line_end]` before it contributes a boundary --
/// `tokens_in_lines` returns every token *overlapping* the queried line,
/// and a multi-line construct (a block comment spanning several lines is a
/// single `Token` per `crates/core/src/syntax.rs`'s `tokenize_span`) can
/// have a `range` that starts before `line_start` or ends after
/// `line_end`. Slicing with an unclamped boundary would either panic (out
/// of bounds) or pull in bytes belonging to a different line.
/// `line_start`/`line_end` and every regex token's own `range.start`/
/// `range.end` are always real `char` boundaries (line boundaries fall on
/// the single-byte `\n`; the tokenizer never splits a token
/// mid-character); a semantic token's or highlight's converted range is
/// checked against `text.len()` in `semantic_token_marks`/
/// `document_highlight_marks` but not independently char-boundary-checked
/// here beyond that -- `str` slicing on a non-boundary index panics, same
/// as the regex path already accepts as a precondition of well-formed
/// tokenizer output; an LSP `Position` that resolves to a byte offset via
/// `position_to_byte_offset` is guaranteed on a char boundary by that
/// function's own contract.
pub fn styled_line(
    text_buffer: &TextBuffer,
    line: usize,
    overlays: &LineOverlays<'_>,
) -> Line<'static> {
    let line_text = text_buffer.line_text(line).unwrap_or("");
    let Some(line_start) = text_buffer.lines().line_start(line) else {
        return Line::from(line_text.to_string());
    };
    let line_end = line_start + line_text.len();

    let regex_tokens = text_buffer.tokens_in_lines(line..line + 1);
    let semantic_for_line = tokens_in_range(overlays.semantic_tokens, line_start..line_end);
    let tokens = merge_semantic_tokens(regex_tokens, semantic_for_line);

    let highlights: Vec<Range<usize>> = overlays
        .highlights
        .iter()
        .filter_map(|h| {
            let start = h.start.clamp(line_start, line_end);
            let end = h.end.clamp(line_start, line_end);
            (start < end).then_some(start..end)
        })
        .collect();

    let bracket_pair: Vec<Range<usize>> = overlays
        .bracket_pair
        .iter()
        .filter_map(|h| {
            let start = h.start.clamp(line_start, line_end);
            let end = h.end.clamp(line_start, line_end);
            (start < end).then_some(start..end)
        })
        .collect();

    let selections: Vec<Range<usize>> = overlays
        .selections
        .iter()
        .filter_map(|s| {
            let start = s.start.clamp(line_start, line_end);
            let end = s.end.clamp(line_start, line_end);
            (start < end).then_some(start..end)
        })
        .collect();

    let chips: Vec<&(usize, String)> = overlays
        .inlay_hints
        .iter()
        .filter(|(offset, _)| (line_start..=line_end).contains(offset))
        .collect();

    let mut boundaries: Vec<usize> = vec![line_start, line_end];
    for token in &tokens {
        boundaries.push(token.range.start.clamp(line_start, line_end));
        boundaries.push(token.range.end.clamp(line_start, line_end));
    }
    for highlight in &highlights {
        boundaries.push(highlight.start);
        boundaries.push(highlight.end);
    }
    for pair in &bracket_pair {
        boundaries.push(pair.start);
        boundaries.push(pair.end);
    }
    for selection in &selections {
        boundaries.push(selection.start);
        boundaries.push(selection.end);
    }
    for (offset, _) in &chips {
        boundaries.push(*offset);
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let chip_spans_at = |offset: usize| -> Vec<Span<'static>> {
        chips
            .iter()
            .filter(|(o, _)| *o == offset)
            .map(|(_, label)| Span::styled(label.clone(), Style::default().fg(Color::DarkGray)))
            .collect()
    };

    let mut spans = Vec::new();
    for pair in boundaries.windows(2) {
        let (b0, b1) = (pair[0], pair[1]);
        spans.extend(chip_spans_at(b0));
        if b1 > b0 {
            let mut style = tokens
                .iter()
                .find(|t| t.range.start <= b0 && b1 <= t.range.end)
                .map(|t| style_for(t.kind))
                .unwrap_or_default();
            if highlights.iter().any(|h| h.start <= b0 && b1 <= h.end) {
                style = style.bg(Color::DarkGray);
            }
            if selections.iter().any(|s| s.start <= b0 && b1 <= s.end) {
                style = style.bg(Color::Yellow);
            }
            if bracket_pair.iter().any(|p| p.start <= b0 && b1 <= p.end) {
                style = style.bg(Color::Blue);
            }
            spans.push(Span::styled(
                line_text[b0 - line_start..b1 - line_start].to_string(),
                style,
            ));
        }
    }
    spans.extend(chip_spans_at(line_end));

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str, syntax: Option<&'static ide_core::SyntaxRules>) -> TextBuffer {
        TextBuffer::new(text, syntax)
    }

    fn no_overlays() -> LineOverlays<'static> {
        LineOverlays {
            semantic_tokens: &[],
            highlights: &[],
            inlay_hints: &[],
            bracket_pair: &[],
            selections: &[],
        }
    }

    #[test]
    fn style_for_maps_each_distinct_kind_to_its_own_color() {
        let expect = [
            (TokenKind::Keyword, Color::Magenta),
            (TokenKind::String, Color::Green),
            (TokenKind::Number, Color::LightYellow),
            (TokenKind::Comment, Color::DarkGray),
            (TokenKind::Key, Color::Blue),
            (TokenKind::Function, Color::Cyan),
            (TokenKind::Type, Color::LightCyan),
            (TokenKind::Macro, Color::LightMagenta),
            (TokenKind::Constant, Color::LightRed),
            (TokenKind::Operator, Color::Red),
        ];
        for (kind, color) in expect {
            assert_eq!(style_for(kind).fg, Some(color), "{kind:?}");
        }
        let colors: Vec<Color> = expect.iter().map(|(_, c)| *c).collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "colors must be pairwise distinct");
            }
        }
    }

    #[test]
    fn style_for_punctuation_and_variable_are_plain_default() {
        assert_eq!(style_for(TokenKind::Punctuation), Style::default());
        assert_eq!(style_for(TokenKind::Variable), Style::default());
    }

    #[test]
    fn styled_line_on_untokenized_buffer_is_one_plain_span() {
        let b = buffer("plain text", None);
        let line = styled_line(&b, 0, &no_overlays());
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "plain text");
        assert_eq!(line.spans[0].style, Style::default());
    }

    #[test]
    fn styled_line_on_out_of_range_line_is_a_safe_fallback() {
        let b = buffer("only one line", None);
        let line = styled_line(&b, 5, &no_overlays());
        let rebuilt: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, "", "an out-of-range line has no text");
    }

    #[test]
    fn styled_line_colors_a_keyword_and_leaves_the_rest_plain() {
        let b = buffer("let x = 1;", Some(&ide_core::RUST));
        let line = styled_line(&b, 0, &no_overlays());
        let keyword_span = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "let")
            .expect("`let` should be its own span");
        assert_eq!(keyword_span.style, style_for(TokenKind::Keyword));
        let rebuilt: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, "let x = 1;");
    }

    #[test]
    fn styled_line_clamps_a_multiline_block_comment_to_each_lines_own_bounds() {
        // A single block-comment token spans both lines -- confirm neither
        // line's styled_line call panics or pulls text from the other
        // line, and each line reconstructs exactly its own text.
        let b = buffer("/* one\ntwo */\n", Some(&ide_core::RUST));
        assert_eq!(b.lines().line_count(), 3);

        let line0 = styled_line(&b, 0, &no_overlays());
        let rebuilt0: String = line0.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt0, "/* one");
        assert!(line0
            .spans
            .iter()
            .all(|s| s.style == style_for(TokenKind::Comment)));

        let line1 = styled_line(&b, 1, &no_overlays());
        let rebuilt1: String = line1.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt1, "two */");
        assert!(line1
            .spans
            .iter()
            .all(|s| s.style == style_for(TokenKind::Comment)));
    }

    #[test]
    fn styled_line_handles_multibyte_utf8_tokens() {
        let b = buffer("// héllo wörld\n", Some(&ide_core::RUST));
        let line = styled_line(&b, 0, &no_overlays());
        let rebuilt: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, "// héllo wörld");
    }

    #[test]
    fn styled_line_a_semantic_token_overrides_the_regex_tokenizers_guess() {
        // "foo" is a plain identifier by shape alone -- the regex
        // tokenizer leaves it untyped (default style); a semantic token
        // saying it's a Type should color it as one instead.
        let b = buffer("foo + 1;\n", Some(&ide_core::RUST));
        let semantic = semantic_token_marks(
            "foo + 1;\n",
            &[SemanticToken {
                position: ide_lsp::Position {
                    line: 0,
                    character: 0,
                },
                length: 3,
                kind: SemanticTokenKind::Type,
            }],
        );
        let line = styled_line(
            &b,
            0,
            &LineOverlays {
                semantic_tokens: &semantic,
                highlights: &[],
                inlay_hints: &[],
                bracket_pair: &[],
                selections: &[],
            },
        );
        let foo_span = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "foo")
            .expect("\"foo\" should be its own span");
        assert_eq!(foo_span.style, style_for(TokenKind::Type));
    }

    fn semantic_token(
        line: u32,
        character: u32,
        length: u32,
        kind: SemanticTokenKind,
    ) -> SemanticToken {
        SemanticToken {
            position: ide_lsp::Position { line, character },
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
    fn document_highlight_marks_converts_ranges_to_byte_ranges() {
        let text = "let foo = 1;";
        let marks = document_highlight_marks(
            text,
            &[ide_lsp::Range {
                start: ide_lsp::Position {
                    line: 0,
                    character: 4,
                },
                end: ide_lsp::Position {
                    line: 0,
                    character: 7,
                },
            }],
        );
        assert_eq!(marks, vec![4..7]);
    }

    #[test]
    fn document_highlight_marks_drops_entries_that_dont_convert() {
        let marks = document_highlight_marks(
            "ab",
            &[ide_lsp::Range {
                start: ide_lsp::Position {
                    line: 9,
                    character: 0,
                },
                end: ide_lsp::Position {
                    line: 9,
                    character: 1,
                },
            }],
        );
        assert!(marks.is_empty());
    }

    #[test]
    fn document_highlight_marks_sorts_by_start_even_if_input_is_unsorted() {
        let text = "aaaa bbbb cccc";
        let range = |start: u32, end: u32| ide_lsp::Range {
            start: ide_lsp::Position {
                line: 0,
                character: start,
            },
            end: ide_lsp::Position {
                line: 0,
                character: end,
            },
        };
        let marks = document_highlight_marks(text, &[range(10, 14), range(0, 4)]);
        assert_eq!(marks, vec![0..4, 10..14]);
    }

    fn inlay_hint(
        character: u32,
        label: &str,
        padding_left: bool,
        padding_right: bool,
    ) -> ide_lsp::InlayHint {
        ide_lsp::InlayHint {
            position: ide_lsp::Position { line: 0, character },
            label: label.to_string(),
            padding_left,
            padding_right,
        }
    }

    #[test]
    fn inlay_hint_chips_bakes_padding_into_the_label() {
        let text = "let x = 1;";
        let chips = inlay_hint_chips(text, &[inlay_hint(5, ":i32", true, true)]);
        assert_eq!(chips, vec![(5, " :i32 ".to_string())]);
    }

    #[test]
    fn inlay_hint_chips_drops_entries_that_dont_convert() {
        let chips = inlay_hint_chips("ab", &[inlay_hint(9, "x", false, false)]);
        assert!(chips.is_empty());
    }

    #[test]
    fn inlay_hint_chips_sorts_by_offset_even_if_input_is_unsorted() {
        let text = "aaaa bbbb cccc";
        let chips = inlay_hint_chips(
            text,
            &[
                inlay_hint(10, "b", false, false),
                inlay_hint(0, "a", false, false),
            ],
        );
        assert_eq!(chips, vec![(0, "a".to_string()), (10, "b".to_string())]);
    }

    fn rebuild(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn styled_line_applies_a_background_to_a_highlighted_span() {
        let b = buffer("let x = 1;", None);
        let highlights = vec![4..5];
        let overlays = LineOverlays {
            semantic_tokens: &[],
            highlights: &highlights,
            inlay_hints: &[],
            bracket_pair: &[],
            selections: &[],
        };
        let line = styled_line(&b, 0, &overlays);
        assert_eq!(rebuild(&line), "let x = 1;");
        let x_span = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "x")
            .expect("\"x\" should be its own span, split out by the highlight bound");
        assert_eq!(x_span.style.bg, Some(Color::DarkGray));
    }

    #[test]
    fn styled_line_inserts_a_chip_before_its_target_offset() {
        // "x" is offset 4..5 -- give it its own semantic token so it's
        // already a boundary, then place the chip at that same offset
        // (right before "x") and confirm it renders as its own span,
        // ahead of "x"'s span.
        let b = buffer("let x = 1;", None);
        let semantic = semantic_token_marks(
            "let x = 1;",
            &[semantic_token(0, 4, 1, SemanticTokenKind::Variable)],
        );
        let overlays = LineOverlays {
            semantic_tokens: &semantic,
            highlights: &[],
            inlay_hints: &[(4, ": i32".to_string())],
            bracket_pair: &[],
            selections: &[],
        };
        let line = styled_line(&b, 0, &overlays);
        let chip_index = line
            .spans
            .iter()
            .position(|s| s.content.as_ref() == ": i32")
            .expect("chip span should be present");
        let x_index = line
            .spans
            .iter()
            .position(|s| s.content.as_ref() == "x")
            .expect("\"x\" should be its own span, split out by its semantic token");
        assert!(
            chip_index < x_index,
            "chip must render before the text at its offset"
        );
        // The chip is a pure insertion -- it doesn't consume any of the
        // buffer's own text, so reconstructing every span whose content
        // isn't the chip itself must still equal the original line.
        let without_chip: String = line
            .spans
            .iter()
            .filter(|s| s.content.as_ref() != ": i32")
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(without_chip, "let x = 1;");
    }

    #[test]
    fn styled_line_inserts_a_chip_positioned_at_the_end_of_the_line() {
        let b = buffer("let x = 1", None);
        let overlays = LineOverlays {
            semantic_tokens: &[],
            highlights: &[],
            inlay_hints: &[(9, ";".to_string())],
            bracket_pair: &[],
            selections: &[],
        };
        let line = styled_line(&b, 0, &overlays);
        assert_eq!(rebuild(&line), "let x = 1;");
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn styled_line_combines_fg_token_and_bg_highlight_on_the_same_span() {
        let b = buffer("foo + 1;", Some(&ide_core::RUST));
        let semantic = semantic_token_marks(
            "foo + 1;",
            &[semantic_token(0, 0, 3, SemanticTokenKind::Type)],
        );
        let highlights = vec![0..3];
        let overlays = LineOverlays {
            semantic_tokens: &semantic,
            highlights: &highlights,
            inlay_hints: &[],
            bracket_pair: &[],
            selections: &[],
        };
        let line = styled_line(&b, 0, &overlays);
        let foo_span = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "foo")
            .expect("\"foo\" should be its own span");
        assert_eq!(foo_span.style.fg, style_for(TokenKind::Type).fg);
        assert_eq!(foo_span.style.bg, Some(Color::DarkGray));
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn styled_line_boundary_walk_covers_the_line_with_no_gap_or_overlap() {
        let b = buffer("let foo = bar + 1;", Some(&ide_core::RUST));
        let semantic = semantic_token_marks(
            "let foo = bar + 1;",
            &[semantic_token(0, 4, 3, SemanticTokenKind::Variable)],
        );
        let highlights = vec![11..14];
        let overlays = LineOverlays {
            semantic_tokens: &semantic,
            highlights: &highlights,
            inlay_hints: &[(4, "«".to_string()), (19, "»".to_string())],
            bracket_pair: &[],
            selections: &[],
        };
        let line = styled_line(&b, 0, &overlays);
        // Reconstructing every span *except* the two pure-insertion chips
        // must exactly reproduce the original line, with nothing lost or
        // duplicated -- the no-gap/no-overlap covering invariant §4
        // requires, now proven with all three overlay sources active at
        // once instead of one at a time.
        let without_chips: String = line
            .spans
            .iter()
            .filter(|s| s.content.as_ref() != "«" && s.content.as_ref() != "»")
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(without_chips, "let foo = bar + 1;");
    }

    #[test]
    fn styled_line_applies_a_distinct_background_to_a_bracket_pair() {
        let b = buffer("let x = (1);", None);
        let bracket_pair = vec![8..9, 10..11];
        let overlays = LineOverlays {
            semantic_tokens: &[],
            highlights: &[],
            inlay_hints: &[],
            bracket_pair: &bracket_pair,
            selections: &[],
        };
        let line = styled_line(&b, 0, &overlays);
        assert_eq!(rebuild(&line), "let x = (1);");
        for content in ["(", ")"] {
            let span = line
                .spans
                .iter()
                .find(|s| s.content.as_ref() == content)
                .unwrap_or_else(|| panic!("{content:?} should be its own span"));
            assert_eq!(span.style.bg, Some(Color::Blue));
        }
        let one_span = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "1")
            .expect("\"1\" should be its own span, not part of the bracket wash");
        assert_eq!(one_span.style.bg, None);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn styled_line_applies_a_yellow_background_to_a_non_empty_selection() {
        let b = buffer("let x = 1;", None);
        let selections = vec![4..5];
        let overlays = LineOverlays {
            semantic_tokens: &[],
            highlights: &[],
            inlay_hints: &[],
            bracket_pair: &[],
            selections: &selections,
        };
        let line = styled_line(&b, 0, &overlays);
        let x_span = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "x")
            .expect("\"x\" should be its own span, split out by the selection bound");
        assert_eq!(x_span.style.bg, Some(Color::Yellow));
        for span in &line.spans {
            if span.content.as_ref() != "x" {
                assert_eq!(
                    span.style.bg, None,
                    "only the selected span should carry the yellow wash, found on {:?}",
                    span.content
                );
            }
        }
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn styled_line_a_bare_caret_selection_contributes_no_wash() {
        // A caret is an empty range (start == end) -- it must clamp to
        // nothing, exactly like an empty `highlights`/`bracket_pair` entry
        // already does, so it never creates a spurious boundary or wash.
        let b = buffer("let x = 1;", None);
        let selections = vec![4..4];
        let overlays = LineOverlays {
            semantic_tokens: &[],
            highlights: &[],
            inlay_hints: &[],
            bracket_pair: &[],
            selections: &selections,
        };
        let line = styled_line(&b, 0, &overlays);
        assert_eq!(rebuild(&line), "let x = 1;");
        assert!(line.spans.iter().all(|s| s.style.bg != Some(Color::Yellow)));
    }

    #[test]
    fn styled_line_two_adjacent_selections_wash_with_no_gap_or_double_wash() {
        // [1..3, 3..5) share the boundary at offset 3 -- the boundary walk
        // must still cover every character exactly once, whichever side of
        // the shared edge it falls on.
        let b = buffer("abcdef", None);
        let selections = vec![1..3, 3..5];
        let overlays = LineOverlays {
            semantic_tokens: &[],
            highlights: &[],
            inlay_hints: &[],
            bracket_pair: &[],
            selections: &selections,
        };
        let line = styled_line(&b, 0, &overlays);
        assert_eq!(rebuild(&line), "abcdef");
        // The shared boundary at offset 3 splits the two selections into
        // their own spans ("bc", "de") -- neither merges into the other nor
        // leaves a gap between them, and the unselected "a"/"f" stay
        // unwashed.
        for (content, expected_bg) in [
            ("a", None),
            ("bc", Some(Color::Yellow)),
            ("de", Some(Color::Yellow)),
            ("f", None),
        ] {
            let span = line
                .spans
                .iter()
                .find(|s| s.content.as_ref() == content)
                .unwrap_or_else(|| panic!("{content:?} should be its own span"));
            assert_eq!(span.style.bg, expected_bg, "mismatch for {content:?}");
        }
    }
}
