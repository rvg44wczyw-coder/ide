//! The two built-in palettes (`docs/features/intellij-look-foundation.md`
//! §2.2). Every value here was picked to clear a contrast floor from that
//! doc's tables -- the tests at the bottom of this file are what keep that
//! true, so a "nicer" colour that fails a floor fails the build.

use super::{Colors, Radii, Spacing, SyntaxColors, TextSizes, Tokens};
use eframe::egui::Color32;

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// Shared across both palettes: only colours differ between themes.
const SPACE: Spacing = Spacing {
    xs: 2.0,
    sm: 4.0,
    md: 8.0,
    lg: 12.0,
    xl: 16.0,
};

const RADIUS: Radii = Radii {
    sm: 3,
    md: 5,
    lg: 8,
};

const TEXT: TextSizes = TextSizes {
    small: 11.0,
    body: 13.0,
    heading: 15.0,
    code: 13.0,
};

pub const DARCULA: Tokens = Tokens {
    color: Colors {
        bg_base: rgb(0x31, 0x33, 0x35),
        bg_elevated: rgb(0x3A, 0x3B, 0x3F),
        bg_editor: rgb(0x2B, 0x2D, 0x30),
        bg_hover: rgb(0x43, 0x45, 0x4A),
        bg_active: rgb(0x4B, 0x4D, 0x52),
        border: rgb(0x45, 0x47, 0x4D),
        border_strong: rgb(0x5A, 0x5D, 0x63),
        fg_primary: rgb(0xDF, 0xE1, 0xE5),
        fg_secondary: rgb(0x9D, 0xA0, 0xA8),
        fg_muted: rgb(0x7B, 0x7E, 0x85),
        fg_on_accent: rgb(0xFF, 0xFF, 0xFF),
        accent: rgb(0x2E, 0x5F, 0xCC),
        accent_hover: rgb(0x4D, 0x7F, 0xE0),
        accent_fg: rgb(0x6D, 0x9F, 0xFF),
        danger: rgb(0xF4, 0x7D, 0x82),
        warning: rgb(0xE3, 0xA1, 0x3C),
        success: rgb(0x67, 0xC7, 0x7A),
        info: rgb(0x6D, 0x9F, 0xFF),
        selection_bg: rgb(0x2B, 0x4A, 0x73),
        caret: rgb(0x56, 0x9C, 0xFF),
        current_line_bg: rgb(0x30, 0x32, 0x36),
        bracket_match_bg: rgb(0x3A, 0x5A, 0x57),
        gutter_fg: rgb(0x78, 0x7B, 0x82),
        gutter_fg_active: rgb(0xB3, 0xB6, 0xBD),
        search_match_bg: rgb(0x46, 0x3D, 0x26),
        search_match_current_bg: rgb(0x6B, 0x52, 0x18),
        symbol_highlight_bg: rgb(0x3E, 0x33, 0x48),
        diff_added_fg: rgb(0x63, 0xB7, 0x7C),
        diff_removed_fg: rgb(0xE0, 0x8A, 0x8A),
        diff_modified_fg: rgb(0x6C, 0xA0, 0xE0),
    },
    syntax: SyntaxColors {
        keyword: rgb(0xC6, 0x78, 0xDD),
        string: rgb(0x98, 0xC3, 0x79),
        number: rgb(0xD1, 0x9A, 0x66),
        comment: rgb(0x96, 0x9B, 0xA3),
        key: rgb(0x61, 0xAF, 0xEF),
        function: rgb(0x61, 0xAF, 0xEF),
        type_: rgb(0xE5, 0xC0, 0x7B),
        macro_: rgb(0xEB, 0x7F, 0x87),
        constant: rgb(0xD1, 0x9A, 0x66),
        operator: rgb(0x56, 0xB6, 0xC4),
    },
    space: SPACE,
    radius: RADIUS,
    text: TEXT,
};

