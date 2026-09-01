//! Fold-range detection (`docs/features/code-folding.md` §2.1, §3.1–§3.3).
//!
//! Three independent, purely additive sources -- brace nesting, indentation,
//! and `// region`/`// endregion` markers -- each degrading to fewer
//! detected folds on malformed or mid-edit input rather than panicking or
//! mismatching, the same principle `brackets.rs`'s scan already follows.

use crate::syntax::{SyntaxRules, MAX_HIGHLIGHTED_FILE_BYTES};

use super::TextBuffer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldKind {
    Brace,
    Indent,
    Region,
}

/// A closed line range: `start_line` is the line that stays visible when
/// collapsed (it renders the placeholder), `end_line` is the last line
/// hidden. Always `end_line > start_line` -- nothing shorter than two lines
/// is foldable, there being nothing to hide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldRange {
    pub start_line: usize,
    pub end_line: usize,
    pub kind: FoldKind,
}

impl TextBuffer {
    /// Every foldable region in the buffer, sorted by `start_line`
    /// ascending. Ranges from different sources may nest or overlap; both
    /// are expected, not errors. Empty when there is no syntax (nothing to
    /// detect structure from) or the buffer is larger than
    /// `MAX_HIGHLIGHTED_FILE_BYTES` -- the same cap
    /// `TextBuffer::matching_bracket` already applies, and for the same
    /// reason: past it there are no tokens to drive brace detection, and a
    /// partial answer would be more confusing than no answer.
    pub fn fold_ranges(&self) -> Vec<FoldRange> {
        let Some(rules) = self.syntax() else {
            return Vec::new();
        };
        if self.len() > MAX_HIGHLIGHTED_FILE_BYTES {
            return Vec::new();
        }

        let mut ranges = self.brace_fold_ranges(rules);
        ranges.extend(self.indent_fold_ranges(rules));
        ranges.extend(self.region_fold_ranges(rules));
        ranges.sort_by_key(|r| r.start_line);
        ranges
    }

    /// §3.1. A stack-based scan over the raw text, left to right, skipping
    /// any character `is_quoted_or_commented` reports as inside a string or
    /// comment -- the same pattern `matching_bracket`/`enclosing_bracket_pair`
    /// already use. **Not** a scan over `tokens()` filtered to
    /// `Punctuation`: `SyntaxRules::punctuation` and `SyntaxRules::brackets`
    /// are independently maintained and not guaranteed consistent (e.g.
    /// `MAKEFILE`/`DOCKERFILE` declare `(`/`)`/`{`/`}` as brackets but omit
    /// them from `punctuation`, and `MARKDOWN` declares `[`/`]`/`(`/`)` with
    /// an empty `punctuation` table entirely) -- a token-only scan would
    /// silently never find a fold for those languages' brackets at all.
    fn brace_fold_ranges(&self, rules: &SyntaxRules) -> Vec<FoldRange> {
        if rules.brackets.is_empty() {
            return Vec::new();
        }
        let text = self.text();
        let mut stack: Vec<(char, usize)> = Vec::new();
        let mut ranges = Vec::new();

        for (at, c) in text.char_indices() {
            let opener = rules.brackets.iter().find(|(open, _)| *open == c);
            let is_closer = rules.brackets.iter().any(|(_, close)| *close == c);
            if opener.is_none() && !is_closer {
                continue;
            }
            if self.is_quoted_or_commented(at) {
                continue;
            }
            if let Some(&(_, close)) = opener {
                let start_line = self.lines().line_at(at);
                stack.push((close, start_line));
                continue;
            }
            let Some(&(expected, start_line)) = stack.last() else {
                continue;
            };
            if expected != c {
                // A closer that doesn't match the stack's top is left
                // alone: the stack is not popped or corrected, so
                // malformed/mid-edit text degrades to fewer detected
                // folds rather than a wrong pairing.
                continue;
            }
            stack.pop();
            let end_line = self.lines().line_at(at);
            if start_line < end_line {
                ranges.push(FoldRange {
                    start_line,
                    end_line,
                    kind: FoldKind::Brace,
                });
            }
        }
        ranges
    }

