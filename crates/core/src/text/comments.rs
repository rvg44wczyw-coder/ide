//! Toggle Line/Block Comment
//! (`docs/features/line-commands-and-editorconfig.md` §2.1, §3.3).

use std::ops::Range;

use super::indent::{leading_whitespace, IndentUnit};
use super::{Change, Selection, Selections, TextBuffer, Transaction};

impl TextBuffer {
    /// Every distinct line number touched by any selection, sorted and
    /// deduped -- two cursors on one line count once.
    fn touched_line_numbers(&self) -> Vec<usize> {
        let mut lines: Vec<usize> = self
            .selections()
            .all()
            .iter()
            .flat_map(|s| self.lines().line_at(s.start())..=self.lines().line_at(s.end()))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    /// §3.3. Adds or removes `syntax()`'s first `line_comment_prefixes`
    /// entry on every line each selection touches. Uncomments only when
    /// *every* touched line is already commented; otherwise comments all of
    /// them. `false` when the language has no line comment.
    pub fn toggle_line_comment(&mut self, unit: IndentUnit) -> bool {
        let Some(rules) = self.syntax() else {
            return false;
        };
        let Some(&prefix) = rules.line_comment_prefixes.first() else {
            return false;
        };
        let lines = self.touched_line_numbers();
        if lines.is_empty() {
            return false;
        }
        let text = self.text().to_string();

        let line_range = |l: usize| self.lines().line_range(l, &text).unwrap();
        let all_commented = lines.iter().all(|&l| {
            let content = &text[line_range(l)];
            content.trim_start().starts_with(prefix)
        });

        let changes: Vec<Change> = if all_commented {
            lines
                .iter()
                .map(|&l| {
                    let range = line_range(l);
                    let content = &text[range.clone()];
                    let ws_len = leading_whitespace(content).len();
                    let after_ws = &content[ws_len..];
                    let mut remove_len = prefix.len();
                    if after_ws[prefix.len()..].starts_with(' ') {
                        remove_len += 1;
                    }
                    Change::new(range.start + ws_len..range.start + ws_len + remove_len, "")
                })
                .collect()
        } else {
            let shallowest = lines
                .iter()
                .map(|&l| unit.columns_of(leading_whitespace(&text[line_range(l)])))
                .min()
                .unwrap_or(0);
            lines
                .iter()
                .map(|&l| {
                    let range = line_range(l);
                    let indent = leading_whitespace(&text[range.clone()]);
                    let at = range.start + indent_bytes_at_column(indent, unit, shallowest);
                    Change::new(at..at, format!("{prefix} "))
                })
                .collect()
        };

        let Ok(transaction) = Transaction::new(changes) else {
            return false;
        };
        self.apply(transaction);
        true
    }

    /// §3.3. Wraps each selection in `syntax()`'s `block_comment`, or
    /// unwraps it when the selection is already exactly a block comment
    /// (ignoring surrounding whitespace). `false` when the language has
    /// none.
    pub fn toggle_block_comment(&mut self) -> bool {
        let Some(rules) = self.syntax() else {
            return false;
        };
        let Some((open, close)) = rules.block_comment else {
            return false;
        };
        let selections = self.selections().all().to_vec();
        let text = self.text().to_string();

        enum Action {
            Wrap,
            Unwrap { inner: Range<usize> },
            InsertEmpty,
        }

        let actions: Vec<Action> = selections
            .iter()
            .map(|s| {
                if s.is_empty() {
                    return Action::InsertEmpty;
                }
                let full = &text[s.range()];
                let trimmed = full.trim();
                let leading = full.len() - full.trim_start().len();
                if trimmed.len() >= open.len() + close.len()
                    && trimmed.starts_with(open)
                    && trimmed.ends_with(close)
                {
                    let inner_start = s.range().start + leading + open.len();
                    let inner_end = inner_start + (trimmed.len() - open.len() - close.len());
                    Action::Unwrap {
                        inner: inner_start..inner_end,
                    }
                } else {
                    Action::Wrap
                }
            })
            .collect();

        let changes: Vec<Change> = selections
            .iter()
            .zip(&actions)
            .map(|(s, a)| match a {
                Action::Wrap => {
                    Change::new(s.range(), format!("{open}{}{close}", &text[s.range()]))
                }
                Action::Unwrap { inner } => Change::new(s.range(), text[inner.clone()].to_string()),
                Action::InsertEmpty => Change::new(s.range(), format!("{open}{close}")),
            })
            .collect();
        let Ok(transaction) = Transaction::new(changes) else {
            return false;
        };

        let mut shift: isize = 0;
        let mut new_selections = Vec::with_capacity(selections.len());
        for (s, a) in selections.iter().zip(&actions) {
            let start = (s.start() as isize + shift) as usize;
            match a {
                Action::Wrap => {
                    let inner_len = s.range().len();
                    let new_end = start + open.len() + inner_len + close.len();
                    shift += (open.len() + close.len()) as isize;
                    new_selections.push(reorder(s, start, new_end));
                }
                Action::Unwrap { inner } => {
                    let inner_len = inner.len();
                    shift -= (s.range().len() - inner_len) as isize;
                    new_selections.push(reorder(s, start, start + inner_len));
                }
                Action::InsertEmpty => {
                    let caret = start + open.len();
                    shift += (open.len() + close.len()) as isize;
                    new_selections.push(Selection::caret(caret));
                }
            }
        }

        let primary = self.selections().primary_index();
        self.apply(transaction);
        self.set_selections(Selections::new(new_selections, primary));
        true
    }
}

fn reorder(original: &Selection, start: usize, end: usize) -> Selection {
    if original.head < original.anchor {
        Selection::new(end, start)
    } else {
        Selection::new(start, end)
    }
}

/// Byte length of the shortest prefix of `indent` whose display width (in
/// `unit`'s columns) reaches `columns` -- the insertion point a comment
/// prefix goes at (§3.3), mirroring how `ops::outdent_len` measures a
/// removal the same way.
fn indent_bytes_at_column(indent: &str, unit: IndentUnit, columns: usize) -> usize {
    if columns == 0 {
        return 0;
    }
    for (byte, c) in indent.char_indices() {
        let end = byte + c.len_utf8();
        if unit.columns_of(&indent[..end]) >= columns {
            return end;
        }
    }
    indent.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{PYTHON, RUST};
    use crate::text::IndentStyle;

    fn rust(text: &str) -> TextBuffer {
        TextBuffer::new(text, Some(&RUST))
    }

    fn select(buffer: &mut TextBuffer, ranges: &[(usize, usize)]) {
        let selections = ranges.iter().map(|(a, h)| Selection::new(*a, *h)).collect();
        buffer.set_selections(Selections::new(selections, 0));
    }

    #[test]
    fn line_comment_round_trips() {
        let mut buffer = rust("let a = 1;\nlet b = 2;\n");
        select(&mut buffer, &[(0, 0), (11, 11)]);
        assert!(buffer.toggle_line_comment(IndentUnit::default()));
        assert_eq!(buffer.text(), "// let a = 1;\n// let b = 2;\n");
        assert!(buffer.toggle_line_comment(IndentUnit::default()));
        assert_eq!(buffer.text(), "let a = 1;\nlet b = 2;\n");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "// let a = 1;\n// let b = 2;\n");
    }

