# GUI Theme: Ember (roadmap G3, scoped)

## 1. Purpose

This is the GUI counterpart of `ide-tui`'s already-shipped `T41`
(`docs/features/tui-theme.md`): a third, named, built-in colour theme for
`ide-ui`, imported from the `claude.ai/design` mockup `Fleet-like
IDE.dc.html` (same design project as the TUI's `Orbit TUI.dc.html`). Both
mockups turn out to share the same accent colour family (`#ec3013` base,
`#ff563c`/`#ff9783` hover/soft variants) — confirmed by inspection, not
assumed — so this theme is named **Ember**, identical to the TUI's own
choice, rather than inventing a second name for what is visibly the same
palette applied to a second frontend.

**Scope note — this is a narrower slice of G3, not all of it.**
`docs/roadmap.md`'s own G3 row describes two things: (a) loadable themes
via an external file, and (b) new built-in named presets ("Fleet Dark/
Light"). This doc implements **only** (b), for exactly one new built-in:
`Theme::Ember`, hardcoded the same way `Theme::Light`/`Theme::Dark`
already are (`palette.rs`'s `DARCULA`/`INTELLIJ_LIGHT` `const` `Tokens`).
File-loadable custom themes remain unimplemented and out of scope — the
same "translate the mockup's actual payoff, don't force the whole
mockup in at once" discipline `tui-custom-actions.md` §1 already
established for the TUI's sibling import.

**Typography is explicitly out of scope.** The mockup specifies Archivo
(UI) + IBM Plex Mono (code); `ide-ui` only has Inter + JetBrains Mono
embedded (`crates/ui/src/theme/fonts.rs`), and `CLAUDE.md`'s Dependencies
table approves only those two font families as embedded assets — adding
new ones needs the user's explicit sign-off, since it's a new asset/
dependency, not a role's call to make. Asked directly (2026-09-05), the
user chose to **keep Inter + JetBrains Mono** and import only the colour
palette. `Ember` therefore renders in the same typefaces `Light`/`Dark`
already use; nothing in `fonts.rs` changes.

**Branding note**, same rule T41/T42 already established: the mockup's
`ORBIT` header wordmark and `orbit-core` sample project name are
placeholder sample content from the design tool, not adopted anywhere in
`ide-ui` — no "Orbit" string in code, UI text, or identifiers.

**Not security-sensitive.** This is pure presentation/config data — no
subprocess, no network, no untrusted-path I/O beyond the already-existing
`eframe::Storage` persistence `Theme`'s `Light`/`Dark` variants already go
through. `hacker` is skipped for this run, same classification T41 got.

## 2. Interface / API

### 2.1 `crates/ui/src/theme/mod.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Theme {
    Light,
    Dark,
    Ember, // new
}

impl Theme {
    /// Cycles forward through every built-in theme: `Light -> Dark ->
    /// Ember -> Light`. Replaces the old two-way `toggled()` -- a fixed
    /// forward cycle is the natural generalisation of a toggle once there
    /// are more than two options, and `ToggleTheme` (§2.3) has no bound
    /// keybinding (`binding: None` in `command.rs`) to reconsider, only a
    /// command-palette/menu entry a user can invoke repeatedly exactly
    /// the way they'd repeatedly click a toggle today.
    pub fn next(self) -> Self {
        match self {
            Theme::Light => Theme::Dark,
            Theme::Dark => Theme::Ember,
            Theme::Ember => Theme::Light,
        }
    }

    pub fn tokens(self) -> &'static Tokens {
        match self {
            Theme::Light => &INTELLIJ_LIGHT,
            Theme::Dark => &DARCULA,
            Theme::Ember => &EMBER, // new
        }
    }

    // `is_dark`/`visuals` unchanged in shape -- `is_dark` gains `Theme::Ember => true`
    // (Ember is a dark theme, same as Dark); `visuals` already resolves everything
    // through `tokens()` and `is_dark()`, so it needs no new match arm at all.
}
```

`is_dark(self) -> bool` gains one arm: `Theme::Ember => true`. `visuals`,
`apply_metrics`, `text_styles`, `severity_color` are already generic over
`Tokens`/`self.is_dark()` and need no change.

**`apply` does need a real change — it is not generic over `theme` today.**
Its current body is a fixed two-slot loop:

```rust
pub fn apply(ctx: &egui::Context, theme: Theme) {
    for (slot, source) in [
        (egui::Theme::Dark, Theme::Dark),
        (egui::Theme::Light, Theme::Light),
    ] {
        ctx.set_visuals_of(slot, source.visuals());
        ctx.style_mut_of(slot, |style| apply_metrics(style, source.tokens()));
    }
    ctx.set_theme(if theme.is_dark() { egui::Theme::Dark } else { egui::Theme::Light });
}
```

It always writes `DARCULA` into egui's native `Dark` slot and
`INTELLIJ_LIGHT` into its `Light` slot regardless of the `theme` argument
— the argument only picks which of those two *fixed* slots `set_theme`
activates. `egui::Theme` (confirmed by reading `egui` 0.36.1's
`Context::style_of`/`style_mut_of`/`set_style_of`) is a strict binary
type with exactly two slots, `Dark`/`Light` — there is no third slot for
`Ember` to occupy independently. Left as-is, `apply(ctx, Theme::Ember)`
would activate egui's `Dark` slot (since `Ember.is_dark() == true`), which
still holds `DARCULA`'s colours — selecting Ember would silently render as
Dark/Darcula, not Ember. This is a functional defect in the *current*
code the doc must have the implementer fix, not something already handled
via `tokens()`/`is_dark()`.

Fix: write the *currently selected* theme's own visuals into whichever
single slot its `is_dark()` maps to, and activate that same slot — do not
also write the other, inactive slot:

```rust
pub fn apply(ctx: &egui::Context, theme: Theme) {
    let slot = if theme.is_dark() { egui::Theme::Dark } else { egui::Theme::Light };
    ctx.set_visuals_of(slot, theme.visuals());
    ctx.style_mut_of(slot, |style| apply_metrics(style, theme.tokens()));
    ctx.set_theme(slot);
}
```

The old "defensively fill both slots on every call" behaviour was only
ever load-bearing for exactly two app-level themes, one per egui slot; it
doesn't generalise once a second dark theme (`Ember`) has to share egui's
single `Dark` slot with `DARCULA`. Nothing else needs the inactive slot
pre-populated: `grep -n "style_of\|global_style\|set_theme\|visuals_of\b"
crates/ui/src` confirms `theme/mod.rs` is the *only* caller of these egui
APIs anywhere in the crate, so no other code path reads
`ctx.style_of(egui::Theme::Dark)` expecting it to always hold `DARCULA`
specifically. This becomes a real invariant future code must respect —
see §4.

**Renaming `toggled` to `next`:** the one call site,
`IdeApp::toggle_theme` (`app.rs`), changes from `self.theme =
self.theme.toggled();` to `self.theme = self.theme.next();`. No other
caller exists (checked: `grep -rn "\.toggled()" crates/ui/src` — the only
other `toggled()` in this crate is `ViewMode::toggled()`, an unrelated
two-way toggle that keeps its own name and is not touched by this doc).
`theme/mod.rs`'s own existing `theme_toggle_flips` test (asserts
`Theme::Light.toggled() == Theme::Dark` etc.) must be replaced — not left
in place under the old name — with a 3-way cycle test of `next()`; §5's
example gives the exact assertions to use.

### 2.2 `crates/ui/src/theme/palette.rs`

One new `pub const EMBER: Tokens`, alongside the existing `DARCULA`/
`INTELLIJ_LIGHT`, reusing the same shared `SPACE`/`RADIUS`/`TEXT` consts
(spacing/radius/type scale is not something either mockup expresses an
opinion on beyond what's already in `Tokens` — same "shared, only colour
differs" convention this file's own header comment already documents).

Colour values, seeded from the mockup where it has an opinion, else
inherited from `DARCULA` (T41's own "don't invent an off-palette hue for
a field the mockup is silent on" rule, applied here identically):

- **Surfaces** (mockup gives four literal dark tones): `bg_base:
  #1a1918` (main app background), `bg_elevated: #201e1d` (header/aside/
  left-rail bars), `bg_editor: #1a1918` (mockup doesn't style the editor
  background separately from the app background), `bg_hover: #262423`,
  `bg_active`: no distinct mockup value — a modest step darker/lighter
  than `bg_hover` in the same family, implementer's call, floor-tested
  the same as every other field (§4).
