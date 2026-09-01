//! Design tokens -- the single authority for colour, spacing, radius and
//! type in this crate (`docs/features/intellij-look-foundation.md`).
//!
//! Nothing outside this module may name a colour: `render.rs` and the panels
//! resolve everything through [`Theme::tokens`], and a test in this file
//! enforces that (§4.1). The structure is deliberately plain data so roadmap
//! phase G3 can deserialize a [`Tokens`] from a file without reshaping it.

mod fonts;
mod palette;

pub use fonts::{install_fonts, UI_MEDIUM};
pub use palette::{DARCULA, INTELLIJ_LIGHT};

use eframe::egui;
use egui::Color32;
use ide_core::TokenKind;
use ide_lsp::DiagnosticSeverity;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    pub fn toggled(self) -> Self {
        match self {
            Theme::Light => Theme::Dark,
            Theme::Dark => Theme::Light,
        }
    }

    pub fn is_dark(self) -> bool {
        matches!(self, Theme::Dark)
    }

    /// The palette backing this theme. `&'static` -- tokens are compile-time
    /// constants, so resolving one is a field read, never an allocation.
    pub fn tokens(self) -> &'static Tokens {
        match self {
            Theme::Light => &INTELLIJ_LIGHT,
            Theme::Dark => &DARCULA,
        }
    }

    /// `egui::Visuals` built from [`Theme::tokens`] (doc §3.2). Starts from
    /// the stock visuals of the matching mode so the many fields this phase
    /// has no opinion about keep values egui already tuned.
    pub fn visuals(self) -> egui::Visuals {
        let t = self.tokens();
        let c = &t.color;
        let mut v = if self.is_dark() {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };

        v.dark_mode = self.is_dark();
        v.panel_fill = c.bg_base;
        v.window_fill = c.bg_elevated;
        v.faint_bg_color = c.bg_elevated;
        v.extreme_bg_color = c.bg_editor;
        v.window_stroke = egui::Stroke::new(1.0, c.border);
        v.window_corner_radius = egui::CornerRadius::same(t.radius.md);

        v.widgets.noninteractive = widget(c.bg_base, c.fg_primary, c.border, t.radius.sm);
        v.widgets.inactive = widget(c.bg_elevated, c.fg_secondary, c.border, t.radius.sm);
        v.widgets.hovered = widget(c.bg_hover, c.fg_primary, c.border_strong, t.radius.sm);
        v.widgets.active = widget(c.bg_active, c.fg_primary, c.accent_fg, t.radius.sm);
        v.widgets.open = widget(c.bg_active, c.fg_primary, c.border_strong, t.radius.sm);

        // Left `None` on purpose: `Visuals::text_color` returns the override
        // when one is set, as does `WidgetText`'s fallback, so setting it
        // would make every plain label ignore the per-state `fg_stroke`
        // colours above and kill hover/press feedback (doc §3.2).
        v.override_text_color = None;
        v.weak_text_color = Some(c.fg_muted);

        v.selection = egui::style::Selection {
            bg_fill: c.selection_bg,
            stroke: egui::Stroke::new(1.0, c.accent_fg),
        };
        v.text_cursor.stroke = egui::Stroke::new(1.5, c.caret);
        v.hyperlink_color = c.accent_fg;
        v.error_fg_color = c.danger;
        v.warn_fg_color = c.warning;

        v
    }
}

/// `bg_fill` and `weak_bg_fill` both come from the same surface token: egui
/// fills buttons from `weak_bg_fill` and checkboxes/sliders from `bg_fill`,
/// so setting only one would leave half the chrome on egui's stock grey.
fn widget(bg: Color32, fg: Color32, stroke: Color32, radius: u8) -> egui::style::WidgetVisuals {
    egui::style::WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: egui::Stroke::new(1.0, stroke),
        corner_radius: egui::CornerRadius::same(radius),
        fg_stroke: egui::Stroke::new(1.0, fg),
        expansion: 0.0,
    }
}

pub struct Tokens {
    pub color: Colors,
    pub syntax: SyntaxColors,
    pub space: Spacing,
    pub radius: Radii,
    pub text: TextSizes,
}

pub struct Colors {
    // Surfaces, back to front. There is deliberately no `bg_inset` yet:
    // while the editor is still an `egui::TextEdit`, egui's single
    // `extreme_bg_color` serves both the editor and ordinary text fields, so
    // a separate inset colour would have no consumer. A2 splits them.
    pub bg_base: Color32,
    pub bg_elevated: Color32,
    pub bg_editor: Color32,
    pub bg_hover: Color32,
    pub bg_active: Color32,

