# IntelliJ New UI Look: Design-Token Foundation v2

Roadmap phase **B1b** (`docs/roadmap.md` §6 track B, §4b, §9). Supersedes
**B1** (`fleet-look-foundation.md`), which shipped the token module this
doc retunes — it does not rebuild the module, it re-parameterizes it.
Owned entirely by `rust-ui-dev` — the diff is confined to
`crates/ui/src/theme/**` (specifically `palette.rs`, plus the two
constant-name references in `mod.rs`). No security-sensitive path is
touched, so `hacker` is skipped for this run.

## 1. Purpose

`docs/roadmap.md` §9 records a full pivot in the project's appearance
target, from JetBrains **Fleet** to **IntelliJ-based IDEs' New UI
(2023.1+)** — GoLand/RustRover/PhpStorm as the concrete references. §4b
lists what that means structurally; this phase covers exactly one item
from that list, §4b point 9 (palette), because it's the shared
precondition for everything else: **B2b** (the actual panel/chrome
redesign, roadmap phase, separate doc) reads its colours from this
module and cannot start until the module reflects the new target.

Two things change, both confined to `theme/palette.rs`:

1. **The two palette constants are retargeted from Fleet's near-flat
   greys to IntelliJ Darcula (dark) and IntelliJ New UI Light (light)** —
   every `Color32` value in `FLEET_DARK`/`FLEET_LIGHT` is replaced.
2. **Editor surface and surrounding chrome surface diverge more than
   Fleet's philosophy called for.** Fleet's `bg_base`/`bg_elevated`/
   `bg_editor` sat within a few `Color32` steps of each other by design
   (§2.3 of the old doc: "near-neutral greys," one flat surface read).
   IntelliJ's chrome is visibly a different, slightly lighter (dark
   theme) or slightly greyer (light theme) tone from the editor
   underneath it — that's what makes a tool window read as a distinct
   panel rather than an extension of the text area. The existing
   `Colors` struct already has three separate surface fields
   (`bg_base`, `bg_elevated`, `bg_editor` — see `theme/mod.rs:120-124`'s
   own comment on why no fourth field was needed), so this is a value
   change, not a struct change: `bg_base` becomes the tool-window/toolbar/
   status-bar surface IntelliJ calls "panel background," `bg_editor`
   stays the editor surface, and the two now sit further apart in
   lightness than Fleet's did.

**Not in scope for this doc** (roadmap **B2b**): panel layout, tool-window
docking/chrome, the toolbar, tabs, status-bar content, tree density,
Zen mode — anything that isn't a colour value. **Also not in scope**:
`Spacing`/`Radii`/`TextSizes` (unchanged — B2b may introduce IntelliJ-
specific radii for tool-window chrome later, but nothing in this phase
requires it) and the embedded fonts (Inter + JetBrains Mono stay; see
`docs/roadmap.md` §4b point 10 for why replacing them with `JetBrains
Sans` is a separate, not-yet-approved dependency decision).

## 2. Interface / API

### 2.1 Renamed constants

```rust
// theme/palette.rs — was:
pub const FLEET_DARK: Tokens = ...;
pub const FLEET_LIGHT: Tokens = ...;

// becomes:
pub const DARCULA: Tokens = ...;
pub const INTELLIJ_LIGHT: Tokens = ...;
```

