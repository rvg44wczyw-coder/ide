//! Fuzzy matching for Search Everywhere (see
//! `docs/features/search-everywhere.md` §2.1). `fuzzy_match_files` walks an
//! already-`Project::scan_tree`-validated `DirEntry` tree — no path
//! construction/resolution of its own, same "inherits `scan_tree`'s
//! symlink-escape protection for free" discipline `search.rs` already
//! established.

use crate::project::{DirEntry, DirEntryKind};
use crate::search::SKIPPED_DIR_NAMES;
use std::path::PathBuf;

const SCORE_CONSECUTIVE: i64 = 15;
const SCORE_BOUNDARY: i64 = 10;
const SCORE_CASE: i64 = 1;
const PENALTY_GAP: i64 = -1;

/// One fuzzy match's score and the matched character positions, for
/// highlighting. Higher `score` is a better match. `indices` are byte
/// offsets into the *original* (not lowercased) candidate string, one per
/// matched pattern character, in ascending order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i64,
    pub indices: Vec<usize>,
}

fn chars_eq_ignore_case(a: char, b: char) -> bool {
    a.to_lowercase().eq(b.to_lowercase())
}

/// Case-insensitive subsequence match: every character of `pattern`, in
/// order, must appear somewhere in `candidate` (not necessarily
/// contiguous). Returns `None` if `pattern` is not a subsequence of
/// `candidate`. An empty `pattern` always matches with
/// `FuzzyMatch { score: 0, indices: vec![] }`.
///
/// Deliberately a single left-to-right greedy pass — each pattern
/// character takes the *nearest* remaining occurrence in `candidate` — not
/// a dynamic-programming search over every possible subsequence
/// assignment. See `docs/features/search-everywhere.md` §2.1 for the
/// accepted-tradeoff rationale.
pub fn fuzzy_score(pattern: &str, candidate: &str) -> Option<FuzzyMatch> {
    if pattern.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            indices: Vec::new(),
        });
    }

    let cand_chars: Vec<(usize, char)> = candidate.char_indices().collect();
    let mut indices = Vec::new();
    let mut score: i64 = 0;
    let mut search_from = 0usize;
    let mut prev_match_idx: Option<usize> = None;

    for p_ch in pattern.chars() {
        let found = cand_chars[search_from..]
            .iter()
            .position(|&(_, c)| chars_eq_ignore_case(c, p_ch))
            .map(|rel| rel + search_from)?;

        let (byte_idx, matched_char) = cand_chars[found];
        indices.push(byte_idx);

        let gap = match prev_match_idx {
            Some(prev) => found - prev - 1,
            None => found,
        };
        if gap == 0 && prev_match_idx.is_some() {
            score += SCORE_CONSECUTIVE;
        } else {
            score += PENALTY_GAP * gap as i64;
        }

        let is_boundary = if found == 0 {
            true
        } else {
            let (_, prev_c) = cand_chars[found - 1];
            let is_separator = matches!(prev_c, '/' | '_' | '-' | '.' | ' ');
            let is_camel =
                (prev_c.is_lowercase() || prev_c.is_ascii_digit()) && matched_char.is_uppercase();
            is_separator || is_camel
        };
        if is_boundary {
            score += SCORE_BOUNDARY;
        }

        if matched_char == p_ch {
            score += SCORE_CASE;
        }

        prev_match_idx = Some(found);
        search_from = found + 1;
    }

    Some(FuzzyMatch { score, indices })
}

/// One file whose path fuzzy-matched a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyFileMatch {
    pub path: PathBuf,
    /// `path` relative to the scanned tree's root, `/`-joined regardless
    /// of platform — what `fuzzy_score` was actually run against, and what
    /// `indices` indexes into.
    pub relative: String,
    pub score: i64,
    pub indices: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyFileResults {
    pub matches: Vec<FuzzyFileMatch>,
    /// `true` if more than `MAX_FUZZY_FILE_RESULTS` files scored a match —
    /// the list was sorted then truncated, not stopped early (ranking
    /// requires scoring every candidate before any of them can be
    /// dropped, unlike `search_tree`'s early stop).
    pub truncated: bool,
}

