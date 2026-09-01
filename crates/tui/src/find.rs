//! Pure in-buffer find/replace state (`docs/features/tui-find.md` §2.1,
//! extended by `docs/features/tui-replace.md` §2.1 and
//! `docs/features/tui-replace-all.md` §2.1) -- a narrow slice of
//! `ide_core::buffer_search` (literal, case-insensitive search, single-
//! match replace, and whole-buffer replace all; no regex/whole-word/
//! scope). No rendering, no `App` dependency, mirroring `T3`'s
//! `highlight.rs` precedent of keeping tested pure logic out of `ui.rs`
//! so that file's line-coverage exemption stays unambiguous.

use std::ops::Range;

use ide_core::{
    find_matches, replace_all, replace_one, ReplaceResult, SearchOptions, SearchQuery, Transaction,
};

/// Which of the two `T5` fields (`docs/features/tui-replace.md` §2.1) is
/// currently focused. Only meaningful while `replace_mode` is `true` --
/// find-only sessions never leave `Query`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FindField {
    Query,
    Replacement,
}

pub(crate) struct FindState {
    query: String,
    matches: Vec<Range<usize>>,
    truncated: bool,
    current: Option<usize>,
    replacement: String,
    replace_mode: bool,
    field: FindField,
    replace_all_truncated: bool,
}

impl FindState {
    pub(crate) fn new() -> Self {
        Self {
            query: String::new(),
            matches: Vec::new(),
            truncated: false,
            current: None,
            replacement: String::new(),
            replace_mode: false,
            field: FindField::Query,
            replace_all_truncated: false,
        }
    }

    /// Reveals the replacement field without resetting `query`/`matches`
    /// -- mirrors `ide-ui`'s own "`⌘R` on an already-open find-only bar
    /// reveals the replace row" behavior (`docs/features/
    /// in-buffer-find-replace.md` §3.1). One-way: nothing in `T5` ever
    /// turns `replace_mode` back off short of closing the whole bar.
    pub(crate) fn enable_replace_mode(&mut self) {
        self.replace_mode = true;
    }

    pub(crate) fn replace_mode(&self) -> bool {
        self.replace_mode
    }

    pub(crate) fn field(&self) -> FindField {
        self.field
    }

    /// Flips `field` between `Query`/`Replacement`. No-op while
    /// `!replace_mode` -- there is only one field to be on in find-only
    /// mode.
    pub(crate) fn toggle_field(&mut self) {
        if !self.replace_mode {
            return;
        }
        self.field = match self.field {
            FindField::Query => FindField::Replacement,
            FindField::Replacement => FindField::Query,
        };
    }

    /// Test-only by design in `T4`: `render_status` composes the status
    /// line through `status_text` rather than reading `query` directly --
    /// kept as part of the documented interface (`docs/features/
    /// tui-find.md` §2.1) for tests to assert on the query independently
    /// of the rendered string's exact formatting.
    #[allow(dead_code)]
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn current_match(&self) -> Option<Range<usize>> {
        self.current.and_then(|i| self.matches.get(i).cloned())
    }

    pub(crate) fn push_char(&mut self, c: char, text: &str) {
        match self.field {
            FindField::Query => {
                self.query.push(c);
                self.refresh(text);
            }
            FindField::Replacement => self.replacement.push(c),
        }
    }

    pub(crate) fn pop_char(&mut self, text: &str) {
        match self.field {
            FindField::Query => {
                self.query.pop();
                self.refresh(text);
            }
            FindField::Replacement => {
                self.replacement.pop();
            }
        }
    }

    /// Re-searches `text` with the current query -- exposed so `app.rs`
    /// can resynchronize `matches`/`current` against the buffer's new
    /// content immediately after applying a replace, without `find.rs`
    /// needing to know anything about how the edit was made.
    pub(crate) fn resync(&mut self, text: &str) {
        self.refresh(text);
    }

