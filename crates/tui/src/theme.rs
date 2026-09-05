//! Selectable color palettes (`docs/features/tui-theme.md`, `T41`) --
//! reverses the earlier `docs/roadmap.md` decision that `ide-tui` gets no
//! theme system. `ThemeKind::Classic` is every color `highlight.rs`/
//! `ui.rs` hardcoded before this feature, extracted verbatim (selecting it
//! is a zero-visual-change no-op); `ThemeKind::Ember` is seeded from a
//! `claude.ai/design` terminal-IDE mockup's palette, applied only to
//! fields the mockup actually gives an opinion on (see `EMBER`'s own field
//! comments for which ones keep `CLASSIC`'s value instead of inventing an
//! off-palette hue).

use ratatui::style::Color;

/// Which built-in palette is active. `Copy` + `serde` so it can live
/// directly in `App` and round-trip through `PersistedState` (`state.rs`)
/// the same way `format_on_save: bool` already does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ThemeKind {
    #[default]
    Classic,
    Ember,
}

impl ThemeKind {
    /// All themes, in the exact order the Theme Settings popup lists them.
    pub const ALL: [ThemeKind; 2] = [ThemeKind::Classic, ThemeKind::Ember];

    /// Display name for the popup and any future command-palette label.
    pub fn label(self) -> &'static str {
        match self {
            ThemeKind::Classic => "Classic",
            ThemeKind::Ember => "Ember",
        }
    }

    /// The palette backing this theme. `&'static` -- both palettes are
    /// compile-time constants (mirrors `ide-ui`'s `Theme::tokens()` shape
    /// in `crates/ui/src/theme/mod.rs`), so resolving one is a field read,
    /// never an allocation.
    pub fn theme(self) -> &'static Theme {
        match self {
            ThemeKind::Classic => &CLASSIC,
            ThemeKind::Ember => &EMBER,
        }
    }
}

/// Ten syntax-token colors, one per `ide_core::TokenKind` variant that
/// `highlight.rs::style_for` gives a distinct color (mirrors that file's
/// own doc comment: "mirrors `crates/ui/src/theme/mod.rs`'s
/// `SyntaxColors::of` shape"). `Punctuation`/`Variable` are intentionally
/// absent -- both stay `Style::default()` (no color at all) in every
/// theme, unchanged from today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxColors {
    pub keyword: Color,
    pub string: Color,
    pub number: Color,
    pub comment: Color,
    pub key: Color,
    pub function: Color,
    pub r#type: Color,
    pub r#macro: Color,
    pub constant: Color,
    pub operator: Color,
}

/// Every color `highlight.rs`/`ui.rs` hardcoded as a literal `Color::`
/// value before this feature -- one field per call site's *semantic
/// role*, not one per literal (several sites already share a role, e.g.
/// every generic error-text display was `Color::Red`; they share
/// `error_text` here too). See `docs/features/tui-theme.md` §3.2/§2.3 for
/// the exact old-literal -> field mapping and the full `ui.rs` line-by-line
/// walkthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub syntax: SyntaxColors,

    // highlight.rs overlay washes (`styled_line`, `LineOverlays`)
    pub wash_document_highlight: Color,
    pub wash_selection: Color,
    pub wash_bracket_pair: Color,
    pub wash_breakpoint_verified: Color,
    pub wash_breakpoint_unverified: Color,
    pub chip_fg: Color,

    // ui.rs chrome
    pub gutter_fg: Color,
    pub fold_marker_fg: Color,
    pub blame_lane_fg: Color,
    pub right_margin_guide_bg: Color,
    pub git_added: Color,
    pub git_modified: Color,
    pub git_deleted: Color,
    pub git_none: Color,
    pub diff_added: Color,
    pub diff_removed: Color,
    pub error_text: Color,
    pub focus_indicator: Color,
}

