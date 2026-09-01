use crate::types::Position;

/// Converts an LSP `Position` (UTF-16 code units into a line) into a
/// byte offset into `text`. `None` if the position is out of range — a
/// buggy/malicious server response, or a transient client/server text
/// desync; callers should skip the diagnostic rather than error.
///
/// Assumes LF line endings (matches this editor's buffer convention);
/// a CRLF file would throw the character count off by one per line.
pub fn position_to_byte_offset(text: &str, position: Position) -> Option<usize> {
    let mut lines = text.split('\n');
    let mut byte_offset = 0usize;
    for _ in 0..position.line {
        let line = lines.next()?;
        byte_offset += line.len() + 1;
    }
    let line = lines.next().unwrap_or("");

    let mut utf16_count = 0u32;
    for (idx, ch) in line.char_indices() {
        if utf16_count == position.character {
            return Some(byte_offset + idx);
        }
        utf16_count += ch.len_utf16() as u32;
        if utf16_count > position.character {
            // `position.character` lands inside a surrogate pair (e.g.
            // pointing at the low half of an astral character) — not a
            // valid UTF-16 code unit boundary. Reject rather than return
            // an offset that isn't on a char boundary.
            return None;
        }
    }
    if utf16_count == position.character {
        Some(byte_offset + line.len())
    } else {
        None
    }
}