- **Borders**: `border: #302e2d` (thin dividers), `border_strong:
  #444141` (the 2px structural borders — tab strip, header, panel edges).
- **Text**: `fg_primary: #f3f2f2`, `fg_secondary: #d7d3d3`, `fg_muted:
  #9b9797` (labels, muted UI text throughout the mockup), `fg_on_accent`:
  white, matching every other palette.
- **Accent**: `accent: #ec3013` (the mockup's own `--accent` CSS custom
  property default), `accent_hover: #ff563c`, `accent_fg: #ff9783` (the
  mockup's own link-hover colour, used here as the accent-as-foreground
  tone against a dark background, the same role `accent_fg` plays in
  `DARCULA`). **Verify this specific literal against
  `accent_carries_its_own_text_and_reads_as_foreground` first** — a manual
  WCAG relative-luminance check of white `fg_on_accent` against `#ec3013`
  comes out to roughly 4.2:1, under this crate's 4.5 floor; §4 already
  requires adjusting a failing colour's lightness rather than the floor,
  but this value is called out explicitly here because it is the one
  concrete literal in this section most likely to need that adjustment,
  not a hypothetical.
- **Selection**: derived from the mockup's own `::selection { background:
  rgba(236, 48, 19, 0.32); }` (`236, 48, 19` = `#ec3013`) — an opaque
  `Color32` approximating that alpha blend against `bg_editor`, since
  `Colors::selection_bg` has no alpha channel to carry the `0.32` through
  directly.
