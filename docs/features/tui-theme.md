# TUI: Color Theme System (T41)

## 1. Purpose

`ide-tui` has never had a selectable color palette: `crates/tui/src/
highlight.rs` (10 syntax-token colors + 5 background-wash overlays + 1
inlay-hint chip color) and `crates/tui/src/ui.rs` (17 more literals: git-
gutter marks, a fold-collapsed-line marker, a git-blame lane prefix, the
right-margin guide, diff-line coloring, several generic error-text
indicators, and the focused-pane indicator) hardcode `ratatui::style::Color`
values directly at each call site. `docs/roadmap.md` previously documented this as
a deliberate decision ("`ide-tui` has no theme -- terminal is a different
rendering environment"). **That decision is explicitly reversed by this
feature**, per the user's direction while scoping the `claude.ai/design`
import of `Orbit TUI.dc.html` (a terminal-IDE mockup): asked to choose
between honoring the old decision or reversing it, the user chose "Reverse
the decision, add theming." `docs/roadmap.md`'s old row is updated instead
of left stale (see that file's own diff alongside this doc's merge).

This doc covers **only** the color-theme half of that design-import work.
The mockup's second, unrelated feature -- per-pane-edge "[+] bind action"
extensibility slots -- is tracked separately and implemented as its own
follow-up feature/doc, per the user's explicit choice to split the two.

**Branding note:** the mockup's sample content names its (fictional)
product "Orbit" / "orbit-core". Per explicit user correction, that is
placeholder sample text in the design tool, not something this project
adopts -- nothing in this feature names anything "Orbit," in code, UI
strings, or docs beyond this historical-context paragraph. The new theme
introduced here is named **`Ember`** (a plain description of its warm
charcoal-and-red-orange palette, not a product name).

Two themes ship: `Classic` (today's exact existing colors, extracted
verbatim -- selecting it produces **zero visual change** from `ide-tui`'s
current behavior) and `Ember` (seeded from the mockup's palette, applied
everywhere `Classic`'s hardcoded literals could safely be replaced with a
value actually grounded in that palette -- see §3.2's per-field table for
which fields differ and which don't, and why).

Per the user's follow-up steering ("I'm thinking that color palette must
be in settings section"), theme selection is **not** a bare command-
palette toggle. It's a dedicated **Theme Settings popup** -- the same
settings-surface shape `ide-tui` already has for keybindings
(`ToggleKeymapSettings` / `KeymapPopupState`, `docs/features/tui-keymap.md`)
-- listing both themes with the active one marked, navigated with
`Up`/`Down`, applied and persisted with `Enter`.

## 2. Interface / API

### 2.1 `ide-core`

None. Pure `ide-tui` presentation state; no shared-library change.

### 2.2 `ide-lsp`

None.

### 2.3 `ide-tui`

**New file `crates/tui/src/theme.rs`:**

```rust
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
    /// All themes, in the exact order the Theme Settings popup lists them
    /// (`ui.rs::render_theme_popup`, `app.rs::theme_popup_rows`).
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

/// Every color `highlight.rs`/`ui.rs` currently hardcode as a literal
/// `Color::` value -- one field per call site's *semantic role*, not one
/// per literal (several existing sites already share a role, e.g. every
/// generic error-text display is `Color::Red` today; they share one field
/// here too). See §3.2 for the exact old-literal -> field mapping, and
/// §2.3's `ui.rs` walkthrough below for every individual call site this
/// covers.
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
```

`CLASSIC: Theme` and `EMBER: Theme` are `static` (or `const`, if every
field type allows it under the pinned `ratatui` version -- use `static`
uniformly if not, for consistency) values defined in this file. §3.2 gives
every field's value in both.

**`crates/tui/src/highlight.rs`** -- two signature changes, both additive
parameters, no restructuring of `LineOverlays`:

```rust
pub fn style_for(kind: TokenKind, theme: &Theme) -> Style { ... }

pub fn styled_line(
    text_buffer: &TextBuffer,
    line: usize,
    overlays: &LineOverlays<'_>,
    tab_width: usize,
    theme: &Theme,
) -> Line<'static> { ... }
```

Every internal `Color::Literal` this function currently hardcodes (the
five `.bg(Color::...)` washes, the inlay-hint chip's `Color::DarkGray`)
becomes the matching `theme.wash_*`/`theme.chip_fg` field read. `style_for`
matches on `kind` exactly as before, reading `theme.syntax.<field>` instead
of a literal in each arm.

**`crates/tui/src/ui.rs`** -- no function signature changes beyond what's
needed to reach a `&Theme`. Every render function that currently hardcodes
one of the 17 identified `Color::` literals already takes `app: &App`; each
gets one added local, `let theme = app.theme.theme();`, and every literal
in that function becomes the matching `theme.<field>` read. `render_editor`
(which builds `LineOverlays` and calls `styled_line` once per visible row)
resolves `theme` once at the top of the function and threads it through,
not once per row.

Every one of the 17 current `grep -n "Color::" crates/tui/src/ui.rs` hits,
by line and target field (all inside/reachable from `render_editor` unless
noted):

| Line(s) | What it renders today | New field |
|---|---|---|
| `496` | Fold-collapsed-line `\u{22ef}` marker's fg | `fold_marker_fg` |
| `502-505` | Git-gutter mark glyph fg (`+`/`~`/`-`/blank) | `git_added`/`git_modified`/`git_deleted`/`git_none` respectively |
| `516` | Git-blame lane prefix's fg | `blame_lane_fg` |
| `539` | Right-margin guide's cell **background** (`Buffer::cell_mut(...).set_bg`, not a `Style`/`Span` -- a direct terminal-cell paint, not a fg/bg on rendered text) | `right_margin_guide_bg` |
| `1259` | An exited Claude-terminal tab's dimmed label fg (`render_claude_tab_strip`, not `render_editor`) | `git_none` is the wrong fit here (different function, different meaning) -- reuses `fold_marker_fg`'s muted-gray role instead, since both are "dim / de-emphasized text," not gutter-specific |
| `1302`, `1450`, `2506`, `2546`, `2595`, `2667` | Generic error-text lines (Claude-panel error, debug-launch error, worktree-add error, clone-panel error, log-filter error) -- none are `render_editor`, none are LSP diagnostics | `error_text` |
| `2807`-`2808` | `diff_spans_to_line("- "/"+ ", ..., Color::Red/Green)` -- no signature change needed, callers just pass `theme.diff_removed`/`theme.diff_added` instead of the literal | `diff_removed` / `diff_added` |
| `2913` | `fn focus_style` -- the **currently-focused pane/panel**'s indicator fg, not a search-match highlight | `focus_indicator` |

(Line `1259`'s reuse of the same field as the fold marker is a judgment
call, not a hard requirement -- an implementer who'd rather give the
exited-Claude-tab dimming its own field is free to, since both are
cosmetic "muted text" roles with no behavioral coupling between them; the
point that must hold is that no `Color::` literal is left unresolved to
some named field, not that the exact field-to-site assignment above is
sacred.)

**`crates/tui/src/lib.rs`** -- the startup save at `lib.rs:130`
(`state::save(&state::PersistedState { last_project: ..., format_on_save:
app.format_on_save })`) is a second `PersistedState` literal-construction
site beyond `app.rs`'s `toggle_format_on_save` (below) -- both must gain a
`theme:` field once `PersistedState` gains one, or neither compiles.
`lib.rs:130`'s existing comment already states the exact rule to extend:
*"`format_on_save` is carried through from whatever `App::new` itself just
loaded... so this startup save never resets a previously-toggled-on
preference back to `false`"* -- the fix is `theme: app.theme` alongside the
existing `format_on_save: app.format_on_save`, for the identical reason:
without it, every `ide-tui` startup would silently reset a chosen `Ember`
theme back to `ThemeKind::default()` (`Classic`) on the very next launch,
even though the field would still compile cleanly with any placeholder
value -- this is a correctness requirement, not just a compile-error
fixup.

**`crates/tui/src/app.rs`:**

```rust
pub(crate) struct App {
    // ...
    pub(crate) theme: ThemeKind,
    theme_popup: Option<ThemePopupState>,
    // ...
}

/// Presence is visibility, same convention as `KeymapPopupState`/every
/// other list-picker popup in this crate.
pub(crate) struct ThemePopupState {
    pub(crate) selected: usize,
}
```

- `App::new` initializes `theme: crate::state::load().theme` (same spot
  `format_on_save: crate::state::load().format_on_save` already does,
  `app.rs:1068`), `theme_popup: None`.
- `close_all_overlays()` gains `self.theme_popup = None;` alongside its
  existing `self.keymap_popup = None;` line.
- `any_popup_open()` gains `|| self.theme_popup.is_some()` alongside its
  existing `|| self.code_actions.is_some()` line.
- The main key-routing chain gains, at the same priority tier as the
  existing `if self.keymap_popup.is_some() { return
  self.handle_keymap_popup_key(key); }` check:
  ```rust
  if self.theme_popup.is_some() {
      return self.handle_theme_popup_key(key);
  }
  ```
- New methods, mirroring `toggle_keymap_popup`/`handle_keymap_popup_key`'s
  exact shape:
  ```rust
  fn toggle_theme_popup(&mut self) {
      let opening = self.theme_popup.is_none();
      self.close_all_overlays();
      if opening {
          let selected = ThemeKind::ALL
              .iter()
              .position(|k| *k == self.theme)
              .unwrap_or(0);
          self.theme_popup = Some(ThemePopupState { selected });
      }
  }

  fn handle_theme_popup_key(&mut self, key: KeyEvent) -> LoopSignal {
      let Some(state) = self.theme_popup.as_mut() else {
          return LoopSignal::Continue;
      };
      match key.code {
          KeyCode::Esc => self.theme_popup = None,
          KeyCode::Up => {
              state.selected = state.selected.saturating_sub(1);
          }
          KeyCode::Down => {
              let len = ThemeKind::ALL.len();
              state.selected = (state.selected + 1).min(len.saturating_sub(1));
          }
          KeyCode::Enter => {
              let kind = ThemeKind::ALL[state.selected];
              self.theme = kind;
              self.persist_theme();
              self.theme_popup = None;
          }
          _ => {}
      }
      LoopSignal::Continue
  }

  /// Same persist-on-change shape `toggle_format_on_save` already
  /// establishes (`app.rs:6357`) -- reload-then-overwrite through
  /// `state_path_override` when set, so tests never touch the real
  /// `$HOME/.config/ide-tui/state.json`.
  fn persist_theme(&mut self) {
      let mut state = match &self.state_path_override {
          Some(path) => crate::state::load_from(path),
          None => crate::state::load(),
      };
      state.theme = self.theme;
      match &self.state_path_override {
          Some(path) => crate::state::save_to(path, &state),
          None => crate::state::save(&state),
      }
  }

  pub(crate) fn theme_popup_rows(&self) -> &'static [ThemeKind] {
      &ThemeKind::ALL
  }
  ```
  `persist_theme` reads-then-writes the full `PersistedState` (unlike
  `toggle_format_on_save`, which only had `last_project`+`format_on_save`
  to reconstruct inline) so that persisting a theme change never clobbers
  `last_project`/`format_on_save` with stale/default values -- `app.rs`
  already tracks `self.project_root` and `self.format_on_save` separately
  from the file, so a naive `PersistedState { last_project: ...,
  format_on_save: ..., theme: kind }` literal would also work and read
  simpler; either is acceptable, but the load-then-overwrite form used
  above is the one that stays correct automatically if `PersistedState`
  gains a fourth field later without every persist-a-setting method being
  revisited. Implementer's choice; note whichever is picked in the
  `## Revision notes`-equivalent commit if it differs from this sketch.
- `run_action`: `Action::ToggleThemeSettings => self.toggle_theme_popup(),`

**`crates/tui/src/commands.rs`:**

```rust
Action::ToggleThemeSettings, // new enum variant

Command {
    id: "ToggleThemeSettings",
    title: "Theme",
    // Palette-only -- same reasoning as `ToggleKeymapSettings` right
    // above it: this is a new settings surface with no reference-IDE
    // keybinding to translate, so per `CLAUDE.md`'s "never invent a
    // binding" rule it gets none.
    binding: None,
    action: Action::ToggleThemeSettings,
},
```

Placed adjacent to the existing `ToggleKeymapSettings` entry.

**`crates/tui/src/state.rs`:**

```rust
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedState {
    pub last_project: Option<PathBuf>,
    #[serde(default)]
    pub format_on_save: bool,
    /// `#[serde(default)]` so a state file written before `T41` still
    /// deserializes -- a missing field means `ThemeKind::Classic`
    /// (`ThemeKind`'s own `#[default]`), i.e. today's unchanged look.
    #[serde(default)]
    pub theme: ThemeKind,
}
```

**`crates/tui/src/ui.rs`** -- one new render function, mirroring
`render_keymap_popup`'s exact centered-popup/`Clear`/`List` shape:

```rust
fn render_theme_popup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.theme_popup.as_ref() else {
        return;
    };
    // centered Rect, `Clear`, then a `List` over `app.theme_popup_rows()`:
    // each row is `kind.label()`, `Modifier::REVERSED` on `state.selected`,
    // a trailing "(current)" marker on whichever row equals `app.theme`
    // (distinct from `state.selected`, since the popup opens with the
    // selection on the current theme but a user can arrow away from it
    // before committing with Enter -- the marker must track the *applied*
    // theme, not the highlighted row). Title: "Theme  (Enter: apply, Esc:
    // close)".
}
```

Wired into the render dispatch the same place/way `render_keymap_popup` is
(`ui.rs:193-194`'s `if app.keymap_popup.is_some() { render_keymap_popup(...); }`
gains a sibling `if app.theme_popup.is_some() { render_theme_popup(...); }`).

## 3. Behaviour

### 3.1 Opening, navigating, applying

- `ToggleThemeSettings` (command palette only, no default binding) with the
  popup closed: closes every other overlay (`close_all_overlays`'s existing
  behavior, `toggle_keymap_popup`'s exact pattern) and opens the Theme
  Settings popup with the selection on whichever theme is currently active.
- Already open: closes it (an idempotent toggle, matching
  `toggle_keymap_popup`'s own re-invocation behavior).
- `Up`/`Down`: move the highlighted row, clamped to `[0, ThemeKind::ALL.len()
  - 1]` -- no wraparound, matching `handle_keymap_popup_key`'s existing
  `Up`/`Down` clamping (not the wrapping style some other popups in this
  crate use).
- `Enter`: sets `App::theme` to the highlighted row's `ThemeKind`, persists
  it to `state.json` via `persist_theme`, closes the popup. The **next
  frame** renders with the new theme -- every render function reads
  `app.theme.theme()` fresh each frame, so there is no separate "apply"
  step beyond setting the field.
- `Esc`: closes the popup without changing `App::theme`, even if `Up`/
  `Down` moved the highlight away from the currently-active theme first
  (mirrors every other list-picker popup's cancel behavior in this crate).
- Restart: `App::new` loads `PersistedState::theme` and starts already
  rendering in that theme -- no popup interaction needed to restore a
  previous choice.

### 3.2 Palette (`Classic` vs `Ember`, field by field)

`Classic` is every literal `highlight.rs`/`ui.rs` already use today,
extracted with zero value changes. `Ember` is seeded from `Orbit TUI.
dc.html`'s CSS custom properties -- fields with a direct hex source in that
mockup take that color (translated to the nearest reasonable `ratatui`
representation, `Color::Rgb` truecolor, which this crate already uses
elsewhere: `claude_terminal.rs`'s xterm-palette mapping); fields the mockup
never gives an opinion on (git-status colors, the focused-pane indicator,
selection/highlight washes) are **not invented** -- they keep `Classic`'s
value in `Ember` too, since guessing a hue not present in the source design
would not be "seeded from the mockup's palette," just decorating past it.

| Field | `Classic` (today, unchanged) | `Ember` | Source / rationale |
|---|---|---|---|
| `syntax.keyword` | `Color::Magenta` | `Rgb(0xff,0x56,0x3c)` | mockup's `--accent-hover` / keyword color |
| `syntax.operator` | `Color::Red` | `Rgb(0xff,0x56,0x3c)` | same accent family as keyword; mockup treats both as "emphasis" |
| `syntax.string` | `Color::Green` | `Rgb(0xba,0xb6,0xb6)` | mockup's string-literal gray |
| `syntax.comment` | `Color::DarkGray` | `Rgb(0x9b,0x97,0x97)` | mockup's muted-text gray |
| `syntax.number` | `Color::LightYellow` | `Rgb(0xff,0x97,0x83)` | mockup's soft-accent (numeric literal in its own code sample) |
| `syntax.constant` | `Color::LightRed` | `Rgb(0xff,0x97,0x83)` | same soft-accent bucket as `number` -- mockup doesn't distinguish these two roles |
| `syntax.key` | `Color::Blue` | `Rgb(0xff,0x97,0x83)` | same soft-accent bucket; no distinct "object key" color in the mockup |
| `syntax.function` | `Color::Cyan` | `Rgb(0xea,0xe7,0xe7)` | mockup's bright default foreground -- no distinct call-site color exists there |
| `syntax.type` | `Color::LightCyan` | `Rgb(0xff,0x97,0x83)` | same soft-accent bucket as `number`/`constant`/`key` |
| `syntax.macro` | `Color::LightMagenta` | `Rgb(0xff,0x56,0x3c)` | same accent family as `keyword`/`operator` |
| `wash_document_highlight` | `Color::DarkGray` | `Color::DarkGray` | not sourced from the mockup -- kept |
| `wash_selection` | `Color::Yellow` | `Color::Yellow` | not sourced from the mockup -- kept |
| `wash_bracket_pair` | `Color::Blue` | `Color::Blue` | not sourced from the mockup -- kept |
| `wash_breakpoint_verified` | `Color::Red` | `Rgb(0xec,0x30,0x13)` | mockup's primary accent (`--accent`) -- a verified breakpoint is exactly the kind of "this line matters" emphasis that color is for |
| `wash_breakpoint_unverified` | `Color::DarkGray` | `Color::DarkGray` | not sourced from the mockup -- kept |
| `chip_fg` | `Color::DarkGray` | `Color::DarkGray` | not sourced from the mockup -- kept |
| `gutter_fg` | `Color::DarkGray` | `Rgb(0x9b,0x97,0x97)` | mockup's muted-text gray |
| `fold_marker_fg` | `Color::DarkGray` | `Rgb(0x9b,0x97,0x97)` | mockup's muted-text gray, same bucket as `gutter_fg` |
| `blame_lane_fg` | `Color::DarkGray` | `Rgb(0x9b,0x97,0x97)` | mockup's muted-text gray, same bucket as `gutter_fg` |
| `right_margin_guide_bg` | `Color::DarkGray` | `Rgb(0x44,0x41,0x41)` | mockup's `--border`/divider color -- a natural fit for a vertical column guide, which is itself a divider |
| `git_added` | `Color::Green` | `Color::Green` | not sourced from the mockup -- kept |
| `git_modified` | `Color::Blue` | `Color::Blue` | not sourced from the mockup -- kept |
| `git_deleted` | `Color::Red` | `Rgb(0xec,0x30,0x13)` | mockup's primary accent -- same "danger/removed" role `Color::Red` already served |
| `git_none` | `Color::DarkGray` | `Rgb(0x9b,0x97,0x97)` | mockup's muted-text gray |
| `diff_added` | `Color::Green` | `Color::Green` | not sourced from the mockup -- kept |
| `diff_removed` | `Color::Red` | `Rgb(0xec,0x30,0x13)` | mockup's primary accent, same reasoning as `git_deleted` |
| `error_text` | `Color::Red` | `Rgb(0xec,0x30,0x13)` | mockup's primary accent, same reasoning |
| `focus_indicator` | `Color::Yellow` | `Color::Yellow` | not sourced from the mockup -- kept (the mockup's own focused-pane treatment is a border/shadow change ratatui has no direct equivalent for, see §3.3) |

`syntax.function`'s choice (plain bright foreground, no distinguishing
color at all) is the one field where "faithful to the mockup" produces a
visibly *less* distinct syntax highlight than `Classic`'s `Color::Cyan` --
worth a reviewer's eyes; it is not a mistake, it is what the source design
actually shows, but a future revision could reasonably add
`Modifier::BOLD` to give it *some* distinction without inventing an
off-palette hue. Not done here, to keep this field's value a pure "read
what the mockup says" fact rather than a judgment call layered on top.

### 3.3 Fonts and CSS-only constructs -- not applicable

The mockup specifies `Archivo`/`IBM Plex Mono` web fonts and several
CSS-only visual constructs (box-shadow underlines on the active tab,
`:hover` state transitions, `border-radius`). None of these have a terminal
equivalent and none are in scope here: `ide-tui` renders whatever
monospace font the user's terminal emulator is configured with (as it
always has -- this feature changes colors only, never font selection), and
`ratatui` has no hover concept (`crossterm` reports discrete key/mouse
events, not continuous pointer state a `:hover` transition would need).
This is stated explicitly per this project's own `CLAUDE.md` /
memory-file guidance to document such translations for reviewability,
rather than silently dropping them.

## 4. Constraints & invariants

- `ThemeKind::Classic` must produce **byte-for-byte identical** rendered
  output to `main` before this feature, for every existing test in
  `highlight.rs`/`ui.rs` that asserts on a specific `Style`/`Color` --
  those tests are updated to pass `&CLASSIC` (or `ThemeKind::Classic.
  theme()`) explicitly and continue asserting the exact same literal
  values, proving the refactor introduced no accidental change alongside
  the new plumbing.
- `Theme`/`SyntaxColors`/`ThemeKind` are plain data (`Copy` where the
  `ratatui::style::Color` type allows it -- `Color` itself is `Copy`, so
  both structs can be too) -- no allocation, no `Rc`/`Arc`, matching every
  other config-shaped type already in this crate.
- `ThemeKind::ALL`'s order is the popup's display order and is a public
  contract of sorts (a persisted `selected` index would break across a
  reorder) -- but since `App::theme_popup`'s `selected` is transient popup
  UI state, never itself persisted (only the resulting `ThemeKind` is),
  reordering `ALL` later is safe and doesn't require a persisted-state
  migration.
- No `unsafe`, no new subprocess/file I/O beyond `state.rs`'s existing
  best-effort load/save (already covered by that module's own doc comment:
  failure is silent, never blocks startup or theme switching).

## 5. Examples

**Switching themes:**

```rust
let mut app = App::new(project_root)?;
assert_eq!(app.theme, ThemeKind::Classic); // default

app.run_action(Action::ToggleThemeSettings);
assert!(app.theme_popup.is_some());

app.handle_theme_popup_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
app.handle_theme_popup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

assert_eq!(app.theme, ThemeKind::Ember);
assert!(app.theme_popup.is_none());
```

**Persistence round-trip:**

```rust
// after the switch above, with `state_path_override` pointed at a tempdir
assert_eq!(crate::state::load_from(&state_path).theme, ThemeKind::Ember);

// a fresh App started against the same state path picks it back up:
let mut app2 = App::new(project_root)?;
app2.state_path_override = Some(state_path.clone());
// (App::new itself reads the *default* state path, not the override --
// tests exercise this by constructing PersistedState/writing it to the
// override path first, then asserting `App::new`'s *initial* `theme`
// field separately via a `new_with_state_path`-style test seam if one
// already exists in this file for `format_on_save`; mirror whatever that
// existing seam is rather than inventing a new one.)
```

**Reading the active palette from a render function:**

```rust
fn render_editor(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let theme = app.theme.theme();
    // ... build `LineOverlays` as today, then:
    let line = styled_line(text_buffer, row, &overlays, tab_width, theme);
}
```

## 6. Dependencies

None beyond what `crates/tui/Cargo.toml` already has (`ratatui`,
`serde`/`serde_json` via `state.rs`'s existing usage). No new crate.

## 7. Diagram

Skipped -- the popup's own state machine (closed -> open -> Enter/Esc ->
closed) is fully described by §3.1 and is materially the same shape
`tui-keymap.md`'s already-shipped `KeymapPopupState` diagram (if any) would
show; a new diagram wouldn't add clarity a reader doesn't already get from
that precedent plus this doc's text.

## Revision notes

- `rev` (doc review, round 1) found three gaps, all fixed in place:
  1. §2.3 never named `lib.rs:130`, a second `PersistedState`
     literal-construction site that also needs a `theme:` field --
     added, with the exact silent-regression risk (theme resetting to
     `Classic` every startup) spelled out.
  2. The field originally named `search_match` (claimed source: a
     search-highlight yellow) was actually sourced from `focus_style`'s
     `Color::Yellow` at `ui.rs:2913` -- the currently-focused-pane
     indicator, an unrelated role. Renamed to `focus_indicator` and
     corrected everywhere it's referenced.
  3. Three real `ui.rs` `Color::` sites (the fold-marker glyph at `496`,
     the blame-lane prefix at `516`, and the right-margin guide's direct
     cell-background paint at `539`) existed in the source but weren't
     individually accounted for in §2.3/§3.2, leaving no designated
     `Theme` field for an implementer to use. Added `fold_marker_fg`,
     `blame_lane_fg`, and `right_margin_guide_bg`, plus a full
     line-by-line table in §2.3 mapping every one of the 17 `ui.rs` sites
     to its field so none can be missed a second time.
  Also applied the review's non-blocking `[quality]` note: renamed
  `diagnostic` to `error_text`, since none of its six source sites are
  actual LSP diagnostics.
