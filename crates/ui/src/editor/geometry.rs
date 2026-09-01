//! Pure geometry for the editor: no `Ui`, no state, no painting. Everything
//! here is a function of the monospace font's metrics and the buffer's line
//! index, which is what makes it the tested core of the widget
//! (`docs/features/code-editor-widget.md` §2.4).

use std::ops::Range;

use ide_core::{Selection, TextBuffer};

use crate::theme::Spacing;

/// Fixed geometry for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub row_height: f32,
    pub char_width: f32,
    pub gutter_width: f32,
    /// Reserved width of the blame lane, `0.0` when blame is off for this
    /// tab (`git-branches-and-blame.md` §2.2.3) -- `paint_gutter`'s own
    /// blame-label painting and `blame_click_target`'s hit-testing both
    /// key off this rather than re-deriving it, so there is exactly one
    /// place the on/off width decision is made.
    pub blame_lane_width: f32,
    /// X of the first text column, relative to the content origin.
    pub text_left: f32,
    /// Rows that fit in the viewport, for `PageUp`/`PageDown`.
    pub page_rows: usize,
}

impl Metrics {
    /// The gutter is an optional blame lane, then a marker lane, then
    /// `digits` digit cells, plus padding, so it steps once when the file
    /// crosses a power of ten rather than jittering as you scroll --
    /// `blame_on` is likewise the only thing that changes `blame_lane_
    /// width`, never the annotation text itself, so toggling blame off and
    /// back on for the same file never jitters the gutter mid-session
    /// either (§2.2.3's "does not vary per-frame or per-annotation-text-
    /// length" constraint).
    pub fn new(
        row_height: f32,
        char_width: f32,
        digits: u32,
        page_rows: usize,
        blame_on: bool,
        space: &Spacing,
    ) -> Self {
        let blame_lane_width = if blame_on {
            BLAME_LANE_CHARS * char_width + space.sm
        } else {
            0.0
        };
        let gutter_width = blame_lane_width
            + space.sm
            + MARKER_LANE_CHARS * char_width
            + space.sm
            + digits as f32 * char_width;
        Self {
            row_height,
            char_width,
            gutter_width,
            blame_lane_width,
            text_left: gutter_width + space.md,
            page_rows,
        }
    }
}

/// Width reserved for git bars (E7), breakpoints (F5), fold arrows (A6) and
/// diagnostic icons -- the gutter must not resize as they arrive or as fold
/// state changes. `pub(crate)` so `editor/mod.rs`'s gutter painting and
/// click hit-testing can position the fold arrow within this exact lane
/// (`code-folding.md` §3.6).
pub(crate) const MARKER_LANE_CHARS: f32 = 2.0;

/// Character budget for the blame lane's `"author, Nx ago"` label --
/// enough for e.g. `"jdoe, 3 days ago"` before truncation kicks in
/// (`git-branches-and-blame.md` §2.2.3). Placed left of `MARKER_LANE_CHARS`
/// (JetBrains/VS Code both put blame left of line numbers), a fixed budget
/// for the same "gutter must not resize" reason as that lane.
pub(crate) const BLAME_LANE_CHARS: f32 = 18.0;

/// Decimal width of the largest line number, never less than 1.
pub fn digits_for(line_count: usize) -> u32 {
    line_count.max(1).ilog10() + 1
}

/// Half-open range of lines to paint for `viewport`, clamped to
/// `line_count`. One row past each edge, so a partially visible line is
/// still drawn.
pub fn visible_lines(viewport: egui::Rect, row_height: f32, line_count: usize) -> Range<usize> {
    if row_height <= 0.0 || line_count == 0 {
        return 0..0;
    }
    let first = (viewport.min.y / row_height).floor().max(0.0) as usize;
    let last = ((viewport.max.y / row_height).ceil() as usize).saturating_add(1);
    let first = first.min(line_count.saturating_sub(1));
    first..last.min(line_count)
}

pub fn line_top(line: usize, row_height: f32) -> f32 {
    line as f32 * row_height
}

/// The line a content-relative y falls on, clamped to the last line.
pub fn line_at_y(y: f32, row_height: f32, line_count: usize) -> usize {
    if row_height <= 0.0 {
        return 0;
    }
    let line = (y / row_height).floor().max(0.0) as usize;
    line.min(line_count.saturating_sub(1))
}

