//! In-buffer find/replace engine (`docs/features/in-buffer-find-replace.md`).
//! Deliberately separate from `text::find` (A3's case-sensitive literal
//! `all_occurrences`/`next_occurrence`, used for multi-cursor occurrence
//! commands) -- that module's own doc comment already anticipated this one
//! and is left untouched, per the feature doc's §4.2 invariant.

use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::text::{Change, Transaction};

/// Ceiling on how many matches one `find_matches`/`replace_all` call
/// reports/replaces. Buffer content is file content, i.e. untrusted, and a
/// one-character literal query against a large file yields hundreds of
/// thousands of matches -- which the panel would try to highlight, list on
/// a scrollbar, and (for Replace All) fold into one gigantic transaction.
/// Same value, same reasoning as `text::find::MAX_OCCURRENCES` and
/// `crate::search::MAX_SEARCH_RESULTS` -- each of those modules keeps its
/// own copy of this constant rather than sharing one, and this module
/// follows that existing precedent rather than introducing a shared one.
pub const MAX_SEARCH_MATCHES: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// The result of one `find_matches` call. A named struct rather than a
/// `(Vec<Range<usize>>, bool)` tuple for the same reason
/// `crate::search::SearchResults` (`crates/core/src/search.rs`) already
/// exists for the identical "matches + truncated" shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResults {
    pub matches: Vec<Range<usize>>,
    /// `true` if [`MAX_SEARCH_MATCHES`] was reached and the scan stopped
    /// early -- same conservative approximation `crate::search::search_tree`
    /// already uses (reaching the cap is treated as "truncated," without a
    /// separate check for a genuine match immediately beyond it).
    pub truncated: bool,
}

/// The result of one `replace_all` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceResult {
    pub transaction: Transaction,
    /// Same meaning as [`MatchResults::truncated`].
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SearchQueryError {
    /// Carries `regex::Error`'s own `Display` output verbatim -- it already
    /// names the offending part of the pattern, which is exactly what a
    /// user typing a regex into the panel needs to see.
    #[error("invalid pattern: {0}")]
    InvalidRegex(String),
}

#[derive(Debug)]
enum Engine {
    Literal {
        needle: String,
        case_sensitive: bool,
    },
    Regex(Regex),
}

/// A compiled, ready-to-search query. Opaque: callers never match on its
/// variants, only pass it to [`find_matches`]/[`replace_one`]/
/// [`replace_all`] and recompile a new one when the query string or options
/// change.
#[derive(Debug)]
pub struct SearchQuery {
    engine: Engine,
    whole_word: bool,
}

