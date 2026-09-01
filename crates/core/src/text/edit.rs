use std::ops::Range;

/// One replacement: delete `range`, insert `insert` in its place. An
/// insertion is an empty `range`; a deletion is an empty `insert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub range: Range<usize>,
    pub insert: String,
}

impl Change {
    pub fn new(range: Range<usize>, insert: impl Into<String>) -> Self {
        let lo = range.start.min(range.end);
        let hi = range.start.max(range.end);
        Self {
            range: lo..hi,
            insert: insert.into(),
        }
    }

    fn delta(&self) -> isize {
        self.insert.len() as isize - (self.range.end - self.range.start) as isize
    }
}

/// Which side of an edit boundary a mapped offset sticks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bias {
    Before,
    After,
}

/// The only way to build an invalid transaction is to overlap two changes.
/// Bounds are deliberately not checked here: a `Transaction` is built
/// without reference to any text, so it cannot know a buffer's length --
/// `TextBuffer::apply` clamps instead.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransactionError {
    #[error("changes overlap: {0:?} and {1:?}")]
    Overlapping(Range<usize>, Range<usize>),
}

/// A set of changes applied as one atomic step and undone as one step.
/// Changes are kept sorted by `range.start` and are guaranteed
/// non-overlapping; construction is the only place that can fail.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Transaction {
    changes: Vec<Change>,
}

impl Transaction {
    /// Sorts `changes` by start offset and rejects any pair that overlaps.
    /// Two changes that merely touch (`a.end == b.start`) are legal, and a
    /// reversed range is normalised the way `Buffer::delete` already
    /// normalises one.
    pub fn new(changes: Vec<Change>) -> Result<Self, TransactionError> {
        let mut changes: Vec<Change> = changes
            .into_iter()
            .map(|c| Change::new(c.range, c.insert))
            .collect();
        changes.sort_by_key(|c| c.range.start);
        for pair in changes.windows(2) {
            if pair[1].range.start < pair[0].range.end {
                return Err(TransactionError::Overlapping(
                    pair[0].range.clone(),
                    pair[1].range.clone(),
                ));
            }
        }
        Ok(Self { changes })
    }

    pub fn replace(range: Range<usize>, insert: impl Into<String>) -> Self {
        Self {
            changes: vec![Change::new(range, insert)],
        }
    }

    pub fn insert(offset: usize, text: impl Into<String>) -> Self {
        Self::replace(offset..offset, text)
    }

    pub fn delete(range: Range<usize>) -> Self {
        Self::replace(range, "")
    }

    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// No changes at all. Not the same as "has no effect against a given
    /// text" -- that can only be known at apply time, after clamping.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Maps an offset in the pre-transaction text to its position in the
    /// post-transaction text. `bias` decides which side of an insertion
    /// exactly at `offset` the result lands on, which is what lets a caret
    /// survive an edit at its own position.
    pub fn map_offset(&self, offset: usize, bias: Bias) -> usize {
        let mut delta: isize = 0;
        for change in &self.changes {
            if offset > change.range.end {
                delta += change.delta();
                continue;
            }
            if offset == change.range.end {
                // A caret sitting exactly where text was inserted only moves
                // along with it under `After`; under `Before` it is the far
                // end of something and must stay put.
                if change.range.is_empty() && bias == Bias::Before {
                    break;
                }
                delta += change.delta();
                continue;
            }
            if offset < change.range.start {
                break;
            }
            if offset == change.range.start && bias == Bias::Before {
                break;
            }
            // `offset` is inside the replaced span: the text it pointed at is
            // gone, so it collapses to the change's new end.
            let new_start = (change.range.start as isize + delta) as usize;
            return new_start + change.insert.len();
        }
        (offset as isize + delta) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(changes: &[(Range<usize>, &str)]) -> Transaction {
        Transaction::new(
            changes
                .iter()
                .map(|(r, s)| Change::new(r.clone(), *s))
                .collect(),
        )
        .expect("test transactions are non-overlapping")
    }

    #[test]
    fn new_sorts_by_start_offset() {
        let t = tx(&[(5..5, "b"), (0..0, "a")]);
        assert_eq!(t.changes()[0].insert, "a");
        assert_eq!(t.changes()[1].insert, "b");
    }

    #[test]
    fn new_accepts_touching_ranges() {
        assert!(Transaction::new(vec![Change::new(0..2, "x"), Change::new(2..4, "y")]).is_ok());
    }

    #[test]
    fn new_rejects_overlapping_ranges() {
        let err = Transaction::new(vec![Change::new(0..3, "x"), Change::new(2..4, "y")])
            .expect_err("overlapping changes must be rejected");
        assert_eq!(err, TransactionError::Overlapping(0..3, 2..4));
        assert!(err.to_string().contains("overlap"));
    }

    #[test]
    fn new_normalizes_reversed_ranges() {
        let reversed = Range { start: 4, end: 1 };
        let t =
            Transaction::new(vec![Change::new(reversed, "")]).expect("reversed range normalizes");
        assert_eq!(t.changes()[0].range, 1..4);
    }

    #[test]
    fn empty_transaction_reports_empty() {
        assert!(Transaction::default().is_empty());
        assert!(!Transaction::insert(0, "a").is_empty());
    }

    #[test]
    fn delete_and_replace_constructors_agree() {
        assert_eq!(Transaction::delete(1..3), Transaction::replace(1..3, ""));
        assert_eq!(Transaction::insert(2, "x"), Transaction::replace(2..2, "x"));
    }

    #[test]
    fn map_offset_before_change_is_unchanged() {
        let t = tx(&[(5..5, "abc")]);
        assert_eq!(t.map_offset(2, Bias::After), 2);
    }

    #[test]
    fn map_offset_after_change_shifts_by_delta() {
        let t = tx(&[(1..3, "xyz")]);
        assert_eq!(t.map_offset(10, Bias::After), 11);
    }

    #[test]
    fn map_offset_at_insertion_point_respects_bias() {
        let t = tx(&[(5..5, "abc")]);
        assert_eq!(t.map_offset(5, Bias::After), 8);
        assert_eq!(t.map_offset(5, Bias::Before), 5);
    }

    #[test]
    fn map_offset_inside_replaced_range_clamps_to_new_end() {
        let t = tx(&[(2..8, "xy")]);
        assert_eq!(t.map_offset(5, Bias::After), 4);
        assert_eq!(t.map_offset(5, Bias::Before), 4);
    }

    #[test]
    fn map_offset_at_deletion_start_stays_put() {
        let t = tx(&[(2..8, "")]);
        assert_eq!(t.map_offset(2, Bias::After), 2);
        assert_eq!(t.map_offset(2, Bias::Before), 2);
    }

    #[test]
    fn map_offset_at_a_replacement_start_follows_the_bias() {
        let t = tx(&[(0..5, "goodbye")]);
        assert_eq!(t.map_offset(0, Bias::After), 7);
        assert_eq!(t.map_offset(0, Bias::Before), 0);
    }

    #[test]
    fn map_offset_accumulates_across_multiple_changes() {
        let t = tx(&[(0..0, "ab"), (4..4, "cd"), (9..9, "ef")]);
        assert_eq!(t.map_offset(4, Bias::After), 8);
        assert_eq!(t.map_offset(9, Bias::After), 15);
    }
}
