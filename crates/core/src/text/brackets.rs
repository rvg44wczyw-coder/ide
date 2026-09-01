//! Matching-bracket search (`docs/features/smart-editing.md` §2.5, §3.4).
//!
//! The scan is a plain depth count over the text, with one lookup per
//! candidate into the buffer's already-maintained `tokens()` to skip
//! brackets that live inside a string or a comment. That lookup is a
//! binary search, so a match `n` bytes away costs O(n log T) and nothing is
//! ever re-tokenized.

use std::ops::Range;

use crate::syntax::{SyntaxRules, TokenKind, MAX_HIGHLIGHTED_FILE_BYTES};

use super::TextBuffer;

/// How far `TextBuffer::matching_bracket` scans away from the caret before
/// giving up. Deliberately **not** `MAX_HIGHLIGHTED_FILE_BYTES`: that one is
/// a file-size threshold, so reusing it as a distance would cap nothing at
/// all in any file below it. This is a distance, and it bounds the
/// per-frame highlight on an unmatched bracket at the top of a large file.
pub const MAX_BRACKET_SCAN_BYTES: usize = 128 * 1024;

/// Not `Copy`: `Range<usize>` isn't, and holding two of them is the whole
/// point of the type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BracketPair {
    /// The opening bracket's byte range, always the earlier of the two.
    pub open: Range<usize>,
    pub close: Range<usize>,
}

impl TextBuffer {
    /// The bracket matching the one at or immediately before `offset`.
    /// `None` when `offset` is not touching a bracket, when the bracket is
    /// unmatched, when it is inside a string or a comment, or when the match
    /// is further away than `MAX_BRACKET_SCAN_BYTES`.
    ///
    /// Also `None` on a buffer the tokenizer refused (larger than
    /// `MAX_HIGHLIGHTED_FILE_BYTES`): without
    /// tokens nothing is a string, and counting brackets inside string
    /// literals is a worse answer than no answer at all
    /// (`smart-editing.md` §3.4).
    pub fn matching_bracket(&self, offset: usize) -> Option<BracketPair> {
        let rules = self.syntax()?;
        // The threshold, not `tokens().is_empty()`: a short Markdown file
        // legitimately produces no tokens, and refusing there would be a
        // bug rather than the documented degradation.
        if rules.brackets.is_empty() || self.len() > MAX_HIGHLIGHTED_FILE_BYTES {
            return None;
        }
        let text = self.text();
        let (at, c) = touched_bracket(text, offset, rules)?;
        if self.is_quoted_or_commented(at) {
            return None;
        }

        if let Some((_, close)) = rules.brackets.iter().find(|(open, _)| *open == c) {
            let end = self.scan(at + c.len_utf8(), c, *close, true)?;
            Some(BracketPair {
                open: at..at + c.len_utf8(),
                close: end..end + close.len_utf8(),
            })
        } else {
            let (open, _) = rules.brackets.iter().find(|(_, close)| *close == c)?;
            let start = self.scan(at, *open, c, false)?;
            Some(BracketPair {
                open: start..start + open.len_utf8(),
                close: at..at + c.len_utf8(),
            })
        }
    }

    /// Walks outward from `from` counting depth, returning the offset of the
    /// bracket that closes (or opens) the one the caller started on.
    fn scan(&self, from: usize, open: char, close: char, forward: bool) -> Option<usize> {
        let text = self.text();
        let mut depth = 1i32;
        let scanned: Box<dyn Iterator<Item = (usize, char)>> = if forward {
            Box::new(text[from..].char_indices().map(move |(i, c)| (from + i, c)))
        } else {
            Box::new(text[..from].char_indices().rev())
        };
        for (at, c) in scanned {
            if at.abs_diff(from) > MAX_BRACKET_SCAN_BYTES {
                return None;
            }
            if c != open && c != close {
                continue;
            }
            if self.is_quoted_or_commented(at) {
                continue;
            }
            let opening = if forward { c == open } else { c == close };
            depth += if opening { 1 } else { -1 };
            if depth == 0 {
                return Some(at);
            }
        }
        None
    }

    /// Whether `offset` falls inside a `String` or `Comment` token. A
    /// `partition_point` rather than a linear walk, since `tokens()` is
    /// sorted by offset. `pub(crate)`: `selection_hierarchy`'s enclosing-pair
    /// scan needs the same quoted/commented skip this module's own scan
    /// already implements.
    pub(crate) fn is_quoted_or_commented(&self, offset: usize) -> bool {
        let tokens = self.tokens();
        let index = tokens.partition_point(|t| t.range.end <= offset);
        tokens.get(index).is_some_and(|token| {
            token.range.start <= offset
                && matches!(token.kind, TokenKind::String | TokenKind::Comment)
        })
    }
}