`theme/mod.rs`'s `pub use palette::{FLEET_DARK, FLEET_LIGHT};` and
`Theme::tokens`'s two-arm match update to the new names — **and so does
every reference inside `theme/mod.rs`'s own `#[cfg(test)] mod tests`**:
11 further direct uses (`each_theme_resolves_to_its_own_palette`,
`severity_color`'s tests, `both_widget_backgrounds_come_from_the_surface_
token`, `visuals_carry_the_mapped_tokens`, and others), all a mechanical
rename with no logic change, same as `palette.rs`'s own test module (§5).
No file outside `theme/**` references either constant by name
(`Theme::tokens()` is the only path anything outside `theme/**` uses to
reach a palette) — grep confirms this before implementation starts, and
stays confirmed by the existing `no_color_literals_outside_this_module`
test in `theme/mod.rs`, which is unrelated to this rename but incidentally
guards it: nothing outside `theme/**` can be naming these constants
without also containing a banned literal pattern this test already scans
for.

`Theme`, `Tokens`, `Colors`, `SyntaxColors`, `Spacing`, `Radii`,
`TextSizes`, `install_fonts`, `apply`, `severity_color`, `UI_MEDIUM` —
**every other public item in the module is unchanged**: same fields, same
signatures, same behaviour. `Theme::Dark`/`Theme::Light` were never named
`Theme::Fleet*`, so the enum itself needs no change; `Theme::tokens` just
starts returning different data.

### 2.2 Palettes

Two `pub const Tokens` values in `theme/palette.rs`, replacing
`FLEET_DARK`/`FLEET_LIGHT`. Every value below was computed and verified
against every contrast/distinctness assertion in `theme/palette.rs`'s
existing test module (`body_text_clears_aaa_on_every_surface`,
`secondary_and_muted_text_clear_their_floors`,
`every_syntax_color_clears_the_floor_on_the_editor_background`,
`gutter_and_caret_clear_their_floors`,
`accent_carries_its_own_text_and_reads_as_foreground`,
`status_colors_clear_the_floor_on_the_panel_background`,
`diff_text_is_legible_on_both_its_tint_and_the_panel`,
`row_tints_are_visible_without_reading_as_another_surface`, and the four
distinctness tests) **before this doc was written, the same way the
original B1 doc's §2.3 verified its light-palette adjustments** — every
floor in that test file is unchanged and unrelaxed by this phase; the
implementation's job is to reproduce these values, not to rediscover them.

| Token | `DARCULA` | `INTELLIJ_LIGHT` |
|---|---|---|
| `bg_base` (tool-window/toolbar/status-bar surface) | `#313335` | `#F7F8FA` |
| `bg_elevated` (popups/menus) | `#3A3B3F` | `#FFFFFF` |
| `bg_editor` | `#2B2D30` | `#FFFFFF` |
| `bg_hover` | `#43454A` | `#EDEEF1` |
| `bg_active` | `#4B4D52` | `#E3E5EA` |
| `border` | `#45474D` | `#DFE1E5` |
| `border_strong` | `#5A5D63` | `#C4C7CE` |
| `fg_primary` | `#DFE1E5` | `#1F2023` |
| `fg_secondary` | `#9DA0A8` | `#5C5F66` |
| `fg_muted` | `#7B7E85` | `#8A8D95` |
| `fg_on_accent` | `#FFFFFF` | `#FFFFFF` |
| `accent` | `#2E5FCC` | `#2E5FCC` |
| `accent_hover` | `#4D7FE0` | `#1D4FB8` |
| `accent_fg` | `#6D9FFF` | `#1B5CD9` |
| `danger` | `#F47D82` | `#C4342B` |
| `warning` | `#E3A13C` | `#9A6400` |
| `success` | `#67C77A` | `#2E7D3A` |
| `info` | `#6D9FFF` | `#1B5CD9` |
| `selection_bg` | `#2B4A73` | `#CFE0FB` |
| `caret` | `#569CFF` | `#3574F0` |
| `current_line_bg` | `#303236` | `#F1F2F5` |
| `bracket_match_bg` | `#3A5A57` | `#BFE2DE` |
| `gutter_fg` | `#787B82` | `#8F929C` |
| `gutter_fg_active` | `#B3B6BD` | `#4A4D54` |
| `search_match_bg` | `#463D26` | `#FFF3C4` |
| `search_match_current_bg` | `#6B5218` | `#FFD866` |
| `symbol_highlight_bg` | `#3E3348` | `#EADCF5` |
| `diff_added_fg` | `#63B77C` | `#2E7D3A` |
| `diff_added_bg` | `#31392F` | `#E9F5EC` |
| `diff_removed_fg` | `#E08A8A` | `#C2342B` |
| `diff_removed_bg` | `#3A2E30` | `#FBEAE9` |

`accent` (`#2E5FCC`) is deliberately not IntelliJ's literal New UI accent
hex (`#3574F0`, used in the JetBrains product itself for links/focus
rings): `white` text on `#3574F0` clears only 4.28:1, below this project's
existing 4.5:1 `fg_on_accent`/`accent` floor (`accent_carries_its_own_
text_and_reads_as_foreground`) — the same floor B1's original doc held
`accent`/`accent_hover` to. `#2E5FCC` is a deliberately-darkened step
along the same hue that clears the floor (5.79:1) while staying
recognisably "the IntelliJ blue family." `accent_fg` (the foreground/link/
focus-stroke role, checked against `bg_base` rather than white) uses a
lighter step of the same hue in each theme, mirroring the two-role split
`accent`/`accent_fg` already had in the Fleet palette (dark:
`#1F6EF4`/`#4C8EFF`; light: `#2F6FEB`/`#1B5CD9`) for the identical reason:
one hue can't simultaneously satisfy "white text readable on a filled
button" and "readable as text on the panel background" at both ends of a
contrast range this wide.

Syntax colours keep the existing hues (unchanged, still the same
family the original B1 doc chose) except two dark-theme values that no
longer clear their floor against the new, darker `bg_editor` (`#2B2D30`
vs. Fleet's `#1B1B1D` — close in lightness, but not identical, and these
two sat close enough to their old floor that the difference matters):

| Kind | `DARCULA` | `INTELLIJ_LIGHT` | Changed from Fleet |
|---|---|---|---|
| `keyword` | `#C678DD` | `#A626A4` | no |
| `string` | `#98C379` | `#418340` | no |
| `number` | `#D19A66` | `#986801` | no |
| `comment` | `#969BA3` | `#72757E` | dark: `#848A94` (3.97 vs. new `bg_editor`, below 4.5) → lightened to 4.94 |
| `key` | `#61AFEF` | `#306CF1` | no |
| `function` | `#61AFEF` | `#306CF1` | no |
| `type_` | `#E5C07B` | `#9A6D00` | no |
| `macro_` | `#EB7F87` | `#CA4A3F` | dark: `#E06C75` (4.32 vs. new `bg_editor`, below 4.5) → lightened to 5.21 |
| `constant` | `#D19A66` | `#986801` | no |
| `operator` | `#56B6C4` | `#017CB1` | no |

The light-theme syntax hues need no change: `bg_editor` stays `#FFFFFF`
in both palettes (§1 point 2 — only `bg_base` moved off white), so every
ratio that held against the old Fleet light `bg_editor` still holds.

### 2.3 Applying tokens — unchanged

`install_fonts`, `apply`, `severity_color` keep their exact B1 signatures
and behaviour (`theme/mod.rs`, unmodified by this phase):

```rust
pub fn install_fonts(ctx: &egui::Context);
pub fn apply(ctx: &egui::Context, theme: Theme);
pub fn severity_color(tokens: &Tokens, severity: DiagnosticSeverity) -> Color32;
pub const UI_MEDIUM: &str = "ui-medium";
```

`Theme::visuals`'s field-by-field mapping from `Tokens` to
`egui::Visuals` (`theme/mod.rs:50-90`) is untouched — it already reads
every value through `self.tokens()`, so retargeting what `tokens()`
returns is sufficient; no line of that function names a colour.

## 3. Behaviour

Identical to B1's, since no code path changes — only the data
`DARCULA`/`INTELLIJ_LIGHT` supply differs from what `FLEET_DARK`/
`FLEET_LIGHT` supplied. Concretely, after this phase:

- `Theme::Dark.tokens()` returns `&DARCULA` instead of `&FLEET_DARK`.
- `Theme::Light.tokens()` returns `&INTELLIJ_LIGHT` instead of
  `&FLEET_LIGHT`.
- Every existing consumer of `Theme::visuals`/`Theme::tokens`
  (`app.rs`'s theme toggle, `render.rs`'s syntax/diagnostic colouring,
  `severity_color`'s callers) picks up the new palette automatically on
  the next repaint after a theme switch or app start — none of them
  needs a code change, which is the same "one authority" property B1
  established and this phase relies on rather than re-proves.
- The visible effect: editor background and surrounding chrome
  (tool-window/toolbar/status-bar surfaces once B2b exists to render
  them — today, `panel_fill`/`window_fill` consumers) now differ in tone
  rather than reading as one flat surface; the accent colour reads as
  IntelliJ blue rather than Fleet's blue; syntax highlighting keeps its
  existing hue family with the two floor-driven adjustments above.

## 4. Constraints & invariants

- **No relaxed floor.** Every assertion in `theme/palette.rs`'s existing
  `#[cfg(test)] mod tests` stays exactly as it is — this phase's
  acceptance bar is that module passing unmodified against the new
  constants, not a modified/loosened version of it. §2.2's tables were
  verified against literally that test file's logic before this doc was
  written.
- **`the_two_palettes_are_actually_different`** (existing test) needs its
  two hardcoded references updated from `FLEET_DARK`/`FLEET_LIGHT` to
  `DARCULA`/`INTELLIJ_LIGHT` — a mechanical rename, not a behaviour
  change (see §5 for the full test-file rename scope).
- **Struct shape frozen.** `Colors`, `SyntaxColors`, `Spacing`, `Radii`,
  `TextSizes` gain no new field and lose none — this is a retarget of
  existing fields' values, confirmed necessary by re-reading
  `theme/mod.rs:115-193`'s `Colors` definition, which already has the
  three-surface split §1 point 2 needs (superseding this doc's own
  earlier planning-level note in `docs/roadmap.md`'s B1b row, which
  mentioned a new `bg_chrome` field before this closer read established
  one wasn't necessary — `docs/roadmap.md` is corrected alongside this
  doc, see §6).
- **No dependency change.** No new crate — this phase edits `Color32`
  literals in an already-approved module (B1's font-embedding table in
  `CLAUDE.md`'s Dependencies section is untouched, since fonts don't
  change).

## 5. Examples

Existing call sites need no change; this section instead pins the exact
scope of the mechanical rename, since that's the only shape of edit this
phase makes outside `palette.rs`'s literal values:

```rust
// theme/mod.rs
pub use palette::{DARCULA, INTELLIJ_LIGHT};   // was FLEET_DARK, FLEET_LIGHT

impl Theme {
    pub fn tokens(self) -> &'static Tokens {
        match self {
            Theme::Light => &INTELLIJ_LIGHT,  // was &FLEET_LIGHT
            Theme::Dark => &DARCULA,          // was &FLEET_DARK
        }
    }
}
```

```rust
// theme/palette.rs — test module, existing test, same assertions:
#[test]
fn the_two_palettes_are_actually_different() {
    assert_ne!(DARCULA.color.bg_base, INTELLIJ_LIGHT.color.bg_base);
    assert_ne!(DARCULA.syntax.keyword, INTELLIJ_LIGHT.syntax.keyword);
}

fn palettes() -> [(&'static str, &'static Tokens); 2] {
    [("DARCULA", &DARCULA), ("INTELLIJ_LIGHT", &INTELLIJ_LIGHT)]
}
```

Every other existing test in that module is unchanged in structure — only
the two names above and the literal `Color32` values under test move.

## 6. Dependencies & integration points

- **Depends on**: nothing new — B1's already-merged token module and its
  existing test harness.
- **Blocks**: roadmap **B2b** (`intellij-shell.md`), which reads
  `bg_base`/`bg_hover`/`bg_active`/`border`/`accent*` through
  `Theme::tokens()` while restyling panels/chrome, and cannot honestly
  target "IntelliJ New UI" chrome colours while `Theme::tokens()` still
  returns Fleet's.
  `A2` (the editor widget) and every panel that already renders through
  `Theme::visuals()`/`tokens.syntax.of()` pick up the new palette for
  free, with no code change on their side (§3).
- **`docs/roadmap.md` correction**: the B1b row in §6 track B is amended
  to drop the "new `bg_chrome` token" wording (§4 constraint above) once
  this doc lands — `docs/roadmap.md` is a planning document and this
  feature doc supersedes its phase-level approximation, the same
  relationship every other phase's doc has had with the roadmap row that
  proposed it.

No diagram: this phase changes constant values behind an interface that
already exists and is already diagrammed structurally by nothing — B1
itself didn't carry a diagram either, for the same reason (a token-value
table *is* the clearest representation of this change, not a sequence/
component/state diagram).
