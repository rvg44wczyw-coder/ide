//! Search in Path v2 (`docs/features/search-in-path-v2.md`): regex/glob-
//! aware global search and Replace in Path, built on A5's `buffer_search`
//! engine and a `.gitignore`/override-aware walk of an already-`Project::
//! scan_tree`-validated `DirEntry` tree. Deliberately does not touch
//! `search.rs`/`search_tree` -- `ide-tui`'s `todo_panel.rs` (T24) still
//! depends on that function's exact current plain-substring behaviour.

use std::fs;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::Match;

use crate::buffer_search::{self, SearchOptions, SearchQuery, SearchQueryError};
use crate::project::{DirEntry, DirEntryKind};
use crate::search::{MAX_SEARCHABLE_FILE_BYTES, MAX_SEARCH_RESULTS, SKIPPED_DIR_NAMES};
use crate::text::LineIndex;
use crate::workspace_edit::{FileEdit, WorkspaceEdit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSearchOptions {
    pub search: SearchOptions,
    /// Glob patterns; empty = no include filter (every file passes).
    pub include: Vec<String>,
    /// Glob patterns; empty = no exclude filter.
    pub exclude: Vec<String>,
    pub respect_gitignore: bool,
}

/// One match of a `search_tree_advanced` query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSearchMatch {
    pub path: PathBuf,
    /// 0-based.
    pub line: u32,
    /// 0-based, **bytes** from the line start -- `ide_core::text::
    /// LineIndex::position_at`'s own convention, deliberately not the
    /// char-based column `search::SearchMatch::column` uses (that module
    /// hand-rolls char tracking because it doesn't route through
    /// `LineIndex`; this one does, and has no consumer that needs chars).
    pub column: u32,
    /// Byte offset of the match's start within the file's full text.
    pub byte_offset: usize,
    pub line_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSearchResults {
    pub matches: Vec<PathSearchMatch>,
    /// `true` if `MAX_SEARCH_RESULTS` (reused from `crate::search`) was
    /// reached and the walk stopped early.
    pub truncated: bool,
}

/// The result of one `replace_in_path` call. `edit` is built in memory
/// only -- nothing is written to disk by this module; the caller previews
/// and applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceInPathResult {
    pub edit: WorkspaceEdit,
    /// Same meaning as [`PathSearchResults::truncated`], but the cap
    /// applies to total `Transaction` changes across every file rather
    /// than to matches -- see [`replace_in_path`].
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PathSearchError {
    #[error("invalid search pattern: {0}")]
    InvalidQuery(#[from] SearchQueryError),
    #[error("invalid glob {glob:?}: {source}")]
    InvalidGlob { glob: String, source: ignore::Error },
}

/// Builds the `.gitignore` and include/exclude override matchers for
/// `root` (`docs/features/search-in-path-v2.md` §3.6). `root` must be the
/// same `tree.path` passed to [`search_tree_advanced`]/[`replace_in_path`]
/// -- both matchers are rooted there.
///
/// Only `<root>/.gitignore` is ever loaded -- not any nested-directory
/// `.gitignore`, not global git excludes. A missing or unreadable root
/// `.gitignore` is silently treated as "no extra rules" (same skip-on-I/O
/// -failure convention `crate::search::search_file` already uses for
/// unreadable files), not a hard error.
fn build_matchers(
    root: &Path,
    options: &PathSearchOptions,
) -> Result<(Gitignore, Override), PathSearchError> {
    let gitignore = if options.respect_gitignore {
        let mut builder = GitignoreBuilder::new(root);
        let _ = builder.add(root.join(".gitignore"));
        builder.build().unwrap_or_else(|_| Gitignore::empty())
    } else {
        Gitignore::empty()
    };

    let mut override_builder = OverrideBuilder::new(root);
    for pattern in &options.include {
        override_builder
            .add(pattern)
            .map_err(|source| PathSearchError::InvalidGlob {
                glob: pattern.clone(),
                source,
            })?;
    }
    for pattern in &options.exclude {
        override_builder
            .add(&format!("!{pattern}"))
            .map_err(|source| PathSearchError::InvalidGlob {
                glob: pattern.clone(),
                source,
            })?;
    }
    let overrides = override_builder
        .build()
        .map_err(|source| PathSearchError::InvalidGlob {
            glob: "include/exclude glob set".to_string(),
            source,
        })?;

    Ok((gitignore, overrides))
}

/// Overrides (include/exclude globs) are checked first and win outright
/// whenever they return anything other than `Match::None`; `.gitignore`
/// is only consulted when the override layer has no opinion. Verified
/// against `ignore` 0.4.33's own `src/dir.rs` walker precedence, not
/// assumed. The "a file matching no include glob is implicitly ignored"
/// rule lives inside `Override::matched` itself (`ignore` crate,
/// `src/overrides.rs`) and is never re-derived here.
fn is_path_included(
    overrides: &Override,
    gitignore: &Gitignore,
    path: &Path,
    is_dir: bool,
) -> bool {
    match overrides.matched(path, is_dir) {
        Match::Whitelist(_) => true,
        Match::Ignore(_) => false,
        Match::None => !gitignore.matched(path, is_dir).is_ignore(),
    }
}

/// Depth-first collection of every candidate file under `entry`, pruning
/// whole subtrees rather than filtering a flat list after the fact:
/// `SKIPPED_DIR_NAMES` unconditionally first (same as `search::walk`),
/// then the gitignore/override verdict. Never visits a path outside
/// `entry`'s own tree -- a glob pattern can only narrow which of
/// `entry`'s existing paths are collected, it can never expand the walk
/// beyond them (the walk only ever recurses into `entry.children`).
fn walk_filtered<'a>(
    entry: &'a DirEntry,
    gitignore: &Gitignore,
    overrides: &Override,
    out: &mut Vec<&'a DirEntry>,
) {
    match entry.kind {
        DirEntryKind::Dir => {
            if SKIPPED_DIR_NAMES.contains(&entry.name.as_str()) {
                return;
            }
            if !is_path_included(overrides, gitignore, &entry.path, true) {
                return;
            }
            for child in &entry.children {
                walk_filtered(child, gitignore, overrides, out);
            }
        }
        DirEntryKind::File => {
            if is_path_included(overrides, gitignore, &entry.path, false) {
                out.push(entry);
            }
        }
    }
}

/// Reads `entry`'s content, applying the same size cap and
/// skip-on-read-failure convention `search::search_file` already uses.
/// `None` means "skip this file" -- not an error.
fn read_candidate(entry: &DirEntry) -> Option<String> {
    let metadata = fs::metadata(&entry.path).ok()?;
    if metadata.len() > MAX_SEARCHABLE_FILE_BYTES {
        return None;
    }
    fs::read_to_string(&entry.path).ok()
}

/// Regex/glob-aware "Find in Path" (`docs/features/search-in-path-v2.md`
/// §2.1, §3). `tree` must be the root `DirEntry` a `Project::scan_tree()`
/// call returned -- `tree.path` is used as the `.gitignore`/override
/// matcher root, mirroring how `search::search_tree`'s own walk assumes
/// `tree` is the root for `SKIPPED_DIR_NAMES` purposes.
/// `query.trim().is_empty()` returns an empty, non-truncated result
/// immediately without walking anything, mirroring `search_tree`'s own
/// contract.
pub fn search_tree_advanced(
    tree: &DirEntry,
    query: &str,
    options: &PathSearchOptions,
) -> Result<PathSearchResults, PathSearchError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(PathSearchResults {
            matches: Vec::new(),
            truncated: false,
        });
    }
    let compiled = SearchQuery::compile(query, options.search)?;
    let (gitignore, overrides) = build_matchers(&tree.path, options)?;

    let mut candidates = Vec::new();
    walk_filtered(tree, &gitignore, &overrides, &mut candidates);

    let mut matches = Vec::new();
    let mut truncated = false;
    for file in candidates {
        if truncated {
            break;
        }
        let Some(text) = read_candidate(file) else {
            continue;
        };
        let line_index = LineIndex::new(&text);
        let results = buffer_search::find_matches(&text, &compiled, None);
        if results.truncated {
            truncated = true;
        }
        for range in results.matches {
            let (line, column) = line_index.position_at(range.start);
            let line_text = line_index
                .line_range(line, &text)
                .map(|r| text[r].to_string())
                .unwrap_or_default();
            matches.push(PathSearchMatch {
                path: file.path.clone(),
                line: line as u32,
                column: column as u32,
                byte_offset: range.start,
                line_text,
            });
            if matches.len() == MAX_SEARCH_RESULTS {
                truncated = true;
                break;
            }
        }
    }

    Ok(PathSearchResults { matches, truncated })
}

