//! Cursor movement against `ide_core::TextBuffer`, kept `char`-boundary-safe
//! (`docs/features/tui-shell-and-editor.md` §2.5). `ide_core`'s own
//! `LineIndex::position_at`/`offset_at` are byte-based (see that type's own
//! doc comment) -- every function here converts to/from a `char` count so
//! nothing this crate produces can land mid-character, which is what keeps
//! every offset safe to hand to `Selection::caret`/`TextBuffer::insert`.

use std::ops::Range;

use ide_core::{SyntaxRules, TextBuffer, TokenKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// `(line, column)` for `offset`, `column` a `char` count from the line's
/// start. Defensively snaps `offset` down to the nearest real `char`
/// boundary first (every offset this crate's own callers pass is already
/// on one, but this keeps the function itself panic-safe against any
/// input) and clamps the resulting byte column past the line's own text
/// (e.g. an offset sitting between a CRLF pair's `\r` and `\n`, which
/// `LineIndex::line_range` excludes from `line_text`) rather than
/// panicking on an out-of-bounds slice.
pub fn cursor_line_column(buffer: &TextBuffer, offset: usize) -> (usize, usize) {
    let offset = floor_char_boundary(buffer.text(), offset);
    let (line, byte_col) = buffer.lines().position_at(offset);
    let line_text = buffer.line_text(line).unwrap_or("");
    let byte_col = byte_col.min(line_text.len());
    let char_col = line_text[..byte_col].chars().count();
    (line, char_col)
}

fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut i = offset.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The offset of `column` `char`s into `line`. `line` past the buffer's
/// last line clamps to the last line; `column` past that line's own
/// `char` count clamps to the line's end.
pub fn offset_for_line_column(buffer: &TextBuffer, line: usize, column: usize) -> usize {
    let lines = buffer.lines();
    let line = line.min(lines.line_count().saturating_sub(1));
    let line_text = buffer.line_text(line).unwrap_or("");
    let byte_col = line_text
        .char_indices()
        .nth(column)
        .map(|(i, _)| i)
        .unwrap_or(line_text.len());
    lines.line_start(line).unwrap_or(0) + byte_col
}

/// Moves `offset` one step in `direction`. `Left`/`Right` step by one
/// `char` (clamped to `0..=buffer.len()`) and clear the sticky column.
/// `Up`/`Down` move to the adjacent line at `desired_column` (falling back
/// to the current column if `None`), carrying that column forward for the
/// next vertical move; `Up` on the first line and `Down` on the last are
/// no-ops (same offset, `desired_column` unchanged).
pub fn move_cursor(
    buffer: &TextBuffer,
    offset: usize,
    desired_column: Option<usize>,
    direction: Direction,
) -> (usize, Option<usize>) {
    match direction {
        Direction::Left => (prev_char_boundary(buffer.text(), offset), None),
        Direction::Right => (next_char_boundary(buffer.text(), offset), None),
        Direction::Up | Direction::Down => {
            let (line, column_here) = cursor_line_column(buffer, offset);
            let column = desired_column.unwrap_or(column_here);
            let last_line = buffer.lines().line_count().saturating_sub(1);
            let target_line = match direction {
                Direction::Up => {
                    if line == 0 {
                        return (offset, desired_column);
                    }
                    line - 1
                }
                Direction::Down => {
                    if line == last_line {
                        return (offset, desired_column);
                    }
                    line + 1
                }
                Direction::Left | Direction::Right => unreachable!(),
            };
            (
                offset_for_line_column(buffer, target_line, column),
                Some(column),
            )
        }
    }
}

/// Minimal-scroll viewport clamp (`docs/features/tui-scroll-follows-
/// cursor.md` §2.1): adjusts `scroll` only enough to bring `cursor_line`
/// back into `[scroll, scroll + viewport_rows)`, leaving it untouched
/// when the cursor is already visible -- this is what makes it safe to
/// call unconditionally after every cursor-moving key, including ones
/// that didn't actually move the cursor (e.g. `Up` on line 0).
///
/// `viewport_rows == 0` is a no-op (nothing is visible to clamp into --
/// `App` starts with an effectively-infinite placeholder instead of `0`
/// for exactly this reason, so this only matters before the first real
/// terminal size is known). `cursor_line` is capped to `u16::MAX` before
/// arithmetic, the same defensive cap `jump_to_match` already applies --
/// no real terminal buffer approaches that many lines, but it keeps this
/// function panic-free by construction rather than by caller discipline.
pub fn scroll_to_keep_visible(scroll: u16, cursor_line: usize, viewport_rows: u16) -> u16 {
    if viewport_rows == 0 {
        return scroll;
    }
    let cursor_line = cursor_line.min(u16::MAX as usize) as u16;
    if cursor_line < scroll {
        cursor_line
    } else if cursor_line >= scroll.saturating_add(viewport_rows) {
        cursor_line.saturating_add(1).saturating_sub(viewport_rows)
    } else {
        scroll
    }
}

fn prev_char_boundary(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut i = offset - 1;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let mut i = offset + 1;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Start of the buffer line `offset` is on -- excludes the line
/// terminator, same convention `Lines::line_range` already follows.
/// `docs/features/tui-word-and-document-navigation.md` §2.1.
pub fn line_start_offset(buffer: &TextBuffer, offset: usize) -> usize {
    let line = buffer.lines().line_at(offset);
    buffer
        .lines()
        .line_range(line, buffer.text())
        .map_or(offset, |r| r.start)
}

/// End of the buffer line `offset` is on -- excludes the line terminator.
pub fn line_end_offset(buffer: &TextBuffer, offset: usize) -> usize {
    let line = buffer.lines().line_at(offset);
    buffer
        .lines()
        .line_range(line, buffer.text())
        .map_or(offset, |r| r.end)
}

/// One word left from `offset`: skips a run of non-identifier characters
/// (including newlines -- word motion crosses blank lines), then the run
/// of identifier characters before that. Ported verbatim (behavior, not
/// code layout) from `ide-ui`'s own `editor::input::word_start_before`
/// (`docs/features/tui-word-and-document-navigation.md` §2.1).
pub fn word_start_before(text: &str, offset: usize) -> usize {
    let mut cursor = offset.min(text.len());
    while cursor > 0 {
        let prev = prev_char_boundary(text, cursor);
        if text[prev..cursor].chars().all(|c| !is_identifier_char(c)) {
            cursor = prev;
        } else {
            break;
        }
    }
    while cursor > 0 {
        let prev = prev_char_boundary(text, cursor);
        if text[prev..cursor].chars().all(is_identifier_char) {
            cursor = prev;
        } else {
            break;
        }
    }
    cursor
}

/// Symmetric, one word right from `offset`.
pub fn word_end_after(text: &str, offset: usize) -> usize {
    let mut cursor = offset.min(text.len());
    while cursor < text.len() {
        let next = next_char_boundary(text, cursor);
        if text[cursor..next].chars().all(|c| !is_identifier_char(c)) {
            cursor = next;
        } else {
            break;
        }
    }
    while cursor < text.len() {
        let next = next_char_boundary(text, cursor);
        if text[cursor..next].chars().all(is_identifier_char) {
            cursor = next;
        } else {
            break;
        }
    }
    cursor
}

/// The identifier `offset` falls inside, as a byte range -- ported
/// verbatim (behavior, not code layout) from `ide-ui`'s own
/// `editor::geometry::word_range_at` (`docs/features/
/// tui-code-actions-and-rename.md` §2.1). `None` when `offset` isn't
/// touching an identifier at all, or when the run it touches starts with
/// a digit (a number literal, not a symbol).
///
/// A caret between two characters is treated as being on the identifier
/// to its left when there's no identifier to its right, so resolving the
/// far edge of a name still resolves to that name rather than to nothing.
pub(crate) fn word_range_at(text: &str, offset: usize) -> Option<Range<usize>> {
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

/// What typing `c` opens, if anything: a bracket from the language's own
/// table, or a quote, which is its own closer (`docs/features/
/// tui-smart-editing.md` §3.2, ported from `ide-ui`'s `input.rs::closer_
/// for`).
pub(crate) fn closer_for(rules: &SyntaxRules, c: char) -> Option<char> {
    rules
        .brackets
        .iter()
        .find(|(open, _)| *open == c)
        .map(|(_, close)| *close)
        .or_else(|| rules.string_quotes.contains(&c).then_some(c))
}

/// A pair is opened only before end-of-line, whitespace, or a closer --
/// before an identifier the user is far likelier to be wrapping what
/// follows than opening an empty pair (`tui-smart-editing.md` §3.2).
pub(crate) fn may_open_pair(text: &str, offset: usize, rules: &SyntaxRules) -> bool {
    match text[offset..].chars().next() {
        None => true,
        Some(next) => {
            next.is_whitespace() || rules.brackets.iter().any(|(_, close)| *close == next)
        }
    }
}

/// Whether `offset` sits inside a `String` or `Comment` token -- the extra
/// guard quotes carry, so an apostrophe inside a comment does not become
/// `''` (`tui-smart-editing.md` §3.2).
pub(crate) fn is_quoted_or_commented(buffer: &TextBuffer, offset: usize) -> bool {
    let tokens = buffer.tokens();
    let index = tokens.partition_point(|token| token.range.end <= offset);
    tokens.get(index).is_some_and(|token| {
        token.range.start <= offset && matches!(token.kind, TokenKind::String | TokenKind::Comment)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::RUST;

    fn buffer(text: &str) -> TextBuffer {
        TextBuffer::new(text, None)
    }

    fn rust_buffer(text: &str) -> TextBuffer {
        TextBuffer::new(text, Some(&RUST))
    }

    #[test]
    fn closer_for_maps_a_bracket_and_a_quote_but_not_a_plain_char() {
        assert_eq!(closer_for(&RUST, '('), Some(')'));
        assert_eq!(closer_for(&RUST, '{'), Some('}'));
        assert_eq!(closer_for(&RUST, '"'), Some('"'));
        assert_eq!(closer_for(&RUST, 'x'), None);
        assert_eq!(closer_for(&RUST, ')'), None);
    }

    #[test]
    fn may_open_pair_admits_eol_whitespace_and_a_closer_but_not_an_identifier() {
        assert!(may_open_pair("", 0, &RUST));
        assert!(may_open_pair(" x", 0, &RUST));
        assert!(may_open_pair(")", 0, &RUST));
        assert!(!may_open_pair("x", 0, &RUST));
    }

    #[test]
    fn is_quoted_or_commented_detects_strings_and_comments_but_not_code() {
        let buf = rust_buffer(r#"let s = "hi"; // c"#);
        let quote_offset = buf.text().find('"').unwrap();
        assert!(is_quoted_or_commented(&buf, quote_offset + 1));
        let comment_offset = buf.text().find("//").unwrap();
        assert!(is_quoted_or_commented(&buf, comment_offset + 1));
        assert!(!is_quoted_or_commented(&buf, 0));
    }

    #[test]
    fn word_range_at_covers_the_whole_identifier_from_any_offset_inside_it() {
        let text = "let some_variable = 1;";
        for offset in 4..17 {
            assert_eq!(word_range_at(text, offset), Some(4..17), "at {offset}");
        }
    }

    #[test]
    fn word_range_at_resolves_the_identifier_to_the_left_of_a_boundary() {
        let text = "abc def";
        assert_eq!(word_range_at(text, 3), Some(0..3));
    }

    #[test]
    fn word_range_at_off_an_identifier_is_none() {
        let text = "a  b";
        assert_eq!(word_range_at(text, 2), None);
        assert_eq!(word_range_at("", 0), None);
        assert_eq!(word_range_at("x", 99), None);
    }

    #[test]
    fn word_range_at_rejects_number_literals() {
        assert_eq!(word_range_at("x = 42;", 5), None);
        // An identifier starting with a letter but containing digits is fine.
        assert_eq!(word_range_at("let u32x = 1;", 5), Some(4..8));
    }

    #[test]
    fn word_range_at_handles_multibyte_identifiers_on_char_boundaries() {
        let text = "let héllo = 1;";
        let range = word_range_at(text, 4).unwrap();
        assert_eq!(&text[range], "héllo");
        // 'h' is a 1-byte char at offset 4, so 'é' spans 5..7 -- offset 6
        // sits mid-character inside it.
        assert_eq!(word_range_at(text, 6), None);
    }

    #[test]
    fn scroll_to_keep_visible_is_a_no_op_when_cursor_already_visible() {
        assert_eq!(scroll_to_keep_visible(5, 5, 10), 5);
        assert_eq!(scroll_to_keep_visible(5, 14, 10), 5); // last visible row
    }

    #[test]
    fn scroll_to_keep_visible_scrolls_down_the_minimum_needed() {
        // scroll=0, viewport=10 (rows 0..10 visible), cursor moves to line 10
        // (just past the bottom) -- must scroll down by exactly 1, not jump
        // to top-align at 10.
        assert_eq!(scroll_to_keep_visible(0, 10, 10), 1);
        assert_eq!(scroll_to_keep_visible(0, 25, 10), 16);
    }

    #[test]
    fn scroll_to_keep_visible_scrolls_up_to_top_align_when_cursor_moves_above() {
        assert_eq!(scroll_to_keep_visible(20, 5, 10), 5);
    }

    #[test]
    fn scroll_to_keep_visible_is_a_no_op_with_an_unknown_zero_viewport() {
        assert_eq!(scroll_to_keep_visible(0, 500, 0), 0);
    }

    #[test]
    fn scroll_to_keep_visible_never_panics_at_u16_max_cursor_line() {
        // cursor_line is capped to u16::MAX first, so the saturating `+1`
        // on the cap itself also saturates (stays at u16::MAX) rather than
        // wrapping to 0 -- this is what proves the cap composes safely
        // with the `+1` in the scroll-down branch, not just that neither
        // step panics in isolation.
        assert_eq!(
            scroll_to_keep_visible(0, u16::MAX as usize + 5, 10),
            u16::MAX - 10,
        );
    }

    #[test]
    fn cursor_line_column_on_ascii() {
        let b = buffer("abc\ndef");
        assert_eq!(cursor_line_column(&b, 0), (0, 0));
        assert_eq!(cursor_line_column(&b, 2), (0, 2));
        assert_eq!(cursor_line_column(&b, 5), (1, 1));
    }

    #[test]
    fn cursor_line_column_on_multibyte_utf8() {
        // "héllo" -- 'é' is 2 bytes, so byte offset 3 (after 'h','é') is
        // char index 2, not byte index 2.
        let b = buffer("héllo\nwörld");
        assert_eq!(cursor_line_column(&b, 3), (0, 2));
        // second line starts at byte 7 ("héllo\n".len() == 7, since 'h'=1,
        // 'é'=2, 'l'=1, 'l'=1, 'o'=1, '\n'=1); 'ö' is 2 bytes, so byte
        // offset 7+3=10 (after 'w','ö') is char index 2.
        assert_eq!(cursor_line_column(&b, 10), (1, 2));
    }

    #[test]
    fn offset_for_line_column_on_multibyte_utf8_round_trips() {
        let b = buffer("héllo\nwörld");
        for offset in [0usize, 1, 3, 5, 6, 7, 8, 10, 11, 13] {
            let (line, col) = cursor_line_column(&b, offset);
            assert_eq!(offset_for_line_column(&b, line, col), offset);
        }
    }

    #[test]
    fn offset_for_line_column_clamps_column_past_line_end() {
        let b = buffer("ab\ncd");
        assert_eq!(offset_for_line_column(&b, 0, 100), 2); // end of "ab"
    }

    #[test]
    fn offset_for_line_column_clamps_line_past_last_line() {
        let b = buffer("ab\ncd");
        assert_eq!(offset_for_line_column(&b, 100, 0), 3); // start of "cd"
    }

    #[test]
    fn move_left_right_step_one_char_and_clear_sticky_column() {
        let b = buffer("abc");
        let (offset, col) = move_cursor(&b, 1, Some(5), Direction::Right);
        assert_eq!(offset, 2);
        assert_eq!(col, None);
        let (offset, col) = move_cursor(&b, 2, Some(5), Direction::Left);
        assert_eq!(offset, 1);
        assert_eq!(col, None);
    }

    #[test]
    fn move_left_at_offset_zero_is_a_no_op() {
        let b = buffer("abc");
        let (offset, _) = move_cursor(&b, 0, None, Direction::Left);
        assert_eq!(offset, 0);
    }

    #[test]
    fn move_right_at_end_of_buffer_is_a_no_op() {
        let b = buffer("abc");
        let (offset, _) = move_cursor(&b, 3, None, Direction::Right);
        assert_eq!(offset, 3);
    }

    #[test]
    fn move_left_right_never_land_mid_multibyte_char() {
        let b = buffer("héllo");
        // 'h' at 0, 'é' spans bytes 1..3, 'l' at 3.
        let (offset, _) = move_cursor(&b, 3, None, Direction::Left);
        assert_eq!(offset, 1);
        let (offset, _) = move_cursor(&b, 1, None, Direction::Right);
        assert_eq!(offset, 3);
    }

    #[test]
    fn move_up_on_first_line_is_a_no_op_and_preserves_desired_column() {
        let b = buffer("abc\ndef");
        let (offset, col) = move_cursor(&b, 1, Some(9), Direction::Up);
        assert_eq!(offset, 1);
        assert_eq!(col, Some(9));
    }

    #[test]
    fn move_down_on_last_line_is_a_no_op_and_preserves_desired_column() {
        let b = buffer("abc\ndef");
        let (offset, col) = move_cursor(&b, 5, None, Direction::Down);
        assert_eq!(offset, 5);
        assert_eq!(col, None);
    }

    #[test]
    fn sticky_column_is_carried_across_consecutive_vertical_moves() {
        // Line 0 is short ("ab"), line 1 is long ("abcdef"), line 2 short
        // ("cd") -- moving down from column 1 on line 0 to line 1 should
        // land at column 1, and the *original* column 1 should carry
        // forward to line 2 even though line 1's cursor position (as
        // reported by cursor_line_column) is also column 1 here.
        let b = buffer("ab\nabcdef\ncd");
        let (offset1, col1) = move_cursor(&b, 1, None, Direction::Down);
        assert_eq!(cursor_line_column(&b, offset1), (1, 1));
        assert_eq!(col1, Some(1));
        let (offset2, col2) = move_cursor(&b, offset1, col1, Direction::Down);
        assert_eq!(cursor_line_column(&b, offset2), (2, 1));
        assert_eq!(col2, Some(1));
    }

    #[test]
    fn sticky_column_survives_a_shorter_line_then_reapplies_on_a_longer_one() {
        // Moving down onto a shorter line clamps column but keeps the
        // original desired_column for the next move.
        let b = buffer("abcdef\nab\nabcdef");
        let (offset1, col1) = move_cursor(&b, 4, None, Direction::Down); // col 4 -> line 1 clamped to 2
        assert_eq!(cursor_line_column(&b, offset1), (1, 2));
        assert_eq!(col1, Some(4));
        let (offset2, col2) = move_cursor(&b, offset1, col1, Direction::Down);
        assert_eq!(cursor_line_column(&b, offset2), (2, 4));
        assert_eq!(col2, Some(4));
    }

    #[test]
    fn line_start_and_end_offset_exclude_the_newline() {
        let b = buffer("abc\ndef\nghi");
        assert_eq!(line_start_offset(&b, 5), 4); // mid "def" -> start of "def"
        assert_eq!(line_end_offset(&b, 5), 7); // end of "def", before \n
        assert_eq!(line_start_offset(&b, 0), 0);
        assert_eq!(line_end_offset(&b, 0), 3);
        assert_eq!(line_start_offset(&b, 11), 8); // last line, no trailing \n
        assert_eq!(line_end_offset(&b, 11), 11);
    }

    #[test]
    fn word_motion_skips_punctuation_and_stops_at_identifier_boundaries() {
        let text = "let some_variable = 1;";
        // From inside "some_variable", one step left lands before "some_variable".
        assert_eq!(word_start_before(text, 10), 4);
        // One step right from there lands right after "some_variable".
        assert_eq!(word_end_after(text, 4), 17);
        // From the space right after "=", stepping right lands after "1".
        assert_eq!(word_end_after(text, 19), 21);
    }

    #[test]
    fn word_motion_crosses_blank_lines() {
        let text = "foo\n\n\nbar";
        assert_eq!(word_end_after(text, 3), 9); // right after "foo" -> end of "bar"
        assert_eq!(word_start_before(text, 9), 6); // right after "bar" -> start of "bar"
    }

    #[test]
    fn word_motion_at_buffer_edges_clamps_rather_than_panics() {
        assert_eq!(word_start_before("abc", 0), 0);
        assert_eq!(word_end_after("abc", 3), 3);
        assert_eq!(word_start_before("", 0), 0);
        assert_eq!(word_end_after("", 0), 0);
    }
}
