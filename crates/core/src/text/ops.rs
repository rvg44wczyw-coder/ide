//! The editing commands A4a adds (`docs/features/smart-editing.md` §2.4).
//!
//! Each is one `Transaction` over every selection -- therefore one undo
//! step -- and each sets the resulting selections itself, which is why they
//! are `TextBuffer` methods rather than free functions returning a
//! `Transaction`: the selection each leaves behind is part of the command,
//! not something the caller can derive from the edit.

use std::ops::Range;

use super::indent::{leading_whitespace, IndentUnit};
use super::{Change, Selection, Selections, TextBuffer, Transaction};

impl TextBuffer {
    /// Adds one indent level to every line each selection touches.
    pub fn indent_selection_lines(&mut self, unit: IndentUnit) -> bool {
        let one = unit.one().into_owned();
        let changes: Vec<Change> = self
            .touched_lines()
            .into_iter()
            .map(|range| Change::new(range.start..range.start, one.clone()))
            .collect();
        self.apply_lines(changes)
    }

    /// Removes up to one indent level from every line each selection
    /// touches. A line with no leading whitespace is left alone rather than
    /// blocking the whole operation.
    pub fn outdent_selection_lines(&mut self, unit: IndentUnit) -> bool {
        let text = self.text();
        let changes: Vec<Change> = self
            .touched_lines()
            .into_iter()
            .filter_map(|range| {
                let indent = leading_whitespace(&text[range.clone()]);
                let removed = outdent_len(indent, unit);
                (removed > 0).then(|| Change::new(range.start..range.start + removed, ""))
            })
            .collect();
        self.apply_lines(changes)
    }

    /// Wraps every non-empty selection in `open`/`close`, leaving each
    /// selection over the original text rather than over the delimiters.
    /// `false` when every selection is empty -- the caller then types the
    /// character normally.
    pub fn surround_selections(&mut self, open: char, close: char) -> bool {
        let wrapped: Vec<Range<usize>> = self
            .selections()
            .all()
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.range())
            .collect();
        if wrapped.is_empty() {
            return false;
        }
        let text = self.text();
        let changes: Vec<Change> = wrapped
            .iter()
            .map(|range| {
                Change::new(
                    range.clone(),
                    format!("{open}{}{close}", &text[range.clone()]),
                )
            })
            .collect();
        let Ok(transaction) = Transaction::new(changes) else {
            return false;
        };

        // The new selections are computed here rather than taken from
        // `Selections::map`: mapping a range that was wholly replaced
        // collapses it to a bare caret past the insertion (the same rule
        // `insert_at_selections` documents), and this command's contract is
        // that each selection still covers its original text. Selections are
        // sorted, so one running delta is enough.
        let widened = open.len_utf8() + close.len_utf8();
        let mut delta = 0;
        let mut moved = Vec::with_capacity(self.selections().len());
        for selection in self.selections().all() {
            let (start, end) = if selection.is_empty() {
                (selection.head + delta, selection.head + delta)
            } else {
                let start = selection.start() + delta + open.len_utf8();
                let end = selection.end() + delta + open.len_utf8();
                delta += widened;
                (start, end)
            };
            moved.push(if selection.head < selection.anchor {
                Selection::new(end, start)
            } else {
                Selection::new(start, end)
            });
        }
        let primary = self.selections().primary_index();
        self.apply(transaction);
        self.set_selections(Selections::new(moved, primary));
        true
    }

    /// Each selection's line span, merged where two selections touch the
    /// same line, so two cursors on one line indent it once.
    fn touched_lines(&self) -> Vec<Range<usize>> {
        let text = self.text();
        let mut lines: Vec<usize> = self
            .selections()
            .all()
            .iter()
            .flat_map(|s| self.lines().line_at(s.start())..=self.lines().line_at(s.end()))
            .collect();
        lines.dedup();
        lines
            .into_iter()
            .filter_map(|line| self.lines().line_range(line, text))
            .collect()
    }

    /// Applies a per-line edit, keeping the selections `Selections::map`
    /// derives -- for indent and outdent that is exactly right, since each
    /// caret should follow the text it was sitting on.
    fn apply_lines(&mut self, changes: Vec<Change>) -> bool {
        if changes.is_empty() {
            return false;
        }
        let Ok(transaction) = Transaction::new(changes) else {
            return false;
        };
        self.apply(transaction);
        true
    }
}