pub const INTELLIJ_LIGHT: Tokens = Tokens {
    color: Colors {
        bg_base: rgb(0xF7, 0xF8, 0xFA),
        bg_elevated: rgb(0xFF, 0xFF, 0xFF),
        bg_editor: rgb(0xFF, 0xFF, 0xFF),
        bg_hover: rgb(0xED, 0xEE, 0xF1),
        bg_active: rgb(0xE3, 0xE5, 0xEA),
        border: rgb(0xDF, 0xE1, 0xE5),
        border_strong: rgb(0xC4, 0xC7, 0xCE),
        fg_primary: rgb(0x1F, 0x20, 0x23),
        fg_secondary: rgb(0x5C, 0x5F, 0x66),
        fg_muted: rgb(0x8A, 0x8D, 0x95),
        fg_on_accent: rgb(0xFF, 0xFF, 0xFF),
        accent: rgb(0x2E, 0x5F, 0xCC),
        accent_hover: rgb(0x1D, 0x4F, 0xB8),
        accent_fg: rgb(0x1B, 0x5C, 0xD9),
        danger: rgb(0xC4, 0x34, 0x2B),
        warning: rgb(0x9A, 0x64, 0x00),
        success: rgb(0x2E, 0x7D, 0x3A),
        info: rgb(0x1B, 0x5C, 0xD9),
        selection_bg: rgb(0xCF, 0xE0, 0xFB),
        caret: rgb(0x35, 0x74, 0xF0),
        current_line_bg: rgb(0xF1, 0xF2, 0xF5),
        bracket_match_bg: rgb(0xBF, 0xE2, 0xDE),
        gutter_fg: rgb(0x8F, 0x92, 0x9C),
        gutter_fg_active: rgb(0x4A, 0x4D, 0x54),
        search_match_bg: rgb(0xFF, 0xF3, 0xC4),
        search_match_current_bg: rgb(0xFF, 0xD8, 0x66),
        symbol_highlight_bg: rgb(0xEA, 0xDC, 0xF5),
        diff_added_fg: rgb(0x2E, 0x7D, 0x3A),
        diff_removed_fg: rgb(0xC2, 0x34, 0x2B),
        diff_modified_fg: rgb(0x1A, 0x5F, 0xB4),
    },
    syntax: SyntaxColors {
        keyword: rgb(0xA6, 0x26, 0xA4),
        string: rgb(0x41, 0x83, 0x40),
        number: rgb(0x98, 0x68, 0x01),
        comment: rgb(0x72, 0x75, 0x7E),
        key: rgb(0x30, 0x6C, 0xF1),
        function: rgb(0x30, 0x6C, 0xF1),
        type_: rgb(0x9A, 0x6D, 0x00),
        macro_: rgb(0xCA, 0x4A, 0x3F),
        constant: rgb(0x98, 0x68, 0x01),
        operator: rgb(0x01, 0x7C, 0xB1),
    },
    space: SPACE,
    radius: RADIUS,
    text: TEXT,
};

