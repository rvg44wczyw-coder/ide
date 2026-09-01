use std::time::{Duration, Instant};

use super::edit::Transaction;
use super::selection::Selections;

/// How long a run of typing stays one undo step. JetBrains-style editors all
/// use a timeout of this order; the exact value is a feel decision, not a
/// derived one.
const UNDO_COALESCE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
struct Step {
    forward: Transaction,
    inverse: Transaction,
}

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    steps: Vec<Step>,
    before: Selections,
    after: Selections,
}

/// Whether an edit is allowed to join the open undo group. Only typing ever
/// is: a programmatic edit means exactly what the caller issued, so it
/// always stands alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKind {
    Typed,
    Programmatic,
}

#[derive(Debug, Default)]
pub(crate) struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    open: Option<OpenGroup>,
}

#[derive(Debug)]
struct OpenGroup {
    /// Where each caret ended up after the last typed call, so the next one
    /// can be checked for continuing the same run.
    ends: Vec<usize>,
    at: Instant,
}

/// One applied edit, as the history needs to see it.
#[derive(Debug)]
pub(crate) struct Recorded {
    pub(crate) forward: Transaction,
    pub(crate) inverse: Transaction,
    pub(crate) before: Selections,
    pub(crate) after: Selections,
    pub(crate) kind: EditKind,
    pub(crate) contains_newline: bool,
}

impl History {
    pub(crate) fn record(&mut self, edit: Recorded, now: Instant) {
        let Recorded {
            forward,
            inverse,
            before,
            after,
            kind,
            contains_newline,
        } = edit;
        self.redo.clear();
        let step = Step { forward, inverse };

        let continues = kind == EditKind::Typed
            && !contains_newline
            && self
                .open
                .as_ref()
                .is_some_and(|group| self.continues_run(group, &before, now));

        if continues {
            if let Some(entry) = self.undo.last_mut() {
                entry.steps.push(step);
                entry.after = after.clone();
            }
        } else {
            self.undo.push(Entry {
                steps: vec![step],
                before,
                after: after.clone(),
            });
        }

        self.open = match kind {
            EditKind::Typed if !contains_newline => Some(OpenGroup {
                ends: after.all().iter().map(|s| s.head).collect(),
                at: now,
            }),
            _ => None,
        };
    }

    fn continues_run(&self, group: &OpenGroup, before: &Selections, now: Instant) -> bool {
        now.duration_since(group.at) < UNDO_COALESCE
            && group.ends.len() == before.len()
            && group
                .ends
                .iter()
                .zip(before.all())
                .all(|(end, selection)| selection.is_empty() && selection.head == *end)
    }

    pub(crate) fn break_group(&mut self) {
        self.open = None;
    }

    /// Hands back the inverses to apply, newest first, plus the selections
    /// to restore afterwards.
    pub(crate) fn pop_undo(&mut self) -> Option<(Vec<Transaction>, Selections)> {
        let entry = self.undo.pop()?;
        self.open = None;
        let transactions = entry
            .steps
            .iter()
            .rev()
            .map(|step| step.inverse.clone())
            .collect();
        let selections = entry.before.clone();
        self.redo.push(entry);
        Some((transactions, selections))
    }

    pub(crate) fn pop_redo(&mut self) -> Option<(Vec<Transaction>, Selections)> {
        let entry = self.redo.pop()?;
        self.open = None;
        let transactions = entry
            .steps
            .iter()
            .map(|step| step.forward.clone())
            .collect();
        let selections = entry.after.clone();
        self.undo.push(entry);
        Some((transactions, selections))
    }
}

#[cfg(test)]
mod tests {
    use super::super::selection::Selection;
    use super::*;

    fn typed(history: &mut History, offset: usize, text: &str, now: Instant) {
        let before = Selections::single(Selection::caret(offset));
        let after = Selections::single(Selection::caret(offset + text.len()));
        history.record(
            Recorded {
                forward: Transaction::insert(offset, text),
                inverse: Transaction::delete(offset..offset + text.len()),
                before,
                after,
                kind: EditKind::Typed,
                contains_newline: text.contains('\n'),
            },
            now,
        );
    }

