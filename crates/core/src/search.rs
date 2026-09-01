//! Global "Find in Files" search (see
//! `docs/features/global-search-and-languages.md`). Walks an
//! already-`Project::scan_tree`-validated `DirEntry` tree — no path
//! construction/resolution of its own, so it inherits `scan_tree`'s
//! symlink-escape protection for free.

use crate::project::{DirEntry, DirEntryKind};
use std::fs;
use std::path::PathBuf;

/// One line containing a match of the search query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: PathBuf,
    /// 0-based.
    pub line: u32,
    /// 0-based **char** index (not byte) of the match's start within the
    /// line.
    pub column: u32,
    /// Byte offset of the match's start within the file's full text.
    pub byte_offset: usize,
    /// The full text of the matching line (no trailing line terminator).
    pub line_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResults {
    pub matches: Vec<SearchMatch>,
    /// `true` if `MAX_SEARCH_RESULTS` was reached and the walk stopped
    /// early.
    pub truncated: bool,
}

/// Caps total matches collected across the whole search — `search_tree`
/// stops walking as soon as this many matches are collected, rather than
/// collecting everything and truncating after (see module docs and
/// `docs/security-findings/rust-lsp-dev-find-usages-2026-08-16.md` for why
/// that distinction matters).
pub const MAX_SEARCH_RESULTS: usize = 1000;

/// Files larger than this are skipped without being read.
pub const MAX_SEARCHABLE_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Directory names never descended into, anywhere in the tree — the
/// heaviest/most irrelevant subtrees this app's supported ecosystems
/// produce (git internals, Rust build output, JS tooling). Not
/// user-configurable in v1; no `.gitignore` parsing. `pub(crate)` so
/// `fuzzy.rs`'s own tree walk can skip the same directories without
/// duplicating the list (`docs/features/search-everywhere.md` §2.1).
pub(crate) const SKIPPED_DIR_NAMES: [&str; 3] = [".git", "target", "node_modules"];

/// Case-insensitive plain-substring search over every file in `tree`,
/// depth-first in `tree`'s existing child order (dirs-then-files,
/// case-insensitive name order — `Project::scan_tree` already sorted it,
/// nothing to re-sort here). `query.trim().is_empty()` returns
/// `SearchResults { matches: vec![], truncated: false }` immediately
/// without walking anything.
pub fn search_tree(tree: &DirEntry, query: &str) -> SearchResults {
    let query = query.trim();
    if query.is_empty() {
        return SearchResults {
            matches: Vec::new(),
            truncated: false,
        };
    }
    let query_lower = query.to_lowercase();

    let mut matches = Vec::new();
    let mut truncated = false;
    walk(tree, &query_lower, &mut matches, &mut truncated);
    SearchResults { matches, truncated }
}

fn walk(entry: &DirEntry, query_lower: &str, matches: &mut Vec<SearchMatch>, truncated: &mut bool) {
    if *truncated {
        return;
    }
    match entry.kind {
        DirEntryKind::Dir => {
            if SKIPPED_DIR_NAMES.contains(&entry.name.as_str()) {
                return;
            }
            for child in &entry.children {
                walk(child, query_lower, matches, truncated);
                if *truncated {
                    return;
                }
            }
        }
        DirEntryKind::File => search_file(entry, query_lower, matches, truncated),
    }
}

fn search_file(
    entry: &DirEntry,
    query_lower: &str,
    matches: &mut Vec<SearchMatch>,
    truncated: &mut bool,
) {
    let Ok(metadata) = fs::metadata(&entry.path) else {
        return;
    };
    if metadata.len() > MAX_SEARCHABLE_FILE_BYTES {
        return;
    }
    // Same "skip on failure" convention `Buffer::open` already uses for
    // its own read — reused here as an accurate-enough "is this binary"
    // proxy rather than a separate byte-sniffing heuristic (invalid UTF-8
    // and permission errors both land here).
    let Ok(text) = fs::read_to_string(&entry.path) else {
        return;
    };

    let mut offset = 0usize;
    for (line_idx, raw_line) in text.split_inclusive('\n').enumerate() {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);

        if let Some((byte_offset_in_line, column)) = find_match_in_line(line, query_lower) {
            matches.push(SearchMatch {
                path: entry.path.clone(),
                line: line_idx as u32,
                column,
                byte_offset: offset + byte_offset_in_line,
                line_text: line.to_string(),
            });
            if matches.len() == MAX_SEARCH_RESULTS {
                *truncated = true;
                return;
            }
        }
        offset += raw_line.len();
    }
}

