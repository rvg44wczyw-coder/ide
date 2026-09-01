use std::ops::Range;

use super::edit::{Bias, Transaction};

/// A cursor, possibly with a selection. `anchor == head` is a bare caret;
/// `head` is where the caret visually is, `anchor` the fixed end. Both are
/// byte offsets on char boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn caret(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
        }
    }

    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    fn reversed(&self) -> bool {
        self.head < self.anchor
    }

    fn spanning(&self, start: usize, end: usize) -> Self {
        if self.reversed() {
            Self {
                anchor: end,
                head: start,
            }
        } else {
            Self {
                anchor: start,
                head: end,
            }
        }
    }
}

/// Every cursor in the buffer. Non-empty by construction: there is always at
/// least one. Kept sorted by `start()`, with overlapping selections merged
/// -- the invariant multi-cursor editing depends on, since two cursors
/// inside one another would produce overlapping changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selections {
    ranges: Vec<Selection>,
    primary: usize,
}

impl Default for Selections {
    fn default() -> Self {
        Self::single(Selection::caret(0))
    }
}

// No `is_empty`: the type is non-empty by construction, so the method could
// only ever return `false` and would read at the call site as a check worth
// making.
#[allow(clippy::len_without_is_empty)]
impl Selections {
    pub fn single(selection: Selection) -> Self {
        Self {
            ranges: vec![selection],
            primary: 0,
        }
    }

    /// Sorts, merges overlaps, and keeps `primary` pointing at whichever
    /// selection absorbed the previous primary.
    ///
    /// Total, never failing, because the non-empty invariant has to hold for
    /// every value that exists: an empty `ranges` yields a single
    /// `Selection::caret(0)`, and a `primary` past the end is clamped.
    pub fn new(ranges: Vec<Selection>, primary: usize) -> Self {
        if ranges.is_empty() {
            return Self::single(Selection::caret(0));
        }
        let primary_marker = ranges[primary.min(ranges.len() - 1)];

        let mut ordered: Vec<Selection> = ranges;
        ordered.sort_by_key(|s| (s.start(), s.end()));

        let mut merged: Vec<Selection> = Vec::with_capacity(ordered.len());
        for selection in ordered {
            match merged.last_mut() {
                Some(last) if Self::should_merge(last, &selection) => {
                    let start = last.start().min(selection.start());
                    let end = last.end().max(selection.end());
                    *last = last.spanning(start, end);
                }
                _ => merged.push(selection),
            }
        }

        let primary = merged
            .iter()
            .position(|s| s.start() <= primary_marker.start() && primary_marker.end() <= s.end())
            .unwrap_or(0);
        Self {
            ranges: merged,
            primary,
        }
    }

    /// Two selections merge only when they genuinely overlap, or when both
    /// are carets at the same offset. Touching selections
    /// (`a.end == b.start`) are deliberately kept apart: they produce
    /// non-overlapping changes, so merging them would destroy a cursor the
    /// user placed without buying any correctness.
    fn should_merge(a: &Selection, b: &Selection) -> bool {
        if a.is_empty() && b.is_empty() {
            return a.start() == b.start();
        }
        b.start() < a.end()
    }

    pub fn all(&self) -> &[Selection] {
        &self.ranges
    }