    pub border: Color32,
    pub border_strong: Color32,

    /// Plain text. Goes on `widgets.noninteractive.fg_stroke`, which is what
    /// `Visuals::text_color` resolves to.
    pub fg_primary: Color32,
    /// A widget's own label at rest -- dimmer until hovered.
    pub fg_secondary: Color32,
    /// Placeholders and de-emphasised text, via `Visuals::weak_text_color`.
    pub fg_muted: Color32,
    /// Text on top of `accent`. First consumer: roadmap B2 (primary buttons).
    #[allow(dead_code)]
    pub fg_on_accent: Color32,

    /// Fill of an accented surface. First consumer: roadmap B2.
    #[allow(dead_code)]
    pub accent: Color32,
    /// First consumer: roadmap B2.
    #[allow(dead_code)]
    pub accent_hover: Color32,
    /// The accent as foreground -- links, focus strokes. Bound by contrast
    /// against `bg_base`, which in a dark theme pulls the opposite way from
    /// `accent`'s own floor, hence two tokens rather than one.
    pub accent_fg: Color32,

    pub danger: Color32,
    pub warning: Color32,
    /// First consumer: roadmap B2 (Smart Mode "on" / clean-repo indicator).
    #[allow(dead_code)]
    pub success: Color32,
    /// Also the Information/Hint diagnostic colour.
    pub info: Color32,

    pub selection_bg: Color32,
    pub caret: Color32,
    pub current_line_bg: Color32,
    /// Behind both halves of a matched bracket pair. Deliberately not
    /// `selection_bg`: a pair inside a selection has to stay readable as a
    /// pair (`smart-editing.md` §2.7).
    pub bracket_match_bg: Color32,
    /// Gutter line numbers.
    pub gutter_fg: Color32,
    /// The current line's number.
    pub gutter_fg_active: Color32,

    /// Behind an in-buffer find/replace match (`in-buffer-find-replace.md`
    /// §3.6). Deliberately its own token rather than `selection_bg` -- a
    /// match under the cursor's real selection still has to read as "this
    /// is a match" and not just vanish into the selection tint.
    pub search_match_bg: Color32,
    /// Behind the *current* match -- has to stay visually distinct from an
    /// ordinary match, the same reason `bracket_match_bg` has its own token
    /// separate from `selection_bg`.
    pub search_match_current_bg: Color32,

    /// Behind every occurrence of the symbol at the caret
    /// (`docs/features/inlay-hints-and-hover.md` §3.4). Deliberately its
    /// own token rather than `search_match_bg` -- occurrence highlighting
    /// is semantically distinct from a find/replace match, the same reason
    /// `bracket_match_bg` isn't `selection_bg`.
    pub symbol_highlight_bg: Color32,

    /// Row tint, change-bar, and intraline highlight box are all derived
    /// from this at render time via `gamma_multiply` at different
    /// strengths (`docs/features/diff-viewer-enhancements.md` §3.3/§3.4) --
    /// no separate dedicated background token, the flat `_bg` pair this
    /// used to be was replaced for being nearly invisible against the
    /// panel background.
    pub diff_added_fg: Color32,
    pub diff_removed_fg: Color32,
    /// The editor gutter's change bar for a line that has both a removed
    /// and an added side (`docs/features/editor-git-gutter.md` §2.2) --
    /// JetBrains' own convention of a third color for "changed" distinct
    /// from a plain addition.
    pub diff_modified_fg: Color32,
}

/// One colour per [`TokenKind`], except `Punctuation` and `Variable`, which
/// deliberately have none: both resolve to the caller's plain text colour,
/// so brackets/separators recede and `Operator` is what stands out, and so
/// a semantic token classified as an ordinary local variable renders
/// identically to unstyled text -- the same choice real JetBrains New UI
/// colour schemes make (`docs/features/semantic-highlighting.md` §3.5).
pub struct SyntaxColors {
    pub keyword: Color32,
    pub string: Color32,
    pub number: Color32,
    pub comment: Color32,
    pub key: Color32,
    pub function: Color32,
    pub type_: Color32,
    pub macro_: Color32,
    pub constant: Color32,
    pub operator: Color32,
}

impl SyntaxColors {
    /// `default` is returned for [`TokenKind::Punctuation`] and
    /// [`TokenKind::Variable`] only.
    pub fn of(&self, kind: TokenKind, default: Color32) -> Color32 {
        match kind {
            TokenKind::Keyword => self.keyword,
            TokenKind::String => self.string,
            TokenKind::Number => self.number,
            TokenKind::Comment => self.comment,
            TokenKind::Punctuation => default,
            TokenKind::Key => self.key,
            TokenKind::Function => self.function,
            TokenKind::Type => self.type_,
            TokenKind::Macro => self.macro_,
            TokenKind::Constant => self.constant,
            TokenKind::Operator => self.operator,
            TokenKind::Variable => default,
        }
    }