    /// §3.2. Only runs for a language with a non-empty
    /// `SyntaxRules::indent_line_suffixes`. Reuses `text/indent.rs`'s exact
    /// `trim_end().ends_with(suffix)` check rather than calling its private
    /// `ends_with_trigger` helper, so folding inherits auto-indent's one
    /// known limitation (a trailing line comment defeating the match)
    /// rather than diverging from it.
    fn indent_fold_ranges(&self, rules: &SyntaxRules) -> Vec<FoldRange> {
        if rules.indent_line_suffixes.is_empty() {
            return Vec::new();
        }
        let text = self.text();
        let line_count = self.lines().line_count();
        let mut ranges = Vec::new();

        for line in 0..line_count {
            let Some(range) = self.lines().line_range(line, text) else {
                continue;
            };
            let content = &text[range];
            let trimmed = content.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            if !rules
                .indent_line_suffixes
                .iter()
                .any(|suffix| !suffix.is_empty() && trimmed.ends_with(suffix))
            {
                continue;
            }

            let trigger_indent = leading_whitespace_len(content);
            let mut last_deep = None;
            let mut boundary = line_count;
            for next in (line + 1)..line_count {
                let Some(next_range) = self.lines().line_range(next, text) else {
                    break;
                };
                let next_content = &text[next_range];
                if next_content.trim().is_empty() {
                    continue;
                }
                if leading_whitespace_len(next_content) <= trigger_indent {
                    boundary = next;
                    break;
                }
                last_deep = Some(next);
            }

            // An empty block (no line after the trigger was ever
            // deeper-indented, including the trigger being the file's last
            // line) emits nothing. Otherwise `end_line` runs up to the line
            // right before the boundary, so trailing blank lines between
            // the last deep line and the dedent are folded away too rather
            // than left dangling below the placeholder.
            if last_deep.is_some() {
                ranges.push(FoldRange {
                    start_line: line,
                    end_line: boundary - 1,
                    kind: FoldKind::Indent,
                });
            }
        }
        ranges
    }

    /// §3.3. Runs when `SyntaxRules::line_comment_prefixes` is non-empty.
    /// Stripping the comment prefix first and trimming the remainder before
    /// comparing is what makes `// region Name`, `//region Name`, and
    /// `# region Name` all recognized uniformly without a per-language
    /// spelling table.
    fn region_fold_ranges(&self, rules: &SyntaxRules) -> Vec<FoldRange> {
        if rules.line_comment_prefixes.is_empty() {
            return Vec::new();
        }
        let text = self.text();
        let line_count = self.lines().line_count();
        let mut stack: Vec<usize> = Vec::new();
        let mut ranges = Vec::new();

        for line in 0..line_count {
            let Some(range) = self.lines().line_range(line, text) else {
                continue;
            };
            let content = &text[range];
            let trimmed_leading = content.trim_start();
            let Some(prefix) = rules
                .line_comment_prefixes
                .iter()
                .find(|prefix| trimmed_leading.starts_with(**prefix))
            else {
                continue;
            };
            let remainder = trimmed_leading[prefix.len()..].trim_start();
            let lower = remainder.to_ascii_lowercase();

            if lower.trim_end() == "endregion" {
                if let Some(start_line) = stack.pop() {
                    ranges.push(FoldRange {
                        start_line,
                        end_line: line,
                        kind: FoldKind::Region,
                    });
                }
                // An `endregion` with nothing to pop is discarded silently
                // -- same "malformed input degrades to fewer folds, never a
                // panic or a wrong pairing" principle as §3.1.
            } else if lower == "region" || lower.starts_with("region ") {
                stack.push(line);
            }
        }
        // Anything still on the stack at end-of-file is discarded silently.
        ranges
    }
}