/// Locates the first case-insensitive occurrence of `query_lower` in
/// `line`, returning `(byte_offset, char_column)` relative to `line`
/// itself. Computed against `line`'s *original* text throughout — never
/// by lowercasing the whole line once and reusing an index found in that
/// copy, since `str::to_lowercase()` can change a string's length for
/// some Unicode input (e.g. `'İ'`, one codepoint, lowercases to `"i̇"`,
/// two), which would desync any such index from the original. Instead,
/// for each candidate start position in the original line, a bounded
/// probe starting there is lowercased and checked for a `starts_with`
/// match — the probe's *start* is always an original-line char boundary
/// we chose before lowering, so there's no index-translation step to get
/// wrong.
fn find_match_in_line(line: &str, query_lower: &str) -> Option<(usize, u32)> {
    if query_lower.is_empty() {
        return None;
    }
    let query_char_count = query_lower.chars().count();
    // Generous bound on how many original chars could possibly be needed
    // to produce `query_char_count` lowered chars — covers any
    // `to_lowercase()` expansion (the largest known case is 1 -> 2, e.g.
    // 'İ' -> "i̇") with headroom to spare.
    let probe_char_budget = query_char_count * 3 + 2;

    for (char_idx, (byte_idx, _)) in line.char_indices().enumerate() {
        let probe: String = line[byte_idx..].chars().take(probe_char_budget).collect();
        if probe.to_lowercase().starts_with(query_lower) {
            return Some((byte_idx, char_idx as u32));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;

    #[test]
    fn empty_query_returns_no_matches_without_walking() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "hello world").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        assert_eq!(
            search_tree(&tree, "   "),
            SearchResults {
                matches: Vec::new(),
                truncated: false
            }
        );
    }

    #[test]
    fn finds_a_case_insensitive_match_with_correct_position_and_text() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "line one\nline TODO here\n").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree(&tree, "todo");
        assert_eq!(results.matches.len(), 1);
        let m = &results.matches[0];
        assert_eq!(m.line, 1);
        assert_eq!(m.column, 5);
        assert_eq!(m.line_text, "line TODO here");
        // byte_offset must index into the *file's full text*, not just the line.
        let full_text = fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(&full_text[m.byte_offset..m.byte_offset + 4], "TODO");
    }

    #[test]
    fn matches_are_located_against_the_original_line_not_a_lowercased_copy() {
        // "İ" (U+0130, capital dotted I) lowercases to "i̇" (2 codepoints:
        // 'i' + a combining dot above) -- one char becomes two. A naive
        // implementation that lowercases the whole line once and reuses
        // the resulting char index against the ORIGINAL line would land
        // on "t" here instead of "s" -- off by one because the lowered
        // copy has one more char than the original up to that point.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "İstanbul").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree(&tree, "stanbul");

        assert_eq!(results.matches.len(), 1);
        let m = &results.matches[0];
        assert_eq!(m.column, 1);
        assert_eq!(m.byte_offset, 2);
        assert_eq!(&"İstanbul"[m.byte_offset..], "stanbul");
    }

    #[test]
    fn at_most_one_match_reported_per_line() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "foo foo foo").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree(&tree, "foo");
        assert_eq!(results.matches.len(), 1);
        assert_eq!(results.matches[0].byte_offset, 0);
    }

    #[test]
    fn stops_and_marks_truncated_at_the_result_cap() {
        let dir = tempfile::tempdir().unwrap();
        // MAX_SEARCH_RESULTS + a handful more one-match-per-file files.
        for i in 0..MAX_SEARCH_RESULTS + 50 {
            fs::write(dir.path().join(format!("f{i:05}.txt")), "needle").unwrap();
        }
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree(&tree, "needle");
        assert_eq!(results.matches.len(), MAX_SEARCH_RESULTS);
        assert!(results.truncated);
    }

    #[test]
    fn skips_git_target_and_node_modules_directories() {
        let dir = tempfile::tempdir().unwrap();
        for skipped in [".git", "target", "node_modules"] {
            fs::create_dir(dir.path().join(skipped)).unwrap();
            fs::write(dir.path().join(skipped).join("f.txt"), "needle").unwrap();
        }
        fs::write(dir.path().join("real.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree(&tree, "needle");
        assert_eq!(results.matches.len(), 1);
        assert_eq!(
            results.matches[0].path,
            fs::canonicalize(dir.path()).unwrap().join("real.txt")
        );
    }

    #[test]
    fn skips_files_larger_than_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let huge = "x".repeat(MAX_SEARCHABLE_FILE_BYTES as usize + 1) + "needle";
        fs::write(dir.path().join("huge.txt"), huge).unwrap();
        fs::write(dir.path().join("small.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree(&tree, "needle");
        assert_eq!(results.matches.len(), 1);
        assert_eq!(
            results.matches[0].path,
            fs::canonicalize(dir.path()).unwrap().join("small.txt")
        );
    }

    #[test]
    fn skips_files_that_are_not_valid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("binary.dat"), [0xff, 0xfe, 0x00, 0xff]).unwrap();
        fs::write(dir.path().join("text.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree(&tree, "needle");
        assert_eq!(results.matches.len(), 1);
        assert_eq!(
            results.matches[0].path,
            fs::canonicalize(dir.path()).unwrap().join("text.txt")
        );
    }

    #[test]
    fn no_match_returns_empty_matches_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "nothing interesting here").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree(&tree, "absent");
        assert_eq!(
            results,
            SearchResults {
                matches: Vec::new(),
                truncated: false
            }
        );
    }
}
