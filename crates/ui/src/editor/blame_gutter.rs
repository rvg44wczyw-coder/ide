//! Pure logic converting a file's blame (`ide_core::GitRepo::blame_file`)
//! into per-line annotations for the blame gutter lane -- no `egui`, no
//! I/O, the same "pure conversion" contract `git_gutter.rs` already keeps
//! (`docs/features/git-branches-and-blame.md` §2.2.3).

use ide_core::BlameLine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameAnnotation {
    /// 0-based buffer line -- the first line of a run of consecutive
    /// lines all attributed to the same commit.
    pub line: usize,
    /// How many consecutive lines from `line` this one annotation covers
    /// (only `line` itself gets the rendered label; the rest of the run
    /// renders blank -- matches real JetBrains/VS Code blame-gutter
    /// behavior, where a label repeated on every line would be noise).
    pub run_len: usize,
    pub commit_id: String,
    pub short_id: String,
    pub author: String,
    pub timestamp: i64,
    pub summary: String,
}

/// Collapses `lines` (already ordered by buffer line, one entry per line --
/// `GitRepo::blame_file`'s own return shape) into runs of buffer-line-
/// contiguous lines attributed to the same commit. `author`/`summary` are
/// run through `strip_bidi_controls` here -- the one place every blame
/// annotation is built from raw `GitRepo` output, so a repository's own
/// (untrusted, possibly attacker-crafted) commit metadata can never carry
/// an unterminated Unicode bidi override into the gutter label
/// (`docs/security-findings/git-branches-and-blame-ui-2026-09-01.md`,
/// finding 1).
pub fn annotations_from_blame(lines: &[BlameLine]) -> Vec<BlameAnnotation> {
    let mut annotations: Vec<BlameAnnotation> = Vec::new();
    for line in lines {
        if let Some(last) = annotations.last_mut() {
            if last.commit_id == line.commit_id && last.line + last.run_len == line.line {
                last.run_len += 1;
                continue;
            }
        }
        annotations.push(BlameAnnotation {
            line: line.line,
            run_len: 1,
            commit_id: line.commit_id.clone(),
            short_id: line.short_id.clone(),
            author: strip_bidi_controls(&line.author),
            timestamp: line.timestamp,
            summary: strip_bidi_controls(&line.summary),
        });
    }
    annotations
}

/// Strips Unicode bidi control characters -- the "Trojan Source"/
/// CVE-2021-42574 character classes, embedding/override/isolate formats
/// plus the implicit directional marks -- from `s`. Repository content
/// (commit author names, emails, summaries, bodies) is untrusted
/// (`crates/core/src/git/**`'s own `CLAUDE.md` entry); an unterminated
/// override left in place would make everything painted after it in the
/// same text run render in an attacker-chosen visual order, spoofing what
/// the label actually says (`docs/security-findings/
/// git-branches-and-blame-ui-2026-09-01.md`, finding 1). Every call site
/// that paints repository-sourced text into the UI must run it through
/// this first.
pub fn strip_bidi_controls(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(
                *c,
                '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
            )
        })
        .collect()
}

