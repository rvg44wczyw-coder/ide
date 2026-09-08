# T58 — Settings consolidation (`ide-tui`)

## 1. Purpose

`ide-ui` shipped **G1**, a unified Settings window (`⌘,`) consolidating
previously-scattered floating popups (Language, Keymap) plus new Appearance/
Editor/Code Style pages into one navigable surface, and — separately —
introduced a genuine user-level (cross-project) settings file
(`~/.config/ide/settings.json`) to fix a real bug: GUI persisted
theme/keymap/format-on-save *per project* (`.ide/preferences.json`), so a
user's theme or keybinding customizations reset to defaults in every new
project.

Neither half of G1's motivation transfers to `ide-tui` unmodified:

- **The per-project-reset bug doesn't exist here.** `ide-tui`'s theme
  (`~/.config/ide-tui/state.json`, `docs/features/tui-theme.md`, T41) and
  keymap overrides (`~/.config/ide-tui/keymap.json`, `docs/features/
  tui-keymap.md`, T22) were **already** user-level, home-directory-scoped
  files from the day each shipped — never project-scoped, never reset by a
  project switch. There is nothing to migrate.
- **Discoverability is a smaller problem here.** `ide-ui`'s popups are
  reached only via a menu/toolbar click or a memorized keybinding; G1's
  "one navigable place to find every preference" is a real usability win
  there. `ide-tui` already has a command palette (`FindAction`,
  `Ctrl+Shift+A`) that reaches `ToggleThemeSettings`/`ToggleKeymapSettings`
  by typing a few letters of "theme"/"keymap" — the underlying "preferences
  are hard to find" problem G1 solves for a GUI is largely already solved
  here by the palette, independent of this feature.

What *does* transfer, and is the actual, honestly-scoped deliverable of
this run: **a single navigable "Settings" concept spanning the two
existing per-user popups**, mirroring G1's own page-tab mechanic
(`SettingsPage`, left-hand page list) adapted to this crate's established
"popup, not window" convention (`tui-keymap.md` §1.1: *"`ide-tui` has no
windowing system... The Keymap UI follows [the popup] shape, not a literal
port of `egui::Window`"*). Concretely: `Left`/`Right` inside either the
Theme Settings popup or the Keymap Settings popup switches to the other,
without closing the "Settings" experience — today a user must close one
popup, reopen the palette, and search for the other by name. Everything
else (both popups' existing content, key handling, persistence, commands)
is unchanged.

**Out of scope**, and why: a Languages page (`ide-ui`'s G1 ships one;
`ide-tui` has no per-language custom-config UI at all — nothing to
consolidate), an Editor/Code-Style page (`ide-ui`'s are diagnostic reads of
`Tab`/`EditorConfig` state this crate doesn't currently surface anywhere;
inventing a page to match a GUI page count would be scope invention, the
same call `settings-window.md` §1.1 itself made for Version Control/Tools),
and migrating `state.rs`/`keymap.rs` to `ide_core::user_settings`
(`settings-window.md` §6 explicitly flags this as "a separate decision for
whoever owns that follow-up, not implied or required" — and since neither
file has a bug to fix, doing it here would be a pure refactor riding along
on an unrelated feature, not something this doc's own scope calls for).

## 2. Interface / API

### 2.1 `ide-core` / `ide-lsp`

None. Pure `ide-tui` presentation-layer change — no shared-library
surface touched.

### 2.2 `crates/tui/src/app.rs`

No new fields, no new `Option<...>` state, no `SettingsPage` enum with
persisted state — `theme_popup: Option<ThemePopupState>` and
`keymap_popup: Option<KeymapPopupState>` (`app.rs:1125`/`1127`) stay
exactly as they are, including their existing shapes
(`ThemePopupState { selected: usize }`, `KeymapPopupState { query,
selected, capturing }`). "Which page is active" is simply "which of the
two `Option`s is `Some`" — the same encoding these two popups already use
individually, extended to mean "current settings page" rather than
inventing a parallel `SettingsPage` discriminant that could desync from
it.

**`handle_theme_popup_key`** (`app.rs:9492`) gains one new arm, inserted
before the existing `_ => {}` catch-all:

```rust
KeyCode::Left | KeyCode::Right => {
    self.theme_popup = None;
    self.toggle_keymap_popup();
}
```

**`handle_keymap_popup_key`** (`app.rs:9423`) gains the symmetric arm,
guarded the same way the existing `Enter`/`Delete` arms already are
(after the early `state.capturing` return, so page-switching is only live
while the popup is in its normal browse/search mode, never mid-capture —
see §3.3):

```rust
KeyCode::Left | KeyCode::Right => {
    self.keymap_popup = None;
    self.toggle_theme_popup();
}
```

Both call the existing `toggle_*_popup` method rather than constructing
the target `Option` inline — this reuses `toggle_keymap_popup`/
`toggle_theme_popup`'s existing `close_all_overlays()` call (a no-op here
beyond re-clearing the `Option` this handler just set to `None` itself,
since nothing else was open) and their existing "open with selection on
the current value" initialization (`toggle_theme_popup`'s
`ThemeKind::ALL.iter().position(...)`), so switching to a page always
lands on a freshly-initialized, correct starting state — not stale
selection/query left over from a previous visit. This means a switch never
preserves scroll position or an in-progress search query on the page being
left; §3.1 states this as a deliberate behavior, not an oversight.

Both `KeyCode::Left`/`KeyCode::Right` are otherwise unused by either
popup's non-capturing key handling today (confirmed by reading both match
arms in full — `handle_theme_popup_key` only matches `Esc`/`Up`/`Down`/
`Enter`; `handle_keymap_popup_key` only matches `Esc`/`Up`/`Down`/
`Backspace`/`Enter`/`Delete`/`Char`), so this introduces no rebinding of
existing behavior.