impl SearchQuery {
    /// Compiles `pattern` under `options`. Empty `pattern` always succeeds
    /// and matches nothing -- mirrors `text::find::all_occurrences`'s
    /// existing "empty needle -> empty result" contract, so an empty search
    /// field never errors, it just shows no matches.
    ///
    /// `options.regex` selects the engine: `false` compiles `pattern` as a
    /// literal substring (`options.case_sensitive` controls case folding);
    /// `true` compiles it as a `regex::Regex` via `RegexBuilder`'s
    /// `case_insensitive` option (the builder API, rather than prepending a
    /// literal `(?i)` to the pattern string, so an already-anchored or
    /// already-flagged user pattern is never disturbed), and a syntax error
    /// is reported via `SearchQueryError::InvalidRegex`. `options.whole_word`
    /// applies identically to both engines as a post-match filter, not by
    /// mangling the pattern.
    pub fn compile(pattern: &str, options: SearchOptions) -> Result<Self, SearchQueryError> {
        let engine = if options.regex {
            let regex = RegexBuilder::new(pattern)
                .case_insensitive(!options.case_sensitive)
                .build()
                .map_err(|e| SearchQueryError::InvalidRegex(e.to_string()))?;
            Engine::Regex(regex)
        } else {
            Engine::Literal {
                needle: pattern.to_string(),
                case_sensitive: options.case_sensitive,
            }
        };
        Ok(Self {
            engine,
            whole_word: options.whole_word,
        })
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether the match `range` in `text` satisfies a whole-word boundary on
/// both sides: the character immediately before `range.start` (if any) and
/// the one immediately after `range.end` (if any) must both be
/// non-word-characters. Uses the exact predicate
/// `crates/core/src/text/selection_hierarchy.rs::word_at` already uses for
/// double-click word selection, redefined locally rather than imported --
/// `word_at` returns a range grown from an offset, a different enough shape
/// from a boundary check that sharing a function would mean one reaching
/// into the other's internals for no real reuse.
fn is_whole_word_match(text: &str, range: &Range<usize>) -> bool {
    let before_ok = text[..range.start]
        .chars()
        .next_back()
        .is_none_or(|c| !is_word_char(c));
    let after_ok = text[range.end..]
        .chars()
        .next()
        .is_none_or(|c| !is_word_char(c));
    before_ok && after_ok
}

/// Case-insensitive literal search for the first occurrence of `needle`
/// (already known non-empty) at or after `start`, Unicode-safe against
/// length-changing case folds (e.g. `'İ' -> "i̇"`, one char to two) the same
/// way `crate::search::search_tree`'s `find_match_in_line` is: never by
/// lowercasing a whole copy of `text` once and reusing an index found in
/// that copy, which would desync from the original wherever a fold changes
/// length. Unlike that function (which only needs a match's start, to
/// report a line/column), this also needs the match's exact *end* byte
/// offset, computed by accumulating each original character's lowercase
/// expansion until it reaches `needle_lower`'s length, then checking
/// equality -- a natural extension of the same "never index a lowered copy
/// with an original-text index" technique.
fn find_case_insensitive_at(text: &str, start: usize, needle_lower: &str) -> Option<usize> {
    let needle_char_count = needle_lower.chars().count();
    let mut lowered = String::with_capacity(needle_lower.len());
    let mut end = start;
    for ch in text[start..].chars() {
        if lowered.chars().count() >= needle_char_count {
            break;
        }
        end += ch.len_utf8();
        for lc in ch.to_lowercase() {
            lowered.push(lc);
        }
    }
    (lowered == needle_lower).then_some(end)
}

fn find_literal_matches(
    text: &str,
    needle: &str,
    case_sensitive: bool,
) -> (Vec<Range<usize>>, bool) {
    let mut found = Vec::new();
    let mut truncated = false;
    if needle.is_empty() {
        return (found, truncated);
    }

    if case_sensitive {
        let mut cursor = 0usize;
        while cursor < text.len() {
            if found.len() >= MAX_SEARCH_MATCHES {
                truncated = true;
                break;
            }
            let Some(offset) = text[cursor..].find(needle) else {
                break;
            };
            let start = cursor + offset;
            let end = start + needle.len();
            found.push(start..end);
            cursor = end;
        }
    } else {
        let needle_lower = needle.to_lowercase();
        let mut cursor = 0usize;
        'scan: while cursor < text.len() {
            if found.len() >= MAX_SEARCH_MATCHES {
                truncated = true;
                break 'scan;
            }
            let Some((byte_idx, ch)) = text[cursor..].char_indices().next() else {
                break;
            };
            let abs_idx = cursor + byte_idx;
            if let Some(end) = find_case_insensitive_at(text, abs_idx, &needle_lower) {
                found.push(abs_idx..end);
                cursor = end;
            } else {
                cursor = abs_idx + ch.len_utf8();
            }
        }
    }
    (found, truncated)
}

/// Zero-width matches are excluded (e.g. a regex like `x*` or `^` can
/// otherwise match an empty range). A highlight with no width is not a
/// useful, clickable, or replaceable target, and excluding them outright
/// avoids an ambiguous whole-word check (§ where "before" and "after" would
/// be the same neighbouring pair on both sides of a single point).
fn find_regex_matches(text: &str, regex: &Regex) -> (Vec<Range<usize>>, bool) {
    let mut found = Vec::new();
    let mut truncated = false;
    for m in regex.find_iter(text) {
        if m.start() == m.end() {
            continue;
        }
        if found.len() >= MAX_SEARCH_MATCHES {
            truncated = true;
            break;
        }
        found.push(m.start()..m.end());
    }
    (found, truncated)
}

fn raw_matches(text: &str, query: &SearchQuery) -> (Vec<Range<usize>>, bool) {
    match &query.engine {
        Engine::Literal {
            needle,
            case_sensitive,
        } => find_literal_matches(text, needle, *case_sensitive),
        Engine::Regex(regex) => find_regex_matches(text, regex),
    }
}

/// Every non-overlapping match of `query` in `text`, left to right, at most
/// [`MAX_SEARCH_MATCHES`] of them. `scope`, if given, restricts results to
/// matches fully contained within that byte range -- but the search still
/// runs over the *whole* `text` (capped at `MAX_SEARCH_MATCHES` over the
/// whole text, before scope filtering), then filters, so a regex's word
/// boundaries and lookaround-free context are computed correctly regardless
/// of where `scope` cuts.
pub fn find_matches(text: &str, query: &SearchQuery, scope: Option<Range<usize>>) -> MatchResults {
    let (mut matches, truncated) = raw_matches(text, query);
    if query.whole_word {
        matches.retain(|range| is_whole_word_match(text, range));
    }
    if let Some(scope) = scope {
        matches.retain(|range| range.start >= scope.start && range.end <= scope.end);
    }
    MatchResults { matches, truncated }
}

/// Expands `replacement` for the match at `range` in `text`. For a regex
/// query, re-derives `Captures` via `Regex::captures_at(text, range.start)`
/// -- searching the *original* `text` from the match's known start, rather
/// than re-running the regex against an isolated `&text[range]` slice.
/// The isolated-slice approach was tried first and rejected: a
/// context-dependent zero-width assertion like `\B` (matches only where
/// there is *no* word boundary) can be true at a position only because of a
/// character just outside the match -- re-checking it against a standalone
/// copy of just the matched text strips that context and can make the
/// assertion evaluate differently (in `\B`'s case, position 0 of any
/// standalone string that starts with a word character always reads as a
/// boundary, i.e. `\B` fails, even though it held in the original text).
/// `captures_at` preserves full surrounding context because it searches the
/// real `text`, just starting the scan later.
fn expand_replacement(
    text: &str,
    query: &SearchQuery,
    range: &Range<usize>,
    replacement: &str,
) -> String {
    match &query.engine {
        Engine::Literal { .. } => replacement.to_string(),
        Engine::Regex(regex) => {
            let captures = regex
                .captures_at(text, range.start)
                .expect("range was produced by this same regex against this same text");
            let mut expanded = String::new();
            captures.expand(replacement, &mut expanded);
            expanded
        }
    }
}

/// Builds the one-`Change`-per-match `Transaction` that replaces every
/// match of `query` in `text` with `replacement`, honouring `scope` and the
/// zero-width exclusion exactly as `find_matches` does. `None` when there is
/// nothing to replace -- the caller must not call `Buffer::apply` on an
/// empty result, so it never pushes a no-op undo step.
///
/// When `query` is a regex, `replacement` is expanded through
/// `regex::Regex`'s own `$1`/`${name}` capture-group syntax (via
/// `Captures::expand` on the captures `expand_replacement` re-derives
/// against the real `text` -- see that function's doc comment for why); a
/// literal `$` that isn't part of a capture reference must be written
/// `$$`. When `query` is a literal (non-regex) query, `replacement` is
/// inserted verbatim, `$` included.
pub fn replace_all(
    text: &str,
    query: &SearchQuery,
    replacement: &str,
    scope: Option<Range<usize>>,
) -> Option<ReplaceResult> {
    let results = find_matches(text, query, scope);
    if results.matches.is_empty() {
        return None;
    }
    let changes: Vec<Change> = results
        .matches
        .iter()
        .map(|range| {
            let expanded = expand_replacement(text, query, range, replacement);
            Change::new(range.clone(), expanded)
        })
        .collect();
    let transaction =
        Transaction::new(changes).expect("find_matches produces non-overlapping ranges");
    Some(ReplaceResult {
        transaction,
        truncated: results.truncated,
    })
}

/// Builds a one-`Change` `Transaction` replacing exactly `range` with
/// `replacement`, applying `replace_all`'s same `$1`-expansion rule when
/// `query` is a regex. `range` is trusted to be a match `query` actually
/// produced against this same `text` -- like `Transaction::replace`, this
/// function does not re-verify that, it only builds the edit.
///
/// Takes `text` (the doc's original §2.1 signature for this function did
/// not) because expanding a regex capture-group reference requires knowing
/// what was actually captured at `range`, which is only recoverable from
/// the source text -- see `expand_replacement`'s doc comment for why that
/// lookup must run against the real `text`, not an isolated copy of the
/// matched slice. `docs/features/in-buffer-find-replace.md` §2.1 has been
/// corrected to match.
pub fn replace_one(
    text: &str,
    query: &SearchQuery,
    range: Range<usize>,
    replacement: &str,
) -> Transaction {
    let expanded = expand_replacement(text, query, &range, replacement);
    Transaction::replace(range, expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(pattern: &str, case_sensitive: bool, whole_word: bool) -> SearchQuery {
        SearchQuery::compile(
            pattern,
            SearchOptions {
                case_sensitive,
                whole_word,
                regex: false,
            },
        )
        .unwrap()
    }

    fn regex(pattern: &str, case_sensitive: bool, whole_word: bool) -> SearchQuery {
        SearchQuery::compile(
            pattern,
            SearchOptions {
                case_sensitive,
                whole_word,
                regex: true,
            },
        )
        .unwrap()
    }

    #[test]
    fn empty_pattern_matches_nothing_for_both_engines() {
        let text = "hello world";
        assert!(find_matches(text, &literal("", false, false), None)
            .matches
            .is_empty());
        // An empty regex pattern compiles and matches a zero-width range at
        // every position -- all excluded, so this also yields nothing.
        assert!(find_matches(text, &regex("", false, false), None)
            .matches
            .is_empty());
    }

    #[test]
    fn literal_case_insensitive_by_default() {
        let text = "Count count COUNT";
        let results = find_matches(text, &literal("count", false, false), None);
        assert_eq!(results.matches, vec![0..5, 6..11, 12..17]);
    }

    #[test]
    fn literal_case_sensitive_when_requested() {
        let text = "Count count COUNT";
        let results = find_matches(text, &literal("count", true, false), None);
        assert_eq!(results.matches, vec![6..11]);
    }

    #[test]
    fn literal_matches_do_not_overlap() {
        let results = find_matches("aaaa", &literal("aa", true, false), None);
        assert_eq!(results.matches, vec![0..2, 2..4]);
    }

    #[test]
    fn unicode_case_fold_length_change_does_not_desync_offsets() {
        // 'İ' (U+0130, 2 UTF-8 bytes) lowercases to "i̇" (2 codepoints, 3
        // UTF-8 bytes: 'i' + a combining dot above) -- both the char count
        // AND the byte count change. A naive implementation that lowercases
        // the whole text once and reuses a byte index found in that copy
        // would land inside the 3-byte "i̇" sequence instead of at 's',
        // since byte offset 2 means something different in each string.
        // Mirrors `crate::search::search_tree`'s own test for this exact
        // case (`matches_are_located_against_the_original_line_not_a_lowercased_copy`).
        let text = "İstanbul";
        let results = find_matches(text, &literal("stanbul", false, false), None);
        assert_eq!(results.matches.len(), 1);
        let m = &results.matches[0];
        assert_eq!(m.start, 2); // byte offset immediately after the 2-byte 'İ'
        assert_eq!(&text[m.clone()], "stanbul");
    }

    #[test]
    fn whole_word_excludes_a_substring_hit_inside_a_longer_word() {
        let text = "todo todoist a-todo-b";
        let results = find_matches(text, &literal("todo", false, true), None);
        // "todo" at 0 (start/space), "todo" inside "a-todo-b" (both sides
        // are '-', non-word) match; "todoist" does not.
        assert_eq!(results.matches.len(), 2);
        assert_eq!(&text[results.matches[0].clone()], "todo");
        assert_eq!(&text[results.matches[1].clone()], "todo");
    }

    #[test]
    fn whole_word_at_start_and_end_of_text_has_no_neighbour_to_fail_on() {
        let results = find_matches("todo", &literal("todo", false, true), None);
        assert_eq!(results.matches, vec![0..4]);
    }

    #[test]
    fn regex_matches_are_found() {
        let text = "a1 b22 c333";
        let results = find_matches(text, &regex(r"\d+", true, false), None);
        assert_eq!(results.matches.len(), 3);
        assert_eq!(&text[results.matches[0].clone()], "1");
        assert_eq!(&text[results.matches[1].clone()], "22");
        assert_eq!(&text[results.matches[2].clone()], "333");
    }

    #[test]
    fn regex_case_insensitive_via_builder_not_pattern_mangling() {
        let text = "Todo TODO todo";
        let results = find_matches(text, &regex("todo", false, false), None);
        assert_eq!(results.matches.len(), 3);
    }

    #[test]
    fn regex_whole_word_applies_as_post_filter() {
        let text = "1 12 123";
        let results = find_matches(text, &regex(r"\d\d", true, true), None);
        // "12" (whole word) matches; the "12" inside "123" does not (followed
        // by '3', a word char).
        assert_eq!(results.matches.len(), 1);
        assert_eq!(&text[results.matches[0].clone()], "12");
    }

    #[test]
    fn zero_width_regex_matches_are_excluded() {
        let results = find_matches("abc", &regex("x*", true, false), None);
        assert!(results.matches.is_empty());
    }

    #[test]
    fn invalid_regex_reports_search_query_error() {
        let err = SearchQuery::compile(
            "(unclosed",
            SearchOptions {
                regex: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        match err {
            SearchQueryError::InvalidRegex(msg) => assert!(!msg.is_empty()),
        }
    }

    #[test]
    fn scope_filters_to_fully_contained_matches_computed_over_the_whole_text() {
        let text = "foo foo foo";
        // Scope covers only the middle "foo" (bytes 4..7).
        let results = find_matches(text, &literal("foo", true, false), Some(4..7));
        assert_eq!(results.matches, vec![4..7]);
    }

    #[test]
    fn scope_uses_whole_text_regex_context_not_a_slice() {
        // `\bfoo` requires a word boundary before "foo". Scoping to exactly
        // "foo" starting mid-word (inside "xfoo") must NOT match, because in
        // the real text there's a word character ('x') immediately before --
        // a naive slice-then-search over text[scope] alone would wrongly see
        // the slice's own start as a boundary and match.
        let text = "xfoo bar";
        let results = find_matches(text, &regex(r"\bfoo", true, false), Some(1..4));
        assert!(results.matches.is_empty());
    }

    #[test]
    fn find_matches_caps_at_max_and_signals_truncated() {
        let text = "a".repeat(MAX_SEARCH_MATCHES + 50);
        let results = find_matches(&text, &literal("a", true, false), None);
        assert_eq!(results.matches.len(), MAX_SEARCH_MATCHES);
        assert!(results.truncated);
    }

    #[test]
    fn find_matches_not_truncated_when_under_the_cap() {
        let results = find_matches("a a a", &literal("a", true, false), None);
        assert_eq!(results.matches.len(), 3);
        assert!(!results.truncated);
    }

    #[test]
    fn replace_all_literal_replaces_every_match_verbatim_dollar_included() {
        let text = "foo foo";
        let query = literal("foo", true, false);
        let result = replace_all(text, &query, "$1 literal", None).unwrap();
        let mut buf = text.to_string();
        for change in result.transaction.changes().iter().rev() {
            buf.replace_range(change.range.clone(), &change.insert);
        }
        assert_eq!(buf, "$1 literal $1 literal");
        assert!(!result.truncated);
    }

    #[test]
    fn replace_all_regex_expands_capture_groups() {
        let text = "John Smith";
        let query = regex(r"(\w+) (\w+)", true, false);
        let result = replace_all(text, &query, "$2 $1", None).unwrap();
        let mut buf = text.to_string();
        for change in result.transaction.changes().iter().rev() {
            buf.replace_range(change.range.clone(), &change.insert);
        }
        assert_eq!(buf, "Smith John");
    }

    #[test]
    fn replace_all_returns_none_when_nothing_matches() {
        let query = literal("zzz", true, false);
        assert!(replace_all("abc", &query, "x", None).is_none());
    }

    #[test]
    fn replace_all_reports_truncated_and_still_builds_a_valid_transaction() {
        let text = "a".repeat(MAX_SEARCH_MATCHES + 10);
        let query = literal("a", true, false);
        let result = replace_all(&text, &query, "b", None).unwrap();
        assert!(result.truncated);
        assert_eq!(result.transaction.changes().len(), MAX_SEARCH_MATCHES);
    }

    #[test]
    fn replace_one_literal_builds_a_single_change() {
        let query = literal("foo", true, false);
        let tx = replace_one("foo bar", &query, 0..3, "baz");
        assert_eq!(tx.changes().len(), 1);
        assert_eq!(tx.changes()[0].insert, "baz");
    }

    #[test]
    fn replace_one_regex_expands_capture_groups_against_the_matched_slice() {
        let text = "John Smith";
        let query = regex(r"(\w+) (\w+)", true, false);
        let tx = replace_one(text, &query, 0..text.len(), "$2 $1");
        assert_eq!(tx.changes()[0].insert, "Smith John");
    }

    #[test]
    fn replace_all_scoped_only_touches_matches_in_scope() {
        let text = "foo foo foo";
        let query = literal("foo", true, false);
        let result = replace_all(text, &query, "bar", Some(4..7)).unwrap();
        assert_eq!(result.transaction.changes().len(), 1);
        assert_eq!(result.transaction.changes()[0].range, 4..7);
    }
}
