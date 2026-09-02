# Right margin guide

## 1. Purpose

A static vertical line in the editor at a configurable column (JetBrains'
"Right Margin" / "Visual Guide"), marking a soft line-length convention
(default 120). No overflow highlighting, no wrapping, no enforcement — a
guide only, matching the user's explicit choice.

Configurable per language in `ide-ui` (which already has a "Languages…"
settings window and per-file language lookup); `ide-tui` has no
per-language settings storage or UI at all today (it only ever detects one
project-wide language via `Cargo.toml`, and doesn't even retain that
`LanguageConfig` after startup — confirmed by reading `App::new`), so
building a language-settings system there just for this is out of
proportion to the feature. Per an explicit user decision, `ide-tui` always
draws the guide at the fixed default column (120), with no way to change
it in that frontend — the same category of accepted TUI/GUI parity gap as
"no custom languages," "no gutter." Only `ide-ui` gets per-language
configurability.

## 2. Interface

### 2.1 `crates/core/src/language.rs`

```rust
pub struct LanguageConfig {
    // ...existing fields...
    /// `None` means "use the global default (120)". Same
    /// backward-compatible `#[serde(default)]` treatment as
    /// `debug_adapter_command`.
    #[serde(default)]
    pub right_margin_column: Option<u32>,
}

impl LanguageConfig {
    /// `right_margin_column`, or `120` if unset -- the one call site that
    /// applies the default (mirrors `debug_adapter()`'s "one call site
    /// decides" shape).
    pub fn right_margin_column(&self) -> u32;
}
```

`LanguageConfig::rust()` leaves `right_margin_column: None` (uses the
default) — nothing in this doc changes Rust's own effective value.

### 2.2 `crates/ui/src/app.rs` — "Languages…" settings window

One new draft field, alongside the existing `new_language_*` fields
`add_custom_language` already reads:

```rust
new_language_right_margin_column: String,
```

`add_custom_language`'s validation gains one more parse step: the field
is optional (blank means `None`, i.e. "use 120"). A non-blank value must
parse as a positive `u32` (`str::parse::<u32>()`, rejecting `0` — a
zero-column margin is meaningless); a value that fails to parse or is `0`
sets `language_settings_error` and leaves everything else untouched,
exactly like the existing "Name, extension, and command are all required"
rejection path. On success the parsed `Option<u32>` goes into the new
`LanguageConfig { ..., right_margin_column, ... }` field, and the draft
field is cleared like every other one.

The settings window itself gains one new text input next to the existing
debug-adapter fields, labeled "Right margin column (blank = 120)".

### 2.3 `crates/ui/src/editor/paint.rs` / `crates/ui/src/editor/mod.rs`

```rust
pub fn paint_right_margin_guide(
    painter: &egui::Painter,
    x: f32,
    top: f32,
    bottom: f32,
    color: egui::Color32,
);
```

A single `painter.line_segment([pos2(x, top), pos2(x, bottom)], Stroke::new(1.0, color))`
— same shape as `paint_code_action_marker`'s existing `line_segment` call.
`EditorState::paint` (`mod.rs`) computes `x = text_left + column as f32 *
self.metrics.char_width` (the same "column → x" arithmetic the caret-
positioning code around `mod.rs:1859` already performs) and calls this
right after the current-line band and before the per-row text-painting
loop, so the guide sits **behind** glyphs, not on top of them (matches the
current-line band's own "everything else draws over it" ordering, applied
in the opposite sense: the guide draws first so text stays legible when it
crosses the line). `top`/`bottom` span the full viewport height
(`origin.y + viewport.min.y` to `+ viewport.height()`), the same span
`paint_gutter`'s own full-height background rect already uses — so the
guide runs the whole visible editor height, including past the buffer's
last line, not just where text exists.

`column` comes from `ide_core::language_for_path(&self.active_languages,
path)` for the active tab's path, `.map(|c| c.right_margin_column())`,
falling back to `120` with no active tab or no language match — the same
`language_for_path` lookup `trigger_debug`/`is_command_enabled` already
use for `debug_adapter()`.

Color: reuses the existing `self.tokens.color.border` token — no new
theme color is introduced.

### 2.4 `crates/tui/src/ui.rs` — `render_editor`

No new public interface. After `frame.render_widget(paragraph, text_area)`
(and only when a buffer is open — the "No file open" placeholder path
draws no guide), tints the background of column `text_area.x + 120` across
every row of `text_area` via `frame.buffer_mut().get_mut(x, y).set_bg(Color::DarkGray)`
— a background tint on the existing cell, not a replacement glyph, so
whatever character (or blank space) was already there stays visible,
matching a "line behind text" look with ratatui's cell model. Skipped
entirely when `text_area.x + 120 >= text_area.x + text_area.width` (the
column is off-screen for the current terminal width) — no horizontal
scroll exists in this crate to bring it into view (confirmed during
`tui-mouse-support.md`'s research: no column-offset state anywhere in
`ide-tui`), so a narrow terminal simply never shows the guide, the same as
a real terminal too narrow to show column 120 of anything else.

The column is always the literal constant `120` — no `LanguageConfig`
lookup, no settings surface, per §1.

## 3. Behaviour

- The guide is a **static** vertical marker at a fixed column. It does
  not move, does not highlight characters that cross it, and does not
  wrap or clip content — a line drawn at a column position that visually
  coincides with wherever the text happens to be at that point, nothing
  more.
- **`ide-ui`**: the column is resolved once per frame from the active
  tab's language (`language_for_path`), so switching tabs between two
  files of different configured languages changes the guide's position
  immediately, the same way switching tabs already changes syntax
  highlighting.
- **`ide-tui`**: the column is always `120`, unconditionally, for every
  file regardless of language — there is no mechanism to change it.
- Neither frontend enforces a maximum line length or flags lines that
  cross the guide — this phase adds no diagnostics, no warnings, no
  status-bar count of "N lines over the limit."
- The guide renders even on a completely empty buffer / blank lines (a
  full-height line, not one that only appears next to actual text) in
  both frontends.

## 4. Constraints & invariants

- No overflow highlighting, no line wrapping — a plain visual guide only
  (explicit user decision, this phase).
- `ide-tui` never reads `LanguageConfig::right_margin_column` — the
  column is a literal constant there. A future feature that gives
  `ide-tui` real per-language settings (out of scope here) would be the
  point to revisit this, not something this phase should half-build.
- `right_margin_column: Option<u32>` on `LanguageConfig` is
  backward-compatible (`#[serde(default)]`) with every already-persisted
  `custom_languages` entry, the same guarantee `debug_adapter_command`
  already established for itself.