    /// Builds the one-`Change` `Transaction` that replaces the current
    /// match with `self.replacement`. `None` if there is no current match
    /// -- the caller must not apply a transaction in that case. Does not
    /// mutate `self` or the match list itself; the caller applies the
    /// transaction to the real buffer, then calls `resync` against its
    /// new text.
    pub(crate) fn replace_current(&self, text: &str) -> Option<Transaction> {
        let current_range = self.current_match()?;
        let query = SearchQuery::compile(&self.query, SearchOptions::default())
            .expect("SearchOptions::default() selects the literal engine, which never fails");
        Some(replace_one(text, &query, current_range, &self.replacement))
    }

    /// Builds the whole-buffer replace transaction via `ide_core::
    /// replace_all` (literal query, no scope -- this crate has no "in
    /// selection" concept). `None` if there is nothing to replace,
    /// mirroring `replace_all`'s own contract. Does not mutate `self` --
    /// the caller applies the transaction, then calls `resync` and
    /// `note_replace_all_result` (`docs/features/tui-replace-all.md`
    /// §2.1/§2.2 -- in that order, since `resync` resets
    /// `replace_all_truncated` as a side effect of its own `refresh`).
    pub(crate) fn replace_all(&self, text: &str) -> Option<ReplaceResult> {
        let query = SearchQuery::compile(&self.query, SearchOptions::default())
            .expect("SearchOptions::default() selects the literal engine, which never fails");
        replace_all(text, &query, &self.replacement, None)
    }

    /// Records whether the just-applied Replace All was capped at
    /// `ide_core::MAX_SEARCH_MATCHES` -- surfaced by `status_text` until
    /// the next query/replacement edit (via `refresh`) or the next
    /// Replace All overwrites it. Must be called *after* `resync`, not
    /// before -- see `replace_all`'s doc comment.
    pub(crate) fn note_replace_all_result(&mut self, truncated: bool) {
        self.replace_all_truncated = truncated;
    }

    /// `SearchOptions::default()` selects the literal (non-regex) engine,
    /// which `SearchQuery::compile` never fails to compile -- only
    /// `options.regex: true` can produce a `SearchQueryError` (an invalid
    /// pattern), and `T4` never sets that flag (`docs/features/
    /// tui-find.md` §4.4/§6 defers a regex toggle to a later batch). This
    /// `.expect` is sound for every `String` a user can type into the
    /// query field, not just typical ones -- covered by
    /// `refresh_never_panics_across_a_wide_range_of_query_characters`
    /// below.
    fn refresh(&mut self, text: &str) {
        self.replace_all_truncated = false;
        let query = SearchQuery::compile(&self.query, SearchOptions::default())
            .expect("SearchOptions::default() selects the literal engine, which never fails");
        let results = find_matches(text, &query, None);
        self.truncated = results.truncated;
        self.current = if results.matches.is_empty() {
            None
        } else {
            Some(0)
        };
        self.matches = results.matches;
    }

