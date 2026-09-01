//! Whole-line editing commands
//! (`docs/features/line-commands-and-editorconfig.md` §2.1, §3.1-§3.2, §3.5):
//! Duplicate/Delete/Join/Move Line, Move Statement, and Toggle Case.
//!
//! Every command here computes its edits from one consistent snapshot of the
//! buffer's text and `LineIndex` before touching anything, then builds
//! exactly one `Transaction` and sets the resulting selections itself --
//! same shape A4a's `ops.rs` already established, needed here because none
//! of these operations can rely on `Selections::map`'s default behaviour
//! (a duplicated/moved span's selections must land on the *new* text, not
//! wherever the old text mapped to).

use std::ops::Range;

use crate::syntax::{TokenKind, MAX_HIGHLIGHTED_FILE_BYTES};

use super::find::MAX_OCCURRENCES;
use super::indent::leading_whitespace;
use super::{Change, Selection, Selections, TextBuffer, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineDirection {
    Up,
    Down,
}

/// A selection's line span, as an inclusive line-number pair.
#[derive(Debug, Clone, Copy)]
struct Span {
    first: usize,
    last: usize,
}

impl TextBuffer {
    /// Every selection's line span, merged where two spans share a line *or
    /// are directly adjacent* -- touching, not just overlapping, which is
    /// what keeps a destructive multi-cursor edit (Delete, Move) from ever
    /// building two changes that both claim the single newline between two
    /// neighbouring spans.
    fn merged_spans(&self) -> Vec<Span> {
        let mut spans: Vec<Span> = self
            .selections()
            .all()
            .iter()
            .map(|s| Span {
                first: self.lines().line_at(s.start()),
                last: self.lines().line_at(s.end()),
            })
            .collect();
        spans.sort_by_key(|s| s.first);
        let mut merged: Vec<Span> = Vec::new();
        for span in spans {
            if let Some(top) = merged.last_mut() {
                if span.first <= top.last + 1 {
                    top.last = top.last.max(span.last);
                    continue;
                }
            }
            merged.push(span);
        }
        merged
    }

    fn line_start_or_end(&self, line: usize) -> usize {
        self.lines().line_start(line).unwrap_or(self.text().len())
    }

    /// The span's own content, start of `first` to end of `last`, excluding
    /// any trailing newline.
    fn span_content_range(&self, span: Span) -> Range<usize> {
        let start = self.line_start_or_end(span.first);
        let end = self
            .lines()
            .line_range(span.last, self.text())
            .map(|r| r.end)
            .unwrap_or(self.text().len());
        start..end
    }

    /// Index into `spans` of whichever one contains the primary selection --
    /// used by ops that collapse each span to one resulting entry, so the
    /// new primary index still points at the group the user's primary caret
    /// was in.
    fn primary_span_index(&self, spans: &[Span]) -> usize {
        let primary_line = self.lines().line_at(self.selections().primary().start());
        spans
            .iter()
            .position(|s| s.first <= primary_line && primary_line <= s.last)
            .unwrap_or(0)
    }

    /// §3.1. Copies each selection's full line span below itself; a
    /// non-empty selection inside one line duplicates the selection instead.
    pub fn duplicate_selection_lines(&mut self) -> bool {
        let selections = self.selections().all().to_vec();
        let text = self.text().to_string();

        struct Event {
            at: usize,
            inserted: String,
            copy_offset: usize,
            span_start: usize,
        }

        let mut events: Vec<Event> = Vec::new();
        let mut event_for: Vec<usize> = Vec::with_capacity(selections.len());
        let mut line_span_selections: Vec<(usize, Selection)> = Vec::new();

        for (i, s) in selections.iter().enumerate() {
            let single_line =
                !s.is_empty() && self.lines().line_at(s.start()) == self.lines().line_at(s.end());
            if single_line {
                event_for.push(events.len());
                events.push(Event {
                    at: s.end(),
                    inserted: text[s.range()].to_string(),
                    copy_offset: 0,
                    span_start: s.start(),
                });
            } else {
                event_for.push(usize::MAX);
                line_span_selections.push((i, *s));
            }
        }

        let mut spans: Vec<Span> = line_span_selections
            .iter()
            .map(|(_, s)| Span {
                first: self.lines().line_at(s.start()),
                last: self.lines().line_at(s.end()),
            })
            .collect();
        spans.sort_by_key(|s| s.first);
        let mut merged: Vec<Span> = Vec::new();
        for span in spans {
            if let Some(top) = merged.last_mut() {
                if span.first <= top.last + 1 {
                    top.last = top.last.max(span.last);
                    continue;
                }
            }
            merged.push(span);
        }

        let span_events_start = events.len();
        for span in &merged {
            let range = self.span_content_range(*span);
            events.push(Event {
                at: range.end,
                inserted: format!("\n{}", &text[range.clone()]),
                copy_offset: 1,
                span_start: range.start,
            });
        }
        for (i, s) in &line_span_selections {
            let line = self.lines().line_at(s.start());
            let idx = merged
                .iter()
                .position(|sp| sp.first <= line && line <= sp.last)
                .unwrap_or(0);
            event_for[*i] = span_events_start + idx;
        }

        if events.is_empty() {
            return false;
        }

        let changes: Vec<Change> = events
            .iter()
            .map(|e| Change::new(e.at..e.at, e.inserted.clone()))
            .collect();
        let Ok(transaction) = Transaction::new(changes) else {
            return false;
        };

        let mut order: Vec<usize> = (0..events.len()).collect();
        order.sort_by_key(|&i| events[i].at);
        let mut shift: isize = 0;
        let mut copy_start = vec![0usize; events.len()];
        for i in order {
            copy_start[i] = (events[i].at as isize + shift) as usize + events[i].copy_offset;
            shift += events[i].inserted.len() as isize;
        }

        let new_selections: Vec<Selection> = selections
            .iter()
            .zip(&event_for)
            .map(|(s, &ei)| {
                let event = &events[ei];
                let base = copy_start[ei] as isize - event.span_start as isize;
                Selection::new(
                    (s.anchor as isize + base) as usize,
                    (s.head as isize + base) as usize,
                )
            })
            .collect();

        let primary = self.selections().primary_index();
        self.apply(transaction);
        self.set_selections(Selections::new(new_selections, primary));
        true
    }

    /// §3.1. Deletes each selection's full line span, including its
    /// trailing newline. Deleting every line leaves an empty buffer, not a
    /// zero-selection one.
    pub fn delete_selection_lines(&mut self) -> bool {
        let spans = self.merged_spans();
        let line_count = self.lines().line_count();

        struct Del {
            range: Range<usize>,
            caret: usize,
        }
        let dels: Vec<Del> = spans
            .iter()
            .map(|span| {
                if span.last + 1 < line_count {
                    let range =
                        self.line_start_or_end(span.first)..self.line_start_or_end(span.last + 1);
                    let caret = range.start;
                    Del { range, caret }
                } else if span.first > 0 {
                    let range = (self.line_start_or_end(span.first) - 1)..self.text().len();
                    let caret = self.line_start_or_end(span.first - 1);
                    Del { range, caret }
                } else {
                    Del {
                        range: 0..self.text().len(),
                        caret: 0,
                    }
                }
            })
            .collect();

        if dels.is_empty() {
            return false;
        }

        let changes: Vec<Change> = dels
            .iter()
            .map(|d| Change::new(d.range.clone(), ""))
            .collect();
        let Ok(transaction) = Transaction::new(changes) else {
            return false;
        };

        let mut order: Vec<usize> = (0..dels.len()).collect();
        order.sort_by_key(|&i| dels[i].range.start);
        let mut shift: isize = 0;
        let mut carets = vec![0usize; dels.len()];
        for i in order {
            carets[i] = (dels[i].caret as isize + shift) as usize;
            shift -= (dels[i].range.end - dels[i].range.start) as isize;
        }

        let primary = self.primary_span_index(&spans);
        self.apply(transaction);
        self.set_selections(Selections::new(
            carets.into_iter().map(Selection::caret).collect(),
            primary,
        ));
        true
    }

    /// §3.1. Joins each selection's line span onto one line, collapsing the
    /// newline and the next line's leading whitespace into a single space --
    /// and into nothing when the next line starts with a closing bracket or
    /// the current line already ends in whitespace.
    pub fn join_selection_lines(&mut self) -> bool {
        let spans = self.merged_spans();
        let line_count = self.lines().line_count();
        let rules = self.syntax();
        let text = self.text().to_string();

        struct SpanJoin {
            span: Span,
            changes: Vec<Change>,
            caret: usize,
        }

        let mut span_joins: Vec<SpanJoin> = Vec::new();
        for span in &spans {
            let effective_last = if span.first == span.last {
                (span.last + 1).min(line_count.saturating_sub(1))
            } else {
                span.last
            };
            if effective_last <= span.first {
                continue;
            }
            let mut changes = Vec::new();
            let mut caret = None;
            for i in span.first..effective_last {
                let line_i_end = self.lines().line_range(i, &text).unwrap().end;
                let next_start = self.line_start_or_end(i + 1);
                let next_range = self.lines().line_range(i + 1, &text).unwrap();
                let next_leading = leading_whitespace(&text[next_range.clone()]);
                let del_end = next_start + next_leading.len();
                let next_first_char = text[next_range.start + next_leading.len()..].chars().next();
                let ends_in_ws = text[..line_i_end]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c == ' ' || c == '\t');
                let is_closer = rules.is_some_and(|r| {
                    next_first_char.is_some_and(|c| r.brackets.iter().any(|(_, close)| *close == c))
                });
                let replacement = if ends_in_ws || is_closer { "" } else { " " };
                changes.push(Change::new(line_i_end..del_end, replacement));
                caret.get_or_insert(line_i_end);
            }
            if let Some(caret) = caret {
                span_joins.push(SpanJoin {
                    span: *span,
                    changes,
                    caret,
                });
            }
        }

        if span_joins.is_empty() {
            return false;
        }

        let all_changes: Vec<Change> = span_joins
            .iter()
            .flat_map(|sj| sj.changes.clone())
            .collect();
        let Ok(transaction) = Transaction::new(all_changes) else {
            return false;
        };

        let mut shift: isize = 0;
        let mut carets = Vec::with_capacity(span_joins.len());
        for sj in &span_joins {
            carets.push((sj.caret as isize + shift) as usize);
            let delta: isize = sj
                .changes
                .iter()
                .map(|c| c.insert.len() as isize - (c.range.end - c.range.start) as isize)
                .sum();
            shift += delta;
        }

        let primary_line = self.lines().line_at(self.selections().primary().start());
        let primary = span_joins
            .iter()
            .position(|sj| sj.span.first <= primary_line && primary_line <= sj.span.last)
            .unwrap_or(0);

        self.apply(transaction);
        self.set_selections(Selections::new(
            carets.into_iter().map(Selection::caret).collect(),
            primary,
        ));
        true
    }

    /// §3.1. Swaps each selection's line span with the line above/below it
    /// and carries the selections with it. No-op for a span already at the
    /// buffer's edge in that direction.
    pub fn move_selection_lines(&mut self, direction: LineDirection) -> bool {
        let spans = self.merged_spans();
        self.move_spans(direction, spans)
    }

    /// §3.2. Like `move_selection_lines`, but the span moved and the span
    /// jumped over are both grown to the smallest bracket-balanced line
    /// spans containing them.
    pub fn move_selection_statements(&mut self, direction: LineDirection) -> bool {
        let rules = self.syntax();
        let untokenized = self.text().len() > MAX_HIGHLIGHTED_FILE_BYTES;
        let spans = self.merged_spans();
        let spans = match rules {
            Some(rules) if !rules.brackets.is_empty() && !untokenized => {
                let line_count = self.lines().line_count();
                let text = self.text().to_string();
                spans
                    .into_iter()
                    .map(|span| {
                        grow_to_balanced_span(&text, self, rules, span, line_count).unwrap_or(span)
                    })
                    .collect()
            }
            _ => spans,
        };
        self.move_spans(direction, spans)
    }

    /// Shared swap machinery for `move_selection_lines`/
    /// `move_selection_statements`: `spans` is already whatever each caller
    /// wants swapped (grown to a balanced span, for the statement variant).
    fn move_spans(&mut self, direction: LineDirection, spans: Vec<Span>) -> bool {
        let line_count = self.lines().line_count();
        let text = self.text().to_string();

        struct Swap {
            range: Range<usize>,
            replacement: String,
            span_start_before: usize,
            span_len: usize,
            span_start_after: usize,
        }

        let mut swaps: Vec<Swap> = Vec::new();
        for span in spans {
            let (target, at_edge) = match direction {
                LineDirection::Up => (span.first.wrapping_sub(1), span.first == 0),
                LineDirection::Down => (span.last + 1, span.last + 1 >= line_count),
            };
            if at_edge {
                continue;
            }

            let (region_first, region_last) = match direction {
                LineDirection::Up => (target, span.last),
                LineDirection::Down => (span.first, target),
            };
            let region_start = self.line_start_or_end(region_first);
            let region_end = self
                .lines()
                .line_start(region_last + 1)
                .unwrap_or(text.len());

            let span_lines: Vec<&str> = (span.first..=span.last)
                .map(|l| &text[self.lines().line_range(l, &text).unwrap()])
                .collect();
            let target_line = &text[self.lines().line_range(target, &text).unwrap()];

            let ordered: Vec<&str> = match direction {
                LineDirection::Up => {
                    let mut v = span_lines.clone();
                    v.push(target_line);
                    v
                }
                LineDirection::Down => {
                    let mut v = vec![target_line];
                    v.extend(span_lines.iter().copied());
                    v
                }
            };
            let has_trailing_newline = self.lines().line_start(region_last + 1).is_some();
            let mut replacement = ordered.join("\n");
            if has_trailing_newline {
                replacement.push('\n');
            }

            let span_start_after = match direction {
                LineDirection::Up => region_start,
                LineDirection::Down => region_start + target_line.len() + 1,
            };
            let span_start_before = self.line_start_or_end(span.first);
            let span_end_before = self.lines().line_start(span.last + 1).unwrap_or(text.len());

            swaps.push(Swap {
                range: region_start..region_end,
                replacement,
                span_start_before,
                span_len: span_end_before - span_start_before,
                span_start_after,
            });
        }

        if swaps.is_empty() {
            return false;
        }

        let changes: Vec<Change> = swaps
            .iter()
            .map(|s| Change::new(s.range.clone(), s.replacement.clone()))
            .collect();
        let Ok(transaction) = Transaction::new(changes) else {
            return false;
        };

        let selections = self.selections().all().to_vec();
        let new_selections: Vec<Selection> = selections
            .iter()
            .map(|s| {
                let swap = swaps.iter().find(|sw| {
                    s.start() >= sw.span_start_before
                        && s.end() <= sw.span_start_before + sw.span_len
                });
                match swap {
                    Some(sw) => {
                        let base = sw.span_start_after as isize - sw.span_start_before as isize;
                        Selection::new(
                            (s.anchor as isize + base) as usize,
                            (s.head as isize + base) as usize,
                        )
                    }
                    _ => *s,
                }
            })
            .collect();

        let primary = self.selections().primary_index();
        self.apply(transaction);
        self.set_selections(Selections::new(new_selections, primary));
        true
    }

    /// §3.5. `lower` -> `UPPER` -> `lower`: a selection that is entirely
    /// lowercase becomes uppercase, anything else becomes lowercase. An
    /// empty selection acts on the word under the caret.
    pub fn toggle_selection_case(&mut self) -> bool {
        let text = self.text().to_string();
        let selections = self.selections().all().to_vec();

        let ranges: Vec<Range<usize>> = selections
            .iter()
            .map(|s| {
                if !s.is_empty() {
                    s.range()
                } else {
                    super::selection_hierarchy::word_at(&text, s.head).unwrap_or(s.head..s.head)
                }
            })
            .collect();

        if ranges.iter().all(Range::is_empty) {
            return false;
        }

        let changes: Vec<Change> = ranges
            .iter()
            .filter(|r| !r.is_empty())
            .cloned()
            .map(|r| {
                let slice = &text[r.clone()];
                let upper = slice.chars().all(|c| !c.is_lowercase());
                let replacement = if upper {
                    slice.to_lowercase()
                } else {
                    slice.to_uppercase()
                };
                Change::new(r, replacement)
            })
            .collect();
        let Ok(transaction) = Transaction::new(changes) else {
            return false;
        };

        let mut shift: isize = 0;
        let mut new_selections = Vec::with_capacity(selections.len());
        for (selection, range) in selections.iter().zip(&ranges) {
            if range.is_empty() {
                new_selections.push(*selection);
                continue;
            }
            let slice = &text[range.clone()];
            let upper = slice.chars().all(|c| !c.is_lowercase());
            let replacement_len = if upper {
                slice.to_lowercase().len()
            } else {
                slice.to_uppercase().len()
            };
            let start = (range.start as isize + shift) as usize;
            let end = start + replacement_len;
            shift += replacement_len as isize - range.len() as isize;
            new_selections.push(if selection.is_empty() {
                Selection::new(start, end)
            } else if selection.head < selection.anchor {
                Selection::new(end, start)
            } else {
                Selection::new(start, end)
            });
        }

        let primary = self.selections().primary_index();
        self.apply(transaction);
        self.set_selections(Selections::new(new_selections, primary));
        true
    }
}