/// Truncates `s` to at most `max_chars` characters (char-boundary-safe --
/// never a byte-boundary panic on multi-byte UTF-8), appending `'…'` when
/// truncation actually happened. Shared by the blame gutter label
/// (`editor/mod.rs`'s `paint_blame_label`) and the commit-details popup so
/// an untrusted repository's arbitrarily long commit message (git has no
/// length cap) never reaches `egui`'s text layout unbounded
/// (`docs/security-findings/git-branches-and-blame-ui-2026-09-01.md`,
/// finding 2).
pub fn truncate_display(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// A coarse `"Nm/h/d/y ago"` label for the gap between `timestamp` and
/// `now` (both Unix seconds) -- self-contained rather than pulling in a
/// date/time-formatting dependency (`CLAUDE.md`'s Dependencies table has
/// none approved) for what's otherwise a single gutter-label string.
/// `now` is a parameter rather than read internally via
/// `SystemTime::now()` so this stays a pure, directly testable function;
/// the one call site (`editor/mod.rs`'s gutter paint code) supplies the
/// real clock.
pub fn relative_time(timestamp: i64, now: i64) -> String {
    let delta = (now - timestamp).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const YEAR: i64 = 365 * DAY;
    if delta < MINUTE {
        "just now".to_string()
    } else if delta < HOUR {
        format!("{}m ago", delta / MINUTE)
    } else if delta < DAY {
        format!("{}h ago", delta / HOUR)
    } else if delta < YEAR {
        format!("{}d ago", delta / DAY)
    } else {
        format!("{}y ago", delta / YEAR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(line: usize, commit_id: &str) -> BlameLine {
        BlameLine {
            line,
            commit_id: commit_id.to_string(),
            short_id: commit_id[..commit_id.len().min(7)].to_string(),
            author: "Test".to_string(),
            timestamp: 0,
            summary: format!("commit {commit_id}"),
        }
    }

    #[test]
    fn a_whole_file_from_one_commit_is_a_single_run() {
        let lines = vec![line(0, "aaa"), line(1, "aaa"), line(2, "aaa")];
        let annotations = annotations_from_blame(&lines);
        assert_eq!(
            annotations,
            vec![BlameAnnotation {
                line: 0,
                run_len: 3,
                commit_id: "aaa".to_string(),
                short_id: "aaa".to_string(),
                author: "Test".to_string(),
                timestamp: 0,
                summary: "commit aaa".to_string(),
            }]
        );
    }

    #[test]
    fn consecutive_lines_from_different_commits_split_into_separate_runs() {
        let lines = vec![line(0, "aaa"), line(1, "aaa"), line(2, "bbb")];
        let annotations = annotations_from_blame(&lines);
        assert_eq!(annotations.len(), 2);
        assert_eq!(annotations[0].line, 0);
        assert_eq!(annotations[0].run_len, 2);
        assert_eq!(annotations[1].line, 2);
        assert_eq!(annotations[1].run_len, 1);
    }

    #[test]
    fn same_commit_id_on_non_adjacent_lines_stays_two_runs() {
        // A rare but possible shape (a moved/reverted line landing back on
        // the same commit elsewhere in the file) -- adjacency in the
        // buffer, not just a matching commit id, is what defines a run.
        let mut lines = vec![line(0, "aaa"), line(1, "bbb")];
        lines.push(BlameLine {
            line: 2,
            ..line(0, "aaa")
        });
        let annotations = annotations_from_blame(&lines);
        assert_eq!(annotations.len(), 3);
        assert_eq!(annotations[0].commit_id, "aaa");
        assert_eq!(annotations[2].commit_id, "aaa");
    }

    #[test]
    fn empty_input_yields_no_annotations() {
        assert!(annotations_from_blame(&[]).is_empty());
    }

    #[test]
    fn relative_time_buckets_by_magnitude() {
        const MINUTE: i64 = 60;
        const HOUR: i64 = 60 * MINUTE;
        const DAY: i64 = 24 * HOUR;
        const YEAR: i64 = 365 * DAY;
        let now = 10 * YEAR;
        assert_eq!(relative_time(now - 30, now), "just now");
        assert_eq!(relative_time(now - 5 * MINUTE, now), "5m ago");
        assert_eq!(relative_time(now - 3 * HOUR, now), "3h ago");
        assert_eq!(relative_time(now - 2 * DAY, now), "2d ago");
        assert_eq!(relative_time(now - 2 * YEAR, now), "2y ago");
    }

    #[test]
    fn relative_time_clamps_a_future_timestamp_to_just_now() {
        assert_eq!(relative_time(100, 0), "just now");
    }

    #[test]
    fn strip_bidi_controls_removes_an_unterminated_rtl_override() {
        let evil = "trusted-dev\u{202E} .exe.gnp.suoicilam";
        let cleaned = strip_bidi_controls(evil);
        assert!(!cleaned.contains('\u{202E}'));
        assert_eq!(cleaned, "trusted-dev .exe.gnp.suoicilam");
    }

    #[test]
    fn strip_bidi_controls_removes_every_covered_class_leaves_plain_text_alone() {
        let mixed = "a\u{202A}b\u{202B}c\u{202C}d\u{202D}e\u{202E}f\u{2066}g\u{2067}h\u{2068}i\u{2069}j\u{200E}k\u{200F}l\u{061C}m";
        assert_eq!(strip_bidi_controls(mixed), "abcdefghijklm");
        assert_eq!(
            strip_bidi_controls("plain ascii, no controls"),
            "plain ascii, no controls"
        );
    }

    #[test]
    fn annotations_from_blame_strips_bidi_controls_from_author_and_summary() {
        let mut evil_line = line(0, "aaa");
        evil_line.author = "A\u{202E}B".to_string();
        evil_line.summary = "S\u{202E}T".to_string();
        let annotations = annotations_from_blame(&[evil_line]);
        assert_eq!(annotations[0].author, "AB");
        assert_eq!(annotations[0].summary, "ST");
    }

    #[test]
    fn truncate_display_leaves_short_text_untouched() {
        assert_eq!(truncate_display("hello", 10), "hello");
        assert_eq!(truncate_display("hello", 5), "hello");
    }

    #[test]
    fn truncate_display_truncates_and_appends_an_ellipsis() {
        assert_eq!(truncate_display("hello world", 6), "hello\u{2026}");
    }

    #[test]
    fn truncate_display_is_char_boundary_safe_on_multi_byte_text() {
        let text =
            "\u{1D518}\u{1D52B}\u{1D526}\u{1D520}\u{1D52C}\u{1D521}\u{1D522} rest of the string";
        let truncated = truncate_display(text, 5);
        assert_eq!(truncated.chars().count(), 5);
        assert!(truncated.ends_with('\u{2026}'));
    }
}
