//! How one indent level is spelled, and what `Enter` should insert to keep
//! a new line aligned (`docs/features/smart-editing.md` §2.3).
//!
//! Everything here is a pure function of text plus rules, so the whole
//! module is testable without a buffer, and A4b can swap where an
//! `IndentUnit` comes from without touching a line of it.

use std::borrow::Cow;

use crate::syntax::SyntaxRules;

use super::lines::LineIndex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndentStyle {
    Spaces,
    Tabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndentUnit {
    pub style: IndentStyle,
    /// Columns per level. For `Tabs` this is the *display* width, used only
    /// to measure existing indentation, never to emit spaces.
    pub width: usize,
}

impl Default for IndentUnit {
    fn default() -> Self {
        Self {
            style: IndentStyle::Spaces,
            width: 4,
        }
    }
}

impl IndentUnit {
    /// One level, as text. Not `&'static str`: the spaces case depends on
    /// the runtime `width`, so only the tab arm can borrow.
    pub fn one(&self) -> Cow<'static, str> {
        match self.style {
            IndentStyle::Tabs => Cow::Borrowed("\t"),
            IndentStyle::Spaces => Cow::Owned(" ".repeat(self.width)),
        }
    }

    /// The display column `indent` ends at, with tabs advancing to the next
    /// multiple of `width`. A zero `width` would make that step meaningless,
    /// so a tab then counts as one column rather than dividing by zero.
    pub fn columns_of(&self, indent: &str) -> usize {
        let stop = self.width.max(1);
        indent.chars().fold(0, |column, c| match c {
            '\t' => column + stop - column % stop,
            _ => column + 1,
        })
    }

    /// `columns` worth of indentation, spelled in this unit. Whole tabs plus
    /// the remainder in spaces for `Tabs`, which is what keeps a
    /// tab-indented file from growing half-tabs at a continuation line.
    pub fn render(&self, columns: usize) -> String {
        match self.style {
            IndentStyle::Spaces => " ".repeat(columns),
            IndentStyle::Tabs => {
                let stop = self.width.max(1);
                "\t".repeat(columns / stop) + &" ".repeat(columns % stop)
            }
        }
    }
}

/// The leading whitespace of `line`, as a subslice.
pub fn leading_whitespace(line: &str) -> &str {
    let end = line
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(line.len());
    &line[..end]
}

/// What `Enter` at `offset` should insert: `"\n"` plus the new line's
/// indentation (`smart-editing.md` §3.1).
///
/// The bracket scan is **local to the caret's line**, which is what the
/// `&str` + `LineIndex` signature implies and what keeps this working on a
/// buffer too large to tokenize (the doc's §4.6 degradation rule therefore
/// never has to fire here). The price is a bracket inside a string literal
/// or a block comment that *began on an earlier line*: the scan tracks only
/// the quotes and the line-comment prefixes it meets on this line, so such a
/// bracket is counted and the new line lands one level too deep.
pub fn newline_indent(
    text: &str,
    lines: &LineIndex,
    offset: usize,
    rules: Option<&SyntaxRules>,
    unit: IndentUnit,
) -> String {
    let offset = offset.min(text.len());
    let line = lines.line_at(offset);
    let Some(range) = lines.line_range(line, text) else {
        return "\n".to_string();
    };
    let line_text = &text[range.clone()];
    let mut columns = unit.columns_of(leading_whitespace(line_text));

    // An offset mid-character would panic the two slices below, and there is
    // no language answer to give for one: the line's own indentation is the
    // same fallback a buffer without rules gets.
    if let Some(rules) = rules.filter(|_| text.is_char_boundary(offset)) {
        let before = &text[range.start..offset.max(range.start)];
        let after = &text[offset.min(range.end)..range.end];
        let opens = opens_a_block(before, rules);
        if opens || ends_with_trigger(before, rules) {
            columns += unit.width;
        } else if starts_with_closer(after, rules) {
            columns = columns.saturating_sub(unit.width);
        }
    }
    format!("\n{}", unit.render(columns))
}