/// Grows `span` outward, one line at a time in both directions, until every
/// bracket opened within it is also closed (counting only brackets outside
/// strings and comments, via `tokens()`) -- capped at `MAX_OCCURRENCES`
/// lines so an unbalanced opener at the top of a file cannot grow the span
/// to the whole buffer (§3.2, §4.6).
fn grow_to_balanced_span(
    text: &str,
    buffer: &TextBuffer,
    rules: &crate::syntax::SyntaxRules,
    span: Span,
    line_count: usize,
) -> Option<Span> {
    let mut first = span.first;
    let mut last = span.last;
    for _ in 0..MAX_OCCURRENCES {
        let range = buffer.lines().line_range(first, text)?.start
            ..buffer
                .lines()
                .line_range(last, text)
                .map(|r| r.end)
                .unwrap_or(text.len());
        if is_balanced(text, buffer, rules, range) {
            return Some(Span { first, last });
        }
        let mut grew = false;
        if first > 0 {
            first -= 1;
            grew = true;
        }
        if last + 1 < line_count {
            last += 1;
            grew = true;
        }
        if !grew {
            return Some(Span { first, last });
        }
    }
    Some(Span { first, last })
}

fn is_balanced(
    text: &str,
    buffer: &TextBuffer,
    rules: &crate::syntax::SyntaxRules,
    range: Range<usize>,
) -> bool {
    let tokens = buffer.tokens();
    let is_quoted_or_commented = |offset: usize| {
        let index = tokens.partition_point(|t| t.range.end <= offset);
        tokens.get(index).is_some_and(|t| {
            t.range.start <= offset && matches!(t.kind, TokenKind::String | TokenKind::Comment)
        })
    };
    let mut depth = 0i32;
    for (at, c) in text[range.clone()].char_indices() {
        let at = range.start + at;
        if is_quoted_or_commented(at) {
            continue;
        }
        if rules.brackets.iter().any(|(open, _)| *open == c) {
            depth += 1;
        } else if rules.brackets.iter().any(|(_, close)| *close == c) {
            depth -= 1;
            if depth < 0 {
                return false;
            }
        }
    }
    depth == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::RUST;

    fn rust(text: &str) -> TextBuffer {
        TextBuffer::new(text, Some(&RUST))
    }

    fn select(buffer: &mut TextBuffer, ranges: &[(usize, usize)]) {
        let selections = ranges.iter().map(|(a, h)| Selection::new(*a, *h)).collect();
        buffer.set_selections(Selections::new(selections, 0));
    }

    #[test]
    fn duplicate_a_single_line_non_empty_selection_copies_just_the_text() {
        let mut buffer = rust("let a = 1;\n");
        select(&mut buffer, &[(4, 5)]);
        assert!(buffer.duplicate_selection_lines());
        assert_eq!(buffer.text(), "let aa = 1;\n");
        assert_eq!(buffer.selections().primary(), Selection::new(5, 6));
    }

    #[test]
    fn duplicate_a_caret_copies_the_whole_line_below_and_lands_on_the_copy() {
        let mut buffer = rust("abc\ndef\n");
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.duplicate_selection_lines());
        assert_eq!(buffer.text(), "abc\nabc\ndef\n");
        assert_eq!(buffer.selections().primary(), Selection::caret(4));
    }

    #[test]
    fn duplicate_two_adjacent_carets_merge_into_one_two_line_copy() {
        let mut buffer = rust("a\nb\nc\n");
        select(&mut buffer, &[(0, 0), (2, 2)]);
        assert!(buffer.duplicate_selection_lines());
        assert_eq!(buffer.text(), "a\nb\na\nb\nc\n");
    }

    #[test]
    fn delete_a_line_lands_the_caret_at_the_start_of_the_next_one() {
        let mut buffer = rust("a\nb\nc\n");
        select(&mut buffer, &[(2, 2)]);
        assert!(buffer.delete_selection_lines());
        assert_eq!(buffer.text(), "a\nc\n");
        assert_eq!(buffer.selections().primary(), Selection::caret(2));
    }

    #[test]
    fn delete_the_final_line_removes_the_preceding_newline_instead() {
        let mut buffer = rust("a\nb\nc\n");
        select(&mut buffer, &[(6, 6)]);
        assert!(buffer.delete_selection_lines());
        assert_eq!(buffer.text(), "a\nb\nc");
        assert_eq!(buffer.selections().primary(), Selection::caret(4));
    }

    #[test]
    fn delete_spanning_the_whole_buffer_leaves_it_empty() {
        let mut buffer = rust("a\nb\n");
        select(&mut buffer, &[(0, 4)]);
        assert!(buffer.delete_selection_lines());
        assert_eq!(buffer.text(), "");
        assert_eq!(buffer.selections().primary(), Selection::caret(0));
    }

    #[test]
    fn delete_collapses_two_carets_on_one_line_into_one_resulting_caret() {
        let mut buffer = rust("abc\n");
        select(&mut buffer, &[(0, 0), (2, 2)]);
        assert!(buffer.delete_selection_lines());
        assert_eq!(buffer.text(), "");
        assert_eq!(buffer.selections().all().len(), 1);
    }

    #[test]
    fn join_inserts_a_single_space_at_the_join_point() {
        let mut buffer = rust("abc\ndef\n");
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.join_selection_lines());
        assert_eq!(buffer.text(), "abc def\n");
        assert_eq!(buffer.selections().primary(), Selection::caret(3));
    }

    #[test]
    fn join_omits_the_space_when_the_line_already_ends_in_whitespace() {
        let mut buffer = rust("abc \ndef\n");
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.join_selection_lines());
        assert_eq!(buffer.text(), "abc def\n");
        assert_eq!(buffer.selections().primary(), Selection::caret(4));
    }

    #[test]
    fn join_collapses_to_nothing_before_a_closing_bracket() {
        let mut buffer = rust("fn f() {\n}\n");
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.join_selection_lines());
        assert_eq!(buffer.text(), "fn f() {}\n");
        assert_eq!(buffer.selections().primary(), Selection::caret(8));
    }

    #[test]
    fn join_on_the_final_line_is_a_no_op() {
        let mut buffer = rust("abc\n");
        select(&mut buffer, &[(4, 4)]);
        assert!(!buffer.join_selection_lines());
        assert_eq!(buffer.text(), "abc\n");
    }

    #[test]
    fn move_line_down_swaps_with_the_following_line() {
        let mut buffer = rust("a\nb\nc\n");
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.move_selection_lines(LineDirection::Down));
        assert_eq!(buffer.text(), "b\na\nc\n");
        assert_eq!(buffer.selections().primary(), Selection::caret(2));
    }

    #[test]
    fn move_line_up_swaps_with_the_previous_line() {
        let mut buffer = rust("a\nb\nc\n");
        select(&mut buffer, &[(2, 2)]);
        assert!(buffer.move_selection_lines(LineDirection::Up));
        assert_eq!(buffer.text(), "b\na\nc\n");
        assert_eq!(buffer.selections().primary(), Selection::caret(0));
    }

    #[test]
    fn move_line_up_at_the_top_is_a_no_op() {
        let mut buffer = rust("a\nb\n");
        select(&mut buffer, &[(0, 0)]);
        assert!(!buffer.move_selection_lines(LineDirection::Up));
        assert_eq!(buffer.text(), "a\nb\n");
    }

    #[test]
    fn move_line_down_at_the_bottom_is_a_no_op() {
        let mut buffer = rust("a\nb\n");
        select(&mut buffer, &[(4, 4)]);
        assert!(!buffer.move_selection_lines(LineDirection::Down));
        assert_eq!(buffer.text(), "a\nb\n");
    }

    #[test]
    fn move_statement_grows_to_a_balanced_bracket_region_before_swapping() {
        let mut buffer = rust("{\na\n}\nb\n");
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.move_selection_statements(LineDirection::Down));
        assert_eq!(buffer.text(), "b\n{\na\n}\n");
    }

    #[test]
    fn move_statement_without_syntax_falls_back_to_a_plain_line_swap() {
        let mut buffer = TextBuffer::new("{\na\nb\n", None);
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.move_selection_statements(LineDirection::Down));
        assert_eq!(buffer.text(), "a\n{\nb\n");
    }

    #[test]
    fn move_statement_on_an_untokenized_buffer_falls_back_to_a_plain_line_swap() {
        // Over MAX_HIGHLIGHTED_FILE_BYTES, `tokens()` is empty (§4.6, the
        // same threshold A4a's `matching_bracket` refuses at) -- the
        // unmatched `{` must not grow the span into the filler line.
        let filler = "x".repeat(crate::syntax::MAX_HIGHLIGHTED_FILE_BYTES + 10);
        let mut buffer = rust(&format!("{{\n{filler}\nb\n"));
        select(&mut buffer, &[(0, 0)]);
        assert!(buffer.move_selection_statements(LineDirection::Down));
        assert!(buffer.text().starts_with(&filler));
    }

    #[test]
    fn move_statement_growth_stops_at_the_occurrence_cap() {
        // Every line opens a bracket that's never closed, so growth would
        // otherwise run to both buffer edges; it must stop after
        // MAX_OCCURRENCES iterations instead.
        let line_count = MAX_OCCURRENCES * 2 + 50;
        let text = "{\n".repeat(line_count);
        let buffer = rust(&text);
        let span = Span {
            first: line_count / 2,
            last: line_count / 2,
        };
        let grown = grow_to_balanced_span(&text, &buffer, &RUST, span, line_count).unwrap();
        assert!(grown.last - grown.first < 2 * MAX_OCCURRENCES + 1);
        assert!(grown.first > 0);
        assert!(grown.last + 1 < line_count);
    }

    #[test]
    fn toggle_case_round_trips_between_upper_and_lower() {
        let mut buffer = rust("abc");
        select(&mut buffer, &[(0, 3)]);
        assert!(buffer.toggle_selection_case());
        assert_eq!(buffer.text(), "ABC");
        assert!(buffer.toggle_selection_case());
        assert_eq!(buffer.text(), "abc");
    }

    #[test]
    fn toggle_case_on_an_empty_selection_selects_the_toggled_word() {
        let mut buffer = rust("foo bar");
        select(&mut buffer, &[(1, 1)]);
        assert!(buffer.toggle_selection_case());
        assert_eq!(buffer.text(), "FOO bar");
        assert_eq!(buffer.selections().primary(), Selection::new(0, 3));
    }

    #[test]
    fn toggle_case_off_any_word_is_a_no_op() {
        let mut buffer = rust(" ");
        select(&mut buffer, &[(0, 0)]);
        assert!(!buffer.toggle_selection_case());
        assert_eq!(buffer.text(), " ");
    }

    #[test]
    fn toggle_case_on_multiple_selections_shifts_each_independently() {
        let mut buffer = rust("ab cd");
        select(&mut buffer, &[(0, 2), (4, 4)]);
        assert!(buffer.toggle_selection_case());
        assert_eq!(buffer.text(), "AB CD");
        assert_eq!(
            buffer.selections().all(),
            &[Selection::new(0, 2), Selection::new(3, 5)]
        );
    }
}
