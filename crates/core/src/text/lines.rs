use std::ops::Range;

use super::edit::Change;

/// Byte offsets of every line start, maintained incrementally.
/// `line_count()` is always >= 1: an empty text is one empty line, so no
/// caller has to special-case an empty buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { starts }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    pub fn line_start(&self, line: usize) -> Option<usize> {
        self.starts.get(line).copied()
    }

    /// Byte range of `line`, excluding its trailing `\n` and, for a CRLF
    /// file, excluding the `\r` as well, so `line_text` never hands back a
    /// stray carriage return. `None` past the end.
    pub fn line_range(&self, line: usize, text: &str) -> Option<Range<usize>> {
        let start = self.line_start(line)?;
        let end = match self.line_start(line + 1) {
            Some(next) => {
                let without_newline = next - 1;
                if text.as_bytes().get(without_newline.wrapping_sub(1)) == Some(&b'\r') {
                    without_newline - 1
                } else {
                    without_newline
                }
            }
            None => text.len(),
        };
        Some(start..end.max(start))
    }

    /// The line containing `offset`; the last line for an out-of-range one.
    pub fn line_at(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        }
    }

    /// `(line, column)`, column in bytes from the line start. UTF-16 column
    /// conversion stays `ide-lsp`'s business.
    pub fn position_at(&self, offset: usize) -> (usize, usize) {
        let line = self.line_at(offset);
        (line, offset - self.starts[line])
    }

    pub fn offset_at(&self, line: usize, column: usize) -> Option<usize> {
        self.line_start(line).map(|start| start + column)
    }

    /// Rebuilds only the lines `change` touched and shifts every later line
    /// start by the change's length delta. `text` is the post-change text;
    /// the delta is derived from `change` itself rather than passed in, so
    /// the two cannot disagree.
    pub fn apply(&mut self, text: &str, change: &Change) {
        let removed = change.range.end - change.range.start;
        let inserted = change.insert.len();

        let first = self.line_at(change.range.start);
        let last = self.line_at(change.range.end);
        let tail_start = last + 1;

        let mut replacement: Vec<usize> = Vec::new();
        let scan_from = self.starts[first];
        let scan_to = change.range.start + inserted;
        replacement.extend(
            text.as_bytes()[scan_from..scan_to]
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| scan_from + i + 1),
        );

        let shifted_tail: Vec<usize> = self.starts[tail_start.min(self.starts.len())..]
            .iter()
            .map(|start| start + inserted - removed)
            .collect();

        self.starts.truncate(first + 1);
        self.starts.extend(replacement);
        self.starts.extend(shifted_tail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_one_empty_line() {
        let index = LineIndex::new("");
        assert_eq!(index.line_count(), 1);
        assert_eq!(index.line_range(0, ""), Some(0..0));
    }

    #[test]
    fn trailing_newline_creates_a_final_empty_line() {
        let text = "a\n";
        let index = LineIndex::new(text);
        assert_eq!(index.line_count(), 2);
        assert_eq!(index.line_range(0, text), Some(0..1));
        assert_eq!(index.line_range(1, text), Some(2..2));
    }

    #[test]
    fn line_range_excludes_the_newline() {
        let text = "one\ntwo";
        let index = LineIndex::new(text);
        assert_eq!(index.line_range(0, text), Some(0..3));
        assert_eq!(index.line_range(1, text), Some(4..7));
        assert_eq!(index.line_range(2, text), None);
    }

    #[test]
    fn line_range_excludes_a_carriage_return() {
        let text = "one\r\ntwo";
        let index = LineIndex::new(text);
        assert_eq!(&text[index.line_range(0, text).unwrap()], "one");
        assert_eq!(&text[index.line_range(1, text).unwrap()], "two");
    }

    #[test]
    fn a_line_that_is_only_crlf_is_empty() {
        let text = "\r\nx";
        let index = LineIndex::new(text);
        assert_eq!(index.line_range(0, text), Some(0..0));
    }

    #[test]
    fn line_at_and_position_at_agree() {
        let text = "ab\ncd\n\nef";
        let index = LineIndex::new(text);
        assert_eq!(index.line_at(0), 0);
        assert_eq!(index.line_at(2), 0);
        assert_eq!(index.line_at(3), 1);
        assert_eq!(index.position_at(4), (1, 1));
        assert_eq!(index.position_at(6), (2, 0));
        assert_eq!(index.position_at(text.len()), (3, 2));
    }

    #[test]
    fn line_at_past_the_end_returns_the_last_line() {
        let index = LineIndex::new("a\nb");
        assert_eq!(index.line_at(999), 1);
    }

    #[test]
    fn offset_at_round_trips_position_at() {
        let text = "alpha\nbeta\ngamma";
        let index = LineIndex::new(text);
        let (line, column) = index.position_at(12);
        assert_eq!(index.offset_at(line, column), Some(12));
        assert_eq!(index.offset_at(99, 0), None);
    }

    #[test]
    fn multi_byte_line_offsets_stay_on_char_boundaries() {
        let text = "\u{1F600}\n\u{4F60}\u{597D}";
        let index = LineIndex::new(text);
        assert_eq!(index.line_count(), 2);
        assert_eq!(
            &text[index.line_range(1, text).unwrap()],
            "\u{4F60}\u{597D}"
        );
    }

    fn assert_incremental_matches_rebuild(initial: &str, change: Change) {
        let mut text = initial.to_string();
        let mut index = LineIndex::new(&text);
        text.replace_range(change.range.clone(), &change.insert);
        index.apply(&text, &change);
        assert_eq!(index, LineIndex::new(&text), "text was {text:?}");
    }

    #[test]
    fn apply_matches_a_full_rebuild() {
        assert_incremental_matches_rebuild("one\ntwo\nthree", Change::new(4..4, "X"));
        assert_incremental_matches_rebuild("one\ntwo\nthree", Change::new(4..4, "a\nb\n"));
        assert_incremental_matches_rebuild("one\ntwo\nthree", Change::new(2..9, ""));
        assert_incremental_matches_rebuild("one\ntwo\nthree", Change::new(0..13, "flat"));
        assert_incremental_matches_rebuild("", Change::new(0..0, "a\nb"));
        assert_incremental_matches_rebuild("a\n", Change::new(2..2, "b"));
        assert_incremental_matches_rebuild("a\nb\nc", Change::new(1..4, "\n\n\n"));
    }
}
