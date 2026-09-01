//! Extend/Shrink Selection (`docs/features/line-commands-and-editorconfig.md`
//! §2.3, §3.4): the caret -> word -> bracket-pair-contents -> pair ladder
//! `extended_selection` climbs one rung at a time.

use std::ops::Range;

use super::{BracketPair, Selection, TextBuffer};

impl TextBuffer {
    /// §3.4. The next range out in the selection hierarchy from `selection`:
    /// caret -> word -> the contents of the innermost enclosing bracket pair
    /// -> that pair including its brackets -> the next pair out -> ... ->
    /// the whole buffer. `None` when `selection` is already the whole
    /// buffer.
    pub fn extended_selection(&self, selection: Selection) -> Option<Selection> {
        let text = self.text();
        if selection.range() == (0..text.len()) {
            return None;
        }

        if selection.is_empty() {
            if let Some(word) = word_at(text, selection.head) {
                return Some(preserve_direction(selection, word));
            }
        }

        if let Some(pair) = self.enclosing_bracket_pair(selection.range()) {
            let contents = pair.open.end..pair.close.start;
            let grown = if selection.range() == contents {
                pair.open.start..pair.close.end
            } else {
                contents
            };
            return Some(preserve_direction(selection, grown));
        }

        Some(Selection::new(0, text.len()))
    }

    /// The innermost bracket pair enclosing (or exactly bounding) `range`:
    /// the nearest opener before `range.start` whose own matching closer
    /// reaches at least `range.end`, skipping any bracket inside a string or
    /// comment the same way `matching_bracket`'s own scan does. `None` when
    /// there is no such pair, when `syntax()` has no rules or no brackets,
    /// or when the buffer is too large to have been tokenized.
    fn enclosing_bracket_pair(&self, range: Range<usize>) -> Option<BracketPair> {
        let rules = self.syntax()?;
        if rules.brackets.is_empty() {
            return None;
        }
        let text = self.text();
        let is_open = |c: char| rules.brackets.iter().any(|(open, _)| *open == c);
        let is_close = |c: char| rules.brackets.iter().any(|(_, close)| *close == c);

        let mut depth = 0i32;
        let mut opener = None;
        for (at, c) in text[..range.start.min(text.len())].char_indices().rev() {
            if self.is_quoted_or_commented(at) {
                continue;
            }
            if is_close(c) {
                depth += 1;
            } else if is_open(c) {
                if depth == 0 {
                    opener = Some(at);
                    break;
                }
                depth -= 1;
            }
        }

        let pair = self.matching_bracket(opener?)?;
        (pair.close.end >= range.end).then_some(pair)
    }
}

/// Preserves `original`'s direction (reversed when its `head` sits before
/// its `anchor`) while replacing its range with `grown`. A caret has no
/// direction of its own, so it grows forward.
fn preserve_direction(original: Selection, grown: Range<usize>) -> Selection {
    if original.head < original.anchor {
        Selection::new(grown.end, grown.start)
    } else {
        Selection::new(grown.start, grown.end)
    }
}