/// Today's exact colors, extracted verbatim -- selecting `Classic`
/// produces byte-for-byte identical rendered output to `ide-tui` before
/// `T41` (`docs/features/tui-theme.md` §4).
pub static CLASSIC: Theme = Theme {
    syntax: SyntaxColors {
        keyword: Color::Magenta,
        string: Color::Green,
        number: Color::LightYellow,
        comment: Color::DarkGray,
        key: Color::Blue,
        function: Color::Cyan,
        r#type: Color::LightCyan,
        r#macro: Color::LightMagenta,
        constant: Color::LightRed,
        operator: Color::Red,
    },
    wash_document_highlight: Color::DarkGray,
    wash_selection: Color::Yellow,
    wash_bracket_pair: Color::Blue,
    wash_breakpoint_verified: Color::Red,
    wash_breakpoint_unverified: Color::DarkGray,
    chip_fg: Color::DarkGray,
    gutter_fg: Color::DarkGray,
    fold_marker_fg: Color::DarkGray,
    blame_lane_fg: Color::DarkGray,
    right_margin_guide_bg: Color::DarkGray,
    git_added: Color::Green,
    git_modified: Color::Blue,
    git_deleted: Color::Red,
    git_none: Color::DarkGray,
    diff_added: Color::Green,
    diff_removed: Color::Red,
    error_text: Color::Red,
    focus_indicator: Color::Yellow,
};