- A `0` right-margin column is rejected at input time (§2.2) — never
  stored, never reaches the paint code, so `paint_right_margin_guide`
  and the TUI's column arithmetic never need to special-case it.
- The guide's color is a plain UI/theme concern in both frontends, not
  security- or correctness-sensitive; no new security-sensitive surface
  is introduced. `custom_languages` is parsed from `.ide/preferences.json`
  by `crates/ui/src/app.rs`'s `ProjectPreferences` struct (**not**
  `crates/core/src/project_settings.rs`, which has no involvement with
  `LanguageConfig` at all), via a plain `#[derive(Deserialize)]` on
  `Vec<LanguageConfig>` — no separate schema to keep in sync. Because
  `right_margin_column` carries its own `#[serde(default)]` (§2.1), an
  older on-disk `preferences.json` written before this feature existed
  deserializes straight through with the field defaulting to `None`,
  the same guarantee `debug_adapter_command` already established for
  itself on this exact struct. Nothing new to validate against path
  traversal or injection — the untrusted-input handling this file already
  needs (`deserialize_bounded_custom_languages`'s entry-count cap) is
  unchanged by adding one more field to each entry.

## 5. Examples

`ide-ui`, per-language override via the Languages… window:

```
User opens Languages… settings, adds a custom "Python" entry with
Right margin column = 79.
-> LanguageConfig { name: "Python", ..., right_margin_column: Some(79) }
-> opening a .py file: language_for_path resolves this config
-> paint_right_margin_guide draws at column 79, not the 120 default
-> switching to a .rs tab: language_for_path finds no match in
   active_languages beyond the built-in Rust config (right_margin_column:
   None) -> guide draws at the default, 120
```

`ide-tui`, always the fixed default:

```
Any file, any language: render_editor tints column text_area.x + 120's
background across every row of text_area. No settings, no per-language
lookup -- always 120, or invisible if the terminal is narrower than that.
```

## 6. Dependencies & integration points

- `crates/core/src/language.rs` (`LanguageConfig`, `debug_adapter()`'s own
  "one call site applies the default" precedent).
- `crates/ui/src/app.rs` (`add_custom_language`, the Languages… settings
  window, `language_for_path`/`active_languages` — the same lookup
  `debug_adapter()`'s call sites already use).
- `crates/ui/src/editor/paint.rs` (`paint_code_action_marker`'s
  `line_segment` precedent) and `crates/ui/src/editor/mod.rs` (`EditorState
  ::paint`, `self.metrics.text_left`/`char_width`, `self.tokens.color
  .border`).
- `crates/tui/src/ui.rs` (`render_editor`'s existing `text_area` rect,
  `Frame::buffer_mut`).
- No `ide-lsp`/`ide-dap` involvement. No new dependency (table in
  `CLAUDE.md`) — everything needed already exists in `egui`/`ratatui`.
- Merge order for this run: `rust-core-dev` → `rust-tui-dev` →
  `rust-ui-dev` (both frontends touched, so `tui` merges before `ui` per
  the dependency-direction rule in `CLAUDE.md`'s workspace-layout
  section).

## Revision notes

- §4's original draft claimed `custom_languages` persistence went through
  `crates/core/src/project_settings.rs`. Verified against the actual code
  during `rev`: that file has zero references to `custom_languages` or
  `LanguageConfig`. The real persistence path is `crates/ui/src/app.rs`'s
  `ProjectPreferences` struct (`.ide/preferences.json`), which derives
  `Deserialize` directly on `Vec<LanguageConfig>` — corrected §4 to name
  the right struct/file and the right backward-compatibility mechanism
  (`#[serde(default)]` on the new field, same as `debug_adapter_command`).
