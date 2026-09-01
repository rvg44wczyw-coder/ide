//! Plain substring search over a buffer's text, for A3's occurrence
//! commands (`docs/features/multiple-cursors.md` §2.1). Case-sensitive and
//! literal on purpose: A5 is the phase that adds case-insensitivity and
//! regex, and half of it landing here is half of it A5 would have to undo.
//!
//! Both functions take `&str` rather than a `TextBuffer`, so nothing has to
//! grow a second way of reaching the text.

use std::ops::Range;

/// Ceiling on how many occurrences one call reports. Buffer content is file
/// content, i.e. untrusted, and a one-character needle in a large file
/// yields hundreds of thousands of matches -- which the editor would turn
/// into that many cursors, walk once per visible line while painting, and
/// fold into a single enormous transaction on the next keystroke. Same
/// value and the same reasoning as `crate::search::MAX_SEARCH_RESULTS`
/// (doc §4.8).
pub const MAX_OCCURRENCES: usize = 1000;

/// Byte range of the first occurrence of `needle` at or after `from`,
/// wrapping once to the start of the text.
///
/// `None` when `needle` is empty, when it does not occur, or when `from` is
/// past `text.len()`. The wrapped half searches `text[..from]`, so a match
/// straddling `from` is not reported -- matches are non-overlapping in the
/// same sense [`all_occurrences`] means it.
pub fn next_occurrence(text: &str, needle: &str, from: usize) -> Option<Range<usize>> {
    if needle.is_empty() || from > text.len() {
        return None;
    }
    // A caller that derived `from` by arithmetic could land mid-codepoint;
    // slicing there would panic, so walk back to the boundary instead.
    let from = floor_boundary(text, from);

    text[from..]
        .find(needle)
        .map(|offset| from + offset)
        .or_else(|| text[..from].find(needle))
        .map(|start| start..start + needle.len())
}

/// Every non-overlapping occurrence of `needle`, left to right, at most
/// [`MAX_OCCURRENCES`] of them. Non-overlapping means the scan resumes at
/// the end of each match, so `"aa"` in `"aaaa"` is two matches, not three.
/// Empty when `needle` is empty.
pub fn all_occurrences(text: &str, needle: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    if needle.is_empty() {
        return found;
    }
    let mut cursor = 0;
    while found.len() < MAX_OCCURRENCES {
        let Some(offset) = text[cursor..].find(needle) else {
            break;
        };
        let start = cursor + offset;
        found.push(start..start + needle.len());
        cursor = start + needle.len();
    }
    found
}

fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_occurrence_finds_the_match_after_from() {
        let text = "one two one two";
        assert_eq!(next_occurrence(text, "two", 0), Some(4..7));
        assert_eq!(next_occurrence(text, "two", 5), Some(12..15));
    }

    #[test]
    fn next_occurrence_matches_exactly_at_from() {
        assert_eq!(next_occurrence("abcabc", "abc", 3), Some(3..6));
    }

    #[test]
    fn next_occurrence_wraps_to_an_earlier_match() {
        let text = "needle haystack";
        assert_eq!(next_occurrence(text, "needle", 7), Some(0..6));
    }

    #[test]
    fn next_occurrence_is_none_when_there_is_nothing_to_find() {
        assert_eq!(next_occurrence("abc", "", 0), None);
        assert_eq!(next_occurrence("abc", "zzz", 0), None);
        assert_eq!(next_occurrence("abc", "a", 4), None);
        assert_eq!(next_occurrence("", "a", 0), None);
    }

    #[test]
    fn next_occurrence_is_case_sensitive() {
        assert_eq!(next_occurrence("Count count", "count", 0), Some(6..11));
    }

    #[test]
    fn next_occurrence_returns_char_boundaries_for_a_multibyte_needle() {
        let text = "let \u{043a}\u{043b}\u{044e}\u{0447} = \u{043a}\u{043b}\u{044e}\u{0447};";
        let range = next_occurrence(text, "\u{043a}\u{043b}\u{044e}\u{0447}", 0).unwrap();
        assert!(text.is_char_boundary(range.start));
        assert!(text.is_char_boundary(range.end));
        assert_eq!(&text[range.clone()], "\u{043a}\u{043b}\u{044e}\u{0447}");

        let second = next_occurrence(text, "\u{043a}\u{043b}\u{044e}\u{0447}", range.end).unwrap();
        assert!(second.start > range.start);
        assert!(text.is_char_boundary(second.start));
    }

    #[test]
    fn next_occurrence_survives_a_from_inside_a_codepoint() {
        let text = "a\u{1F600}b";
        // Offset 2 is inside the emoji: floored to 1, which still finds "b".
        assert_eq!(next_occurrence(text, "b", 2), Some(5..6));
    }

    #[test]
    fn all_occurrences_finds_every_match_left_to_right() {
        assert_eq!(all_occurrences("a b a b a", "a"), vec![0..1, 4..5, 8..9]);
    }

    #[test]
    fn all_occurrences_does_not_overlap() {
        assert_eq!(all_occurrences("aaaa", "aa"), vec![0..2, 2..4]);
    }

    #[test]
    fn all_occurrences_is_empty_without_a_match() {
        assert!(all_occurrences("abc", "").is_empty());
        assert!(all_occurrences("abc", "z").is_empty());
        assert!(all_occurrences("", "a").is_empty());
    }

    #[test]
    fn all_occurrences_stops_at_the_cap() {
        let text = "e".repeat(MAX_OCCURRENCES + 500);
        let found = all_occurrences(&text, "e");
        assert_eq!(found.len(), MAX_OCCURRENCES);
        assert_eq!(
            found[MAX_OCCURRENCES - 1],
            MAX_OCCURRENCES - 1..MAX_OCCURRENCES
        );
    }
}
