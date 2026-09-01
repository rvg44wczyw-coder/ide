//! Pure logic converting a file's `DiffHunk`s (`ide_core::GitRepo::
//! diff_file`) into gutter marks and hunk-revert edits -- no `egui`, no
//! I/O (`docs/features/editor-git-gutter.md` §2.1).

use ide_core::text::{Change, TextBuffer};
use ide_core::{DiffHunk, DiffLine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GutterMarkKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GutterMark {
    /// 0-based buffer line this mark decorates -- for `Deleted`, the line
    /// immediately after the removed run (§2.1).
    pub line: usize,
    pub kind: GutterMarkKind,
}

/// Converts `hunks` (already ordered by `GitRepo::diff_file`) into gutter
/// marks -- see `docs/features/editor-git-gutter.md` §2.1 for the
/// per-segment algorithm this implements.
pub fn marks_from_hunks(hunks: &[DiffHunk]) -> Vec<GutterMark> {
    let mut marks = Vec::new();
    for hunk in hunks {
        let mut new_line = (hunk.new_start as usize).saturating_sub(1);
        let mut i = 0;
        while i < hunk.lines.len() {
            if matches!(hunk.lines[i], DiffLine::Context(_)) {
                new_line += 1;
                i += 1;
                continue;
            }
            let start = i;
            while i < hunk.lines.len() && !matches!(hunk.lines[i], DiffLine::Context(_)) {
                i += 1;
            }
            let segment = &hunk.lines[start..i];
            let removed_count = segment
                .iter()
                .filter(|l| matches!(l, DiffLine::Removed(..)))
                .count();
            let mut added_seen = 0usize;
            for line in segment {
                if matches!(line, DiffLine::Added(..)) {
                    let kind = if added_seen < removed_count {
                        GutterMarkKind::Modified
                    } else {
                        GutterMarkKind::Added
                    };
                    marks.push(GutterMark {
                        line: new_line,
                        kind,
                    });
                    new_line += 1;
                    added_seen += 1;
                }
            }
            if removed_count > added_seen {
                marks.push(GutterMark {
                    line: new_line,
                    kind: GutterMarkKind::Deleted,
                });
            }
        }
    }
    marks
}

/// The `Change` that undoes exactly the hunk containing `clicked_line` (a
/// buffer line, 0-based) against `buffer`'s current text -- `None` if no
/// hunk covers it (`docs/features/editor-git-gutter.md` §2.1).
pub fn revert_hunk_change(
    hunks: &[DiffHunk],
    clicked_line: usize,
    buffer: &TextBuffer,
) -> Option<Change> {
    for hunk in hunks {
        let start_line = (hunk.new_start as usize).saturating_sub(1);
        let affected = hunk
            .lines
            .iter()
            .filter(|l| !matches!(l, DiffLine::Removed(..)))
            .count();
        let end_line = start_line + affected;
        let matches_click = if affected == 0 {
            clicked_line == start_line
        } else {
            clicked_line >= start_line && clicked_line < end_line
        };
        if !matches_click {
            continue;
        }

        let mut replacement = String::new();
        for line in &hunk.lines {
            match line {
                DiffLine::Context(text) | DiffLine::Removed(text, _) => {
                    replacement.push_str(text);
                    replacement.push('\n');
                }
                DiffLine::Added(_, _) => {}
            }
        }

        let text = buffer.text();
        let start = buffer.lines().line_start(start_line)?;
        let end = buffer.lines().line_start(end_line).unwrap_or(text.len());
        return Some(Change::new(start..end, replacement));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(new_start: u32, lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            old_start: 1,
            new_start,
            lines,
        }
    }

    fn added(text: &str) -> DiffLine {
        DiffLine::Added(text.to_string(), Vec::new())
    }

    fn removed(text: &str) -> DiffLine {
        DiffLine::Removed(text.to_string(), Vec::new())
    }

    fn context(text: &str) -> DiffLine {
        DiffLine::Context(text.to_string())
    }

    #[test]
    fn a_single_line_replacement_is_modified() {
        let hunks = vec![hunk(5, vec![removed("old"), added("new")])];
        let marks = marks_from_hunks(&hunks);
        assert_eq!(
            marks,
            vec![GutterMark {
                line: 4,
                kind: GutterMarkKind::Modified
            }]
        );
    }

    #[test]
    fn pure_insertion_lines_are_added() {
        let hunks = vec![hunk(
            10,
            vec![context("before"), added("x"), added("y"), added("z")],
        )];
        let marks = marks_from_hunks(&hunks);
        assert_eq!(
            marks,
            vec![
                GutterMark {
                    line: 10,
                    kind: GutterMarkKind::Added
                },
                GutterMark {
                    line: 11,
                    kind: GutterMarkKind::Added
                },
                GutterMark {
                    line: 12,
                    kind: GutterMarkKind::Added
                },
            ]
        );
    }

    #[test]
    fn pure_deletion_with_nothing_added_gets_one_deleted_mark() {
        let hunks = vec![hunk(3, vec![removed("a"), removed("b")])];
        let marks = marks_from_hunks(&hunks);
        assert_eq!(
            marks,
            vec![GutterMark {
                line: 2,
                kind: GutterMarkKind::Deleted
            }]
        );
    }

    #[test]
    fn excess_removed_lines_beyond_the_added_run_get_one_deleted_mark() {
        // 3 removed, 1 added -- first added is Modified, then 2 leftover
        // removed lines with nothing to pair against.
        let hunks = vec![hunk(
            7,
            vec![removed("a"), removed("b"), removed("c"), added("x")],
        )];
        let marks = marks_from_hunks(&hunks);
        assert_eq!(
            marks,
            vec![
                GutterMark {
                    line: 6,
                    kind: GutterMarkKind::Modified
                },
                GutterMark {
                    line: 7,
                    kind: GutterMarkKind::Deleted
                },
            ]
        );
    }

    #[test]
    fn added_lines_beyond_the_removed_count_are_added_not_modified() {
        // 1 removed, 3 added -- first is Modified, the other two Added.
        let hunks = vec![hunk(
            1,
            vec![removed("old"), added("new1"), added("new2"), added("new3")],
        )];
        let marks = marks_from_hunks(&hunks);
        assert_eq!(
            marks,
            vec![
                GutterMark {
                    line: 0,
                    kind: GutterMarkKind::Modified
                },
                GutterMark {
                    line: 1,
                    kind: GutterMarkKind::Added
                },
                GutterMark {
                    line: 2,
                    kind: GutterMarkKind::Added
                },
            ]
        );
    }

    #[test]
    fn context_lines_produce_no_marks_and_advance_the_cursor() {
        let hunks = vec![hunk(
            1,
            vec![context("a"), context("b"), removed("c"), added("d")],
        )];
        let marks = marks_from_hunks(&hunks);
        assert_eq!(
            marks,
            vec![GutterMark {
                line: 2,
                kind: GutterMarkKind::Modified
            }]
        );
    }

    #[test]
    fn multiple_hunks_each_contribute_their_own_marks() {
        let hunks = vec![
            hunk(1, vec![added("x")]),
            hunk(10, vec![removed("y"), removed("z")]),
        ];
        let marks = marks_from_hunks(&hunks);
        assert_eq!(
            marks,
            vec![
                GutterMark {
                    line: 0,
                    kind: GutterMarkKind::Added
                },
                GutterMark {
                    line: 9,
                    kind: GutterMarkKind::Deleted
                },
            ]
        );
    }

    #[test]
    fn empty_hunks_yield_no_marks() {
        assert!(marks_from_hunks(&[]).is_empty());
    }

    fn buffer(text: &str) -> TextBuffer {
        TextBuffer::new(text, None)
    }

    #[test]
    fn revert_hunk_change_reconstructs_a_modified_line() {
        let buf = buffer("one\nTWO\nthree\n");
        let hunks = vec![hunk(2, vec![removed("two"), added("TWO")])];
        let change = revert_hunk_change(&hunks, 1, &buf).unwrap();
        assert_eq!(change.range, "one\n".len().."one\nTWO\n".len());
        assert_eq!(change.insert, "two\n");
    }

    #[test]
    fn revert_hunk_change_reconstructs_a_pure_deletion_at_its_marker_line() {
        let buf = buffer("one\nthree\n");
        let hunks = vec![hunk(2, vec![removed("two-a"), removed("two-b")])];
        // The Deleted mark for this hunk sits at line 1 (new_start - 1).
        let change = revert_hunk_change(&hunks, 1, &buf).unwrap();
        assert_eq!(change.range, "one\n".len().."one\n".len());
        assert_eq!(change.insert, "two-a\ntwo-b\n");
    }

    #[test]
    fn revert_hunk_change_reconstructs_a_pure_addition_by_deleting_it() {
        let buf = buffer("one\nx\ny\ntwo\n");
        let hunks = vec![hunk(2, vec![added("x"), added("y")])];
        let change = revert_hunk_change(&hunks, 1, &buf).unwrap();
        assert_eq!(change.range, "one\n".len().."one\nx\ny\n".len());
        assert_eq!(change.insert, "");
    }

    #[test]
    fn revert_hunk_change_at_end_of_file_falls_back_to_text_len() {
        let buf = buffer("one\nTWO");
        let hunks = vec![hunk(2, vec![removed("two"), added("TWO")])];
        let change = revert_hunk_change(&hunks, 1, &buf).unwrap();
        assert_eq!(change.range, "one\n".len()..buf.text().len());
        assert_eq!(change.insert, "two\n");
    }

    #[test]
    fn revert_hunk_change_with_no_covering_hunk_is_none() {
        let buf = buffer("one\ntwo\nthree\n");
        let hunks = vec![hunk(1, vec![added("x")])];
        assert!(revert_hunk_change(&hunks, 5, &buf).is_none());
    }
}