- **Diagnostics/diff** (`danger`/`warning`/`success`/`info`,
  `diff_added_fg`/`diff_removed_fg`/`diff_modified_fg`): the mockup shows
  no dedicated example of any of these (its one diff/debug screen reuses
  the accent colour for "this changed" highlighting, not a distinct
  green/red pair) — inherit `DARCULA`'s hues, adjusted in lightness only
  if needed to clear this palette's own floor tests against `EMBER`'s
  different `bg_base`/`bg_editor` (§4). Do not invent new hues for these;
  keep the same colour *role* Darcula already validated.
- **Syntax** (mockup's one code sample gives a *partial* opinion, same
  situation T41 hit and documented as `[controversial]` rather than
  papered over): `keyword: #ff563c` (visible on `use`/`pub`/`impl`/
  `match`/`let`/`async`/`await`/`fn`), `type_: #ff9783` (visible on
  `Session`/`Result`/`Ok`/`Some`/`Arc`/`Peer`/`Envelope`), `comment:
  #9b9797`, `string: #bab6b6` (the one string literal shown,
  `"dropped envelope"`). `function`/`macro_`/`constant`/`key`/`operator`/
  `number` have no distinguishable mockup example — inherit `DARCULA`'s
  hues (adjusted for the floor against `EMBER`'s editor background if
  needed), same "don't invent" rule as the non-opinionated `Colors`
  fields above. Record this collapse explicitly in this doc's own
  Revision notes if a reviewer wants a fuller accounting, mirroring
  T41's own `[controversial]` note about `EMBER`'s TUI counterpart doing
  the same thing.

### 2.3 `crates/ui/src/theme/palette.rs`'s test helper

```rust
fn palettes() -> [(&'static str, &'static Tokens); 3] { // was 2
    [
        ("DARCULA", &DARCULA),
        ("INTELLIJ_LIGHT", &INTELLIJ_LIGHT),
        ("EMBER", &EMBER), // new
    ]
}
```

Every existing test that iterates `palettes()` (contrast floors,
row-tint bounds, distinctness checks, spacing/`i8`-cast survival) then
automatically covers `EMBER` too — **do not** write parallel duplicate
tests for `EMBER` specifically; extending this one array is the entire
integration point (this file's own established pattern, since
`DARCULA`/`INTELLIJ_LIGHT` already share this helper rather than each
having their own copy of every test).

`the_two_palettes_are_actually_different` (currently checks exactly one
pair) needs extending to check all three pairwise, not just adding
`EMBER` to one side of the existing two-palette comparison.

**This helper only covers `palette.rs`'s own tests — `theme/mod.rs` has no
equivalent shared array.** Its tests each hardcode their own inline
`Theme`/`Tokens` list or a single `Theme::Dark` case
(`visuals_carry_the_mapped_tokens`,
`plain_text_color_comes_from_noninteractive_not_an_override`,
`both_widget_backgrounds_come_from_the_surface_token`,
`apply_after_install_fonts_resolves_every_text_style`,
`apply_pins_the_theme_and_fills_both_slots`,
`metrics_come_from_the_spacing_tokens`). None of these will exercise
`Ember` automatically the way `palette.rs`'s tests do. At minimum, add a
direct test proving `apply(ctx, Theme::Ember)` actually activates
`EMBER`'s own visuals in the slot it occupies (e.g. asserting
`ctx.global_style().visuals.panel_fill == EMBER.color.bg_base` after
calling `apply(&ctx, Theme::Ember)`, mirroring
`apply_after_install_fonts_resolves_every_text_style`'s existing shape for
`Dark`) — this is the test that would catch the exact `apply()` defect
§2.1 describes, and its absence is why that defect wasn't caught by this
doc's first draft. `apply_pins_the_theme_and_fills_both_slots` must be
replaced (not left in place under its old name), since "fills both slots
on every call" is no longer what `apply` does per §2.1's fix.