No change to `Action`, `commands.rs`, `run_action`, `close_all_overlays`,
`any_popup_open`, `ToggleThemeSettings`/`ToggleKeymapSettings`'s bindings,
or either popup's persistence path (`persist_theme`/`persist_keymap`) —
all unchanged, including the existing commands' `id` strings, which must
never change since `KeymapOverlay` (T22) persists user overrides keyed by
exactly these id strings (renaming either command's `id` would silently
orphan an existing user's custom binding for it).

### 2.3 `crates/tui/src/ui.rs`

Both popups' title strings gain a page indicator and a switch hint,
otherwise unchanged (same `render_scrollable_list` call, same `Block`,
same list-building logic):

`render_theme_popup` (`ui.rs:1932-1934`), was:

```rust
.title("Theme  (Enter: apply, Esc: close)");
```

becomes:

```rust
.title("Settings: Appearance  (Enter: apply, \u{2190}\u{2192}: Keymap, Esc: close)");
```

`render_keymap_popup` (`ui.rs:1861-1864`), was:

```rust
.title(format!(
    "Keymap: {}  (Enter: rebind, Delete: reset, Esc: close)",
    state.query
));
```

becomes:

```rust
.title(format!(
    "Settings: Keymap: {}  (Enter: rebind, Delete: reset, \u{2190}\u{2192}: Appearance, Esc: close)",
    state.query
));
```

(`\u{2190}`/`\u{2192}` are `←`/`→` — this crate's existing titles already
use non-ASCII glyphs for similar cues, e.g. `render_keys_screen`'s own
`\u{2014}` em dash for an unbound row; matching that convention rather than
spelling out "Left/Right".)

No change to `render_keys_screen` (T44's read-only reference screen) — it
is a distinct, already-existing surface (§1's "out of scope" list doesn't
touch it) and this doc does not fold it into the popup pair; a future run
could reconsider that overlap, but nothing here requires it (§3.4).

## 3. Behaviour

### 3.1 Switching pages resets the target page's transient state

Pressing `Left` or `Right` inside either popup closes it and opens the
other via its existing `toggle_*_popup` method — which always
re-initializes fresh (`ThemePopupState`'s selection reset to the
currently-*applied* theme's row, `KeymapPopupState`'s `query` reset to
empty, `selected` reset to `0`, `capturing` reset to `None`). A search
query typed into the Keymap page, or a highlight moved away from the
active theme in the Appearance page without pressing `Enter`, is
**discarded** on switching away — consistent with each popup's own
existing `Esc`-cancels behavior (moving the highlight without committing
was already non-durable before this feature), and with `close_all_overlays`
already being how every overlay-to-overlay transition in this crate is
implemented today (§3.6 of `tui-screen-navigation.md`'s own popup-layer
convention).

Since there are exactly two pages, `Left` and `Right` are both
"switch to the other page" — there is no third page to distinguish a
direction toward. A future page addition (§1's out-of-scope list) would
need to give `Left`/`Right` actual directional meaning; not needed here.

### 3.2 Entry points are unchanged

`ToggleThemeSettings` opens directly on the Appearance page (as it always
has); `ToggleKeymapSettings` opens directly on the Keymap page (as it
always has). There is no new third "open Settings" command — either
existing entry point already lands the user inside the same two-page
experience this feature connects, and adding a third command whose only
job would be "open the Appearance page" would be a redundant alias for
`ToggleThemeSettings`, not a new capability (see §1's discoverability
argument for why a dedicated always-lands-here launcher matters less when
the palette already reaches both by name).

### 3.3 Page-switching is unavailable mid-capture

`Left`/`Right` inside the Keymap page while `state.capturing.is_some()`
(i.e. the popup is waiting for the next physical keystroke to become a
new binding) are **not** intercepted by the new arm — the existing early
return to `handle_keymap_capture_key` in `handle_keymap_popup_key`
(`app.rs:9424-9428`) still fires first, so `Left`/`Right` are captured as
the literal arrow-key chord being assigned to a command, exactly as
`Up`/`Down`/every other key already is during capture today. This is
existing, unchanged behavior — stated here only to make explicit that the
new page-switch arm sits *after* that early return, not to describe a
new carve-out.

### 3.4 `Keys` screen is untouched

T44's `Keys` screen (`AppScreen::Keys`, a full-screen read-only reference
list reusing `keymap_popup_rows`) is unaffected — it has no `Left`/`Right`
handling today and gains none here; it is not a "page" of this feature's
two-page popup pair, and this doc does not attempt to unify it with them.
Whether `Keys` and the Keymap Settings popup should eventually merge
(one already-flagged, not-yet-built idea being T47's "editable Keys
screen") is a decision for whichever future run actually builds that,
not implied or required by this one.

## 4. Constraints & invariants

- No `Action`/`Command`/persisted-state schema changes — `state.json`'s
  `theme` field and `keymap.json`'s `KeymapOverlay` are both untouched by
  this doc, byte-for-byte.
- Both popups' existing key-handling arms (`Esc`/`Up`/`Down`/`Enter`,
  plus `Keymap`'s `Backspace`/`Delete`/`Char`) are unchanged — only a new
  `Left | Right` arm is added to each, and only in the non-capturing
  branch of `handle_keymap_popup_key`.
- Neither popup's rendered *content* (row list, selection marker, current-
  theme/current-binding indicator) changes — only each `Block`'s `title`
  string.
- `ToggleThemeSettings`/`ToggleKeymapSettings` command `id`s, titles, and
  bindings (both `None`, palette-only, unchanged since T41/T22) are not
  touched — no persisted `KeymapOverlay` override can be orphaned by this
  change.

## 5. Examples

Starting from the Appearance page, switching to Keymap and back:

```rust
let mut app = App::new(project_root)?;
app.run_action(Action::ToggleThemeSettings);
assert!(app.theme_popup.is_some());
assert!(app.keymap_popup.is_none());

app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
assert!(app.theme_popup.is_none());
assert!(app.keymap_popup.is_some());

app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
assert!(app.theme_popup.is_some());
assert!(app.keymap_popup.is_none());
```

A query typed on the Keymap page is discarded by switching away and back:

```rust
app.run_action(Action::ToggleKeymapSettings);
app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
assert_eq!(app.keymap_popup.as_ref().unwrap().query, "s");

app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)); // -> Appearance
app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)); // -> Keymap again
assert_eq!(app.keymap_popup.as_ref().unwrap().query, ""); // reset, not preserved
```

## 6. Dependencies / integration points / tests

No new dependency. Diff scope: `crates/tui/src/{app,ui}.rs` only, plus this
doc and `docs/roadmap.md`. Not security-sensitive per `CLAUDE.md`'s list —
no subprocess, no path/file I/O beyond the two popups' existing,
already-non-sensitive persistence calls (`persist_theme`/`persist_keymap`,
unchanged); `hacker` is skipped for this phase, matching T41's/T22's own
"not security-sensitive" determination for the exact same files.

Tests (extend the existing `theme_popup_*`/`keymap_popup_*` test groups in
`app.rs`, mirroring their established shape):

- `Right` from the Theme page opens the Keymap page (`theme_popup` becomes
  `None`, `keymap_popup` becomes `Some`).
- `Left` from the Keymap page opens the Theme page (symmetric).
- Switching away and back to the Keymap page resets `query`/`selected` to
  their fresh-open defaults, even if they were previously non-default.
- Switching away from the Theme page after moving the highlight (via
  `Down`) without pressing `Enter` does not change `App::theme` — the
  move is discarded, matching `Esc`'s existing cancel behavior.
- `Left`/`Right` while `keymap_popup`'s `capturing` is `Some(id)` are
  captured as the literal chord (routed to `handle_keymap_capture_key`),
  not intercepted as a page switch — proves the ordering in §3.3.
- Existing `theme_popup_*`/`keymap_popup_*` tests (`Esc`/`Up`/`Down`/
  `Enter`/`Backspace`/`Delete`/`Char` handling, persistence round-trips)
  continue passing unmodified — this feature adds one match arm to each
  handler, it does not restructure either.

## 7. Diagram

Skipped — the added behavior is fully described by one state-transition
sentence per direction (§3.1) between two already-diagrammed-elsewhere
popup state machines (`tui-theme.md`/`tui-keymap.md`); a new diagram
would not add clarity beyond that text.
