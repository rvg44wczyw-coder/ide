//! The text model the editor is built on: a `String` plus an incrementally
//! maintained line index, transactions, multi-cursor selections, grouped
//! undo and incremental retokenization
//! (`docs/features/editor-engine.md`).
//!
//! Storage is a single `String`, so a splice is O(n) in the buffer length
//! while `line_at`/`position_at` are O(log L) and a token lookup is O(log T).
//! A rope would make the splice sub-linear, but every offset in this module
//! is storage-agnostic by design, so swapping the backing store later stays
//! confined to `TextBuffer` -- see the doc's §4.5 for what would force that.

mod brackets;
mod comments;
mod edit;
mod find;
mod folding;
mod history;
mod indent;
mod line_ops;
mod lines;
mod ops;
mod selection;
mod selection_hierarchy;

pub use brackets::{BracketPair, MAX_BRACKET_SCAN_BYTES};
pub use edit::{Bias, Change, Transaction, TransactionError};
pub use find::{all_occurrences, next_occurrence, MAX_OCCURRENCES};
pub use folding::{FoldKind, FoldRange};
pub use indent::{leading_whitespace, newline_indent, splits_a_pair, IndentStyle, IndentUnit};
pub use line_ops::LineDirection;
pub use lines::LineIndex;
pub use selection::{Selection, Selections};
pub use selection_hierarchy::word_at;

use std::ops::Range;
use std::time::Instant;

use history::{EditKind, History, Recorded};

use crate::syntax::{tokenize_range, LineState, SyntaxRules, Token, MAX_HIGHLIGHTED_FILE_BYTES};

#[derive(Debug)]
pub struct TextBuffer {
    text: String,
    lines: LineIndex,
    selections: Selections,
    history: History,
    syntax: Option<&'static SyntaxRules>,
    tokens: Vec<Token>,
    /// Tokenizer state at the start of each line; always as long as
    /// `lines.line_count()`, with `Normal` at index 0.
    line_states: Vec<LineState>,
}

impl TextBuffer {
    /// Starts with a single caret at offset 0, a clean history, and the text
    /// fully tokenized.
    pub fn new(text: impl Into<String>, syntax: Option<&'static SyntaxRules>) -> Self {
        let text = text.into();
        let lines = LineIndex::new(&text);
        let mut buffer = Self {
            text,
            lines,
            selections: Selections::default(),
            history: History::default(),
            syntax,
            tokens: Vec::new(),
            line_states: Vec::new(),
        };
        buffer.retokenize_all();
        buffer
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn lines(&self) -> &LineIndex {
        &self.lines
    }

    pub fn line_text(&self, line: usize) -> Option<&str> {
        self.lines
            .line_range(line, &self.text)
            .map(|range| &self.text[range])
    }

    pub fn selections(&self) -> &Selections {
        &self.selections
    }

    pub fn set_selections(&mut self, selections: Selections) {
        self.selections = selections;
        self.history.break_group();
    }

    /// Applies `transaction`, maps every selection through it, records one
    /// undo entry, and retokenizes only the affected line span. The single
    /// mutation entry point: nothing else modifies `text`. Never coalesces
    /// -- one call is one undo step, so a programmatic edit is always undone
    /// as the caller issued it.
    pub fn apply(&mut self, transaction: Transaction) {
        self.edit(transaction, EditKind::Programmatic, Instant::now());
    }

    /// Replaces every selection's range with `text`. One transaction, one
    /// undo step; each selection is left as a bare caret just past the text
    /// it received.
    pub fn insert_at_selections(&mut self, text: &str) {
        let transaction = self.selection_transaction(text);
        self.apply(transaction);
    }

    /// The typing entry point, and the only thing that coalesces:
    /// consecutive calls continuing one run of typed text on one line
    /// collapse into a single undo step. Otherwise identical to
    /// `insert_at_selections`.
    pub fn type_text(&mut self, text: &str) {
        self.type_text_at(text, Instant::now());
    }

    fn type_text_at(&mut self, text: &str, now: Instant) {
        let transaction = self.selection_transaction(text);
        self.edit(transaction, EditKind::Typed, now);
    }

    fn selection_transaction(&self, text: &str) -> Transaction {
        let changes = self
            .selections
            .all()
            .iter()
            .map(|selection| Change::new(selection.range(), text))
            .collect();
        Transaction::new(changes).expect("selections are non-overlapping by construction")
    }

    /// Undoes one group, restoring the selections active when it was made.
    pub fn undo(&mut self) -> bool {
        let Some((transactions, selections)) = self.history.pop_undo() else {
            return false;
        };
        self.replay(transactions, selections);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some((transactions, selections)) = self.history.pop_redo() else {
            return false;
        };
        self.replay(transactions, selections);
        true
    }

    /// Ends the current coalescing group, so the next `type_text` starts a
    /// new undo step regardless of timing.
    pub fn break_undo_group(&mut self) {
        self.history.break_group();
    }

    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    /// Tokens overlapping `lines`, for a viewport-limited repaint. A
    /// subslice of `tokens()`, since tokens are sorted by offset.
    pub fn tokens_in_lines(&self, lines: Range<usize>) -> &[Token] {
        let Some(start) = self.lines.line_start(lines.start) else {
            return &[];
        };
        let end = self
            .lines
            .line_start(lines.end)
            .unwrap_or(self.text.len() + 1);
        let first = self.tokens.partition_point(|t| t.range.end <= start);
        let last = self.tokens.partition_point(|t| t.range.start < end);
        &self.tokens[first..last.max(first)]
    }

    /// The rules this buffer is tokenized under. Every language-dependent
    /// operation needs them -- brackets, auto-indent, and the UI's typing
    /// behaviours -- and until A4a the field was write-only through
    /// `set_syntax`.
    pub fn syntax(&self) -> Option<&'static SyntaxRules> {
        self.syntax
    }