### 2.4 `crates/ui/src/app.rs` / `crates/ui/src/command.rs` / `crates/ui/src/app/menu.rs`

No structural change. `toggle_theme`'s one-line body updates per §2.1.
`Command { id: "ToggleTheme", .. }`'s `title`/`binding`/`category` stay
exactly as they are — it has no doc comment today (checked: the
`command.rs` registration is a plain struct literal), so one describing
a 3-way cycle should be **added**, not updated, since a future reader
would otherwise reasonably assume "toggle" means exactly two states.
`app/menu.rs`'s `MENU_GROUPS` "View" entry (`Some("ToggleTheme")`) is
untouched — it already references the command by id, not by counting
states.

## 3. Behaviour

- Repeated invocation of the `ToggleTheme` command (command palette, or
  the native "View" menu item) cycles `Light -> Dark -> Ember -> Light`,
  forever forward, no reverse direction — identical UX shape to today's
  toggle, just with a third stop.
- `Ember` persists through the same `eframe::Storage` RON round-trip
  `Light`/`Dark` already use (`app.rs`'s `"ide_theme"` key) — adding a
  unit variant to a `serde`-derived enum is additive; a session that last
  saved `"Ember"` restores `Theme::Ember` on next launch exactly the way
  `"Light"`/`"Dark"` already do. A project last saved under the *old*
  two-variant build still loads correctly (`Light`/`Dark` are untouched),
  the same backward-compatibility property `a_theme_persisted_by_the_
  old_enum_still_loads` already proves for the `Light`/`Dark` pair —
  extend that test to also round-trip `Ember` rather than replacing it.
- Every panel, the editor, and diagnostics render in `Ember` exactly as
  they do in `Light`/`Dark` today, since **nothing outside
  `crates/ui/src/theme/` may name a colour** (this module's own header
  comment, enforced by `no_color_literals_outside_this_module`) — `Ember`
  needs zero changes anywhere else in the crate to "just work" the moment
  `EMBER`/`Theme::Ember` exist, the same zero-blast-radius property T41
  relied on for `ide-tui`.

## 4. Constraints & invariants

- **`Light` and `Dark`'s existing token values are byte-for-byte
  unchanged.** This is a pure addition — zero visual regression for
  every current user, the same invariant T41's `Classic` held for
  `ide-tui`. A reviewer should diff `DARCULA`/`INTELLIJ_LIGHT`'s literals
  against `main` and confirm nothing moved.
- **Every existing contrast-floor/distinctness test in `palette.rs`
  passes for `EMBER`** via the extended `palettes()` helper (§2.3) — no
  floor gets loosened to make `EMBER` pass; if a mockup-sourced colour
  fails a floor, the implementer adjusts that colour's *lightness* (not
  its hue, and not the floor) until it clears, the same resolution path
  `intellij-look-foundation.md`'s own floors already assume for any
  future palette.
- **`Ember` and `Dark` share egui's single native `Dark` theme slot**
  (§2.1) — only one of them is ever the *active* theme's own visuals at a
  time, written fresh by `apply()` on every call. No code outside
  `theme/mod.rs` may read `ctx.style_of(egui::Theme::Dark)` (or
  `global_style()`/`set_theme`/`set_visuals_of` directly) assuming it
  always holds `DARCULA`'s colours specifically — confirmed via grep that
  no such assumption exists elsewhere in the crate today, but this is now
  a real constraint future code must respect rather than an accident of
  there having been only two themes.
- **No new dependency.** Fonts are unchanged (§1) — this doc adds no
  entry to `CLAUDE.md`'s Dependencies table.
- **Not security-sensitive** (§1) — `hacker` is skipped for this run.
- No "Orbit" or "Fleet" branding string anywhere in code or UI text (§1).

## 5. Examples

```rust
let mut theme = Theme::Light;
theme = theme.next();
assert_eq!(theme, Theme::Dark);
theme = theme.next();
assert_eq!(theme, Theme::Ember);
theme = theme.next();
assert_eq!(theme, Theme::Light); // wraps

assert_eq!(Theme::Ember.tokens().color.bg_base, EMBER.color.bg_base);
assert!(Theme::Ember.is_dark());
```

