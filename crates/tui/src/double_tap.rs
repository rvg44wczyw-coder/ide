//! The double-tap detector `docs/features/tui-unified-finder.md` §2.3
//! assigns to `T46`'s `⇧⇧` Search Everywhere gesture -- a trimmed port of
//! `ide-ui`'s own `editor::double_tap::DoubleTap`, which backs its
//! already-shipped Search Everywhere gesture the same way.
//!
//! Deliberately not a timer: it is fed the caller's own clock reading, so
//! every rule it encodes is testable without a real clock.

/// Two presses within this window count as a double-tap.
pub const DOUBLE_TAP_WINDOW: f64 = 0.35;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct DoubleTap {
    last_press: Option<f64>,
}

impl DoubleTap {
    /// Call on every tap. Returns whether this tap completed a
    /// double-tap (i.e. a previous tap landed within `DOUBLE_TAP_WINDOW`
    /// of `now`).
    pub(crate) fn press(&mut self, now: f64) -> bool {
        let armed = self
            .last_press
            .is_some_and(|last| now - last <= DOUBLE_TAP_WINDOW);
        self.last_press = Some(now);
        armed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_presses_inside_the_window_complete_a_double_tap() {
        let mut tap = DoubleTap::default();
        assert!(!tap.press(0.0));
        assert!(tap.press(0.2));
    }

    #[test]
    fn two_presses_outside_the_window_do_not() {
        let mut tap = DoubleTap::default();
        assert!(!tap.press(0.0));
        assert!(!tap.press(0.5));
    }

    #[test]
    fn a_third_press_right_after_a_completed_double_tap_arms_again() {
        let mut tap = DoubleTap::default();
        assert!(!tap.press(0.0));
        assert!(tap.press(0.2));
        assert!(tap.press(0.3));
    }

    #[test]
    fn exactly_the_window_boundary_counts() {
        let mut tap = DoubleTap::default();
        assert!(!tap.press(0.0));
        assert!(tap.press(DOUBLE_TAP_WINDOW));
    }

    #[test]
    fn a_single_press_never_completes_on_its_own() {
        let mut tap = DoubleTap::default();
        assert!(!tap.press(1.0));
    }
}
