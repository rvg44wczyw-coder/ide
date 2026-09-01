//! Per-tab find/replace session state (`docs/features/in-buffer-find-replace.md`
//! §2.2). No `egui` dependency here -- the panel that reads this lives in
//! `app/render.rs`, so every method here is plain, unit-testable state
//! logic, the same split `app.rs`'s own module doc establishes between
//! itself and `app/render.rs`.

use std::ops::Range;

use ide_core::{find_matches, MatchResults, SearchOptions, SearchQuery, SearchQueryError};

/// Per-tab find/replace session -- one `FindBar` per `Tab`, not one shared
/// by the whole app, because "3 of 17" and "which match is current" are
/// meaningless once you're not looking at the buffer they were computed
/// against (§3.7 covers what *does* carry over between tabs).
#[derive(Default)]
pub struct FindBar {
    open: bool,
    replace_open: bool,
    query: String,
    replacement: String,
    options: SearchOptions,
    /// Restrict matches to the selection active when the bar was opened or
    /// "In Selection" was turned on (§3.5). `None` means "whole buffer."
    scope: Option<Range<usize>>,
    matches: Vec<Range<usize>>,
    truncated: bool,
    /// Index into `matches` of the current match, `None` when `matches` is
    /// empty.
    current: Option<usize>,
    /// Set when `query`, compiled with `regex: true`, fails to parse.
    /// Cleared on the next successful compile. Mutually exclusive with a
    /// non-empty `matches` -- a query that doesn't compile has no matches.
    error: Option<String>,
}

impl FindBar {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the replace row is showing.
    pub fn replace_open(&self) -> bool {
        self.replace_open
    }

    /// Opens the bar. `with_replace` sets `replace_open`; opening an
    /// already-open bar with `with_replace: true` just reveals the replace
    /// row without resetting `query`/`matches` (§3.1's `⌘R`-on-open-bar
    /// case) -- it never turns `replace_open` back off, so a plain `⌘F` on
    /// an already-`with_replace` bar leaves the replace row showing.
    /// `initial_query`, if `Some`, replaces `query` and re-searches (the
    /// "seed from selection" behaviour, §3.1); `None` leaves whatever query
    /// was already there (possibly from a previous session on this tab) and
    /// re-searches `text` with it, since `text` may have changed since the
    /// bar was last open.
    pub fn open(&mut self, with_replace: bool, initial_query: Option<String>, text: &str) {
        self.open = true;
        self.replace_open = self.replace_open || with_replace;
        if let Some(query) = initial_query {
            self.query = query;
        }
        self.refresh(text, None);
    }

    /// Clears `matches`/`current`/`error` and `open`/`replace_open`.
    /// Deliberately keeps `query`/`replacement`/`options`/`scope` -- the
    /// next `open` on this tab starts from where this one left off (§3.7).
    pub fn close(&mut self) {
        self.matches.clear();
        self.truncated = false;
        self.current = None;
        self.error = None;
        self.open = false;
        self.replace_open = false;
    }

    /// Recompiles and re-searches `text` with the current `query`/
    /// `options`/`scope`. Called after every edit to `query`, every toggle,
    /// and after `text` itself changes (an edit in the buffer, or a switch
    /// back to this tab after an external reload, §3.6). `near`, if given,
    /// picks the match `current` lands on (the first match at or after
    /// `near`, wrapping); `None` keeps `current` at the same match index if
    /// one still exists at that index, else `0` if `matches` is non-empty,
    /// else `None`.
    pub fn refresh(&mut self, text: &str, near: Option<usize>) {
        match SearchQuery::compile(&self.query, self.options) {
            Ok(query) => {
                let MatchResults { matches, truncated } =
                    find_matches(text, &query, self.scope.clone());
                self.current = Self::pick_current(&matches, self.current, near);
                self.matches = matches;
                self.truncated = truncated;
                self.error = None;
            }
            Err(SearchQueryError::InvalidRegex(message)) => {
                self.matches.clear();
                self.truncated = false;
                self.current = None;
                self.error = Some(message);
            }
        }
    }