/// Raw byte count of `line`'s leading whitespace, not a tab-width-resolved
/// column -- a deliberate simplification consistent with this project's "no
/// premature generality" convention.
fn leading_whitespace_len(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{TokenKind, PYTHON, RUST, YAML};

    fn rust(text: &str) -> TextBuffer {
        TextBuffer::new(text, Some(&RUST))
    }

    fn python(text: &str) -> TextBuffer {
        TextBuffer::new(text, Some(&PYTHON))
    }

    fn yaml(text: &str) -> TextBuffer {
        TextBuffer::new(text, Some(&YAML))
    }

    #[test]
    fn a_brace_pair_spanning_multiple_lines_folds() {
        let buffer = rust("fn f() {\n    g();\n}\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 2,
                kind: FoldKind::Brace,
            }]
        );
    }

    #[test]
    fn a_brace_pair_on_one_line_does_not_fold() {
        let buffer = rust("fn f() { g(); }\n");
        assert_eq!(buffer.fold_ranges(), vec![]);
    }

    #[test]
    fn nested_braces_each_produce_their_own_range() {
        let buffer = rust("fn f() {\n    if x {\n        g();\n    }\n}\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![
                FoldRange {
                    start_line: 0,
                    end_line: 4,
                    kind: FoldKind::Brace,
                },
                FoldRange {
                    start_line: 1,
                    end_line: 3,
                    kind: FoldKind::Brace,
                },
            ]
        );
    }

    #[test]
    fn a_bracket_inside_a_string_or_comment_is_not_a_fold_boundary() {
        let buffer = rust("fn f() {\n    let s = \"{}\";\n    // }\n}\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 3,
                kind: FoldKind::Brace,
            }]
        );
    }

    #[test]
    fn an_unmatched_closer_is_left_alone_rather_than_mispairing() {
        // The stray `}` on line 1 does not match the stack (which is
        // empty), so it is ignored; the real pair on lines 0/2 still folds.
        let buffer = rust("fn f() {\n}\n    g();\n}\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 1,
                kind: FoldKind::Brace,
            }]
        );
    }

    #[test]
    fn an_unclosed_bracket_at_eof_emits_nothing() {
        let buffer = rust("fn f() {\n    g();\n");
        assert_eq!(buffer.fold_ranges(), vec![]);
    }

    #[test]
    fn python_indent_block_folds_to_the_last_deep_line() {
        let buffer = python("def f():\n    a()\n    b()\nc()\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 2,
                kind: FoldKind::Indent,
            }]
        );
    }

    #[test]
    fn python_indent_block_swallows_trailing_blank_lines() {
        let buffer = python("def f():\n    a()\n    b()\n\n\nc()\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 4,
                kind: FoldKind::Indent,
            }]
        );
    }

    #[test]
    fn python_indent_block_running_to_eof_folds_to_the_last_line() {
        // No trailing newline, so `line_count()` is exactly 3 -- this
        // isolates "scan reaches EOF with no dedent boundary" from the
        // separate, harmless case of a trailing-newline file's phantom
        // final empty line also getting swallowed into the fold.
        let buffer = python("def f():\n    a()\n    b()");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 2,
                kind: FoldKind::Indent,
            }]
        );
    }

    #[test]
    fn an_indent_block_running_to_eof_also_swallows_the_trailing_newlines_phantom_line() {
        let buffer = python("def f():\n    a()\n    b()\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 3,
                kind: FoldKind::Indent,
            }]
        );
    }

    #[test]
    fn an_empty_indent_block_emits_nothing() {
        let buffer = python("def f():\nc()\n");
        assert_eq!(buffer.fold_ranges(), vec![]);
    }

    #[test]
    fn a_trigger_line_at_eof_emits_nothing() {
        let buffer = python("def f():\n    a()\ndef g():\n");
        // `def g():` is the file's last line -- no line after it, so it
        // gets no fold even though it ends with `:`.
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 1,
                kind: FoldKind::Indent,
            }]
        );
    }

    #[test]
    fn a_blank_line_inside_an_indent_block_does_not_end_it_early() {
        let buffer = python("def f():\n    a()\n\n    b()\nc()\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 3,
                kind: FoldKind::Indent,
            }]
        );
    }

    #[test]
    fn yaml_indent_blocks_use_the_same_colon_trigger() {
        let buffer = yaml("handlers:\n  a: 1\n  b: 2\nother: 3\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 2,
                kind: FoldKind::Indent,
            }]
        );
    }

    #[test]
    fn a_language_with_no_indent_suffixes_never_produces_indent_folds() {
        let buffer = rust("fn f() {\n    g();\n}\n");
        assert!(buffer
            .fold_ranges()
            .iter()
            .all(|r| r.kind != FoldKind::Indent));
    }

    #[test]
    fn region_markers_fold_regardless_of_braces() {
        let buffer = python("# region Handlers\ndef f():\n    pass\n# endregion\n");
        let ranges = buffer.fold_ranges();
        assert!(ranges.contains(&FoldRange {
            start_line: 0,
            end_line: 3,
            kind: FoldKind::Region,
        }));
    }

    #[test]
    fn region_markers_recognize_varied_spelling_and_spacing() {
        let buffer = rust("//region Handlers\nfn f() {}\n//endregion\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 2,
                kind: FoldKind::Region,
            }]
        );
    }

    #[test]
    fn nested_regions_pair_innermost_first_but_sort_by_start_line() {
        let buffer =
            rust("// region Outer\n// region Inner\nfn f() {}\n// endregion\n// endregion\n");
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![
                FoldRange {
                    start_line: 0,
                    end_line: 4,
                    kind: FoldKind::Region,
                },
                FoldRange {
                    start_line: 1,
                    end_line: 3,
                    kind: FoldKind::Region,
                },
            ]
        );
    }

    #[test]
    fn an_unmatched_endregion_and_an_unclosed_region_are_both_discarded() {
        let buffer = rust("// endregion\n// region Dangling\nfn f() {}\n");
        assert!(buffer
            .fold_ranges()
            .iter()
            .all(|r| r.kind != FoldKind::Region));
    }

    #[test]
    fn ranges_from_different_sources_are_sorted_by_start_line() {
        let buffer = rust("// region A\nfn f() {\n    g();\n}\n// endregion\n");
        let ranges = buffer.fold_ranges();
        let starts: Vec<usize> = ranges.iter().map(|r| r.start_line).collect();
        let mut sorted = starts.clone();
        sorted.sort();
        assert_eq!(starts, sorted);
    }

    #[test]
    fn a_buffer_with_no_syntax_never_folds() {
        assert_eq!(
            TextBuffer::new("fn f() {\n    g();\n}\n", None).fold_ranges(),
            vec![]
        );
    }

    #[test]
    fn a_buffer_larger_than_the_highlight_cap_never_folds() {
        let padding = "x".repeat(MAX_HIGHLIGHTED_FILE_BYTES + 1);
        let buffer = rust(&format!("fn f() {{\n    g();\n}}\n{padding}"));
        assert!(buffer.tokens().is_empty());
        assert_eq!(buffer.fold_ranges(), vec![]);
    }

    #[test]
    fn brace_folds_do_not_depend_on_the_punctuation_token_table() {
        // Markdown's `punctuation` list is empty even though `brackets`
        // declares `[`/`]`/`(`/`)` (the same pair `matching_bracket`'s own
        // test proves works for Markdown) -- a scan limited to
        // `tokens()`-classified `Punctuation` would silently never find
        // this fold.
        use crate::syntax::MARKDOWN;
        let buffer = TextBuffer::new("[a](\nb)\n", Some(&MARKDOWN));
        assert!(buffer
            .tokens()
            .iter()
            .all(|t| t.kind != TokenKind::Punctuation));
        let ranges = buffer.fold_ranges();
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 1,
                kind: FoldKind::Brace,
            }]
        );
    }

    #[test]
    fn makefile_and_dockerfile_brace_folds_also_do_not_depend_on_punctuation() {
        use crate::syntax::{DOCKERFILE, MAKEFILE};
        let makefile = TextBuffer::new("VAR = (\n    a\n)\n", Some(&MAKEFILE));
        assert_eq!(
            makefile.fold_ranges(),
            vec![FoldRange {
                start_line: 0,
                end_line: 2,
                kind: FoldKind::Brace,
            }]
        );

        let dockerfile = TextBuffer::new("RUN echo {\n    a\n}\n", Some(&DOCKERFILE));
        assert_eq!(
            dockerfile.fold_ranges(),
            vec![FoldRange {
                start_line: 0,
                end_line: 2,
                kind: FoldKind::Brace,
            }]
        );
    }
}