    /// Discards every cached token and every cached per-line state and
    /// retokenizes from scratch -- states cached under the old rules say
    /// nothing about the new ones. `None` clears highlighting.
    pub fn set_syntax(&mut self, syntax: Option<&'static SyntaxRules>) {
        self.syntax = syntax;
        self.retokenize_all();
    }

    fn edit(&mut self, transaction: Transaction, kind: EditKind, now: Instant) {
        if transaction.is_empty() {
            return;
        }
        let clamped = self.clamp(transaction);
        if clamped
            .changes()
            .iter()
            .all(|c| c.range.is_empty() && c.insert.is_empty())
        {
            return;
        }

        let before = self.selections.clone();
        let contains_newline = clamped.changes().iter().any(|c| c.insert.contains('\n'));
        let inverse = self.inverse_of(&clamped);
        let first_line = self.apply_to_text(&clamped);
        self.selections = self.selections.map(&clamped);
        let after = self.selections.clone();
        self.retokenize_from(first_line, &clamped);
        self.history.record(
            Recorded {
                forward: clamped,
                inverse,
                before,
                after,
                kind,
                contains_newline,
            },
            now,
        );
    }

    fn replay(&mut self, transactions: Vec<Transaction>, selections: Selections) {
        for transaction in transactions {
            // Clamped like any other edit: the inverses were built from
            // already-valid offsets, but nothing downstream should depend on
            // that holding at a distance.
            let transaction = self.clamp(transaction);
            let first_line = self.apply_to_text(&transaction);
            self.retokenize_from(first_line, &transaction);
        }
        self.selections = selections;
    }

    /// Clamps every change to the buffer: first to `text.len()`, then to the
    /// nearest char boundary, mirroring what `Buffer` has always done rather
    /// than panicking on an offset a caller got slightly wrong.
    fn clamp(&self, transaction: Transaction) -> Transaction {
        let changes = transaction
            .changes()
            .iter()
            .map(|change| {
                let start = clamp_offset(&self.text, change.range.start);
                let end = clamp_offset(&self.text, change.range.end).max(start);
                Change::new(start..end, change.insert.clone())
            })
            .collect();
        Transaction::new(changes).expect("clamping preserves ordering and cannot create overlap")
    }

    fn inverse_of(&self, transaction: &Transaction) -> Transaction {
        let mut delta: isize = 0;
        let changes = transaction
            .changes()
            .iter()
            .map(|change| {
                let removed = self.text[change.range.clone()].to_string();
                let start = (change.range.start as isize + delta) as usize;
                delta +=
                    change.insert.len() as isize - (change.range.end - change.range.start) as isize;
                Change::new(start..start + change.insert.len(), removed)
            })
            .collect();
        Transaction::new(changes).expect("inverting preserves ordering and cannot create overlap")
    }

    /// Splices every change into the text back-to-front, so the offsets of
    /// the changes still pending stay valid, and keeps the line index in
    /// step. Returns the first line the edit touched.
    fn apply_to_text(&mut self, transaction: &Transaction) -> usize {
        let mut first_line = self.lines.line_count();
        for change in transaction.changes().iter().rev() {
            first_line = first_line.min(self.lines.line_at(change.range.start));
            self.text
                .replace_range(change.range.clone(), &change.insert);
            self.lines.apply(&self.text, change);
        }
        first_line
    }