/// Regex/glob-aware "Replace in Path" (`docs/features/search-in-path-v2.md`
/// §2.1, §3.3). Builds a `WorkspaceEdit` **in memory only** -- this
/// function never writes to disk; the caller (`ide-ui`) is responsible for
/// previewing and applying it. `tree`'s contract and the
/// `query.trim().is_empty()` behaviour are identical to
/// [`search_tree_advanced`]'s.
///
/// Walks the same `.gitignore`/override-filtered candidate set
/// `search_tree_advanced` would for the same `query`+`options`, and for
/// every file with at least one match, reuses `buffer_search::replace_all`
/// -- the exact same engine that powers in-buffer Replace All, so regex
/// capture-group expansion, whole-word filtering and case sensitivity
/// behave identically here. A per-file `ReplaceResult::truncated` (more
/// than `buffer_search::MAX_SEARCH_MATCHES` matches in one file) and an
/// aggregate cap (total changes across every file reaching
/// `crate::search::MAX_SEARCH_RESULTS`, reused rather than introducing a
/// third constant) both set the result's `truncated`; hitting the
/// aggregate cap stops the walk early, the same early-stop-not-truncate-
/// after-the-fact policy `search_tree` already uses.
pub fn replace_in_path(
    tree: &DirEntry,
    query: &str,
    replacement: &str,
    options: &PathSearchOptions,
) -> Result<ReplaceInPathResult, PathSearchError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(ReplaceInPathResult {
            edit: WorkspaceEdit { edits: Vec::new() },
            truncated: false,
        });
    }
    let compiled = SearchQuery::compile(query, options.search)?;
    let (gitignore, overrides) = build_matchers(&tree.path, options)?;

    let mut candidates = Vec::new();
    walk_filtered(tree, &gitignore, &overrides, &mut candidates);

    let mut edits = Vec::new();
    let mut total_changes = 0usize;
    let mut truncated = false;
    for file in candidates {
        if truncated {
            break;
        }
        let Some(text) = read_candidate(file) else {
            continue;
        };
        let Some(result) = buffer_search::replace_all(&text, &compiled, replacement, None) else {
            continue;
        };
        if result.truncated {
            truncated = true;
        }
        total_changes += result.transaction.changes().len();
        edits.push(FileEdit {
            path: file.path.clone(),
            transaction: result.transaction,
        });
        if total_changes >= MAX_SEARCH_RESULTS {
            truncated = true;
        }
    }

    Ok(ReplaceInPathResult {
        edit: WorkspaceEdit { edits },
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;
    use std::fs as stdfs;

    fn opts() -> PathSearchOptions {
        PathSearchOptions {
            search: SearchOptions::default(),
            include: Vec::new(),
            exclude: Vec::new(),
            respect_gitignore: true,
        }
    }

    fn literal() -> PathSearchOptions {
        PathSearchOptions {
            search: SearchOptions {
                case_sensitive: true,
                ..Default::default()
            },
            ..opts()
        }
    }

    #[test]
    fn empty_query_returns_no_matches_without_error() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("f.txt"), "hello").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree_advanced(&tree, "   ", &opts()).unwrap();
        assert_eq!(
            results,
            PathSearchResults {
                matches: Vec::new(),
                truncated: false
            }
        );
    }

    #[test]
    fn literal_case_sensitive_search_finds_matches_with_byte_column() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("f.txt"), "line one\nneedle here\n").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree_advanced(&tree, "needle", &literal()).unwrap();
        assert_eq!(results.matches.len(), 1);
        let m = &results.matches[0];
        assert_eq!(m.line, 1);
        assert_eq!(m.column, 0);
        assert_eq!(m.line_text, "needle here");
    }

    #[test]
    fn regex_search_matches_via_the_same_a5_engine() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("f.txt"), "a1 b22 c333").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let options = PathSearchOptions {
            search: SearchOptions {
                regex: true,
                case_sensitive: true,
                whole_word: false,
            },
            ..opts()
        };
        let results = search_tree_advanced(&tree, r"\d+", &options).unwrap();
        assert_eq!(results.matches.len(), 3);
    }

    #[test]
    fn invalid_regex_reports_path_search_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let options = PathSearchOptions {
            search: SearchOptions {
                regex: true,
                ..Default::default()
            },
            ..opts()
        };
        let err = search_tree_advanced(&tree, "(unclosed", &options).unwrap_err();
        assert!(matches!(err, PathSearchError::InvalidQuery(_)));
    }

    #[test]
    fn invalid_include_glob_reports_path_search_error() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("f.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let options = PathSearchOptions {
            include: vec!["[".to_string()],
            ..opts()
        };
        let err = search_tree_advanced(&tree, "needle", &options).unwrap_err();
        assert!(matches!(err, PathSearchError::InvalidGlob { .. }));
    }

    #[test]
    fn include_glob_narrows_to_matching_files_only() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("a.rs"), "needle").unwrap();
        stdfs::write(dir.path().join("b.md"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let options = PathSearchOptions {
            include: vec!["*.rs".to_string()],
            ..opts()
        };
        let results = search_tree_advanced(&tree, "needle", &options).unwrap();
        assert_eq!(results.matches.len(), 1);
        assert_eq!(
            results.matches[0].path,
            stdfs::canonicalize(dir.path()).unwrap().join("a.rs")
        );
    }

    #[test]
    fn include_glob_still_descends_into_a_non_matching_directory() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::create_dir_all(dir.path().join("subdir")).unwrap();
        stdfs::write(dir.path().join("subdir/a.rs"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        // "subdir" itself doesn't match "*.rs" -- an include glob describes
        // files, not directory membership, so the walk must still descend
        // into it to find the match inside (doc §3.6).
        let options = PathSearchOptions {
            include: vec!["*.rs".to_string()],
            ..opts()
        };
        let results = search_tree_advanced(&tree, "needle", &options).unwrap();
        assert_eq!(results.matches.len(), 1);
        assert_eq!(
            results.matches[0].path,
            stdfs::canonicalize(dir.path()).unwrap().join("subdir/a.rs")
        );
    }

    #[test]
    fn exclude_glob_removes_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("a.rs"), "needle").unwrap();
        stdfs::write(dir.path().join("b.lock"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let options = PathSearchOptions {
            exclude: vec!["*.lock".to_string()],
            ..opts()
        };
        let results = search_tree_advanced(&tree, "needle", &options).unwrap();
        assert_eq!(results.matches.len(), 1);
        assert_eq!(
            results.matches[0].path,
            stdfs::canonicalize(dir.path()).unwrap().join("a.rs")
        );
    }

    #[test]
    fn override_wins_over_gitignore_when_both_have_an_opinion() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join(".gitignore"), "a.rs\n").unwrap();
        stdfs::write(dir.path().join("a.rs"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        // .gitignore excludes a.rs; an include override for *.rs must win
        // and still surface it (verified precedence, doc §3.6).
        let options = PathSearchOptions {
            include: vec!["*.rs".to_string()],
            ..opts()
        };
        let results = search_tree_advanced(&tree, "needle", &options).unwrap();
        assert_eq!(results.matches.len(), 1);
    }

    #[test]
    fn root_gitignore_excludes_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        stdfs::write(dir.path().join("ignored.txt"), "needle").unwrap();
        stdfs::write(dir.path().join("kept.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree_advanced(&tree, "needle", &opts()).unwrap();
        assert_eq!(results.matches.len(), 1);
        assert_eq!(
            results.matches[0].path,
            stdfs::canonicalize(dir.path()).unwrap().join("kept.txt")
        );
    }

    #[test]
    fn gitignored_directory_is_pruned_not_just_its_direct_contents() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join(".gitignore"), "vendor/\n").unwrap();
        stdfs::create_dir_all(dir.path().join("vendor/nested")).unwrap();
        stdfs::write(dir.path().join("vendor/nested/f.txt"), "needle").unwrap();
        stdfs::write(dir.path().join("kept.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree_advanced(&tree, "needle", &opts()).unwrap();
        assert_eq!(results.matches.len(), 1);
        assert_eq!(
            results.matches[0].path,
            stdfs::canonicalize(dir.path()).unwrap().join("kept.txt")
        );
    }

    #[test]
    fn respect_gitignore_false_ignores_the_gitignore_file_entirely() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        stdfs::write(dir.path().join("ignored.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let options = PathSearchOptions {
            respect_gitignore: false,
            ..opts()
        };
        let results = search_tree_advanced(&tree, "needle", &options).unwrap();
        assert_eq!(results.matches.len(), 1);
    }

    #[test]
    fn missing_gitignore_file_is_safely_ignored() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("f.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        // No .gitignore exists at all -- must not error.
        let results = search_tree_advanced(&tree, "needle", &opts()).unwrap();
        assert_eq!(results.matches.len(), 1);
    }

    #[test]
    fn skipped_dir_names_are_still_pruned_unconditionally() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::create_dir(dir.path().join("target")).unwrap();
        stdfs::write(dir.path().join("target/f.txt"), "needle").unwrap();
        stdfs::write(dir.path().join("real.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        // No .gitignore mentions "target" at all -- SKIPPED_DIR_NAMES
        // still prunes it independently.
        let results = search_tree_advanced(&tree, "needle", &opts()).unwrap();
        assert_eq!(results.matches.len(), 1);
        assert_eq!(
            results.matches[0].path,
            stdfs::canonicalize(dir.path()).unwrap().join("real.txt")
        );
    }

    #[test]
    fn traversal_looking_glob_pattern_only_narrows_never_escapes_root() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("a.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        // An include pattern that looks like it might reach outside the
        // root can only fail to match anything inside the (already
        // root-confined) DirEntry tree -- it cannot expand the walk to
        // visit paths outside `tree`.
        let options = PathSearchOptions {
            include: vec!["../../../etc/*".to_string()],
            ..opts()
        };
        let results = search_tree_advanced(&tree, "needle", &options).unwrap();
        assert!(results.matches.is_empty());
    }

    #[test]
    fn aggregate_result_cap_truncates_and_stops_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_SEARCH_RESULTS + 5 {
            stdfs::write(dir.path().join(format!("f{i:05}.txt")), "needle").unwrap();
        }
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let results = search_tree_advanced(&tree, "needle", &opts()).unwrap();
        assert_eq!(results.matches.len(), MAX_SEARCH_RESULTS);
        assert!(results.truncated);
    }

    #[test]
    fn replace_all_engine_is_reused_including_regex_capture_groups() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("f.txt"), "John Smith").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let options = PathSearchOptions {
            search: SearchOptions {
                regex: true,
                case_sensitive: true,
                whole_word: false,
            },
            ..opts()
        };
        let result = replace_in_path(&tree, r"(\w+) (\w+)", "$2 $1", &options).unwrap();
        assert_eq!(result.edit.edits.len(), 1);
        let edit = &result.edit.edits[0];
        let new_text =
            crate::workspace_edit::apply_transaction("John Smith", &edit.transaction).unwrap();
        assert_eq!(new_text, "Smith John");
        assert!(!result.truncated);
    }

    #[test]
    fn replace_in_path_only_touches_files_with_a_match() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("a.txt"), "needle").unwrap();
        stdfs::write(dir.path().join("b.txt"), "nothing here").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let result = replace_in_path(&tree, "needle", "found", &literal()).unwrap();
        assert_eq!(result.edit.edits.len(), 1);
        assert_eq!(
            result.edit.edits[0].path,
            stdfs::canonicalize(dir.path()).unwrap().join("a.txt")
        );
    }

    #[test]
    fn replace_in_path_respects_include_exclude_and_gitignore_too() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        stdfs::write(dir.path().join("ignored.txt"), "needle").unwrap();
        stdfs::write(dir.path().join("kept.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let result = replace_in_path(&tree, "needle", "found", &opts()).unwrap();
        assert_eq!(result.edit.edits.len(), 1);
        assert_eq!(
            result.edit.edits[0].path,
            stdfs::canonicalize(dir.path()).unwrap().join("kept.txt")
        );
    }

    #[test]
    fn empty_replace_query_is_a_noop_edit() {
        let dir = tempfile::tempdir().unwrap();
        stdfs::write(dir.path().join("f.txt"), "needle").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let result = replace_in_path(&tree, "   ", "found", &opts()).unwrap();
        assert!(result.edit.edits.is_empty());
        assert!(!result.truncated);
    }

    #[test]
    fn replace_aggregate_change_cap_marks_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let text = "a ".repeat(MAX_SEARCH_RESULTS + 20);
        stdfs::write(dir.path().join("f.txt"), &text).unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let result = replace_in_path(&tree, "a", "b", &literal()).unwrap();
        assert!(result.truncated);
    }
}