/// Converts a byte offset into `text` into an LSP `Position` (UTF-16 code
/// units into its line) — the inverse of [`position_to_byte_offset`].
/// `None` if `byte_offset` is out of range or not on a UTF-8 char
/// boundary. Same LF-line-ending assumption as the forward direction.
pub fn byte_offset_to_position(text: &str, byte_offset: usize) -> Option<Position> {
    if byte_offset > text.len() || !text.is_char_boundary(byte_offset) {
        return None;
    }

    let mut line = 0u32;
    let mut line_start = 0usize;
    for (idx, ch) in text[..byte_offset].char_indices() {
        if ch == '\n' {
            line += 1;
            line_start = idx + 1;
        }
    }

    let character = text[line_start..byte_offset]
        .chars()
        .map(|ch| ch.len_utf16() as u32)
        .sum();

    Some(Position { line, character })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn ascii_middle_of_line() {
        let text = "hello\nworld";
        assert_eq!(position_to_byte_offset(text, pos(1, 2)), Some(8));
    }

    #[test]
    fn ascii_start_of_text() {
        assert_eq!(position_to_byte_offset("hello", pos(0, 0)), Some(0));
    }

    #[test]
    fn end_of_line_position_is_valid() {
        let text = "hi\nbye";
        assert_eq!(position_to_byte_offset(text, pos(0, 2)), Some(2));
    }

    #[test]
    fn end_of_empty_line_is_valid() {
        let text = "\nx";
        assert_eq!(position_to_byte_offset(text, pos(0, 0)), Some(0));
    }

    #[test]
    fn multi_byte_utf8_bmp_character() {
        // "café": c a f é(2 bytes, 1 UTF-16 unit)
        let text = "café";
        assert_eq!(position_to_byte_offset(text, pos(0, 3)), Some(3)); // start of é
        assert_eq!(position_to_byte_offset(text, pos(0, 4)), Some(5)); // end of line
    }

    #[test]
    fn astral_plane_surrogate_pair() {
        // U+1F600 (grinning face): 4 UTF-8 bytes, 2 UTF-16 code units.
        let text = "\u{1F600}x";
        assert_eq!(position_to_byte_offset(text, pos(0, 0)), Some(0));
        assert_eq!(position_to_byte_offset(text, pos(0, 2)), Some(4)); // 'x'
    }

    #[test]
    fn astral_plane_position_mid_surrogate_pair_is_rejected() {
        let text = "\u{1F600}x";
        assert_eq!(position_to_byte_offset(text, pos(0, 1)), None);
    }

    #[test]
    fn character_beyond_line_length_is_rejected() {
        assert_eq!(position_to_byte_offset("hi", pos(0, 100)), None);
    }

    #[test]
    fn line_beyond_text_length_is_rejected() {
        assert_eq!(position_to_byte_offset("only one line", pos(5, 0)), None);
    }

    #[test]
    fn empty_text_line_zero_character_zero() {
        assert_eq!(position_to_byte_offset("", pos(0, 0)), Some(0));
    }

    #[test]
    fn byte_offset_ascii_middle_of_line() {
        let text = "hello\nworld";
        assert_eq!(byte_offset_to_position(text, 8), Some(pos(1, 2)));
    }

    #[test]
    fn byte_offset_ascii_start_of_text() {
        assert_eq!(byte_offset_to_position("hello", 0), Some(pos(0, 0)));
    }

    #[test]
    fn byte_offset_end_of_line_is_valid() {
        let text = "hi\nbye";
        assert_eq!(byte_offset_to_position(text, 2), Some(pos(0, 2)));
    }

    #[test]
    fn byte_offset_end_of_empty_line_is_valid() {
        let text = "\nx";
        assert_eq!(byte_offset_to_position(text, 0), Some(pos(0, 0)));
    }

    #[test]
    fn byte_offset_multi_byte_utf8_bmp_character() {
        // "café": c a f é(2 bytes, 1 UTF-16 unit)
        let text = "café";
        assert_eq!(byte_offset_to_position(text, 3), Some(pos(0, 3))); // start of é
        assert_eq!(byte_offset_to_position(text, 5), Some(pos(0, 4))); // end of line
    }

    #[test]
    fn byte_offset_astral_plane_surrogate_pair() {
        // U+1F600 (grinning face): 4 UTF-8 bytes, 2 UTF-16 code units.
        let text = "\u{1F600}x";
        assert_eq!(byte_offset_to_position(text, 0), Some(pos(0, 0)));
        assert_eq!(byte_offset_to_position(text, 4), Some(pos(0, 2))); // 'x'
    }

    #[test]
    fn byte_offset_mid_char_boundary_is_rejected() {
        // Byte 1 lands inside the 4-byte encoding of U+1F600.
        let text = "\u{1F600}x";
        assert_eq!(byte_offset_to_position(text, 1), None);
    }

    #[test]
    fn byte_offset_beyond_text_length_is_rejected() {
        assert_eq!(byte_offset_to_position("hi", 100), None);
    }

    #[test]
    fn byte_offset_at_exact_text_length_is_valid() {
        assert_eq!(byte_offset_to_position("hi", 2), Some(pos(0, 2)));
    }

    #[test]
    fn byte_offset_empty_text_zero_is_valid() {
        assert_eq!(byte_offset_to_position("", 0), Some(pos(0, 0)));
    }

    #[test]
    fn byte_offset_multiple_lines_lands_on_correct_line() {
        let text = "one\ntwo\nthree";
        assert_eq!(byte_offset_to_position(text, 9), Some(pos(2, 1)));
    }

    #[test]
    fn round_trip_ascii_every_offset() {
        let text = "hello\nworld\nfoo bar baz";
        for byte_offset in 0..=text.len() {
            if !text.is_char_boundary(byte_offset) {
                continue;
            }
            let position = byte_offset_to_position(text, byte_offset).unwrap();
            assert_eq!(
                position_to_byte_offset(text, position),
                Some(byte_offset),
                "round-trip failed at byte offset {byte_offset}"
            );
        }
    }

    #[test]
    fn round_trip_multi_byte_and_astral_every_char_boundary() {
        let text = "café \u{1F600} 日本語\nsecond line";
        for byte_offset in 0..=text.len() {
            if !text.is_char_boundary(byte_offset) {
                continue;
            }
            let position = byte_offset_to_position(text, byte_offset).unwrap();
            assert_eq!(
                position_to_byte_offset(text, position),
                Some(byte_offset),
                "round-trip failed at byte offset {byte_offset}"
            );
        }
    }
}