/// The bracket the caret touches: the character *after* `offset` first,
/// because that is where the caret sits when the user has just typed one,
/// then the character before it.
fn touched_bracket(text: &str, offset: usize, rules: &SyntaxRules) -> Option<(usize, char)> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let is_bracket = |c: char| {
        rules
            .brackets
            .iter()
            .any(|(open, close)| *open == c || *close == c)
    };
    if let Some(c) = text[offset..].chars().next().filter(|c| is_bracket(*c)) {
        return Some((offset, c));
    }
    let c = text[..offset]
        .chars()
        .next_back()
        .filter(|c| is_bracket(*c))?;
    Some((offset - c.len_utf8(), c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{MARKDOWN, RUST};

    fn rust(text: &str) -> TextBuffer {
        TextBuffer::new(text, Some(&RUST))
    }

    #[test]
    fn matches_forward_from_an_opener_and_backward_from_a_closer() {
        let buffer = rust("fn f() { g(); }");
        let pair = buffer.matching_bracket(7).expect("the block's braces");
        assert_eq!(pair.open, 7..8);
        assert_eq!(pair.close, 14..15);
        // Backward from the closer names the same pair.
        assert_eq!(buffer.matching_bracket(15), Some(pair));
    }

    #[test]
    fn the_bracket_after_the_caret_wins_over_the_one_before_it() {
        let buffer = rust("(){}");
        // Offset 2 has ')' before it and '{' after it.
        let pair = buffer
            .matching_bracket(2)
            .expect("the braces, not the parens");
        assert_eq!(pair.open, 2..3);
        assert_eq!(pair.close, 3..4);
    }

    #[test]
    fn brackets_inside_a_string_or_comment_are_skipped() {
        let buffer = rust(r#"f("(" , x)"#);
        let pair = buffer.matching_bracket(1).expect("the call's parens match");
        assert_eq!(pair.open, 1..2);
        assert_eq!(pair.close, 9..10);

        // The one inside the literal is not a bracket at all.
        assert_eq!(buffer.matching_bracket(3), None);

        let commented = rust("f(\n// )\n)");
        let pair = commented.matching_bracket(1).expect("the real closer");
        assert_eq!(pair.close, 8..9);
    }

    #[test]
    fn an_unmatched_bracket_or_a_non_bracket_is_none() {
        assert_eq!(rust("f(").matching_bracket(1), None);
        assert_eq!(rust(")").matching_bracket(0), None);
        assert_eq!(rust("abc").matching_bracket(1), None);
        assert_eq!(rust("").matching_bracket(0), None);
    }

    #[test]
    fn nesting_is_counted_rather_than_matched_greedily() {
        let buffer = rust("((a))");
        let outer = buffer.matching_bracket(0).expect("outer");
        assert_eq!(outer.close, 4..5);
        let inner = buffer.matching_bracket(1).expect("inner");
        assert_eq!(inner.close, 3..4);
    }

    #[test]
    fn a_language_without_brackets_or_without_syntax_never_matches() {
        assert_eq!(TextBuffer::new("[a](b)", None).matching_bracket(0), None);
        // Markdown does have brackets, so this proves the `None` above is
        // about the missing rules and not about the text.
        assert!(TextBuffer::new("[a](b)", Some(&MARKDOWN))
            .matching_bracket(0)
            .is_some());
    }

    #[test]
    fn an_untokenized_buffer_refuses_rather_than_guessing() {
        // Above the tokenizer's threshold `tokens()` is empty, so nothing
        // reads as a string and a brace inside one would count.
        let padding = "x".repeat(MAX_HIGHLIGHTED_FILE_BYTES + 1);
        let buffer = rust(&format!("{{}}\n{padding}"));
        assert!(buffer.tokens().is_empty());
        assert_eq!(buffer.matching_bracket(0), None);
    }

    #[test]
    fn the_scan_gives_up_past_the_distance_cap() {
        let filler = " ".repeat(MAX_BRACKET_SCAN_BYTES + 8);
        let buffer = rust(&format!("({filler})"));
        assert_eq!(buffer.matching_bracket(0), None);
    }

    #[test]
    fn multibyte_text_never_splits_a_character() {
        let buffer = rust("(\u{4F60}\u{597D})");
        let pair = buffer.matching_bracket(0).expect("the parens");
        assert_eq!(pair.open, 0..1);
        assert_eq!(pair.close, 7..8);
        // An offset inside the multi-byte character is not a boundary.
        assert_eq!(buffer.matching_bracket(2), None);
    }
}