    #[test]
    fn line_comment_comments_everything_when_the_touched_set_is_mixed() {
        let mut buffer = rust("// a\nb\n");
        select(&mut buffer, &[(0, 6)]);
        assert!(buffer.toggle_line_comment(IndentUnit::default()));
        assert_eq!(buffer.text(), "// // a\n// b\n");
    }

    #[test]
    fn line_comment_lands_at_the_shallowest_common_indentation() {
        let mut buffer = rust("  a\n    b\n");
        select(&mut buffer, &[(0, 9)]);
        assert!(buffer.toggle_line_comment(IndentUnit::default()));
        assert_eq!(buffer.text(), "  // a\n  //   b\n");
    }

    #[test]
    fn line_comment_uncommenting_removes_one_following_space() {
        let mut buffer = rust("//no space\n// with space\n");
        select(&mut buffer, &[(0, 0), (11, 11)]);
        assert!(buffer.toggle_line_comment(IndentUnit::default()));
        assert_eq!(buffer.text(), "no space\nwith space\n");
    }

    #[test]
    fn line_comment_no_ops_without_a_line_comment_style() {
        // XML has no `line_comment_prefixes`.
        let mut buffer = TextBuffer::new("<a/>\n", Some(&crate::syntax::XML));
        select(&mut buffer, &[(0, 0)]);
        assert!(!buffer.toggle_line_comment(IndentUnit::default()));
    }

    #[test]
    fn line_comment_never_falls_back_to_block_comment() {
        // TOML has neither block comments nor this test's concern directly
        // -- Python has line comments but no block comment, the case that
        // actually matters: toggling block on Python must no-op, not borrow
        // Python's line style.
        let mut buffer = TextBuffer::new("x = 1\n", Some(&PYTHON));
        select(&mut buffer, &[(0, 0)]);
        assert!(!buffer.toggle_block_comment());
    }

    #[test]
    fn block_comment_wraps_and_unwraps() {
        let mut buffer = rust("x");
        select(&mut buffer, &[(0, 1)]);
        assert!(buffer.toggle_block_comment());
        assert_eq!(buffer.text(), "/*x*/");
        assert_eq!(
            &buffer.text()[buffer.selections().primary().range()],
            "/*x*/"
        );
        assert!(buffer.toggle_block_comment());
        assert_eq!(buffer.text(), "x");
    }

    #[test]
    fn block_comment_on_an_empty_selection_leaves_the_caret_between_the_delimiters() {
        let mut buffer = rust("");
        assert!(buffer.toggle_block_comment());
        assert_eq!(buffer.text(), "/**/");
        assert_eq!(buffer.selections().primary(), Selection::caret(2));
    }

    #[test]
    fn block_comment_unwrap_ignores_surrounding_whitespace() {
        let mut buffer = rust("  /*x*/  ");
        select(&mut buffer, &[(0, 9)]);
        assert!(buffer.toggle_block_comment());
        assert_eq!(buffer.text(), "x");
    }

    #[test]
    fn a_tab_indented_file_measures_comment_alignment_in_columns() {
        let unit = IndentUnit {
            style: IndentStyle::Tabs,
            width: 4,
        };
        let mut buffer = rust("\ta\n\t\tb\n");
        select(&mut buffer, &[(0, 6)]);
        assert!(buffer.toggle_line_comment(unit));
        assert_eq!(buffer.text(), "\t// a\n\t// \tb\n");
    }
}