Persistence round-trip (mirrors this file's own existing
`a_theme_persisted_by_the_old_enum_still_loads` shape):

```rust
let mut storage = FakeStorage::default();
eframe::set_value(&mut storage, "ide_theme", &Theme::Ember);
assert_eq!(
    eframe::get_value::<Theme>(&storage, "ide_theme"),
    Some(Theme::Ember)
);
```

## 6. Dependencies

None beyond what `ide-ui` already has (§1/§4 — no new font, no new
crate).

## 7. Diagram

Skipped — this is a pure data addition (one new `const Tokens` and one
new enum variant) with no new control flow, protocol, or lifecycle worth
a PlantUML diagram; §2-§4's prose already fully describes it, the same
"skip for a change too small to benefit" rule the `dev-chain` skill's own
instructions allow for.

## Revision notes

**Round 1 (`rev`, documentation review) — required changes, all applied:**

- §2.1 originally claimed `apply` "already resolves everything through
  `tokens()`/`is_dark()`, so it needs no new match arm at all." This was
  wrong: `apply`'s body is a hardcoded two-slot loop that always writes
  `DARCULA` into egui's `Dark` slot and `INTELLIJ_LIGHT` into its `Light`
  slot regardless of the `theme` argument. Left as originally written,
  selecting `Ember` would have activated egui's `Dark` slot while it still
  held `DARCULA`'s colours — Ember would have silently rendered as Dark.
  Fixed by rewriting §2.1 with the correct `apply` implementation (write
  only the currently-selected theme's own visuals into the single slot its
  `is_dark()` maps to, then activate that slot) and a §4 invariant that no
  other code may assume egui's `Dark` slot always means `DARCULA`.
- Added a requirement (§2.3) for a direct test asserting `apply(ctx,
  Theme::Ember)` actually applies `EMBER`'s own visuals — `theme/mod.rs`
  has no shared per-theme test array the way `palette.rs` does, so several
  of its existing tests (listed in §2.3) needed calling out individually
  as not automatically covering `Ember`, and
  `apply_pins_the_theme_and_fills_both_slots` needed flagging for
  replacement since "fills both slots" is no longer the design.
- §2.1 now calls out that `theme_toggle_flips` must be replaced (not left
  in place) when `toggled` is renamed to `next`.
- §2.4's claim that `ToggleTheme`'s doc comment should be "updated" was
  wrong — it has none today; corrected to "added."
- §2.2 now flags the literal `accent: #ec3013` value as likely to fail
  the crate's own 4.5 contrast floor against white `fg_on_accent`
  (manually computed at ~4.2:1), so the implementer checks that specific
  value first rather than discovering it only after a failing test.

**Controversial findings from round 1 (non-blocking, recorded per project
convention):**

- The blind forward-only `next()` cycle (§3) is a step down from this
  same import's own TUI precedent: T41 did not keep a two-way toggle once
  there were more than two themes — it built a dedicated Theme Settings
  popup specifically because the user required "colour palette must be in
  settings section." This doc keeps the cruder cycle-only mechanism for
  the GUI despite citing T41 as its model throughout, and doesn't
  acknowledge the tension. A cheaper middle ground than a full settings
  page (not yet built, roadmap G1) would be adding three separate
  palette-only commands ("Theme: Light"/"Theme: Dark"/"Theme: Ember") in
  addition to the cycle command, the same two-commands-per-feature shape
  T42's `ManageCustomActions`/`ToggleCustomActionsPanel` already used.
  Not required for this doc to proceed — recorded as a design
  disagreement worth revisiting once G1 lands.
- §2.2 sets `bg_editor` identical to `bg_base` (`#1a1918` for both)
  because the mockup's own CSS doesn't style them separately. Every other
  built-in theme gives the editor its own distinct depth from the
  surrounding chrome (`DARCULA`'s editor is darker than its base;
  `INTELLIJ_LIGHT`'s is lighter) — a deliberate IDE-design convention this
  codebase already relies on, which this literal reading of the mockup
  would abandon for Ember specifically. The mockup does have a distinct
  darker tone available elsewhere (`#141312`, its top tab-bar colour) that
  could give `bg_editor` its own depth instead of matching `bg_base`. Not
  required — recorded so an implementer can weigh mockup-literalism
  against this codebase's own established convention rather than the
  choice passing by unremarked.