    fn retokenize_all(&mut self) {
        self.tokens.clear();
        self.line_states = vec![LineState::Normal; self.lines.line_count()];
        if self.syntax.is_none() || self.text.len() > MAX_HIGHLIGHTED_FILE_BYTES {
            return;
        }
        self.rescan(0, None);
    }

    fn retokenize_from(&mut self, first_line: usize, transaction: &Transaction) {
        if self.syntax.is_none() || self.text.len() > MAX_HIGHLIGHTED_FILE_BYTES {
            self.tokens.clear();
            self.line_states = vec![LineState::Normal; self.lines.line_count()];
            return;
        }
        let byte_delta: isize = transaction
            .changes()
            .iter()
            .map(|c| c.insert.len() as isize - (c.range.end - c.range.start) as isize)
            .sum();
        let last_touched = transaction
            .changes()
            .last()
            .map(|c| {
                let end = (c.range.end as isize + byte_delta).max(0) as usize;
                self.lines.line_at(end.min(self.text.len()))
            })
            .unwrap_or(first_line);
        self.rescan(first_line, Some((last_touched, byte_delta)));
    }

    /// Retokenizes line by line from `first_line`. With `resume` set, stops
    /// at the first line past the edit whose entry state matches the one
    /// recorded before the edit -- from there the old tokens are still
    /// correct and are reused, shifted by the edit's byte delta.
    fn rescan(&mut self, first_line: usize, resume: Option<(usize, isize)>) {
        let Some(rules) = self.syntax else {
            return;
        };
        let old_states = std::mem::take(&mut self.line_states);
        let old_tokens = std::mem::take(&mut self.tokens);
        let line_count = self.lines.line_count();
        let line_delta = line_count as isize - old_states.len() as isize;

        // Resuming *inside* a block comment would re-emit it starting at the
        // wrong line, so back up to the line that opened it. Line numbering
        // at or before the edit is the same in both indices, which is what
        // makes the old states safe to read here.
        let mut first_line = first_line;
        while first_line > 0
            && old_states
                .get(first_line)
                .is_some_and(|state| *state == LineState::InBlockComment)
        {
            first_line -= 1;
        }

        let scan_start = self.lines.line_start(first_line).unwrap_or(0);
        let mut tokens: Vec<Token> = old_tokens
            .iter()
            .take_while(|t| t.range.end <= scan_start)
            .cloned()
            .collect();
        let mut states = Vec::with_capacity(line_count);
        states.extend(old_states.iter().take(first_line + 1).copied());
        if states.is_empty() {
            states.push(LineState::Normal);
        }
        states.truncate(first_line + 1);

        let mut line = first_line;
        let mut state = states[first_line];
        while let Some(start) = self.lines.line_start(line) {
            let end = self.lines.line_start(line + 1).unwrap_or(self.text.len());
            let (mut produced, next_state) = tokenize_range(&self.text, rules, start..end, state);
            // A block comment carried in from the previous line is re-emitted
            // whole from this line start; it is the same token the previous
            // line already produced, not a second one.
            if state == LineState::InBlockComment {
                if let (Some(last), Some(first)) = (tokens.last_mut(), produced.first()) {
                    if last.range.end >= first.range.start {
                        last.range.end = last.range.end.max(first.range.end);
                        produced.remove(0);
                    }
                }
            }
            tokens.extend(produced);
            state = next_state;
            line += 1;
            if line >= line_count {
                break;
            }
            states.push(state);

            if let Some((last_touched, byte_delta)) = resume {
                if line > last_touched {
                    let old_line = line as isize - line_delta;
                    let converged = old_line >= 0
                        && old_states
                            .get(old_line as usize)
                            .is_some_and(|old| *old == state);
                    if converged {
                        let boundary = (self.lines.line_start(line).unwrap_or(0) as isize
                            - byte_delta)
                            .max(0) as usize;
                        tokens.extend(old_tokens.iter().filter(|t| t.range.start >= boundary).map(
                            |t| Token {
                                range: shift(t.range.start, byte_delta)
                                    ..shift(t.range.end, byte_delta),
                                kind: t.kind,
                            },
                        ));
                        states.extend(
                            old_states
                                .iter()
                                .skip((old_line as usize) + 1)
                                .take(line_count - line - 1)
                                .copied(),
                        );
                        break;
                    }
                }
            }
        }

        states.resize(line_count, LineState::Normal);
        self.tokens = tokens;
        self.line_states = states;
    }
}

fn shift(offset: usize, delta: isize) -> usize {
    (offset as isize + delta).max(0) as usize
}

fn clamp_offset(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests;