    #[test]
    fn consecutive_typing_coalesces_into_one_entry() {
        let mut history = History::default();
        let now = Instant::now();
        typed(&mut history, 0, "a", now);
        typed(&mut history, 1, "b", now);
        typed(&mut history, 2, "c", now);
        assert_eq!(history.undo.len(), 1);
        assert_eq!(history.undo[0].steps.len(), 3);
    }

    #[test]
    fn a_newline_never_joins_a_run() {
        let mut history = History::default();
        let now = Instant::now();
        typed(&mut history, 0, "a", now);
        typed(&mut history, 1, "\n", now);
        typed(&mut history, 2, "b", now);
        assert_eq!(history.undo.len(), 3);
    }

    #[test]
    fn a_non_adjacent_caret_starts_a_new_entry() {
        let mut history = History::default();
        let now = Instant::now();
        typed(&mut history, 0, "a", now);
        typed(&mut history, 9, "b", now);
        assert_eq!(history.undo.len(), 2);
    }

    #[test]
    fn the_timeout_starts_a_new_entry() {
        let mut history = History::default();
        let now = Instant::now();
        typed(&mut history, 0, "a", now);
        typed(&mut history, 1, "b", now + UNDO_COALESCE);
        assert_eq!(history.undo.len(), 2);
    }

    #[test]
    fn break_group_starts_a_new_entry() {
        let mut history = History::default();
        let now = Instant::now();
        typed(&mut history, 0, "a", now);
        history.break_group();
        typed(&mut history, 1, "b", now);
        assert_eq!(history.undo.len(), 2);
    }

    #[test]
    fn a_programmatic_edit_never_coalesces() {
        let mut history = History::default();
        let now = Instant::now();
        let before = Selections::single(Selection::caret(0));
        let after = Selections::single(Selection::caret(1));
        for _ in 0..2 {
            history.record(
                Recorded {
                    forward: Transaction::insert(0, "a"),
                    inverse: Transaction::delete(0..1),
                    before: before.clone(),
                    after: after.clone(),
                    kind: EditKind::Programmatic,
                    contains_newline: false,
                },
                now,
            );
        }
        assert_eq!(history.undo.len(), 2);
    }

    #[test]
    fn a_programmatic_edit_closes_an_open_run() {
        let mut history = History::default();
        let now = Instant::now();
        typed(&mut history, 0, "a", now);
        history.record(
            Recorded {
                forward: Transaction::insert(1, "x"),
                inverse: Transaction::delete(1..2),
                before: Selections::single(Selection::caret(1)),
                after: Selections::single(Selection::caret(2)),
                kind: EditKind::Programmatic,
                contains_newline: false,
            },
            now,
        );
        typed(&mut history, 2, "b", now);
        assert_eq!(history.undo.len(), 3);
    }

    #[test]
    fn undo_then_redo_moves_entries_between_stacks() {
        let mut history = History::default();
        let now = Instant::now();
        typed(&mut history, 0, "a", now);
        let (undo, selections) = history.pop_undo().expect("one entry recorded");
        assert_eq!(undo.len(), 1);
        assert_eq!(selections.primary(), Selection::caret(0));
        assert!(history.undo.is_empty());

        let (redo, selections) = history.pop_redo().expect("one entry undone");
        assert_eq!(redo.len(), 1);
        assert_eq!(selections.primary(), Selection::caret(1));
        assert_eq!(history.undo.len(), 1);
        assert!(history.pop_redo().is_none());
    }

    #[test]
    fn a_new_edit_clears_the_redo_stack() {
        let mut history = History::default();
        let now = Instant::now();
        typed(&mut history, 0, "a", now);
        history.pop_undo();
        typed(&mut history, 0, "b", now);
        assert!(history.pop_redo().is_none());
    }

    #[test]
    fn pop_undo_on_an_empty_stack_is_none() {
        let mut history = History::default();
        assert!(history.pop_undo().is_none());
    }
}