/// How many bytes of `indent` one outdent removes: the shortest prefix
/// worth at least one level in display columns, never splitting a
/// character. Measured against the prefix rather than the remainder because
/// a tab's width depends on where it starts, and an indent starts at
/// column zero.
fn outdent_len(indent: &str, unit: IndentUnit) -> usize {
    let wanted = unit.width.max(1).min(unit.columns_of(indent));
    if wanted == 0 {
        return 0;
    }
    for (byte, c) in indent.char_indices() {
        let end = byte + c.len_utf8();
        if unit.columns_of(&indent[..end]) >= wanted {
            return end;
        }
    }
    indent.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::RUST;
    use crate::text::IndentStyle;

    fn rust(text: &str) -> TextBuffer {
        TextBuffer::new(text, Some(&RUST))
    }

    fn select(buffer: &mut TextBuffer, ranges: &[(usize, usize)]) {
        let selections = ranges.iter().map(|(a, h)| Selection::new(*a, *h)).collect();
        buffer.set_selections(Selections::new(selections, 0));
    }

    #[test]
    fn indent_adds_one_level_to_every_touched_line() {
        let mut buffer = rust("a\nb\nc\n");
        select(&mut buffer, &[(0, 3)]);
        assert!(buffer.indent_selection_lines(IndentUnit::default()));
        assert_eq!(buffer.text(), "    a\n    b\nc\n");
    }

    #[test]
    fn indent_and_outdent_round_trip() {
        let mut buffer = rust("a\nb\n");
        select(&mut buffer, &[(0, 3)]);
        let unit = IndentUnit::default();
        assert!(buffer.indent_selection_lines(unit));
        assert!(buffer.outdent_selection_lines(unit));
        assert_eq!(buffer.text(), "a\nb\n");
    }

    #[test]
    fn two_cursors_on_one_line_indent_it_once() {
        let mut buffer = rust("hello\n");
        select(&mut buffer, &[(0, 0), (3, 3)]);
        assert!(buffer.indent_selection_lines(IndentUnit::default()));
        assert_eq!(buffer.text(), "    hello\n");
    }

    #[test]
    fn outdent_leaves_an_unindented_line_alone_without_blocking_the_rest() {
        let mut buffer = rust("    a\nb\n");
        select(&mut buffer, &[(0, 7)]);
        assert!(buffer.outdent_selection_lines(IndentUnit::default()));
        assert_eq!(buffer.text(), "a\nb\n");
    }

    #[test]
    fn outdent_with_nothing_to_remove_reports_no_change() {
        let mut buffer = rust("a\nb\n");
        select(&mut buffer, &[(0, 3)]);
        assert!(!buffer.outdent_selection_lines(IndentUnit::default()));
        assert_eq!(buffer.text(), "a\nb\n");
    }

    #[test]
    fn outdent_removes_a_partial_level_rather_than_refusing() {
        let mut buffer = rust("  a\n");
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.outdent_selection_lines(IndentUnit::default()));
        assert_eq!(buffer.text(), "a\n");
    }

    #[test]
    fn a_tab_unit_indents_with_tabs() {
        let unit = IndentUnit {
            style: IndentStyle::Tabs,
            width: 4,
        };
        let mut buffer = rust("a\n");
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.indent_selection_lines(unit));
        assert_eq!(buffer.text(), "\ta\n");
        assert!(buffer.outdent_selection_lines(unit));
        assert_eq!(buffer.text(), "a\n");
    }

    #[test]
    fn indenting_is_one_undo_step_across_every_cursor() {
        let mut buffer = rust("a\nb\nc\n");
        select(&mut buffer, &[(0, 0), (4, 4)]);
        assert!(buffer.indent_selection_lines(IndentUnit::default()));
        assert_eq!(buffer.text(), "    a\nb\n    c\n");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "a\nb\nc\n");
    }

    #[test]
    fn surround_wraps_every_selection_and_keeps_it_on_the_original_text() {
        let mut buffer = rust("alpha bravo");
        select(&mut buffer, &[(0, 5), (6, 11)]);
        assert!(buffer.surround_selections('"', '"'));
        assert_eq!(buffer.text(), r#""alpha" "bravo""#);
        let selections = buffer.selections();
        assert_eq!(&buffer.text()[selections.all()[0].range()], "alpha");
        assert_eq!(&buffer.text()[selections.all()[1].range()], "bravo");
    }

    #[test]
    fn surround_nests_and_undoes_in_one_step() {
        let mut buffer = rust("x");
        select(&mut buffer, &[(0, 1)]);
        assert!(buffer.surround_selections('(', ')'));
        assert!(buffer.surround_selections('[', ']'));
        assert_eq!(buffer.text(), "([x])");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "(x)");
    }

    #[test]
    fn surround_keeps_a_reversed_selections_direction() {
        let mut buffer = rust("word");
        select(&mut buffer, &[(4, 0)]);
        assert!(buffer.surround_selections('(', ')'));
        let primary = buffer.selections().primary();
        assert!(primary.head < primary.anchor, "direction was reversed");
        assert_eq!(&buffer.text()[primary.range()], "word");
    }

    #[test]
    fn surround_with_only_empty_selections_reports_no_change() {
        let mut buffer = rust("abc");
        select(&mut buffer, &[(1, 1)]);
        assert!(!buffer.surround_selections('(', ')'));
        assert_eq!(buffer.text(), "abc");
    }

    #[test]
    fn surround_carries_the_empty_selections_among_non_empty_ones() {
        let mut buffer = rust("ab cd");
        select(&mut buffer, &[(0, 2), (4, 4)]);
        assert!(buffer.surround_selections('(', ')'));
        assert_eq!(buffer.text(), "(ab) cd");
        // The bare caret was on 'd'; it must still be, two delimiters later.
        assert_eq!(buffer.selections().all()[1], Selection::caret(6));
        assert_eq!(&buffer.text()[6..7], "d");
    }

    #[test]
    fn surround_handles_multibyte_delimiters_and_content() {
        let mut buffer = rust("\u{4F60}\u{597D}");
        select(&mut buffer, &[(0, 6)]);
        assert!(buffer.surround_selections('\u{ABB}', '\u{ABB}'));
        assert_eq!(buffer.text(), "\u{ABB}\u{4F60}\u{597D}\u{ABB}");
        assert_eq!(
            &buffer.text()[buffer.selections().primary().range()],
            "\u{4F60}\u{597D}"
        );
    }
}