/// Byte offset within `line_text` for a char index into it. Clamps past the
/// end, which is what `Galley::cursor_from_pos` returns for a click to the
/// right of the last character.
pub fn byte_offset_in_line(line_text: &str, char_index: usize) -> usize {
    line_text
        .char_indices()
        .nth(char_index)
        .map(|(i, _)| i)
        .unwrap_or(line_text.len())
}

/// Char index within `line_text` for a byte offset into it.
pub fn char_index_in_line(line_text: &str, byte_offset: usize) -> usize {
    line_text
        .get(..byte_offset.min(line_text.len()))
        .map(|s| s.chars().count())
        .unwrap_or(0)
}

/// Absolute byte offset for a click at content-relative `x` on `line`.
pub fn offset_at_pos(buffer: &TextBuffer, galley: &egui::Galley, line: usize, x: f32) -> usize {
    let Some(range) = buffer.lines().line_range(line, buffer.text()) else {
        return buffer.len();
    };
    let cursor = galley.cursor_from_pos(egui::vec2(x, 0.0));
    let line_text = &buffer.text()[range.clone()];
    range.start + byte_offset_in_line(line_text, cursor.index.0)
}

/// The column -- char index within its line -- that an absolute byte offset
/// sits at (`docs/features/multiple-cursors.md` §2.3).
pub fn column_of(buffer: &TextBuffer, offset: usize) -> usize {
    let line = buffer.lines().line_at(offset);
    let Some(range) = buffer.lines().line_range(line, buffer.text()) else {
        return 0;
    };
    char_index_in_line(
        &buffer.text()[range.clone()],
        offset.saturating_sub(range.start),
    )
}

/// One `Selection` per line in `lines`, spanning `columns` clamped to each
/// line's own length. A line shorter than `columns.start` yields a bare
/// caret at its end rather than being skipped, which is what makes a column
/// selection usable for appending to ragged lines. Both ranges are
/// normalised and inclusive of their far end, so dragging up-and-left
/// produces the same rectangle as dragging back down-and-right.
pub fn column_selections(
    buffer: &TextBuffer,
    lines: Range<usize>,
    columns: Range<usize>,
) -> Vec<Selection> {
    let (first, last) = ordered(lines.start, lines.end);
    let (from, to) = ordered(columns.start, columns.end);
    (first..=last)
        .filter_map(|line| {
            let range = buffer.lines().line_range(line, buffer.text())?;
            let line_text = &buffer.text()[range.clone()];
            Some(Selection::new(
                range.start + byte_offset_in_line(line_text, from),
                range.start + byte_offset_in_line(line_text, to),
            ))
        })
        .collect()
}

fn ordered(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}

fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The identifier `offset` falls inside, as a byte range — the symbol a
/// `Cmd`-hover should underline and a `Cmd`-click should query
/// (`docs/features/richer-highlighting-and-usages-popup.md` §3). `None`
/// when `offset` isn't touching an identifier at all, or when the run it
/// touches starts with a digit (a number literal, not a symbol).
///
/// A caret between two characters is treated as being on the identifier to
/// its left when there's no identifier to its right, so hovering the far
/// edge of a name still resolves to that name rather than to nothing.
pub fn word_range_at(text: &str, offset: usize) -> Option<Range<usize>> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let inside = if text[offset..]
        .chars()
        .next()
        .is_some_and(is_identifier_char)
    {
        offset
    } else {
        let prev = text[..offset].chars().next_back()?;
        if !is_identifier_char(prev) {
            return None;
        }
        offset - prev.len_utf8()
    };

    let mut start = inside;
    while let Some(c) = text[..start].chars().next_back() {
        if !is_identifier_char(c) {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = inside;
    while let Some(c) = text[end..].chars().next() {
        if !is_identifier_char(c) {
            break;
        }
        end += c.len_utf8();
    }

    if text[start..end].starts_with(char::is_numeric) {
        return None;
    }
    Some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> Spacing {
        Spacing {
            xs: 2.0,
            sm: 4.0,
            md: 8.0,
            lg: 12.0,
            xl: 16.0,
        }
    }

    fn rect(top: f32, bottom: f32) -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(0.0, top), egui::pos2(100.0, bottom))
    }

    #[test]
    fn digits_step_only_at_powers_of_ten() {
        assert_eq!(digits_for(0), 1);
        assert_eq!(digits_for(1), 1);
        assert_eq!(digits_for(9), 1);
        assert_eq!(digits_for(10), 2);
        assert_eq!(digits_for(99), 2);
        assert_eq!(digits_for(100), 3);
        assert_eq!(digits_for(9_999), 4);
        assert_eq!(digits_for(10_000), 5);
    }

    #[test]
    fn gutter_width_grows_only_with_digit_count() {
        let two = Metrics::new(18.0, 8.0, 2, 40, false, &space());
        let three = Metrics::new(18.0, 8.0, 3, 40, false, &space());
        assert!(three.gutter_width > two.gutter_width);
        assert_eq!(three.gutter_width - two.gutter_width, 8.0);
        assert!(two.text_left > two.gutter_width);
    }

    #[test]
    fn blame_lane_only_reserved_when_on() {
        let off = Metrics::new(18.0, 8.0, 2, 40, false, &space());
        let on = Metrics::new(18.0, 8.0, 2, 40, true, &space());
        assert_eq!(off.blame_lane_width, 0.0);
        assert!(on.blame_lane_width > 0.0);
        assert_eq!(on.gutter_width - off.gutter_width, on.blame_lane_width);
    }

    #[test]
    fn blame_lane_width_does_not_depend_on_digit_count() {
        let small_file = Metrics::new(18.0, 8.0, 1, 40, true, &space());
        let big_file = Metrics::new(18.0, 8.0, 6, 40, true, &space());
        assert_eq!(small_file.blame_lane_width, big_file.blame_lane_width);
    }

    #[test]
    fn visible_lines_covers_the_viewport_with_a_row_of_slack() {
        let range = visible_lines(rect(0.0, 100.0), 20.0, 1000);
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 6);
    }

    #[test]
    fn visible_lines_scales_to_a_huge_file() {
        let range = visible_lines(rect(18_000.0, 18_720.0), 18.0, 100_000);
        assert_eq!(range.start, 1000);
        assert!(range.len() < 64, "{range:?}");
    }

    #[test]
    fn visible_lines_clamps_past_the_end() {
        assert_eq!(visible_lines(rect(0.0, 1000.0), 20.0, 3), 0..3);
        assert_eq!(visible_lines(rect(500.0, 600.0), 20.0, 3), 2..3);
    }

    #[test]
    fn visible_lines_handles_degenerate_input() {
        assert_eq!(visible_lines(rect(0.0, 100.0), 0.0, 10), 0..0);
        assert_eq!(visible_lines(rect(0.0, 100.0), 20.0, 0), 0..0);
        assert_eq!(visible_lines(rect(0.0, 100.0), 20.0, 1), 0..1);
    }

    #[test]
    fn line_at_y_clamps_both_ends() {
        assert_eq!(line_at_y(-50.0, 20.0, 10), 0);
        assert_eq!(line_at_y(25.0, 20.0, 10), 1);
        assert_eq!(line_at_y(9_999.0, 20.0, 10), 9);
        assert_eq!(line_at_y(10.0, 0.0, 10), 0);
        assert_eq!(line_top(3, 20.0), 60.0);
    }

    #[test]
    fn byte_and_char_offsets_round_trip_through_a_line() {
        let line = "a\u{4F60}b";
        assert_eq!(byte_offset_in_line(line, 0), 0);
        assert_eq!(byte_offset_in_line(line, 1), 1);
        assert_eq!(byte_offset_in_line(line, 2), 4);
        assert_eq!(byte_offset_in_line(line, 99), line.len());
        assert_eq!(char_index_in_line(line, 4), 2);
        assert_eq!(char_index_in_line(line, 99), 3);
    }

    /// A bare `Context` has no font atlas until it has run a frame, and
    /// `offset_at_pos` needs a real galley to ask about columns.
    fn laid_out(text: &str) -> (egui::Context, std::sync::Arc<egui::Galley>) {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        let galley = ctx.fonts_mut(|f| {
            f.layout_no_wrap(
                text.to_string(),
                egui::FontId::monospace(12.0),
                crate::theme::Theme::Dark.tokens().color.fg_primary,
            )
        });
        (ctx, galley)
    }

    #[test]
    fn offset_at_pos_maps_a_click_to_a_byte_offset_on_every_edge() {
        let buffer = ide_core::TextBuffer::new("let x = 1;\nano\u{4F60}her\nlast", None);
        let width = laid_out("x").1.rect.width();

        let (_ctx, first) = laid_out("let x = 1;");
        // Before the first character, and far past the end of the line.
        assert_eq!(offset_at_pos(&buffer, &first, 0, -50.0), 0);
        assert_eq!(offset_at_pos(&buffer, &first, 0, 10_000.0), 10);
        assert_eq!(offset_at_pos(&buffer, &first, 0, width * 4.2), 4);

        // A multi-byte character: the offset lands on its boundary, never
        // inside it.
        let (_ctx, second) = laid_out("ano\u{4F60}her");
        let offset = offset_at_pos(&buffer, &second, 1, width * 3.2);
        assert!(buffer.text().is_char_boundary(offset));
        assert_eq!(offset, 11 + 3);
        assert_eq!(offset_at_pos(&buffer, &second, 1, 10_000.0), 11 + 9);

        let (_ctx, third) = laid_out("last");
        assert_eq!(offset_at_pos(&buffer, &third, 2, 0.0), 21);
        // A line past the end has no range: the buffer's end is the answer.
        assert_eq!(offset_at_pos(&buffer, &third, 99, 0.0), buffer.len());
    }

    /// Ragged on purpose: the second line is shorter than the rectangle.
    fn ragged() -> ide_core::TextBuffer {
        ide_core::TextBuffer::new("alpha bravo\nab\ncharlie delta", None)
    }

    #[test]
    fn column_of_counts_chars_not_bytes() {
        let buffer = ide_core::TextBuffer::new("a\u{4F60}b\nxy", None);
        assert_eq!(column_of(&buffer, 0), 0);
        assert_eq!(column_of(&buffer, 4), 2);
        // Second line, first column.
        assert_eq!(column_of(&buffer, 6), 0);
    }

    #[test]
    fn column_selections_span_every_line_clamped_to_its_length() {
        let buffer = ragged();
        let selections = column_selections(&buffer, 0..2, 3..7);
        assert_eq!(selections.len(), 3);
        assert_eq!(&buffer.text()[selections[0].range()], "ha b");
        // "ab" is shorter than column 3: a bare caret at its end.
        assert!(selections[1].is_empty());
        assert_eq!(selections[1].head, buffer.text().find("ab").unwrap() + 2);
        assert_eq!(&buffer.text()[selections[2].range()], "rlie");
    }

    #[test]
    // A drag that starts bottom-right hands the function reversed ranges on
    // purpose -- normalising them is exactly what is under test here.
    #[allow(clippy::reversed_empty_ranges)]
    fn column_selections_normalise_a_reversed_rectangle() {
        let buffer = ragged();
        let forward = column_selections(&buffer, 0..2, 3..7);
        let backward = column_selections(&buffer, 2..0, 7..3);
        assert_eq!(forward, backward);
    }

    #[test]
    fn a_zero_width_rectangle_is_all_carets() {
        let buffer = ragged();
        let selections = column_selections(&buffer, 0..2, 1..1);
        assert_eq!(selections.len(), 3);
        assert!(selections.iter().all(|s| s.is_empty()));
    }

    #[test]
    fn column_selections_past_the_last_line_stop_at_it() {
        let buffer = ragged();
        assert_eq!(column_selections(&buffer, 1..99, 0..2).len(), 2);
    }

    #[test]
    fn word_range_at_covers_the_whole_identifier_from_any_offset_inside_it() {
        let text = "let total_count = 1;";
        for offset in 4..=15 {
            assert_eq!(word_range_at(text, offset), Some(4..15), "at {offset}");
        }
        assert_eq!(&text[4..15], "total_count");
    }

    #[test]
    fn word_range_at_resolves_the_identifier_to_the_left_of_a_boundary() {
        // The caret sitting just past a name's last character -- where a
        // click on its right half lands -- must still resolve to that name.
        let text = "foo(bar)";
        assert_eq!(word_range_at(text, 3), Some(0..3));
    }

    #[test]
    fn word_range_at_off_an_identifier_is_none() {
        let text = "a + b";
        assert_eq!(word_range_at(text, 2), None);
        assert_eq!(word_range_at("", 0), None);
        assert_eq!(word_range_at("x", 99), None);
    }

    #[test]
    fn word_range_at_rejects_number_literals() {
        assert_eq!(word_range_at("x = 42;", 5), None);
        // ...but not an identifier that merely contains digits.
        assert_eq!(word_range_at("let u32x = 1;", 5), Some(4..8));
    }

    #[test]
    fn word_range_at_handles_multibyte_identifiers_on_char_boundaries() {
        let text = "let ключ = 1;";
        let range = word_range_at(text, 4).unwrap();
        assert_eq!(&text[range.clone()], "ключ");
        // An offset mid-codepoint is rejected rather than panicking.
        assert_eq!(word_range_at(text, 5), None);
    }
}