    /// Every field with its name, so the contrast tests can't silently skip
    /// one that gets added later. Test-only by design: production resolves a
    /// single colour through `of`, never the whole set.
    #[allow(dead_code)]
    pub fn all(&self) -> [(&'static str, Color32); 10] {
        [
            ("keyword", self.keyword),
            ("string", self.string),
            ("number", self.number),
            ("comment", self.comment),
            ("key", self.key),
            ("function", self.function),
            ("type_", self.type_),
            ("macro_", self.macro_),
            ("constant", self.constant),
            ("operator", self.operator),
        ]
    }
}

pub struct Spacing {
    /// `f32` because these feed `Vec2` fields and `Spacing::indent`.
    pub xs: f32,
    pub sm: f32,
    pub md: f32,
    pub lg: f32,
    /// Top-bar and status-bar padding (`intellij-shell.md` §2.1/§2.6).
    pub xl: f32,
}

/// `u8` deliberately: egui's `CornerRadius` stores each corner as `u8`, so an
/// `f32` here would exist only to be cast at every use.
pub struct Radii {
    pub sm: u8,
    /// Boxed tab top corners (`intellij-shell.md` §2.5), and
    /// `window_corner_radius` above.
    pub md: u8,
    /// First consumer: roadmap B2 (popovers and the command palette).
    #[allow(dead_code)]
    pub lg: u8,
}

pub struct TextSizes {
    pub small: f32,
    pub body: f32,
    pub heading: f32,
    pub code: f32,
}

/// Applies `theme` to `ctx`: visuals plus the spacing/radius/type parts of
/// `egui::Style`. Idempotent -- call at startup and on every theme change.
///
/// **Precondition:** [`install_fonts`] must already have run on `ctx`. The
/// `Heading` text style names a custom font family, and egui panics on a
/// family bound to no fonts.
///
/// Writes *both* theme slots and then pins egui's preference. egui 0.36 keeps
/// a separate `Style` per theme and picks between them by system preference,
/// so writing only the active slot would let an OS theme change silently
/// swap the app's palette out from under the user's own choice.
pub fn apply(ctx: &egui::Context, theme: Theme) {
    for (slot, source) in [
        (egui::Theme::Dark, Theme::Dark),
        (egui::Theme::Light, Theme::Light),
    ] {
        ctx.set_visuals_of(slot, source.visuals());
        ctx.style_mut_of(slot, |style| apply_metrics(style, source.tokens()));
    }
    ctx.set_theme(if theme.is_dark() {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
}

fn apply_metrics(style: &mut egui::Style, t: &Tokens) {
    style.spacing.item_spacing = egui::vec2(t.space.md, t.space.sm);
    style.spacing.button_padding = egui::vec2(t.space.md, t.space.sm);
    style.spacing.window_margin = egui::Margin::same(t.space.md as i8);
    style.spacing.menu_margin = egui::Margin::same(t.space.sm as i8);
    style.spacing.indent = t.space.lg;
    style.text_styles = text_styles(t);
}

fn text_styles(t: &Tokens) -> std::collections::BTreeMap<egui::TextStyle, egui::FontId> {
    use egui::{FontFamily, FontId, TextStyle};
    [
        (
            TextStyle::Small,
            FontId::new(t.text.small, FontFamily::Proportional),
        ),
        (
            TextStyle::Body,
            FontId::new(t.text.body, FontFamily::Proportional),
        ),
        (
            TextStyle::Button,
            FontId::new(t.text.body, FontFamily::Proportional),
        ),
        (
            TextStyle::Heading,
            FontId::new(t.text.heading, FontFamily::Name(UI_MEDIUM.into())),
        ),
        (
            TextStyle::Monospace,
            FontId::new(t.text.code, FontFamily::Monospace),
        ),
    ]
    .into()
}

/// Error -> `danger`, Warning -> `warning`, Information/Hint -> `info`.
pub fn severity_color(tokens: &Tokens, severity: DiagnosticSeverity) -> Color32 {
    match severity {
        DiagnosticSeverity::Error => tokens.color.danger,
        DiagnosticSeverity::Warning => tokens.color.warning,
        DiagnosticSeverity::Information | DiagnosticSeverity::Hint => tokens.color.info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::Storage as _;

    #[test]
    fn theme_toggle_flips() {
        assert_eq!(Theme::Light.toggled(), Theme::Dark);
        assert_eq!(Theme::Dark.toggled(), Theme::Light);
    }

    #[test]
    fn each_theme_resolves_to_its_own_palette() {
        assert_eq!(Theme::Dark.tokens().color.bg_base, DARCULA.color.bg_base);
        assert_eq!(
            Theme::Light.tokens().color.bg_base,
            INTELLIJ_LIGHT.color.bg_base
        );
        assert!(Theme::Dark.is_dark());
        assert!(!Theme::Light.is_dark());
    }

    #[test]
    fn visuals_carry_the_mapped_tokens() {
        for theme in [Theme::Dark, Theme::Light] {
            let c = &theme.tokens().color;
            let v = theme.visuals();
            assert_eq!(v.dark_mode, theme.is_dark());
            assert_eq!(v.panel_fill, c.bg_base);
            assert_eq!(v.window_fill, c.bg_elevated);
            assert_eq!(v.extreme_bg_color, c.bg_editor);
            assert_eq!(v.text_cursor.stroke.color, c.caret);
            assert_eq!(v.selection.bg_fill, c.selection_bg);
            assert_eq!(v.hyperlink_color, c.accent_fg);
            assert_eq!(v.error_fg_color, c.danger);
            assert_eq!(v.warn_fg_color, c.warning);
            assert_eq!(v.weak_text_color, Some(c.fg_muted));
        }
    }

    /// The pair that keeps the per-state colours effective (doc §3.2): with
    /// an override set, every plain label would ignore `fg_stroke`.
    #[test]
    fn plain_text_color_comes_from_noninteractive_not_an_override() {
        for theme in [Theme::Dark, Theme::Light] {
            let v = theme.visuals();
            let c = &theme.tokens().color;
            assert_eq!(v.override_text_color, None);
            assert_eq!(v.widgets.noninteractive.fg_stroke.color, c.fg_primary);
            assert_eq!(v.text_color(), c.fg_primary);
        }
    }

    #[test]
    fn both_widget_backgrounds_come_from_the_surface_token() {
        let v = Theme::Dark.visuals();
        let c = &DARCULA.color;
        assert_eq!(v.widgets.inactive.bg_fill, c.bg_elevated);
        assert_eq!(v.widgets.inactive.weak_bg_fill, c.bg_elevated);
        assert_eq!(v.widgets.hovered.bg_fill, c.bg_hover);
        assert_eq!(v.widgets.hovered.weak_bg_fill, c.bg_hover);
        assert_eq!(v.widgets.inactive.fg_stroke.color, c.fg_secondary);
        assert_eq!(v.widgets.hovered.fg_stroke.color, c.fg_primary);
    }

    #[test]
    fn every_token_kind_maps_to_a_color_except_punctuation_and_variable() {
        let default = Color32::from_rgb(1, 2, 3);
        let s = &DARCULA.syntax;
        let kinds = [
            TokenKind::Keyword,
            TokenKind::String,
            TokenKind::Number,
            TokenKind::Comment,
            TokenKind::Key,
            TokenKind::Function,
            TokenKind::Type,
            TokenKind::Macro,
            TokenKind::Constant,
            TokenKind::Operator,
        ];
        for kind in kinds {
            assert_ne!(s.of(kind, default), default, "{kind:?} fell through");
        }
        assert_eq!(s.of(TokenKind::Punctuation, default), default);
        assert_eq!(s.of(TokenKind::Variable, default), default);
        assert_eq!(s.all().len(), kinds.len());
    }

    #[test]
    fn severity_color_covers_every_severity() {
        let c = &DARCULA.color;
        assert_eq!(
            severity_color(&DARCULA, DiagnosticSeverity::Error),
            c.danger
        );
        assert_eq!(
            severity_color(&DARCULA, DiagnosticSeverity::Warning),
            c.warning
        );
        assert_eq!(
            severity_color(&DARCULA, DiagnosticSeverity::Information),
            c.info
        );
        assert_eq!(severity_color(&DARCULA, DiagnosticSeverity::Hint), c.info);
    }

    #[derive(Default)]
    struct FakeStorage(std::collections::HashMap<String, String>);

    impl eframe::Storage for FakeStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    /// A `Theme` persisted before this module existed must still load after
    /// the move out of `app.rs`: `eframe` stores it as RON, so the variant
    /// names are the wire format and can't change. Exercised through the
    /// real `eframe` helpers rather than a hand-written string, so the test
    /// tracks whatever encoding `eframe` actually uses.
    #[test]
    fn a_theme_persisted_by_the_old_enum_still_loads() {
        let mut storage = FakeStorage::default();
        storage.set_string("ide_theme", "Dark".to_owned());
        assert_eq!(
            eframe::get_value::<Theme>(&storage, "ide_theme"),
            Some(Theme::Dark)
        );

        for theme in [Theme::Dark, Theme::Light] {
            let mut storage = FakeStorage::default();
            eframe::set_value(&mut storage, "ide_theme", &theme);
            assert_eq!(
                eframe::get_value::<Theme>(&storage, "ide_theme"),
                Some(theme)
            );
        }
    }

    #[test]
    fn apply_after_install_fonts_resolves_every_text_style() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        apply(&ctx, Theme::Dark);

        // Resolving `Heading` exercises the custom family; egui panics on a
        // family bound to no fonts, so reaching the assert is the proof.
        ctx.begin_pass(egui::RawInput::default());
        let style = ctx.global_style();
        for text_style in [
            egui::TextStyle::Small,
            egui::TextStyle::Body,
            egui::TextStyle::Button,
            egui::TextStyle::Heading,
            egui::TextStyle::Monospace,
        ] {
            let font_id = text_style.resolve(&style);
            let height = ctx.fonts_mut(|f| f.row_height(&font_id));
            assert!(height > 0.0, "{text_style:?} resolved to zero height");
        }
        // No renderer here to upload the font atlas epaint just built, and
        // `TexturesDelta`'s `Drop` asserts it was applied -- `clear` is the
        // escape hatch epaint documents for exactly this case.
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert_eq!(ctx.global_style().visuals.panel_fill, DARCULA.color.bg_base);
    }

    #[test]
    fn apply_pins_the_theme_and_fills_both_slots() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);

        apply(&ctx, Theme::Light);
        assert_eq!(ctx.theme(), egui::Theme::Light);
        assert_eq!(
            ctx.global_style().visuals.panel_fill,
            INTELLIJ_LIGHT.color.bg_base
        );
        // The dark slot is populated too, so an OS theme change can't fall
        // back to egui's stock palette.
        ctx.set_theme(egui::Theme::Dark);
        assert_eq!(ctx.global_style().visuals.panel_fill, DARCULA.color.bg_base);
    }

    #[test]
    fn metrics_come_from_the_spacing_tokens() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        apply(&ctx, Theme::Dark);
        let style = ctx.global_style();
        let t = &DARCULA;
        assert_eq!(
            style.spacing.item_spacing,
            egui::vec2(t.space.md, t.space.sm)
        );
        assert_eq!(style.spacing.indent, t.space.lg);
        assert_eq!(
            style.spacing.window_margin,
            egui::Margin::same(t.space.md as i8)
        );
        assert_eq!(
            style.visuals.window_corner_radius,
            egui::CornerRadius::same(t.radius.md)
        );
    }