    fn pick_current(
        matches: &[Range<usize>],
        previous: Option<usize>,
        near: Option<usize>,
    ) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }
        if let Some(near) = near {
            return Some(matches.iter().position(|m| m.start >= near).unwrap_or(0));
        }
        match previous {
            Some(index) if index < matches.len() => Some(index),
            _ => Some(0),
        }
    }

    pub fn set_query(&mut self, query: String, text: &str) {
        self.query = query;
        self.refresh(text, None);
    }

    pub fn set_replacement(&mut self, replacement: String) {
        self.replacement = replacement;
    }

    pub fn set_case_sensitive(&mut self, on: bool, text: &str) {
        self.options.case_sensitive = on;
        self.refresh(text, None);
    }

    pub fn set_whole_word(&mut self, on: bool, text: &str) {
        self.options.whole_word = on;
        self.refresh(text, None);
    }

    pub fn set_regex(&mut self, on: bool, text: &str) {
        self.options.regex = on;
        self.refresh(text, None);
    }

    /// `Some(range)` turns scope restriction on with that range (typically
    /// the selection active the moment the checkbox was ticked); `None`
    /// turns it off. Either way, re-searches `text`.
    pub fn set_scope(&mut self, scope: Option<Range<usize>>, text: &str) {
        self.scope = scope;
        self.refresh(text, None);
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn replacement(&self) -> &str {
        &self.replacement
    }

    pub fn options(&self) -> SearchOptions {
        self.options
    }

    pub fn is_scoped(&self) -> bool {
        self.scope.is_some()
    }

    /// The raw scope range, if any -- not in the doc's original §2.2 list:
    /// `IdeApp::replace_all_matches` (§2.3) has to pass `scope` through to
    /// `ide_core::replace_all`, and `is_scoped`'s plain `bool` can't supply
    /// that. See `docs/features/in-buffer-find-replace.md`'s revision notes.
    pub fn scope(&self) -> Option<Range<usize>> {
        self.scope.clone()
    }

    pub fn matches(&self) -> &[Range<usize>] {
        &self.matches
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn current_match(&self) -> Option<Range<usize>> {
        self.current
            .and_then(|index| self.matches.get(index))
            .cloned()
    }

    /// 0-based index for internal use; the panel displays `current_index()
    /// + 1` (§3.4's "3 of 17").
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Advances `current` to the next match, wrapping past the end.
    /// Returns the new current match, or `None` if `matches` is empty.
    pub fn next(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            return None;
        }
        self.current = Some(match self.current {
            Some(index) => (index + 1) % self.matches.len(),
            None => 0,
        });
        self.current_match()
    }

    /// Same, backward.
    pub fn prev(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            return None;
        }
        self.current = Some(match self.current {
            Some(0) | None => self.matches.len() - 1,
            Some(index) => index - 1,
        });
        self.current_match()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_bar_is_closed_with_no_query_or_matches() {
        let bar = FindBar::default();
        assert!(!bar.is_open());
        assert!(!bar.replace_open());
        assert_eq!(bar.query(), "");
        assert!(bar.matches().is_empty());
        assert_eq!(bar.current_match(), None);
    }

    #[test]
    fn open_seeds_the_query_and_searches() {
        let mut bar = FindBar::default();
        bar.open(false, Some("needle".to_string()), "a needle in a haystack");
        assert!(bar.is_open());
        assert!(!bar.replace_open());
        assert_eq!(bar.query(), "needle");
        assert_eq!(bar.matches().len(), 1);
        assert_eq!(bar.matches()[0], 2..8);
        assert_eq!(bar.current_index(), Some(0));
    }

    #[test]
    fn open_with_no_initial_query_reuses_and_researches() {
        let mut bar = FindBar::default();
        bar.open(false, Some("foo".to_string()), "foo foo");
        bar.close();
        // Text changed while closed -- reopening must re-search it.
        bar.open(false, None, "foo foo foo");
        assert_eq!(bar.query(), "foo");
        assert_eq!(bar.matches().len(), 3);
    }

    #[test]
    fn with_replace_reveals_the_row_and_never_hides_it_again() {
        let mut bar = FindBar::default();
        bar.open(false, None, "text");
        assert!(!bar.replace_open());
        bar.open(true, None, "text");
        assert!(bar.replace_open());
        // A plain ⌘F on an already-with_replace bar must not collapse it.
        bar.open(false, None, "text");
        assert!(bar.replace_open());
    }

    #[test]
    fn close_keeps_query_and_options_but_clears_matches() {
        let mut bar = FindBar::default();
        bar.set_case_sensitive(true, "");
        bar.open(false, Some("x".to_string()), "x x x");
        bar.close();
        assert!(!bar.is_open());
        assert_eq!(bar.query(), "x");
        assert!(bar.options().case_sensitive);
        assert!(bar.matches().is_empty());
        assert_eq!(bar.current_match(), None);
    }

    #[test]
    fn set_query_recompiles_and_researches() {
        let mut bar = FindBar::default();
        bar.open(false, Some("a".to_string()), "abc abc");
        bar.set_query("b".to_string(), "abc abc");
        assert_eq!(bar.query(), "b");
        assert_eq!(bar.matches().len(), 2);
    }

    #[test]
    fn invalid_regex_sets_error_and_clears_matches() {
        let mut bar = FindBar::default();
        bar.set_regex(true, "");
        bar.open(false, Some("foo".to_string()), "foo foo");
        assert!(!bar.matches().is_empty() || bar.error().is_none());

        bar.set_query("(unclosed".to_string(), "foo foo");
        assert!(bar.error().is_some());
        assert!(bar.matches().is_empty());
        assert_eq!(bar.current_match(), None);
    }

    #[test]
    fn a_later_valid_query_clears_a_previous_error() {
        let mut bar = FindBar::default();
        bar.set_regex(true, "");
        bar.open(false, Some("(unclosed".to_string()), "foo");
        assert!(bar.error().is_some());

        bar.set_query("foo".to_string(), "foo");
        assert!(bar.error().is_none());
        assert_eq!(bar.matches().len(), 1);
    }

    #[test]
    fn whole_word_and_case_sensitive_toggles_research_immediately() {
        let mut bar = FindBar::default();
        bar.open(false, Some("Foo".to_string()), "Foo foo Foobar");
        assert_eq!(
            bar.matches().len(),
            3,
            "case-insensitive by default: Foo, foo, and Foo-in-Foobar"
        );

        bar.set_case_sensitive(true, "Foo foo Foobar");
        assert_eq!(bar.matches().len(), 2, "Foo and Foobar, case-sensitive");

        bar.set_whole_word(true, "Foo foo Foobar");
        assert_eq!(bar.matches().len(), 1, "only the standalone Foo");
    }

    #[test]
    fn set_scope_restricts_and_can_be_cleared() {
        let mut bar = FindBar::default();
        bar.open(false, Some("foo".to_string()), "foo foo foo");
        assert_eq!(bar.matches().len(), 3);
        assert!(!bar.is_scoped());
        assert_eq!(bar.scope(), None);

        bar.set_scope(Some(4..7), "foo foo foo");
        assert!(bar.is_scoped());
        assert_eq!(bar.scope(), Some(4..7));
        assert_eq!(bar.matches().len(), 1);
        assert_eq!(bar.matches()[0], 4..7);

        bar.set_scope(None, "foo foo foo");
        assert!(!bar.is_scoped());
        assert_eq!(bar.matches().len(), 3);
    }

    #[test]
    fn next_and_prev_wrap() {
        let mut bar = FindBar::default();
        bar.open(false, Some("x".to_string()), "x x x");
        assert_eq!(bar.current_index(), Some(0));

        assert_eq!(bar.next(), Some(2..3));
        assert_eq!(bar.current_index(), Some(1));
        assert_eq!(bar.next(), Some(4..5));
        assert_eq!(bar.next(), Some(0..1), "wraps past the last match");

        assert_eq!(bar.prev(), Some(4..5), "wraps back past the first");
        assert_eq!(bar.prev(), Some(2..3));
    }

    #[test]
    fn next_and_prev_are_noops_with_no_matches() {
        let mut bar = FindBar::default();
        bar.open(false, Some("zzz".to_string()), "abc");
        assert_eq!(bar.next(), None);
        assert_eq!(bar.prev(), None);
    }

    #[test]
    fn refresh_with_near_lands_on_the_first_match_at_or_after_it() {
        let mut bar = FindBar::default();
        bar.open(false, Some("x".to_string()), "x x x x");
        bar.refresh("x x x x", Some(3));
        // Matches at 0, 2, 4, 6; the first at or after 3 is at 4.
        assert_eq!(bar.current_match(), Some(4..5));
    }

    #[test]
    fn refresh_with_near_past_every_match_wraps_to_the_first() {
        let mut bar = FindBar::default();
        bar.open(false, Some("x".to_string()), "x x");
        bar.refresh("x x", Some(100));
        assert_eq!(bar.current_match(), Some(0..1));
    }

    #[test]
    fn refresh_without_near_keeps_the_current_index_when_still_valid() {
        let mut bar = FindBar::default();
        bar.open(false, Some("x".to_string()), "x x x");
        bar.next(); // current = 1
        assert_eq!(bar.current_index(), Some(1));
        bar.refresh("x x x", None);
        assert_eq!(bar.current_index(), Some(1));
    }

    #[test]
    fn refresh_without_near_falls_back_to_zero_when_the_index_no_longer_exists() {
        let mut bar = FindBar::default();
        bar.open(false, Some("x".to_string()), "x x x");
        bar.next();
        bar.next(); // current = 2
        bar.refresh("x", None); // now only one match
        assert_eq!(bar.current_index(), Some(0));
    }

    #[test]
    fn empty_query_matches_nothing_and_is_not_an_error() {
        let mut bar = FindBar::default();
        bar.open(false, Some(String::new()), "anything");
        assert!(bar.matches().is_empty());
        assert!(bar.error().is_none());
    }
}