    pub fn primary(&self) -> Selection {
        self.ranges[self.primary]
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_multiple(&self) -> bool {
        self.ranges.len() > 1
    }

    /// Adds a cursor, re-normalising. Returns `false` when normalisation
    /// absorbed it into an existing selection -- i.e. when `len()` did not
    /// grow -- which is how "add cursor at next occurrence" knows it has run
    /// out of new places to go.
    pub fn push(&mut self, selection: Selection) -> bool {
        let before = self.ranges.len();
        let mut ranges = self.ranges.clone();
        ranges.push(selection);
        *self = Self::new(ranges, self.primary);
        self.ranges.len() > before
    }

    /// Adds a cursor and makes it primary -- the difference from `push`,
    /// which keeps the existing one. "Add selection for next occurrence"
    /// needs it: the occurrence just added is what unselect removes next
    /// and what the view scrolls to.
    ///
    /// Returns `false` when normalisation absorbed the new cursor, exactly
    /// as `push` does, and then leaves the primary where it was.
    pub fn push_primary(&mut self, selection: Selection) -> bool {
        let before = self.ranges.len();
        let old_primary = self.primary;
        let mut ranges = self.ranges.clone();
        ranges.push(selection);

        let added = Self::new(ranges.clone(), ranges.len() - 1);
        if added.ranges.len() > before {
            *self = added;
            true
        } else {
            *self = Self::new(ranges, old_primary);
            false
        }
    }

    /// Removes the primary selection. `false` and no change when it is the
    /// only one -- the type is non-empty by construction.
    pub fn remove_primary(&mut self) -> bool {
        self.remove_at(self.primary)
    }

    /// Removes the selection at `index`. `false` and no change when `index`
    /// is out of range or when it would empty the set.
    ///
    /// `primary` keeps pointing at the same *selection* it pointed at,
    /// whatever index that selection shifts to; only when `index` is the
    /// primary does it fall back -- to the predecessor, or to the successor
    /// at index 0.
    pub fn remove_at(&mut self, index: usize) -> bool {
        if index >= self.ranges.len() || self.ranges.len() == 1 {
            return false;
        }
        self.ranges.remove(index);
        self.primary = match index.cmp(&self.primary) {
            std::cmp::Ordering::Less => self.primary - 1,
            std::cmp::Ordering::Equal => index.saturating_sub(1),
            std::cmp::Ordering::Greater => self.primary,
        };
        true
    }

    /// Index of the selection `offset` falls in. A bare caret matches at
    /// its own offset; a non-empty selection matches when
    /// `start() <= offset` and `offset < end()`.
    pub fn index_at(&self, offset: usize) -> Option<usize> {
        self.ranges.iter().position(|selection| {
            if selection.is_empty() {
                selection.head == offset
            } else {
                selection.start() <= offset && offset < selection.end()
            }
        })
    }

    /// Collapses to the primary selection alone.
    pub fn collapse_to_primary(&mut self) {
        let primary = self.primary();
        *self = Self::single(primary);
    }

    /// Every selection through `Transaction::map_offset`, then re-normalised.
    pub fn map(&self, transaction: &Transaction) -> Selections {
        let mapped: Vec<Selection> = self
            .ranges
            .iter()
            .map(|s| Selection {
                anchor: transaction.map_offset(s.anchor, Bias::After),
                head: transaction.map_offset(s.head, Bias::After),
            })
            .collect();
        Self::new(mapped, self.primary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_is_empty_and_spans_nothing() {
        let c = Selection::caret(4);
        assert!(c.is_empty());
        assert_eq!(c.range(), 4..4);
        assert_eq!((c.start(), c.end()), (4, 4));
    }

    #[test]
    fn reversed_selection_reports_ordered_bounds() {
        let s = Selection::new(9, 3);
        assert_eq!((s.start(), s.end()), (3, 9));
        assert_eq!(s.range(), 3..9);
    }

    #[test]
    fn new_sorts_by_start_offset() {
        let s = Selections::new(vec![Selection::caret(7), Selection::caret(2)], 0);
        assert_eq!(s.all()[0], Selection::caret(2));
        assert_eq!(s.all()[1], Selection::caret(7));
    }

    #[test]
    fn new_merges_genuine_overlap() {
        let s = Selections::new(vec![Selection::new(0, 5), Selection::new(3, 9)], 0);
        assert_eq!(s.len(), 1);
        assert_eq!(s.all()[0].range(), 0..9);
    }

    #[test]
    fn merged_selection_keeps_the_earlier_direction() {
        let s = Selections::new(vec![Selection::new(5, 0), Selection::new(3, 9)], 0);
        assert_eq!(s.len(), 1);
        assert_eq!(s.all()[0], Selection::new(9, 0));
    }

    #[test]
    fn new_merges_duplicate_carets() {
        let s = Selections::new(vec![Selection::caret(3), Selection::caret(3)], 0);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn new_keeps_touching_selections_apart() {
        let s = Selections::new(vec![Selection::new(0, 3), Selection::new(3, 6)], 0);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn new_keeps_a_caret_at_a_selection_edge() {
        let s = Selections::new(vec![Selection::new(0, 3), Selection::caret(3)], 0);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn new_on_empty_ranges_yields_one_caret_at_zero() {
        let s = Selections::new(Vec::new(), 4);
        assert_eq!(s.len(), 1);
        assert_eq!(s.primary(), Selection::caret(0));
    }

    #[test]
    fn new_clamps_out_of_range_primary() {
        let s = Selections::new(vec![Selection::caret(1), Selection::caret(5)], 99);
        assert_eq!(s.primary(), Selection::caret(5));
    }

    #[test]
    fn primary_follows_the_selection_that_absorbed_it() {
        let s = Selections::new(vec![Selection::new(0, 5), Selection::new(3, 9)], 1);
        assert_eq!(s.primary_index(), 0);
        assert_eq!(s.primary().range(), 0..9);
    }

    #[test]
    fn push_reports_whether_the_cursor_survived() {
        let mut s = Selections::single(Selection::new(0, 5));
        assert!(s.push(Selection::caret(9)));
        assert_eq!(s.len(), 2);
        assert!(!s.push(Selection::caret(2)));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn push_primary_makes_the_new_cursor_primary() {
        let mut s = Selections::single(Selection::caret(0));
        assert!(s.push_primary(Selection::caret(9)));
        assert_eq!(s.len(), 2);
        assert_eq!(s.primary(), Selection::caret(9));
        assert_eq!(s.primary_index(), 1);
    }

    #[test]
    fn push_primary_keeps_the_old_primary_when_absorbed() {
        let mut s = Selections::new(vec![Selection::new(0, 5), Selection::caret(9)], 1);
        assert!(!s.push_primary(Selection::caret(2)));
        assert_eq!(s.len(), 2);
        assert_eq!(s.primary(), Selection::caret(9));
    }

    #[test]
    fn remove_primary_falls_back_to_the_predecessor() {
        let mut s = Selections::new(
            vec![
                Selection::caret(1),
                Selection::caret(5),
                Selection::caret(9),
            ],
            2,
        );
        assert!(s.remove_primary());
        assert_eq!(s.len(), 2);
        assert_eq!(s.primary(), Selection::caret(5));
    }

    #[test]
    fn remove_primary_at_index_zero_falls_back_to_the_successor() {
        let mut s = Selections::new(vec![Selection::caret(1), Selection::caret(5)], 0);
        assert!(s.remove_primary());
        assert_eq!(s.primary(), Selection::caret(5));
    }

    #[test]
    fn remove_primary_refuses_to_empty_the_set() {
        let mut s = Selections::single(Selection::caret(3));
        assert!(!s.remove_primary());
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn remove_at_keeps_primary_on_the_same_selection() {
        let mut s = Selections::new(
            vec![
                Selection::caret(1),
                Selection::caret(5),
                Selection::caret(9),
            ],
            2,
        );
        // Removing a selection *before* the primary shifts its index down
        // but must not move it to another selection.
        assert!(s.remove_at(0));
        assert_eq!(s.primary(), Selection::caret(9));
        assert_eq!(s.primary_index(), 1);

        // ...and one after it leaves the index alone.
        let mut s = Selections::new(
            vec![
                Selection::caret(1),
                Selection::caret(5),
                Selection::caret(9),
            ],
            0,
        );
        assert!(s.remove_at(2));
        assert_eq!(s.primary(), Selection::caret(1));
        assert_eq!(s.primary_index(), 0);
    }

    #[test]
    fn remove_at_rejects_out_of_range_and_the_last_selection() {
        let mut s = Selections::new(vec![Selection::caret(1), Selection::caret(5)], 0);
        assert!(!s.remove_at(9));
        assert_eq!(s.len(), 2);
        assert!(s.remove_at(1));
        assert!(!s.remove_at(0));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn index_at_finds_a_caret_at_its_own_offset() {
        let s = Selections::new(vec![Selection::caret(3), Selection::caret(7)], 0);
        assert_eq!(s.index_at(3), Some(0));
        assert_eq!(s.index_at(7), Some(1));
        assert_eq!(s.index_at(4), None);
    }

    #[test]
    fn index_at_covers_a_selections_interior_but_not_its_end() {
        let s = Selections::single(Selection::new(2, 6));
        assert_eq!(s.index_at(2), Some(0));
        assert_eq!(s.index_at(5), Some(0));
        assert_eq!(s.index_at(6), None);
        assert_eq!(s.index_at(1), None);
    }

    #[test]
    fn collapse_to_primary_drops_the_others() {
        let mut s = Selections::new(vec![Selection::caret(1), Selection::caret(5)], 1);
        s.collapse_to_primary();
        assert_eq!(s.len(), 1);
        assert_eq!(s.primary(), Selection::caret(5));
        assert!(!s.is_multiple());
    }

    #[test]
    fn map_moves_every_cursor_through_the_edit() {
        let s = Selections::new(vec![Selection::caret(0), Selection::caret(4)], 0);
        let t = Transaction::insert(0, "ab");
        let mapped = s.map(&t);
        assert_eq!(mapped.all()[0], Selection::caret(2));
        assert_eq!(mapped.all()[1], Selection::caret(6));
    }

    #[test]
    fn map_can_merge_cursors_that_collide() {
        let s = Selections::new(vec![Selection::caret(2), Selection::caret(6)], 0);
        let t = Transaction::delete(0..6);
        let mapped = s.map(&t);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped.primary(), Selection::caret(0));
    }

    #[test]
    fn default_is_a_single_caret_at_zero() {
        assert_eq!(
            Selections::default(),
            Selections::single(Selection::caret(0))
        );
    }
}