    /// Doc §4.1: colour literals live in `theme/` and nowhere else, so a
    /// future edit can't quietly re-scatter them. This test deliberately
    /// never scans `theme/**` -- its own source contains every banned
    /// string, so scanning itself would fail unconditionally.
    #[test]
    fn no_color_literals_outside_this_module() {
        const BANNED: [&str; 10] = [
            "Color32::from_rgb",
            "Color32::from_rgba",
            "Color32::RED",
            "Color32::YELLOW",
            "Color32::LIGHT_RED",
            "Color32::LIGHT_GREEN",
            "Color32::LIGHT_BLUE",
            "Color32::GREEN",
            "Color32::WHITE",
            "Color32::BLACK",
        ];
        let sources: [(&str, &str); 13] = [
            ("editor/mod.rs", include_str!("../editor/mod.rs")),
            (
                "editor/double_tap.rs",
                include_str!("../editor/double_tap.rs"),
            ),
            ("editor/geometry.rs", include_str!("../editor/geometry.rs")),
            ("editor/input.rs", include_str!("../editor/input.rs")),
            ("editor/paint.rs", include_str!("../editor/paint.rs")),
            ("app.rs", include_str!("../app.rs")),
            ("app/render.rs", include_str!("../app/render.rs")),
            ("cargo_panel.rs", include_str!("../cargo_panel.rs")),
            ("claude_panel.rs", include_str!("../claude_panel.rs")),
            ("git_panel.rs", include_str!("../git_panel.rs")),
            ("lsp_bridge.rs", include_str!("../lsp_bridge.rs")),
            ("search_panel.rs", include_str!("../search_panel.rs")),
            ("main.rs", include_str!("../main.rs")),
        ];
        for (name, source) in sources {
            for banned in BANNED {
                assert!(
                    !source.contains(banned),
                    "{name} contains the colour literal `{banned}` -- \
                     resolve it through `crate::theme` instead"
                );
            }
        }
    }
}