/// Whether `Enter` at `offset` sits between an opening bracket and its
/// closer, both on the caret's line -- the `{|}` case §3.1 expands into
/// three lines.
pub fn splits_a_pair(text: &str, offset: usize, rules: Option<&SyntaxRules>) -> bool {
    let Some(rules) = rules else {
        return false;
    };
    let offset = offset.min(text.len());
    if !text.is_char_boundary(offset) {
        return false;
    }
    let Some(before) = text[..offset].chars().next_back() else {
        return false;
    };
    let Some(after) = text[offset..].chars().next() else {
        return false;
    };
    rules.brackets.contains(&(before, after))
}

/// Net bracket depth of `before`, ignoring quoted spans and anything after a
/// line-comment prefix. A pure scan rather than a token lookup because the
/// caller has only the current line, and the line is short.
fn opens_a_block(before: &str, rules: &SyntaxRules) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (byte, c) in before.char_indices() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == open {
                quote = None;
            }
            continue;
        }
        if rules.string_quotes.contains(&c) {
            quote = Some(c);
            continue;
        }
        if rules
            .line_comment_prefixes
            .iter()
            .any(|p| before[byte..].starts_with(p))
        {
            break;
        }
        if rules.brackets.iter().any(|(open, _)| *open == c) {
            depth += 1;
        } else if rules.brackets.iter().any(|(_, close)| *close == c) {
            depth -= 1;
        }
    }
    depth > 0
}

fn ends_with_trigger(before: &str, rules: &SyntaxRules) -> bool {
    let trimmed = before.trim_end();
    rules
        .indent_line_suffixes
        .iter()
        .any(|suffix| !suffix.is_empty() && trimmed.ends_with(suffix))
}