    /// Advances to the next match, wrapping past the end. `None` if there
    /// are no matches.
    pub(crate) fn next(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            return None;
        }
        let next = self.current.map_or(0, |i| (i + 1) % self.matches.len());
        self.current = Some(next);
        self.current_match()
    }

    /// Same, backward.
    pub(crate) fn prev(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            return None;
        }
        let len = self.matches.len();
        let prev = self.current.map_or(len - 1, |i| (i + len - 1) % len);
        self.current = Some(prev);
        self.current_match()
    }

    pub(crate) fn status_text(&self) -> String {
        let suffix = if self.query.is_empty() {
            String::new()
        } else {
            match self.current {
                Some(idx) => {
                    let plus = if self.truncated { "+" } else { "" };
                    format!("  ({} of {}{plus})", idx + 1, self.matches.len())
                }
                None => "  (No matches)".to_string(),
            }
        };

        if !self.replace_mode {
            return if self.query.is_empty() {
                "Find: ".to_string()
            } else {
                format!("Find: {}{suffix}", self.query)
            };
        }

        let query_marker = if self.field == FindField::Query {
            "\u{25b8} "
        } else {
            "  "
        };
        let replacement_marker = if self.field == FindField::Replacement {
            "\u{25b8} "
        } else {
            "  "
        };
        let replace_all_notice = if self.replace_all_truncated {
            format!("  (capped at {}, run again)", ide_core::MAX_SEARCH_MATCHES)
        } else {
            String::new()
        };
        format!(
            "{query_marker}Find: {}  {replacement_marker}Replace: {}{suffix}{replace_all_notice}",
            self.query, self.replacement
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_has_an_empty_query_and_no_matches() {
        let find = FindState::new();
        assert_eq!(find.query(), "");
        assert_eq!(find.current_match(), None);
        assert_eq!(find.status_text(), "Find: ");
    }

    #[test]
    fn status_text_empty_query_has_no_suffix() {
        let find = FindState::new();
        assert_eq!(find.status_text(), "Find: ");
    }

    #[test]
    fn status_text_shows_n_of_m_when_there_is_a_current_match() {
        let text = "foo bar foo baz foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        assert_eq!(find.query(), "foo");
        assert_eq!(find.status_text(), "Find: foo  (1 of 3)");
    }

    #[test]
    fn status_text_shows_no_matches_for_a_non_empty_unmatched_query() {
        let text = "foo bar";
        let mut find = FindState::new();
        find.push_char('z', text);
        find.push_char('z', text);
        find.push_char('z', text);
        assert_eq!(find.status_text(), "Find: zzz  (No matches)");
        assert_eq!(find.current_match(), None);
    }

    #[test]
    fn status_text_appends_plus_when_truncated() {
        // MAX_SEARCH_MATCHES is 1000 -- a single-character literal query
        // against a long enough repeated string reaches the cap.
        let text = "a".repeat(ide_core::MAX_SEARCH_MATCHES + 10);
        let mut find = FindState::new();
        find.push_char('a', &text);
        assert!(find.status_text().ends_with(&format!(
            "({} of {}+)",
            1,
            ide_core::MAX_SEARCH_MATCHES
        )));
    }

    #[test]
    fn next_cycles_through_matches_and_wraps() {
        let text = "foo bar foo baz foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        // current starts at Some(0) after the first successful search.
        assert_eq!(find.current_match(), Some(0..3));
        assert_eq!(find.next(), Some(8..11));
        assert_eq!(find.next(), Some(16..19));
        assert_eq!(find.next(), Some(0..3)); // wraps
    }

    #[test]
    fn prev_cycles_backward_and_wraps() {
        let text = "foo bar foo baz foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        assert_eq!(find.current_match(), Some(0..3));
        // Backward from index 0 wraps to the last match.
        assert_eq!(find.prev(), Some(16..19));
        assert_eq!(find.prev(), Some(8..11));
        assert_eq!(find.prev(), Some(0..3));
    }

    #[test]
    fn next_and_prev_are_none_with_no_matches() {
        let mut find = FindState::new();
        assert_eq!(find.next(), None);
        assert_eq!(find.prev(), None);
    }

    #[test]
    fn empty_query_matches_nothing() {
        let find = FindState::new();
        assert_eq!(find.current_match(), None);
    }

    #[test]
    fn backspacing_to_an_empty_query_clears_matches() {
        let text = "foo bar foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        assert!(find.current_match().is_some());
        find.pop_char(text);
        assert_eq!(find.current_match(), None);
        assert_eq!(find.status_text(), "Find: ");
    }

    #[test]
    fn pop_char_on_an_empty_query_is_a_no_op() {
        let text = "foo";
        let mut find = FindState::new();
        find.pop_char(text);
        assert_eq!(find.query(), "");
        assert_eq!(find.current_match(), None);
    }

    #[test]
    fn current_resets_to_none_when_a_further_keystroke_matches_nothing() {
        let text = "foo bar";
        let mut find = FindState::new();
        find.push_char('f', text);
        assert!(find.current_match().is_some());
        find.push_char('z', text); // "fz" matches nothing in "foo bar"
        assert_eq!(find.current_match(), None);
    }

    #[test]
    fn refresh_never_panics_across_a_wide_range_of_query_characters() {
        // Proves the `.expect()` in `refresh` is sound for any `char` a
        // user can type, not just typical ASCII letters -- regex-special
        // characters, whitespace, and multi-byte UTF-8 must all compile
        // cleanly under the literal (non-regex) engine.
        let text = "a (b) [c] {d} $e^ f* g+ h? i| j\\k .l héllo wörld 日本語";
        let query_chars = "a(b)[c]{d}$e^f*g+h?i|j\\k.lhéllowörld日本語 \t";
        let mut find = FindState::new();
        for c in query_chars.chars() {
            find.push_char(c, text);
        }
        // Reaching here without panicking is the real assertion; also
        // sanity-check every pushed character actually landed in `query`.
        assert_eq!(find.query(), query_chars);
    }

    #[test]
    fn new_state_is_not_in_replace_mode_and_focused_on_query() {
        let find = FindState::new();
        assert!(!find.replace_mode());
        assert_eq!(find.field(), FindField::Query);
    }

    #[test]
    fn enable_replace_mode_reveals_the_row_without_resetting_query_or_matches() {
        let text = "foo bar foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        let matches_before = find.current_match();
        find.enable_replace_mode();
        assert!(find.replace_mode());
        assert_eq!(find.query(), "foo");
        assert_eq!(find.current_match(), matches_before);
    }

    #[test]
    fn enable_replace_mode_is_idempotent() {
        let mut find = FindState::new();
        find.enable_replace_mode();
        find.enable_replace_mode();
        assert!(find.replace_mode());
    }

    #[test]
    fn toggle_field_is_a_no_op_while_not_in_replace_mode() {
        let mut find = FindState::new();
        find.toggle_field();
        assert_eq!(find.field(), FindField::Query);
    }

    #[test]
    fn toggle_field_flips_between_query_and_replacement_in_replace_mode() {
        let mut find = FindState::new();
        find.enable_replace_mode();
        assert_eq!(find.field(), FindField::Query);
        find.toggle_field();
        assert_eq!(find.field(), FindField::Replacement);
        find.toggle_field();
        assert_eq!(find.field(), FindField::Query);
    }

    #[test]
    fn push_char_and_pop_char_route_to_the_focused_field() {
        let text = "foo bar foo";
        let mut find = FindState::new();
        find.enable_replace_mode();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        assert_eq!(find.query(), "foo");
        find.toggle_field();
        find.push_char('b', text);
        find.push_char('a', text);
        find.push_char('z', text);
        assert_eq!(find.query(), "foo"); // unaffected
        let matches_before = find.current_match();
        find.pop_char(text);
        assert_eq!(matches_before, find.current_match()); // matches unaffected
    }

    #[test]
    fn editing_the_replacement_field_never_triggers_a_re_search() {
        let text = "foo bar foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        find.enable_replace_mode();
        find.toggle_field();
        let matches_before = find.current_match();
        find.push_char('x', text);
        find.push_char('y', text);
        find.pop_char(text);
        assert_eq!(find.current_match(), matches_before);
    }

    #[test]
    fn replace_current_is_none_with_no_current_match() {
        let text = "foo bar";
        let mut find = FindState::new();
        find.push_char('z', text);
        assert_eq!(find.current_match(), None);
        assert_eq!(find.replace_current(text), None);
    }

    #[test]
    fn replace_current_builds_a_transaction_for_the_current_match() {
        let text = "foo bar foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        find.enable_replace_mode();
        find.toggle_field();
        find.push_char('x', text);
        let tx = find.replace_current(text).expect("current match exists");
        let mut buffer = ide_core::TextBuffer::new(text, None);
        buffer.apply(tx);
        assert_eq!(buffer.text(), "x bar foo");
    }

    #[test]
    fn resync_recomputes_matches_against_new_text() {
        let text = "foo bar foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        assert_eq!(find.current_match(), Some(0..3));
        find.resync("x bar foo");
        assert_eq!(find.current_match(), Some(6..9));
    }

    #[test]
    fn status_text_in_replace_mode_marks_the_focused_field() {
        let text = "foo bar foo foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        find.enable_replace_mode();
        assert_eq!(
            find.status_text(),
            "\u{25b8} Find: foo    Replace:   (1 of 3)"
        );
        find.toggle_field();
        find.push_char('b', text);
        find.push_char('a', text);
        find.push_char('z', text);
        assert_eq!(
            find.status_text(),
            "  Find: foo  \u{25b8} Replace: baz  (1 of 3)"
        );
    }

    #[test]
    fn status_text_replace_mode_with_empty_query_degrades_gracefully() {
        let mut find = FindState::new();
        find.enable_replace_mode();
        assert_eq!(find.status_text(), "\u{25b8} Find:     Replace: ");
    }

    #[test]
    fn replace_all_is_none_with_no_matches() {
        let text = "foo bar";
        let mut find = FindState::new();
        find.push_char('z', text);
        assert_eq!(find.replace_all(text), None);
    }

    #[test]
    fn replace_all_builds_a_transaction_replacing_every_match() {
        let text = "foo bar foo baz foo";
        let mut find = FindState::new();
        find.push_char('f', text);
        find.push_char('o', text);
        find.push_char('o', text);
        find.enable_replace_mode();
        find.toggle_field();
        find.push_char('x', text);
        let result = find.replace_all(text).expect("3 matches exist");
        assert!(!result.truncated);
        let mut buffer = ide_core::TextBuffer::new(text, None);
        buffer.apply(result.transaction);
        assert_eq!(buffer.text(), "x bar x baz x");
    }

    #[test]
    fn note_replace_all_result_surfaces_a_truncation_notice_in_status_text() {
        let text = "a".repeat(ide_core::MAX_SEARCH_MATCHES + 10);
        let mut find = FindState::new();
        find.push_char('a', &text);
        find.enable_replace_mode();
        let result = find.replace_all(&text).expect("matches exist");
        assert!(result.truncated, "MAX_SEARCH_MATCHES+10 a's must truncate");

        let mut buffer = ide_core::TextBuffer::new(text.as_str(), None);
        buffer.apply(result.transaction);
        let new_text = buffer.text().to_string();

        // Order matters: `resync` first (which resets the flag via
        // `refresh`), then `note_replace_all_result` -- the reverse order
        // is covered by the regression test below.
        find.resync(&new_text);
        find.note_replace_all_result(result.truncated);

        assert!(find.status_text().contains(&format!(
            "(capped at {}, run again)",
            ide_core::MAX_SEARCH_MATCHES
        )));
    }

    #[test]
    fn calling_note_replace_all_result_before_resync_is_clobbered_by_it() {
        // Regression for the exact ordering bug `tui-replace-all.md` §2.2
        // calls out: `resync`'s own `refresh` resets `replace_all_truncated`
        // to `false` as a side effect, so the wrong call order silently
        // loses the notice. `enable_replace_mode` is on throughout so the
        // notice would actually be rendered if it survived -- without it,
        // this assertion would trivially pass for the wrong reason.
        let text = "foo";
        let mut find = FindState::new();
        find.enable_replace_mode();
        find.push_char('f', text);
        find.note_replace_all_result(true);
        find.resync(text);
        assert!(!find.status_text().contains("capped at"));
    }

    #[test]
    fn a_fresh_query_edit_clears_a_stale_truncation_notice() {
        let mut find = FindState::new();
        find.enable_replace_mode();
        find.note_replace_all_result(true);
        assert!(find.status_text().contains("capped at"));
        find.push_char('x', "xxx");
        assert!(!find.status_text().contains("capped at"));
    }
}
