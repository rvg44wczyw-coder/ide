//! The minimal double-tap detector `docs/roadmap.md` §5.1 assigns to A3 for
//! `⌥⌥`+`↑`/`↓`, and that G2 later generalises into the command registry
//! (`docs/features/multiple-cursors.md` §2.2).
//!
//! Deliberately not a timer: it is fed the frame's time, so every rule it
//! encodes is testable without a clock.

/// Two presses of the tracked modifier within this long arm the gesture.
pub const DOUBLE_TAP_WINDOW: f64 = 0.35;

/// How long the armed state survives without an arrow key.
pub const ARMED_WINDOW: f64 = 1.0;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct DoubleTap {
    last_press: Option<f64>,
    armed_until: Option<f64>,
}

impl DoubleTap {
    /// Call on the modifier's rising edge. Returns whether this press armed
    /// the gesture.
    pub fn press(&mut self, now: f64) -> bool {
        let armed = self
            .last_press
            .is_some_and(|last| now - last <= DOUBLE_TAP_WINDOW);
        // Kept even when it armed, so a third tap inside the window arms
        // again rather than starting the count from scratch.
        self.last_press = Some(now);
        self.armed_until = armed.then_some(now + ARMED_WINDOW);
        armed
    }

    pub fn is_armed(&self, now: f64) -> bool {
        self.armed_until.is_some_and(|until| now <= until)
    }

    /// Drops the armed state without forgetting the last press -- releasing
    /// the modifier ends the gesture, but the press that ended it is still
    /// the first half of the next double-tap.
    pub fn disarm(&mut self) {
        self.armed_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_presses_inside_the_window_arm_the_gesture() {
        let mut tap = DoubleTap::default();
        assert!(!tap.press(0.0));
        assert!(!tap.is_armed(0.0));
        assert!(tap.press(0.2));
        assert!(tap.is_armed(0.2));
    }

    #[test]
    fn two_presses_outside_the_window_do_not() {
        let mut tap = DoubleTap::default();
        assert!(!tap.press(0.0));
        assert!(!tap.press(DOUBLE_TAP_WINDOW + 0.01));
        assert!(!tap.is_armed(DOUBLE_TAP_WINDOW + 0.01));
    }

    #[test]
    fn the_armed_state_expires() {
        let mut tap = DoubleTap::default();
        tap.press(0.0);
        assert!(tap.press(0.1));
        assert!(tap.is_armed(0.1 + ARMED_WINDOW));
        assert!(!tap.is_armed(0.1 + ARMED_WINDOW + 0.01));
    }

    #[test]
    fn disarm_clears_the_armed_state_but_not_the_press() {
        let mut tap = DoubleTap::default();
        tap.press(0.0);
        assert!(tap.press(0.1));
        tap.disarm();
        assert!(!tap.is_armed(0.1));
        // The press that armed it still counts as the first half of the
        // next double-tap.
        assert!(tap.press(0.2));
    }

    #[test]
    fn a_third_press_inside_the_window_re_arms() {
        let mut tap = DoubleTap::default();
        tap.press(0.0);
        assert!(tap.press(0.2));
        assert!(tap.press(0.4));
        assert!(tap.is_armed(0.4));
    }
}
