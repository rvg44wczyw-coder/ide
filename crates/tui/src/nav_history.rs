//! Back/forward navigation (`docs/features/tui-back-forward-navigation.md`,
//! T31) -- near-verbatim port of `crates/ui/src/nav_history.rs`. No `App`
//! dependency, same boundary `commands.rs`/`keymap.rs` hold.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct NavLocation {
    pub path: PathBuf,
    pub offset: usize,
}

#[derive(Default)]
pub struct NavHistory {
    entries: Vec<NavLocation>,
    /// Index of the *current* location within `entries`, or `None` when
    /// empty. Back/forward move this index; they never remove entries.
    current: Option<usize>,
}

impl NavHistory {
    /// Pushes `location` as the new current entry. Any entries past the
    /// old `current` (the "forward" branch) are dropped first -- standard
    /// browser-history semantics: navigating from the middle of history
    /// abandons the old forward branch rather than branching it.
    /// A push identical to the current entry's `path` (same file, new
    /// offset from cursor movement within it) replaces that entry instead
    /// of growing history with every keystroke.
    pub fn push(&mut self, location: NavLocation) {
        if let Some(current) = self.current {
            if self.entries[current].path == location.path {
                self.entries[current] = location;
                return;
            }
            self.entries.truncate(current + 1);
        }
        self.entries.push(location);
        self.current = Some(self.entries.len() - 1);
    }

    pub fn can_go_back(&self) -> bool {
        matches!(self.current, Some(i) if i > 0)
    }

    pub fn can_go_forward(&self) -> bool {
        matches!(self.current, Some(i) if i + 1 < self.entries.len())
    }

    /// Moves `current` back one and returns that entry, or `None` if
    /// already at the oldest entry.
    pub fn go_back(&mut self) -> Option<NavLocation> {
        let current = self.current?;
        if current == 0 {
            return None;
        }
        self.current = Some(current - 1);
        self.entries.get(current - 1).cloned()
    }

    pub fn go_forward(&mut self) -> Option<NavLocation> {
        let current = self.current?;
        if current + 1 >= self.entries.len() {
            return None;
        }
        self.current = Some(current + 1);
        self.entries.get(current + 1).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(path: &str, offset: usize) -> NavLocation {
        NavLocation {
            path: PathBuf::from(path),
            offset,
        }
    }

    #[test]
    fn empty_history_cannot_go_back_or_forward() {
        let nav = NavHistory::default();
        assert!(!nav.can_go_back());
        assert!(!nav.can_go_forward());
    }

    #[test]
    fn push_then_back_and_forward() {
        let mut nav = NavHistory::default();
        nav.push(loc("a.rs", 0));
        nav.push(loc("b.rs", 5));

        assert!(nav.can_go_back());
        assert!(!nav.can_go_forward());

        assert_eq!(nav.go_back(), Some(loc("a.rs", 0)));
        assert!(!nav.can_go_back());
        assert!(nav.can_go_forward());

        assert_eq!(nav.go_forward(), Some(loc("b.rs", 5)));
        assert!(!nav.can_go_forward());
    }

    #[test]
    fn go_back_at_oldest_entry_returns_none() {
        let mut nav = NavHistory::default();
        nav.push(loc("a.rs", 0));
        assert_eq!(nav.go_back(), None);
    }

    #[test]
    fn go_forward_at_newest_entry_returns_none() {
        let mut nav = NavHistory::default();
        nav.push(loc("a.rs", 0));
        assert_eq!(nav.go_forward(), None);
    }

    #[test]
    fn same_file_push_coalesces_offset_instead_of_appending() {
        let mut nav = NavHistory::default();
        nav.push(loc("a.rs", 0));
        nav.push(loc("a.rs", 42));

        assert!(!nav.can_go_back());
        assert_eq!(nav.go_forward(), None);
    }

    #[test]
    fn push_from_the_middle_of_history_truncates_the_forward_branch() {
        let mut nav = NavHistory::default();
        nav.push(loc("a.rs", 0));
        nav.push(loc("b.rs", 0));
        nav.push(loc("c.rs", 0));

        nav.go_back(); // now at b.rs
        nav.go_back(); // now at a.rs

        nav.push(loc("d.rs", 0));

        assert!(!nav.can_go_forward());
        assert_eq!(nav.go_back(), Some(loc("a.rs", 0)));
    }

    #[test]
    fn different_file_jump_within_history_grows_a_new_entry() {
        let mut nav = NavHistory::default();
        nav.push(loc("a.rs", 0));
        nav.push(loc("b.rs", 5));
        nav.push(loc("a.rs", 99));

        assert_eq!(nav.go_back(), Some(loc("b.rs", 5)));
        assert_eq!(nav.go_back(), Some(loc("a.rs", 0)));
        assert_eq!(nav.go_back(), None);
    }
}