fn starts_with_closer(after: &str, rules: &SyntaxRules) -> bool {
    after
        .trim_start()
        .chars()
        .next()
        .is_some_and(|c| rules.brackets.iter().any(|(_, close)| *close == c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{PYTHON, RUST};

    fn tabs() -> IndentUnit {
        IndentUnit {
            style: IndentStyle::Tabs,
            width: 4,
        }
    }

    fn newline_at(text: &str, offset: usize, rules: Option<&SyntaxRules>) -> String {
        newline_indent(
            text,
            &LineIndex::new(text),
            offset,
            rules,
            IndentUnit::default(),
        )
    }

    #[test]
    fn columns_of_measures_tabs_by_their_stop_not_their_byte() {
        let unit = tabs();
        assert_eq!(unit.columns_of(""), 0);
        assert_eq!(unit.columns_of("\t"), 4);
        assert_eq!(unit.columns_of("\t  "), 6);
        assert_eq!(unit.columns_of("  \t"), 4);
        assert_eq!(IndentUnit::default().columns_of("      "), 6);
    }

    #[test]
    fn render_spells_a_column_count_in_this_unit() {
        assert_eq!(tabs().render(6), "\t  ");
        assert_eq!(tabs().render(8), "\t\t");
        assert_eq!(IndentUnit::default().render(6), "      ");
        assert_eq!(tabs().render(0), "");
    }

    #[test]
    fn one_borrows_for_tabs_and_owns_for_spaces() {
        assert_eq!(tabs().one(), "\t");
        assert!(matches!(tabs().one(), Cow::Borrowed(_)));
        assert_eq!(IndentUnit::default().one(), "    ");
        assert!(matches!(IndentUnit::default().one(), Cow::Owned(_)));
    }

    #[test]
    fn a_zero_width_unit_does_not_divide_by_zero() {
        let unit = IndentUnit {
            style: IndentStyle::Tabs,
            width: 0,
        };
        assert_eq!(unit.columns_of("\t\t"), 2);
        assert_eq!(unit.render(2), "\t\t");
    }

    #[test]
    fn leading_whitespace_stops_at_the_first_real_character() {
        assert_eq!(leading_whitespace("  \tlet x"), "  \t");
        assert_eq!(leading_whitespace("let x"), "");
        assert_eq!(leading_whitespace("   "), "   ");
        assert_eq!(leading_whitespace(""), "");
    }

    #[test]
    fn newline_indent_copies_the_current_lines_indent() {
        let text = "    let x = 1;";
        assert_eq!(newline_at(text, text.len(), Some(&RUST)), "\n    ");
        // ...and with no rules at all, which is the untokenized fallback.
        assert_eq!(newline_at(text, text.len(), None), "\n    ");
    }

    #[test]
    fn newline_indent_adds_a_level_after_an_open_bracket() {
        let text = "fn main() {";
        assert_eq!(newline_at(text, text.len(), Some(&RUST)), "\n    ");
        // A pair opened and closed on the same line leaves nothing open.
        let balanced = "let v = vec![1];";
        assert_eq!(newline_at(balanced, balanced.len(), Some(&RUST)), "\n");
    }

    #[test]
    fn newline_indent_adds_a_level_after_a_python_colon() {
        let text = "def f():";
        assert_eq!(newline_at(text, text.len(), Some(&PYTHON)), "\n    ");
        // Rust has no `:` trigger, so the same line does not indent there.
        assert_eq!(newline_at(text, text.len(), Some(&RUST)), "\n");
    }

    #[test]
    fn a_bracket_and_a_trigger_together_add_only_one_level() {
        let text = "if d[k]: {";
        let with_both = newline_at(text, text.len(), Some(&PYTHON));
        assert_eq!(with_both, "\n    ");
    }

    #[test]
    fn newline_indent_ignores_a_bracket_inside_a_string_or_comment() {
        let quoted = r#"let s = "{";"#;
        assert_eq!(newline_at(quoted, quoted.len(), Some(&RUST)), "\n");
        let escaped = r#"let s = "\"{";"#;
        assert_eq!(newline_at(escaped, escaped.len(), Some(&RUST)), "\n");
        let commented = "let x = 1; // {";
        assert_eq!(newline_at(commented, commented.len(), Some(&RUST)), "\n");
    }

    #[test]
    fn newline_indent_dedents_before_a_dangling_closer() {
        let text = "    }";
        assert_eq!(newline_at(text, 4, Some(&RUST)), "\n");
        // Deeper in, the closer only removes one level.
        let deeper = "        }";
        assert_eq!(newline_at(deeper, 8, Some(&RUST)), "\n    ");
    }

    #[test]
    fn newline_indent_handles_degenerate_offsets() {
        assert_eq!(newline_at("", 0, Some(&RUST)), "\n");
        assert_eq!(newline_at("abc", 99, Some(&RUST)), "\n");
    }

    #[test]
    fn newline_indent_never_slices_a_character_in_half() {
        let text = "    \u{4F60}\u{597D} {";
        // Offset 5 is inside the first multi-byte character.
        assert_eq!(newline_at(text, 5, Some(&RUST)), "\n    ");
        // The same offset on a boundary still consults the rules.
        assert_eq!(newline_at(text, text.len(), Some(&RUST)), "\n        ");
    }

    #[test]
    fn splits_a_pair_sees_only_a_closer_directly_after_the_caret() {
        assert!(splits_a_pair("{}", 1, Some(&RUST)));
        assert!(splits_a_pair("f()", 2, Some(&RUST)));
        assert!(!splits_a_pair("{ }", 1, Some(&RUST)));
        assert!(!splits_a_pair("{}", 0, Some(&RUST)));
        assert!(!splits_a_pair("{}", 1, None));
        // Mismatched halves are not a pair.
        assert!(!splits_a_pair("{)", 1, Some(&RUST)));
        // Never panics off a boundary or past the end.
        assert!(!splits_a_pair("\u{4F60}", 1, Some(&RUST)));
        assert!(!splits_a_pair("{}", 99, Some(&RUST)));
    }
}