/// Seeded from the mockup's palette. Fields with a direct hex source
/// there take that color (as `Color::Rgb` truecolor, which this crate
/// already uses elsewhere for `claude_terminal.rs`'s xterm-palette
/// mapping); fields the mockup never gives an opinion on keep `CLASSIC`'s
/// value rather than inventing an off-palette hue (`docs/features/
/// tui-theme.md` §3.2 explains each such field's "kept" reasoning).
pub static EMBER: Theme = Theme {
    syntax: SyntaxColors {
        keyword: Color::Rgb(0xff, 0x56, 0x3c),
        string: Color::Rgb(0xba, 0xb6, 0xb6),
        number: Color::Rgb(0xff, 0x97, 0x83),
        comment: Color::Rgb(0x9b, 0x97, 0x97),
        key: Color::Rgb(0xff, 0x97, 0x83),
        function: Color::Rgb(0xea, 0xe7, 0xe7),
        r#type: Color::Rgb(0xff, 0x97, 0x83),
        r#macro: Color::Rgb(0xff, 0x56, 0x3c),
        constant: Color::Rgb(0xff, 0x97, 0x83),
        operator: Color::Rgb(0xff, 0x56, 0x3c),
    },
    wash_document_highlight: Color::DarkGray,
    wash_selection: Color::Yellow,
    wash_bracket_pair: Color::Blue,
    wash_breakpoint_verified: Color::Rgb(0xec, 0x30, 0x13),
    wash_breakpoint_unverified: Color::DarkGray,
    chip_fg: Color::DarkGray,
    gutter_fg: Color::Rgb(0x9b, 0x97, 0x97),
    fold_marker_fg: Color::Rgb(0x9b, 0x97, 0x97),
    blame_lane_fg: Color::Rgb(0x9b, 0x97, 0x97),
    right_margin_guide_bg: Color::Rgb(0x44, 0x41, 0x41),
    git_added: Color::Green,
    git_modified: Color::Blue,
    git_deleted: Color::Rgb(0xec, 0x30, 0x13),
    git_none: Color::Rgb(0x9b, 0x97, 0x97),
    diff_added: Color::Green,
    diff_removed: Color::Rgb(0xec, 0x30, 0x13),
    error_text: Color::Rgb(0xec, 0x30, 0x13),
    focus_indicator: Color::Yellow,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_kind_is_classic() {
        assert_eq!(ThemeKind::default(), ThemeKind::Classic);
    }

    #[test]
    fn all_lists_both_kinds_in_display_order() {
        assert_eq!(ThemeKind::ALL, [ThemeKind::Classic, ThemeKind::Ember]);
    }

    #[test]
    fn label_is_distinct_and_non_empty_for_every_kind() {
        let labels: Vec<&str> = ThemeKind::ALL.iter().map(|k| k.label()).collect();
        assert!(labels.iter().all(|l| !l.is_empty()));
        assert_ne!(labels[0], labels[1]);
    }

    #[test]
    fn theme_resolves_each_kind_to_its_own_static() {
        assert_eq!(
            ThemeKind::Classic.theme() as *const Theme,
            &CLASSIC as *const Theme
        );
        assert_eq!(
            ThemeKind::Ember.theme() as *const Theme,
            &EMBER as *const Theme
        );
    }

    #[test]
    fn classic_matches_every_documented_pre_t41_literal() {
        let t = ThemeKind::Classic.theme();
        assert_eq!(t.syntax.keyword, Color::Magenta);
        assert_eq!(t.syntax.string, Color::Green);
        assert_eq!(t.syntax.number, Color::LightYellow);
        assert_eq!(t.syntax.comment, Color::DarkGray);
        assert_eq!(t.syntax.key, Color::Blue);
        assert_eq!(t.syntax.function, Color::Cyan);
        assert_eq!(t.syntax.r#type, Color::LightCyan);
        assert_eq!(t.syntax.r#macro, Color::LightMagenta);
        assert_eq!(t.syntax.constant, Color::LightRed);
        assert_eq!(t.syntax.operator, Color::Red);
        assert_eq!(t.wash_document_highlight, Color::DarkGray);
        assert_eq!(t.wash_selection, Color::Yellow);
        assert_eq!(t.wash_bracket_pair, Color::Blue);
        assert_eq!(t.wash_breakpoint_verified, Color::Red);
        assert_eq!(t.wash_breakpoint_unverified, Color::DarkGray);
        assert_eq!(t.chip_fg, Color::DarkGray);
        assert_eq!(t.gutter_fg, Color::DarkGray);
        assert_eq!(t.fold_marker_fg, Color::DarkGray);
        assert_eq!(t.blame_lane_fg, Color::DarkGray);
        assert_eq!(t.right_margin_guide_bg, Color::DarkGray);
        assert_eq!(t.git_added, Color::Green);
        assert_eq!(t.git_modified, Color::Blue);
        assert_eq!(t.git_deleted, Color::Red);
        assert_eq!(t.git_none, Color::DarkGray);
        assert_eq!(t.diff_added, Color::Green);
        assert_eq!(t.diff_removed, Color::Red);
        assert_eq!(t.error_text, Color::Red);
        assert_eq!(t.focus_indicator, Color::Yellow);
    }

    #[test]
    fn ember_keeps_classics_value_for_fields_not_sourced_from_the_mockup() {
        let (classic, ember) = (ThemeKind::Classic.theme(), ThemeKind::Ember.theme());
        assert_eq!(
            classic.wash_document_highlight,
            ember.wash_document_highlight
        );
        assert_eq!(classic.wash_selection, ember.wash_selection);
        assert_eq!(classic.wash_bracket_pair, ember.wash_bracket_pair);
        assert_eq!(
            classic.wash_breakpoint_unverified,
            ember.wash_breakpoint_unverified
        );
        assert_eq!(classic.chip_fg, ember.chip_fg);
        assert_eq!(classic.git_added, ember.git_added);
        assert_eq!(classic.git_modified, ember.git_modified);
        assert_eq!(classic.diff_added, ember.diff_added);
        assert_eq!(classic.focus_indicator, ember.focus_indicator);
    }

    #[test]
    fn ember_diverges_from_classic_for_fields_sourced_from_the_mockup() {
        let (classic, ember) = (ThemeKind::Classic.theme(), ThemeKind::Ember.theme());
        assert_ne!(classic.syntax.keyword, ember.syntax.keyword);
        assert_ne!(classic.syntax.string, ember.syntax.string);
        assert_ne!(
            classic.wash_breakpoint_verified,
            ember.wash_breakpoint_verified
        );
        assert_ne!(classic.gutter_fg, ember.gutter_fg);
        assert_ne!(classic.right_margin_guide_bg, ember.right_margin_guide_bg);
        assert_ne!(classic.git_deleted, ember.git_deleted);
        assert_ne!(classic.diff_removed, ember.diff_removed);
        assert_ne!(classic.error_text, ember.error_text);
    }

    #[test]
    fn embers_danger_fields_all_share_the_same_accent_red() {
        // git_deleted/diff_removed/error_text/wash_breakpoint_verified all
        // documented as "mockup's primary accent, same reasoning" --
        // confirm they're actually the same color, not four independent
        // guesses that happen to look similar.
        let ember = ThemeKind::Ember.theme();
        let accent = Color::Rgb(0xec, 0x30, 0x13);
        assert_eq!(ember.git_deleted, accent);
        assert_eq!(ember.diff_removed, accent);
        assert_eq!(ember.error_text, accent);
        assert_eq!(ember.wash_breakpoint_verified, accent);
    }
}