/// Cap on `FuzzyFileResults::matches`' length after sorting.
pub const MAX_FUZZY_FILE_RESULTS: usize = 200;

/// Fuzzy-matches every **file** (not directory) in `tree` against `query`
/// by its path relative to `tree`'s own root, using `fuzzy_score`.
/// `query.trim().is_empty()` returns
/// `FuzzyFileResults { matches: vec![], truncated: false }` immediately
/// without walking anything. Skips the same directory names `search_tree`
/// skips (`.git`, `target`, `node_modules`). Sorted by score descending;
/// ties broken by shorter `relative` length, then lexicographically. Every
/// file is scored (no early stop); only the final sorted list is capped to
/// `MAX_FUZZY_FILE_RESULTS`.
pub fn fuzzy_match_files(tree: &DirEntry, query: &str) -> FuzzyFileResults {
    let query = query.trim();
    if query.is_empty() {
        return FuzzyFileResults {
            matches: Vec::new(),
            truncated: false,
        };
    }

    let mut matches = Vec::new();
    walk_children(tree, "", query, &mut matches);

    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.relative.len().cmp(&b.relative.len()))
            .then_with(|| a.relative.cmp(&b.relative))
    });

    let truncated = matches.len() > MAX_FUZZY_FILE_RESULTS;
    matches.truncate(MAX_FUZZY_FILE_RESULTS);

    FuzzyFileResults { matches, truncated }
}

