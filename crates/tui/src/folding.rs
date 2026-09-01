//! Buffer-line <-> visual-row mapping and collapse/expand state
//! transitions for code folding (`docs/features/tui-code-folding.md`
//! §2.1). State-free except for the plain `BTreeSet<usize>` callers pass
//! in and out -- the same boundary `editor.rs` holds relative to `app.rs`.

use std::collections::BTreeSet;

use ide_core::FoldRange;

/// Maps buffer lines to visual rows, hiding every line inside a collapsed
/// fold range except the range's own `start_line`.
pub struct VisualLines {
    rows: Vec<usize>,
}

impl VisualLines {
    /// `folded` is the set of currently-collapsed `start_line`s. When two
    /// or more of `ranges` share a `start_line` and more than one is
    /// collapsed, the one with the largest `end_line` (outermost)
    /// determines how much is hidden.
    pub fn build(line_count: usize, ranges: &[FoldRange], folded: &BTreeSet<usize>) -> Self {
        let mut hidden = vec![false; line_count];
        for start in folded {
            let Some(outermost) = ranges
                .iter()
                .filter(|r| r.start_line == *start)
                .max_by_key(|r| r.end_line)
            else {
                continue;
            };
            let start = outermost.start_line + 1;
            let end = (outermost.end_line + 1).min(line_count);
            if start < end {
                for hidden_line in &mut hidden[start..end] {
                    *hidden_line = true;
                }
            }
        }
        let rows = (0..line_count).filter(|line| !hidden[*line]).collect();
        Self { rows }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The buffer line for visual `row`, clamped to the last row.
    pub fn buffer_line(&self, row: usize) -> usize {
        let row = row.min(self.rows.len().saturating_sub(1));
        self.rows.get(row).copied().unwrap_or(0)
    }

    /// The visual row for `buffer_line` -- its own row if visible,
    /// otherwise the row of the collapsed fold that hides it.
    pub fn row_of(&self, buffer_line: usize) -> usize {
        let row = self.rows.partition_point(|&line| line <= buffer_line);
        row.saturating_sub(1).min(self.rows.len().saturating_sub(1))
    }
}

/// The innermost uncollapsed range in `ranges` containing `caret_line`,
/// collapsed into `folded`. No-op if none does.
pub fn collapse_at_caret(folded: &mut BTreeSet<usize>, ranges: &[FoldRange], caret_line: usize) {
    let innermost = ranges
        .iter()
        .filter(|r| {
            r.start_line <= caret_line
                && caret_line <= r.end_line
                && !folded.contains(&r.start_line)
        })
        .min_by_key(|r| r.end_line - r.start_line);
    if let Some(range) = innermost {
        folded.insert(range.start_line);
    }
}

/// Uncollapses the range whose `start_line` is `caret_line`, if one is
/// currently collapsed there. No-op otherwise.
pub fn expand_at_caret(folded: &mut BTreeSet<usize>, caret_line: usize) {
    folded.remove(&caret_line);
}

pub fn collapse_all(folded: &mut BTreeSet<usize>, ranges: &[FoldRange]) {
    folded.clear();
    folded.extend(ranges.iter().map(|r| r.start_line));
}

pub fn expand_all(folded: &mut BTreeSet<usize>) {
    folded.clear();
}

/// Uncollapses every currently-collapsed range whose
/// `start_line..=end_line` contains `line`.
pub fn reveal_line(folded: &mut BTreeSet<usize>, ranges: &[FoldRange], line: usize) {
    folded.retain(|&start| {
        !ranges
            .iter()
            .any(|r| r.start_line == start && r.start_line <= line && line <= r.end_line)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::FoldKind;

    fn range(start: usize, end: usize) -> FoldRange {
        FoldRange {
            start_line: start,
            end_line: end,
            kind: FoldKind::Brace,
        }
    }

    #[test]
    fn build_with_nothing_folded_shows_every_line() {
        let visual = VisualLines::build(5, &[range(1, 3)], &BTreeSet::new());
        assert_eq!(visual.row_count(), 5);
        for line in 0..5 {
            assert_eq!(visual.buffer_line(visual.row_of(line)), line);
        }
    }

    #[test]
    fn build_with_one_folded_range_hides_its_interior() {
        let folded = BTreeSet::from([1]);
        let visual = VisualLines::build(5, &[range(1, 3)], &folded);
        // Lines 0,1,4 visible; 2,3 hidden.
        assert_eq!(visual.row_count(), 3);
        assert_eq!(visual.buffer_line(0), 0);
        assert_eq!(visual.buffer_line(1), 1);
        assert_eq!(visual.buffer_line(2), 4);
    }

    #[test]
    fn row_of_a_hidden_line_resolves_to_the_folds_start_line_row() {
        let folded = BTreeSet::from([1]);
        let visual = VisualLines::build(5, &[range(1, 3)], &folded);
        let start_row = visual.row_of(1);
        assert_eq!(visual.row_of(2), start_row);
        assert_eq!(visual.row_of(3), start_row);
        assert_eq!(visual.buffer_line(start_row), 1);
    }

    #[test]
    fn buffer_line_clamps_past_the_last_row() {
        let visual = VisualLines::build(3, &[], &BTreeSet::new());
        assert_eq!(visual.buffer_line(100), 2);
    }

    #[test]
    fn row_of_clamps_past_the_last_line() {
        let visual = VisualLines::build(3, &[], &BTreeSet::new());
        assert_eq!(visual.row_of(100), 2);
    }

    #[test]
    fn build_with_an_empty_buffer_still_has_at_least_no_rows_and_never_panics() {
        let visual = VisualLines::build(0, &[], &BTreeSet::new());
        assert_eq!(visual.row_count(), 0);
        assert_eq!(visual.buffer_line(0), 0);
        assert_eq!(visual.row_of(0), 0);
    }

    #[test]
    fn build_uses_the_outermost_range_when_two_share_a_start_line() {
        let folded = BTreeSet::from([0]);
        let ranges = vec![range(0, 2), range(0, 5)];
        let visual = VisualLines::build(7, &ranges, &folded);
        // Outermost (end_line 5) wins: lines 1..=5 hidden, 0 and 6 visible.
        assert_eq!(visual.row_count(), 2);
        assert_eq!(visual.buffer_line(1), 6);
    }

    #[test]
    fn build_ignores_a_stale_folded_start_line_with_no_matching_range() {
        let folded = BTreeSet::from([1, 42]);
        let visual = VisualLines::build(5, &[range(1, 3)], &folded);
        assert_eq!(visual.row_count(), 3);
    }

    #[test]
    fn collapse_at_caret_picks_the_innermost_containing_range() {
        let mut folded = BTreeSet::new();
        let ranges = vec![range(0, 10), range(2, 4)];
        collapse_at_caret(&mut folded, &ranges, 3);
        assert_eq!(folded, BTreeSet::from([2]));
    }

    #[test]
    fn collapse_at_caret_skips_a_range_that_is_already_collapsed() {
        let mut folded = BTreeSet::from([2]);
        let ranges = vec![range(0, 10), range(2, 4)];
        collapse_at_caret(&mut folded, &ranges, 3);
        // range(2, 4) (start_line 2) is already collapsed, so the only
        // eligible containing range is range(0, 10) -- its start_line
        // joins the existing entry rather than replacing it.
        assert_eq!(folded, BTreeSet::from([0, 2]));
    }

    #[test]
    fn collapse_at_caret_with_no_containing_range_is_a_noop() {
        let mut folded = BTreeSet::new();
        collapse_at_caret(&mut folded, &[range(5, 10)], 2);
        assert!(folded.is_empty());
    }

    #[test]
    fn expand_at_caret_removes_only_the_matching_start_line() {
        let mut folded = BTreeSet::from([1, 2]);
        expand_at_caret(&mut folded, 1);
        assert_eq!(folded, BTreeSet::from([2]));
    }

    #[test]
    fn expand_at_caret_with_no_match_is_a_noop() {
        let mut folded = BTreeSet::from([1]);
        expand_at_caret(&mut folded, 5);
        assert_eq!(folded, BTreeSet::from([1]));
    }

    #[test]
    fn collapse_all_folds_every_range() {
        let mut folded = BTreeSet::new();
        collapse_all(&mut folded, &[range(1, 3), range(5, 8)]);
        assert_eq!(folded, BTreeSet::from([1, 5]));
    }

    #[test]
    fn expand_all_clears_everything() {
        let mut folded = BTreeSet::from([1, 5]);
        expand_all(&mut folded);
        assert!(folded.is_empty());
    }

    #[test]
    fn reveal_line_uncollapses_only_the_range_containing_it() {
        let mut folded = BTreeSet::from([1, 10]);
        let ranges = vec![range(1, 3), range(10, 20)];
        reveal_line(&mut folded, &ranges, 2);
        assert_eq!(folded, BTreeSet::from([10]));
    }

    #[test]
    fn reveal_line_on_the_start_line_itself_still_reveals_it() {
        let mut folded = BTreeSet::from([1]);
        reveal_line(&mut folded, &[range(1, 3)], 1);
        assert!(folded.is_empty());
    }

    #[test]
    fn reveal_line_with_no_containing_range_is_a_noop() {
        let mut folded = BTreeSet::from([1]);
        reveal_line(&mut folded, &[range(1, 3)], 99);
        assert_eq!(folded, BTreeSet::from([1]));
    }
}