/// Seeded from the `claude.ai/design` mockup `Fleet-like IDE.dc.html`
/// (`docs/features/themes.md` §2.2). Fields the mockup gives no opinion on
/// inherit `DARCULA`'s hue; `accent`/`accent_hover` and the row-tint pair
/// `current_line_bg`/`bracket_match_bg` are adjusted in lightness only
/// (mockup hue preserved) where the literal mockup/inherited value fails a
/// floor against this palette's much darker `bg_base`/`bg_editor` --
/// `docs/features/themes.md`'s own Revision notes record the exact
/// contrast numbers that drove each adjustment.
pub const EMBER: Tokens = Tokens {
    color: Colors {
        bg_base: rgb(0x1A, 0x19, 0x18),
        bg_elevated: rgb(0x20, 0x1E, 0x1D),
        bg_editor: rgb(0x1A, 0x19, 0x18),
        bg_hover: rgb(0x26, 0x24, 0x23),
        bg_active: rgb(0x2D, 0x2A, 0x29),
        border: rgb(0x30, 0x2E, 0x2D),
        border_strong: rgb(0x44, 0x41, 0x41),
        fg_primary: rgb(0xF3, 0xF2, 0xF2),
        fg_secondary: rgb(0xD7, 0xD3, 0xD3),
        fg_muted: rgb(0x9B, 0x97, 0x97),
        fg_on_accent: rgb(0xFF, 0xFF, 0xFF),
        // Mockup literal #ec3013 fails `fg_on_accent/accent` (4.20 < 4.5);
        // darkened in lightness only (same hue) to #de2d12 (4.68).
        accent: rgb(0xDE, 0x2D, 0x12),
        // Mockup literal #ff563c fails `fg_on_accent/accent_hover`
        // (3.16 < 3.5); darkened in lightness only to #ff3414 (3.65).
        accent_hover: rgb(0xFF, 0x34, 0x14),
        accent_fg: rgb(0xFF, 0x97, 0x83),
        danger: rgb(0xF4, 0x7D, 0x82),
        warning: rgb(0xE3, 0xA1, 0x3C),
        success: rgb(0x67, 0xC7, 0x7A),
        info: rgb(0x6D, 0x9F, 0xFF),
        // `rgba(236, 48, 19, 0.32)` (the mockup's `::selection` colour)
        // alpha-blended over `bg_editor`, since `selection_bg` has no
        // alpha channel to carry the 0.32 through directly.
        selection_bg: rgb(0x5D, 0x20, 0x16),
        caret: rgb(0x56, 0x9C, 0xFF),
        // Darcula's literal (0x30, 0x32, 0x36) falls outside the
        // current-line row-tint ratio ceiling (1.37 > 1.35) against this
        // palette's much darker `bg_editor`; darkened in lightness only.
        current_line_bg: rgb(0x2C, 0x2D, 0x31),
        // Darcula's literal (0x3A, 0x5A, 0x57) falls outside the
        // bracket-match row-tint ratio ceiling (2.32 > 2.05) against this
        // palette's much darker `bg_editor`; darkened in lightness only.
        bracket_match_bg: rgb(0x31, 0x4C, 0x4A),
        gutter_fg: rgb(0x78, 0x7B, 0x82),
        gutter_fg_active: rgb(0xB3, 0xB6, 0xBD),
        search_match_bg: rgb(0x46, 0x3D, 0x26),
        search_match_current_bg: rgb(0x6B, 0x52, 0x18),
        symbol_highlight_bg: rgb(0x3E, 0x33, 0x48),
        diff_added_fg: rgb(0x63, 0xB7, 0x7C),
        diff_removed_fg: rgb(0xE0, 0x8A, 0x8A),
        diff_modified_fg: rgb(0x6C, 0xA0, 0xE0),
    },
    syntax: SyntaxColors {
        keyword: rgb(0xFF, 0x56, 0x3C),
        string: rgb(0xBA, 0xB6, 0xB6),
        number: rgb(0xD1, 0x9A, 0x66),
        comment: rgb(0x9B, 0x97, 0x97),
        key: rgb(0x61, 0xAF, 0xEF),
        function: rgb(0x61, 0xAF, 0xEF),
        type_: rgb(0xFF, 0x97, 0x83),
        macro_: rgb(0xEB, 0x7F, 0x87),
        constant: rgb(0xD1, 0x9A, 0x66),
        operator: rgb(0x56, 0xB6, 0xC4),
    },
    space: SPACE,
    radius: RADIUS,
    text: TEXT,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.1 relative luminance.
    fn luminance(c: Color32) -> f64 {
        let channel = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    /// WCAG 2.1 contrast ratio, always >= 1.0.
    fn contrast_ratio(a: Color32, b: Color32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    fn palettes() -> [(&'static str, &'static Tokens); 3] {
        [
            ("DARCULA", &DARCULA),
            ("INTELLIJ_LIGHT", &INTELLIJ_LIGHT),
            ("EMBER", &EMBER),
        ]
    }

    fn assert_floor(theme: &str, what: &str, fg: Color32, bg: Color32, floor: f64) {
        let ratio = contrast_ratio(fg, bg);
        assert!(
            ratio >= floor,
            "{theme}: {what} contrast {ratio:.2} < {floor}"
        );
    }

    #[test]
    fn contrast_ratio_matches_known_values() {
        let white = Color32::WHITE;
        let black = Color32::BLACK;
        assert!((contrast_ratio(white, black) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 0.001);
        // Symmetric in its arguments.
        assert!((contrast_ratio(black, white) - contrast_ratio(white, black)).abs() < 1e-9);
    }

    #[test]
    fn body_text_clears_aaa_on_every_surface() {
        for (name, t) in palettes() {
            assert_floor(
                name,
                "fg_primary/bg_base",
                t.color.fg_primary,
                t.color.bg_base,
                7.0,
            );
            assert_floor(
                name,
                "fg_primary/bg_editor",
                t.color.fg_primary,
                t.color.bg_editor,
                7.0,
            );
        }
    }

    #[test]
    fn secondary_and_muted_text_clear_their_floors() {
        for (name, t) in palettes() {
            assert_floor(
                name,
                "fg_secondary/bg_base",
                t.color.fg_secondary,
                t.color.bg_base,
                4.5,
            );
            assert_floor(
                name,
                "fg_muted/bg_base",
                t.color.fg_muted,
                t.color.bg_base,
                3.0,
            );
        }
    }

    #[test]
    fn every_syntax_color_clears_the_floor_on_the_editor_background() {
        for (name, t) in palettes() {
            for (kind, color) in t.syntax.all() {
                assert_floor(name, kind, color, t.color.bg_editor, 4.5);
            }
        }
    }

    #[test]
    fn gutter_and_caret_clear_their_floors() {
        for (name, t) in palettes() {
            assert_floor(
                name,
                "gutter_fg/bg_editor",
                t.color.gutter_fg,
                t.color.bg_editor,
                3.0,
            );
            assert_floor(
                name,
                "gutter_fg_active/bg_editor",
                t.color.gutter_fg_active,
                t.color.bg_editor,
                4.5,
            );
            assert_floor(
                name,
                "caret/bg_editor",
                t.color.caret,
                t.color.bg_editor,
                3.0,
            );
        }
    }

    #[test]
    fn accent_carries_its_own_text_and_reads_as_foreground() {
        for (name, t) in palettes() {
            assert_floor(
                name,
                "fg_on_accent/accent",
                t.color.fg_on_accent,
                t.color.accent,
                4.5,
            );
            // Looser on hover by design (doc §2.2): a hover lighter than
            // rest costs white-text contrast, and a darker hover would read
            // as "pressed".
            assert_floor(
                name,
                "fg_on_accent/accent_hover",
                t.color.fg_on_accent,
                t.color.accent_hover,
                3.5,
            );
            assert_floor(
                name,
                "accent_fg/bg_base",
                t.color.accent_fg,
                t.color.bg_base,
                4.5,
            );
        }
    }

    #[test]
    fn status_colors_clear_the_floor_on_the_panel_background() {
        for (name, t) in palettes() {
            for (what, color) in [
                ("danger", t.color.danger),
                ("warning", t.color.warning),
                ("success", t.color.success),
                ("info", t.color.info),
            ] {
                assert_floor(name, what, color, t.color.bg_base, 4.5);
            }
        }
    }

    #[test]
    fn diff_text_is_legible_on_the_panel() {
        // The diff row tint/change-bar/highlight-box are all `diff_added_fg`/
        // `diff_removed_fg` at various `gamma_multiply` strengths applied at
        // render time (docs/features/diff-viewer-enhancements.md §3.3/§3.4),
        // not a fixed dedicated background token -- there is no static pair
        // left to floor-check for those, only the base text-on-panel case.
        for (name, t) in palettes() {
            assert_floor(
                name,
                "diff_added_fg/bg_base",
                t.color.diff_added_fg,
                t.color.bg_base,
                4.5,
            );
            assert_floor(
                name,
                "diff_removed_fg/bg_base",
                t.color.diff_removed_fg,
                t.color.bg_base,
                4.5,
            );
            assert_floor(
                name,
                "diff_modified_fg/bg_base",
                t.color.diff_modified_fg,
                t.color.bg_base,
                4.5,
            );
        }
    }

    #[test]
    fn row_tints_are_visible_without_reading_as_another_surface() {
        for (name, t) in palettes() {
            for (what, tint, hi) in [
                ("current_line_bg", t.color.current_line_bg, 1.35),
                ("bracket_match_bg", t.color.bracket_match_bg, 2.05),
                ("search_match_bg", t.color.search_match_bg, 3.20),
                (
                    "search_match_current_bg",
                    t.color.search_match_current_bg,
                    5.50,
                ),
                ("symbol_highlight_bg", t.color.symbol_highlight_bg, 3.20),
            ] {
                let ratio = contrast_ratio(tint, t.color.bg_editor);
                assert!(
                    (1.05..=hi).contains(&ratio),
                    "{name}: {what}/bg_editor tint {ratio:.2} outside 1.05..={hi}"
                );
            }
        }
    }

    #[test]
    fn a_matched_pair_never_paints_the_selection_colour() {
        for (name, t) in palettes() {
            assert_ne!(
                t.color.bracket_match_bg, t.color.selection_bg,
                "{name}: a pair inside a selection would vanish into it"
            );
        }
    }

    #[test]
    fn a_search_match_stays_visually_distinct_from_everything_else() {
        for (name, t) in palettes() {
            let c = &t.color;
            assert_ne!(
                c.search_match_bg, c.search_match_current_bg,
                "{name}: the current match must stand out from an ordinary one"
            );
            for (what, other) in [
                ("selection_bg", c.selection_bg),
                ("bracket_match_bg", c.bracket_match_bg),
                ("current_line_bg", c.current_line_bg),
            ] {
                assert_ne!(
                    c.search_match_bg, other,
                    "{name}: search_match_bg collides with {what}"
                );
                assert_ne!(
                    c.search_match_current_bg, other,
                    "{name}: search_match_current_bg collides with {what}"
                );
            }
        }
    }

    #[test]
    fn a_symbol_highlight_stays_visually_distinct_from_everything_else() {
        for (name, t) in palettes() {
            let c = &t.color;
            for (what, other) in [
                ("selection_bg", c.selection_bg),
                ("bracket_match_bg", c.bracket_match_bg),
                ("current_line_bg", c.current_line_bg),
                ("search_match_bg", c.search_match_bg),
                ("search_match_current_bg", c.search_match_current_bg),
            ] {
                assert_ne!(
                    c.symbol_highlight_bg, other,
                    "{name}: symbol_highlight_bg collides with {what}"
                );
            }
        }
    }

    #[test]
    fn the_two_palettes_are_actually_different() {
        let all = palettes();
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                let (name_a, a) = all[i];
                let (name_b, b) = all[j];
                assert_ne!(
                    a.color.bg_base, b.color.bg_base,
                    "{name_a} and {name_b} share a bg_base"
                );
                assert_ne!(
                    a.syntax.keyword, b.syntax.keyword,
                    "{name_a} and {name_b} share a syntax.keyword"
                );
            }
        }
    }

    #[test]
    fn scales_are_positive_and_ordered() {
        for (name, t) in palettes() {
            let s = &t.space;
            assert!(
                0.0 < s.xs && s.xs < s.sm && s.sm < s.md && s.md < s.lg && s.lg < s.xl,
                "{name}: spacing scale not strictly increasing"
            );
            let r = &t.radius;
            assert!(
                0 < r.sm && r.sm < r.md && r.md < r.lg,
                "{name}: radii not ordered"
            );
            let x = &t.text;
            assert!(
                0.0 < x.small && x.small < x.body && x.body < x.heading && 0.0 < x.code,
                "{name}: text sizes not ordered"
            );
        }
    }

    /// The margin assignments in `apply` cast spacing to `i8` (egui's
    /// `Margin` is `i8`-per-side), so every value has to survive that cast.
    #[test]
    fn spacing_survives_the_i8_cast_used_for_margins() {
        for (name, t) in palettes() {
            for (what, v) in [
                ("xs", t.space.xs),
                ("sm", t.space.sm),
                ("md", t.space.md),
                ("lg", t.space.lg),
                ("xl", t.space.xl),
            ] {
                assert!(
                    v.fract() == 0.0 && v <= f32::from(i8::MAX),
                    "{name}: space.{what} = {v} does not survive `as i8`"
                );
            }
        }
    }
}