fn walk_children(dir: &DirEntry, prefix: &str, query: &str, matches: &mut Vec<FuzzyFileMatch>) {
    for child in &dir.children {
        let relative = if prefix.is_empty() {
            child.name.clone()
        } else {
            format!("{prefix}/{}", child.name)
        };
        match child.kind {
            DirEntryKind::Dir => {
                if SKIPPED_DIR_NAMES.contains(&child.name.as_str()) {
                    continue;
                }
                walk_children(child, &relative, query, matches);
            }
            DirEntryKind::File => {
                if let Some(m) = fuzzy_score(query, &relative) {
                    matches.push(FuzzyFileMatch {
                        path: child.path.clone(),
                        relative,
                        score: m.score,
                        indices: m.indices,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;
    use std::fs;

    #[test]
    fn empty_pattern_matches_everything_with_zero_score() {
        let m = fuzzy_score("", "anything").unwrap();
        assert_eq!(m.score, 0);
        assert!(m.indices.is_empty());
    }

    #[test]
    fn non_subsequence_returns_none() {
        assert!(fuzzy_score("xyz", "abc").is_none());
        assert!(fuzzy_score("ba", "ab").is_none());
    }

    #[test]
    fn exact_prefix_match_scores_boundary_and_consecutive_and_case() {
        let m = fuzzy_score("app", "app.rs").unwrap();
        assert_eq!(m.indices, vec![0, 1, 2]);
        // first char: boundary (10), no case bonus needed check separately
        // second/third chars: consecutive (15 each) + case (1 each)
        // first char: boundary (10) + case (1)
        assert_eq!(
            m.score,
            (SCORE_BOUNDARY + SCORE_CASE) + 2 * (SCORE_CONSECUTIVE + SCORE_CASE)
        );
    }

    #[test]
    fn case_insensitive_but_case_match_scores_higher() {
        let lower = fuzzy_score("app", "app.rs").unwrap();
        let upper_pattern = fuzzy_score("APP", "app.rs").unwrap();
        assert!(lower.score > upper_pattern.score);
    }

    #[test]
    fn separator_boundary_is_rewarded() {
        // "au" against "app/util.rs": 'a' at 0 (boundary), 'u' at start of
        // "util" right after '/' (boundary) -- both matches get the
        // boundary bonus despite not being consecutive.
        let m = fuzzy_score("au", "app/util.rs").unwrap();
        let separator_pos = "app/util.rs".find('/').unwrap();
        assert_eq!(m.indices[0], 0);
        assert_eq!(m.indices[1], separator_pos + 1);
        assert!(m.score >= 2 * SCORE_BOUNDARY - 3); // gap penalty for the skipped "pp/" chars
    }

    #[test]
    fn camel_case_boundary_is_rewarded() {
        let m = fuzzy_score("ab", "fooABar").unwrap();
        // 'A' immediately follows lowercase 'o' -> camelCase boundary.
        let a_idx = "fooABar".find('A').unwrap();
        assert_eq!(m.indices[0], a_idx);
    }

    #[test]
    fn greedy_pass_takes_the_nearest_occurrence_not_necessarily_optimal() {
        // Documented v1 tradeoff: "ab" against "a_ab" greedily matches the
        // first 'a' then the 'b' two characters later, rather than the
        // second 'a' immediately followed by 'b'.
        let m = fuzzy_score("ab", "a_ab").unwrap();
        assert_eq!(m.indices, vec![0, 3]);
    }

    #[test]
    fn consecutive_match_scores_higher_than_a_gapped_one() {
        let consecutive = fuzzy_score("ab", "ab").unwrap();
        let gapped = fuzzy_score("ab", "a_b").unwrap();
        assert!(consecutive.score > gapped.score);
    }

    #[test]
    fn fuzzy_match_files_empty_query_returns_nothing_without_walking() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = fuzzy_match_files(&tree, "   ");
        assert_eq!(
            results,
            FuzzyFileResults {
                matches: Vec::new(),
                truncated: false
            }
        );
    }

    #[test]
    fn fuzzy_match_files_builds_relative_paths_without_the_root_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("crates")).unwrap();
        fs::create_dir(dir.path().join("crates/ui")).unwrap();
        fs::write(dir.path().join("crates/ui/app.rs"), "").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = fuzzy_match_files(&tree, "app");
        assert_eq!(results.matches.len(), 1);
        assert_eq!(results.matches[0].relative, "crates/ui/app.rs");
        assert_eq!(
            results.matches[0].path,
            fs::canonicalize(dir.path())
                .unwrap()
                .join("crates/ui/app.rs")
        );
    }

    #[test]
    fn fuzzy_match_files_ranks_a_better_match_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("app.rs"), "").unwrap();
        fs::write(dir.path().join("z_a_p_p.rs"), "").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = fuzzy_match_files(&tree, "app");
        assert_eq!(results.matches.len(), 2);
        assert_eq!(results.matches[0].relative, "app.rs");
    }

    #[test]
    fn fuzzy_match_files_skips_git_target_and_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        for skipped in [".git", "target", "node_modules"] {
            fs::create_dir(dir.path().join(skipped)).unwrap();
            fs::write(dir.path().join(skipped).join("needle.txt"), "").unwrap();
        }
        fs::write(dir.path().join("needle.txt"), "").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = fuzzy_match_files(&tree, "needle");
        assert_eq!(results.matches.len(), 1);
        assert_eq!(results.matches[0].relative, "needle.txt");
    }

    #[test]
    fn fuzzy_match_files_no_match_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = fuzzy_match_files(&tree, "zzzzz");
        assert_eq!(
            results,
            FuzzyFileResults {
                matches: Vec::new(),
                truncated: false
            }
        );
    }

    #[test]
    fn fuzzy_match_files_truncates_and_reports_truncated() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_FUZZY_FILE_RESULTS + 10 {
            fs::write(dir.path().join(format!("match_{i:05}.txt")), "").unwrap();
        }
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = fuzzy_match_files(&tree, "match");
        assert_eq!(results.matches.len(), MAX_FUZZY_FILE_RESULTS);
        assert!(results.truncated);
    }

    #[test]
    fn fuzzy_match_files_directories_are_never_matched_themselves() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("target_folder")).unwrap();
        fs::write(dir.path().join("target_folder").join("f.txt"), "").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = fuzzy_match_files(&tree, "target_folder");
        // "target_folder" as a directory name never appears as a
        // `FuzzyFileMatch::relative` by itself -- only "target_folder/f.txt".
        assert_eq!(results.matches.len(), 1);
        assert_eq!(results.matches[0].relative, "target_folder/f.txt");
    }
}
