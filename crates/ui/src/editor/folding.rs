//! Collapsed-fold visual-row mapping (`docs/features/code-folding.md` §2.2,
//! §3.4, §3.6). No `IdeApp`/`egui` dependency -- a sibling of `geometry.rs`'s
//! own "no `Ui`, no state, no painting" boundary, except for
//! `reveal_caret_after_collapse`, which needs `Buffer` for the same reason
//! `CodeEditor`'s own `goto_offset` handling does (§2.6).

use std::collections::{BTreeMap, BTreeSet};

use ide_core::{Buffer, FoldRange, Selection, Selections};

use super::EditorState;

/// Maps buffer lines to visual rows, hiding every line inside a collapsed
/// fold range except the range's own `start_line`.
pub struct VisualLines {
    rows: Vec<usize>,
}

impl VisualLines {
    /// `folded` is the set of currently-collapsed `start_line`s. When two or
    /// more of `ranges` share a `start_line`, only the largest `end_line`
    /// among them (the outermost) determines how much is hidden if that
    /// `start_line` is collapsed -- the same tie-break `paint_gutter`'s
    /// arrow uses (§3.6), since `folded` has no way to distinguish between
    /// two ranges sharing one `start_line`.
    pub fn build(line_count: usize, ranges: &[FoldRange], folded: &BTreeSet<usize>) -> Self {
        let mut outermost_end: BTreeMap<usize, usize> = BTreeMap::new();
        for range in ranges {
            outermost_end
                .entry(range.start_line)
                .and_modify(|end| *end = (*end).max(range.end_line))
                .or_insert(range.end_line);
        }

        let mut hidden = vec![false; line_count];
        for (&start_line, &end_line) in &outermost_end {
            if !folded.contains(&start_line) {
                continue;
            }
            for line in (start_line + 1)..=end_line {
                if let Some(slot) = hidden.get_mut(line) {
                    *slot = true;
                }
            }
        }

        let rows = (0..line_count).filter(|line| !hidden[*line]).collect();
        Self { rows }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The buffer line for visual `row`, clamped to the last row. `rows` is
    /// never empty: line `0` can never be hidden (the smallest possible
    /// hidden line is `start_line + 1 >= 1`), so at least one row always
    /// exists.
    pub fn buffer_line(&self, row: usize) -> usize {
        match self.rows.get(row) {
            Some(&line) => line,
            None => *self.rows.last().unwrap_or(&0),
        }
    }

    /// The visual row for `buffer_line` -- its own row if visible, otherwise
    /// the row of the collapsed fold that hides it.
    pub fn row_of(&self, buffer_line: usize) -> usize {
        match self.rows.binary_search(&buffer_line) {
            Ok(row) => row,
            Err(row) => row.saturating_sub(1),
        }
    }
}

/// The collapse-side mirror of `EditorState::reveal_line`: call after any
/// operation that may have just collapsed a range covering a caret's current
/// line. A no-op for any selection whose line is still visible; otherwise
/// moves that selection to a bare caret at the end of the nearest visible
/// line at or before it -- which, for a range that was just collapsed around
/// the caret, is exactly that range's `start_line`. Applies to every
/// selection, not just the primary one: `CollapseAllFolds` can hide more
/// than one cursor's line at once in a multi-cursor buffer.
pub fn reveal_caret_after_collapse(buffer: &mut Buffer, state: &EditorState) {
    let text_buffer = buffer.text_buffer();
    let line_count = text_buffer.lines().line_count();
    let ranges = text_buffer.fold_ranges();
    let visual = state.visual_lines(line_count, &ranges);
    let selections = text_buffer.selections().clone();

    let mut changed = false;
    let revealed: Vec<Selection> = selections
        .all()
        .iter()
        .map(|selection| {
            let head_line = text_buffer.lines().line_at(selection.head);
            let visible_line = visual.buffer_line(visual.row_of(head_line));
            if visible_line == head_line {
                return *selection;
            }
            changed = true;
            let offset = text_buffer
                .lines()
                .line_range(visible_line, text_buffer.text())
                .map_or(selection.head, |range| range.end);
            Selection::caret(offset)
        })
        .collect();

    if changed {
        let primary = selections.primary_index();
        buffer
            .text_buffer_mut()
            .set_selections(Selections::new(revealed, primary));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::FoldKind;
    use std::collections::BTreeSet;

    fn range(start_line: usize, end_line: usize) -> FoldRange {
        FoldRange {
            start_line,
            end_line,
            kind: FoldKind::Brace,
        }
    }

    #[test]
    fn nothing_folded_maps_rows_to_themselves() {
        let visual = VisualLines::build(5, &[], &BTreeSet::new());
        assert_eq!(visual.row_count(), 5);
        for line in 0..5 {
            assert_eq!(visual.buffer_line(line), line);
            assert_eq!(visual.row_of(line), line);
        }
    }

    #[test]
    fn a_collapsed_range_hides_its_interior_but_not_its_start_line() {
        let ranges = vec![range(1, 3)];
        let folded = BTreeSet::from([1]);
        let visual = VisualLines::build(5, &ranges, &folded);
        // Lines 0, 1, 4 remain: 1's interior (2, 3) is hidden.
        assert_eq!(visual.row_count(), 3);
        assert_eq!(visual.buffer_line(0), 0);
        assert_eq!(visual.buffer_line(1), 1);
        assert_eq!(visual.buffer_line(2), 4);
    }

    #[test]
    fn an_uncollapsed_range_hides_nothing() {
        let ranges = vec![range(1, 3)];
        let visual = VisualLines::build(5, &ranges, &BTreeSet::new());
        assert_eq!(visual.row_count(), 5);
    }

    #[test]
    fn row_of_a_hidden_line_resolves_to_its_folds_start_line_row() {
        let ranges = vec![range(1, 3)];
        let folded = BTreeSet::from([1]);
        let visual = VisualLines::build(5, &ranges, &folded);
        assert_eq!(visual.row_of(1), 1);
        assert_eq!(visual.row_of(2), 1);
        assert_eq!(visual.row_of(3), 1);
        assert_eq!(visual.row_of(4), 2);
    }

    #[test]
    fn buffer_line_clamps_a_row_past_the_end() {
        let visual = VisualLines::build(3, &[], &BTreeSet::new());
        assert_eq!(visual.buffer_line(99), 2);
    }

    #[test]
    fn two_ranges_sharing_a_start_line_use_the_outermost_when_collapsed() {
        // Both open on line 0: one closes at line 2, the other at line 4.
        let ranges = vec![range(0, 2), range(0, 4)];
        let folded = BTreeSet::from([0]);
        let visual = VisualLines::build(6, &ranges, &folded);
        // Everything through line 4 is hidden (the outermost's end_line),
        // not just through line 2.
        assert_eq!(visual.row_count(), 2);
        assert_eq!(visual.buffer_line(0), 0);
        assert_eq!(visual.buffer_line(1), 5);
    }

    #[test]
    fn nested_collapsed_ranges_compose() {
        // 0..=5 and 1..=3, both collapsed: everything but line 0 is hidden.
        let ranges = vec![range(0, 5), range(1, 3)];
        let folded = BTreeSet::from([0, 1]);
        let visual = VisualLines::build(6, &ranges, &folded);
        assert_eq!(visual.row_count(), 1);
        assert_eq!(visual.buffer_line(0), 0);
    }

    #[test]
    fn line_zero_is_never_hidden() {
        // A pathological range claiming to start before the buffer even
        // begins can't occur from `fold_ranges()`, but `build` still must
        // not hide line 0 -- the invariant every other mechanism relies on.
        let visual = VisualLines::build(3, &[], &BTreeSet::new());
        assert_eq!(visual.buffer_line(0), 0);
    }

    #[test]
    fn reveal_caret_after_collapse_is_a_no_op_when_still_visible() {
        let mut buffer = Buffer::untitled();
        buffer.insert(0, "a\nb\nc\n");
        buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(0)));
        let state = EditorState::default();
        let before = buffer.text_buffer().selections().clone();
        reveal_caret_after_collapse(&mut buffer, &state);
        assert_eq!(buffer.text_buffer().selections(), &before);
    }

    #[test]
    fn reveal_caret_after_collapse_moves_every_hidden_caret() {
        use ide_core::RUST;
        let mut buffer = Buffer::untitled();
        buffer.set_syntax(Some(&RUST));
        buffer.insert(0, "fn f() {\n    a();\n    b();\n}\n");
        // Two carets: one on the signature line (stays put), one inside the
        // body (line 2, about to be hidden).
        let line2_offset = buffer.text().find("b()").unwrap();
        buffer.text_buffer_mut().set_selections(Selections::new(
            vec![Selection::caret(0), Selection::caret(line2_offset)],
            0,
        ));

        let mut state = EditorState::default();
        state.collapse_all(&buffer.text_buffer().fold_ranges());

        reveal_caret_after_collapse(&mut buffer, &state);

        let selections = buffer.text_buffer().selections().all().to_vec();
        assert_eq!(selections.len(), 2);
        // Both carets now sit on line 0, the fold's start_line.
        for selection in &selections {
            assert_eq!(buffer.text_buffer().lines().line_at(selection.head), 0);
        }
    }
}