/// The run of identifier characters `offset` touches -- `[A-Za-z0-9_]` plus
/// any `char::is_alphanumeric`, resolving leftwards at a boundary (a caret
/// right after a word, with a non-word character or nothing following,
/// selects the word behind it rather than nothing). Unlike
/// `ide_ui::word_range_at`, this does **not** reject a run starting with a
/// digit: `⌥↑` on `42` should select `42`, while a hover link on `42`
/// should stay unlit. The two rules differ on purpose.
pub fn word_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let offset = offset.min(text.len());

    let grow = |probe: usize| -> Option<Range<usize>> {
        if !text[probe..].chars().next().is_some_and(is_word) {
            return None;
        }
        let mut start = probe;
        while start > 0 {
            let prev = text[..start].chars().next_back().unwrap();
            if !is_word(prev) {
                break;
            }
            start -= prev.len_utf8();
        }
        let mut end = probe;
        while end < text.len() {
            let c = text[end..].chars().next().unwrap();
            if !is_word(c) {
                break;
            }
            end += c.len_utf8();
        }
        Some(start..end)
    };

    if offset > 0 {
        let prev = text[..offset].chars().next_back().unwrap();
        let prev_start = offset - prev.len_utf8();
        if is_word(prev) {
            return grow(prev_start);
        }
    }
    grow(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{MARKDOWN, RUST};
    use crate::text::Selections;

    fn rust(text: &str) -> TextBuffer {
        TextBuffer::new(text, Some(&RUST))
    }

    #[test]
    fn word_at_grows_in_both_directions() {
        assert_eq!(word_at("let value = 1;", 6), Some(4..9));
    }

    #[test]
    fn word_at_resolves_a_boundary_leftwards() {
        // Offset 5 sits right after "alpha" and right before " ".
        assert_eq!(word_at("alpha beta", 5), Some(0..5));
    }

    #[test]
    fn word_at_accepts_a_run_starting_with_a_digit() {
        assert_eq!(word_at("x 42 y", 2), Some(2..4));
    }

    #[test]
    fn word_at_is_none_off_any_word() {
        assert_eq!(word_at("a   b", 2), None);
        assert_eq!(word_at("", 0), None);
    }

    #[test]
    fn extended_selection_walks_caret_to_word_to_pair_to_buffer() {
        let buffer = rust("f(x + 1)");
        let mut selection = Selection::caret(2); // inside `x`
        selection = buffer.extended_selection(selection).unwrap();
        assert_eq!(selection.range(), 2..3); // `x`
        selection = buffer.extended_selection(selection).unwrap();
        assert_eq!(selection.range(), 2..7); // `x + 1`
        selection = buffer.extended_selection(selection).unwrap();
        assert_eq!(selection.range(), 1..8); // `(x + 1)`
        selection = buffer.extended_selection(selection).unwrap();
        assert_eq!(selection.range(), 0..8); // whole buffer
        assert_eq!(buffer.extended_selection(selection), None);
    }

    #[test]
    fn extended_selection_walks_outward_through_nested_pairs() {
        // The inner pair sits inside more of the outer pair's contents than
        // just itself, so the outer pair's "contents" rung (1..8) is a real,
        // distinct step between the inner pair-with-brackets and the outer
        // pair-with-brackets.
        let buffer = rust("(x (y) z)");
        let inner_contents = Selection::new(4, 5); // `y`
        let with_inner_brackets = buffer.extended_selection(inner_contents).unwrap();
        assert_eq!(with_inner_brackets.range(), 3..6); // `(y)`
        let outer_contents = buffer.extended_selection(with_inner_brackets).unwrap();
        assert_eq!(outer_contents.range(), 1..8); // `x (y) z`
        let with_outer_brackets = buffer.extended_selection(outer_contents).unwrap();
        assert_eq!(with_outer_brackets.range(), 0..9); // `(x (y) z)`
    }

    #[test]
    fn extended_selection_skips_brackets_inside_a_string_or_comment() {
        let buffer = rust(r#"f("(", x)"#);
        let caret = Selection::caret(4); // inside the string literal
        let word_step = buffer.extended_selection(caret);
        // No word under a quote char; goes straight to the enclosing pair's
        // contents, which must be the call's real parens, not the quoted
        // one -- `f(` `"(", x` `)`.
        let pair = word_step.unwrap();
        assert_eq!(pair.range(), 2..8);
    }

    #[test]
    fn extended_selection_on_a_language_without_brackets_skips_straight_to_buffer() {
        let buffer = TextBuffer::new("plain text", None);
        let caret = Selection::caret(2); // inside "plain"
        let word = buffer.extended_selection(caret).unwrap();
        assert_eq!(word.range(), 0..5);
        let whole = buffer.extended_selection(word).unwrap();
        assert_eq!(whole.range(), 0..10);
        assert_eq!(buffer.extended_selection(whole), None);
    }

    #[test]
    fn extended_selection_none_at_the_whole_buffer() {
        let buffer = rust("abc");
        assert_eq!(buffer.extended_selection(Selection::new(0, 3)), None);
    }

    #[test]
    fn extended_selection_preserves_a_reversed_direction() {
        let buffer = rust("f(x)");
        let reversed = Selection::new(3, 2); // head before anchor, over `x`
        let grown = buffer.extended_selection(reversed).unwrap();
        assert!(grown.head < grown.anchor);
        assert_eq!(grown.range(), 1..4);
    }

    #[test]
    fn extended_selection_uses_markdown_brackets() {
        let buffer = TextBuffer::new("[a](b)", Some(&MARKDOWN));
        let caret = Selection::caret(1);
        let word = buffer.extended_selection(caret).unwrap();
        assert_eq!(word.range(), 1..2); // `a`
        let with_brackets = buffer.extended_selection(word).unwrap();
        assert_eq!(with_brackets.range(), 0..3); // `[a]`
    }

    #[test]
    fn extended_selection_is_deterministic_regardless_of_other_selections() {
        // extended_selection reads only `selection`/`syntax()`, not
        // `self.selections()` -- unaffected by whatever else is selected.
        let mut buffer = rust("f(x)");
        buffer.set_selections(Selections::single(Selection::caret(0)));
        assert_eq!(
            buffer.extended_selection(Selection::caret(2)),
            Some(Selection::new(2, 3))
        );
    }
}
