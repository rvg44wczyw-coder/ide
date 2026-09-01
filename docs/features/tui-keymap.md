# T22 — Keymap customization (`ide-tui`)

## 1. Purpose

Ports **G2** (`docs/features/keymap.md`) to `ide-tui`. `commands.rs`'s
`binding_for` is a static, compile-time table — nothing today lets a user
change a binding. `docs/roadmap.md`'s T22 row names exactly this gap:
"`commands.rs`'s `binding_for` — static table, doesn't read a user
overlay."

`ide-ui`'s G2 is a large feature (preset schemes, an accord/double-tap
gesture registry, a settings *window*, plain-text export/import). Very
little of that surface has an `ide-tui` equivalent to port faithfully, for
reasons specific to how this crate already handles bindings — see §1.1.
What this phase ports is the part of G2 that this crate's own T22 roadmap
row actually asks for: a per-user override layer over the existing
defaults, editable through a popup (this crate's established UI shape for
every other list-like feature), persisted across restarts.

### 1.1 Why the rest of G2 doesn't port, one item at a time

- **Preset schemes (JetBrains/Fleet/VS Code).** `ide-ui`'s schemes work
  because its `Binding` type carries a real `{mac, other}` chord pair per
  command, and each scheme is a from-scratch table of such pairs sourced
  from that product's own reference. `ide-tui`'s `commands.rs` has
  already collapsed every command to *one* terminal-reachable chord, each
  individually hand-translated from the JetBrains mac default with its
  own documented reasoning (masking a `Ctrl+letter` byte, a literal
  function key, an intentionally-substituted free letter, …). A Fleet or
  VS Code "preset" for this crate would mean re-deriving that same
  translation exercise from a *different* product's keymap for all ~40
  commands, sourced and justified individually — a large, separate,
  doc-worthy feature in its own right, not implied by this roadmap row's
  one-line description ("static table, doesn't read a user overlay").
  Cut for this phase; nothing here forecloses adding it later as its own
  T-item if wanted.
- **Gestures / accords (§2.3/§2.4 of the `ide-ui` doc).** This crate has
  no double-tap or accord primitive at all, and their one current/planned
  consumer on the `ide-ui` side (`CloneCaretUpDown`, `⌥⌥`) was already
  *deliberately cut* from `ide-tui` in `T20` (`tui-multiple-cursors.md`
  §1.1: "`⌥Click`/Clone Caret/Column Selection сознательно вырезаны —
  нет мыши"). With no gesture to display and no accord consumer scheduled
  for this frontend, there is nothing for this section to port.
- **A settings *window*.** `ide-tui` has no windowing system — every
  multi-row, searchable, editable list in this crate (Recent Files,
  Bookmarks, TODO panel, the command palette itself) is a centered popup
  over the full-screen layout, not a separate window. The Keymap UI
  follows that same shape (§2.3), not a literal port of `egui::Window`.
- **Plain-text export/import.** `ide-ui`'s version exists because
  `rfd::FileDialog` already gives it a "pick a file" UI for free
  (`save_active_as`'s own precedent). `ide-tui` has no file-picker
  dependency or established flow of any kind — building one just for this
  phase would be a new, disproportionate piece of infrastructure for a
  convenience feature the roadmap row doesn't call for. Cut; `reset` (per
  row) and `reset_all` (new, §2.4) already give an escape hatch back to
  defaults without needing a file round-trip.
- **Two-step "propose, see conflicts, confirm/cancel" capture flow.**
  `ide-ui`'s version needs this because `poll_keymap_capture` runs across
  frames while a modal window stays open, and committing immediately would
  leave no chance to review a conflict before it takes effect. This
  crate's capture happens synchronously inside one `handle_key` call
  (§3.3) — there's no multi-frame window to hold a "pending" state across,
  so the assignment applies immediately and any conflict is surfaced via
  this crate's existing `notify()` log (the same "signal, don't silence"
  treatment this crate already gives `todo-panel.md`/`file-watcher.md`
  outcomes) rather than a second confirmation step.

What *does* port faithfully: the override-over-defaults model itself
(§2.5 of the `ide-ui` doc — `effective_binding`/`set_override`/`reset`/
`reset_all`/`is_customized`/`conflicts`, conflicts warn but never block),
and dispatch changing from "look up the static default" to "look up the
effective binding, first match in registry order wins" (§3.1 there, §3.2
here).

## 2. Interface

New module `crates/tui/src/keymap.rs`. Depends on `commands.rs` (reads
`commands()`), not the reverse — same boundary the `ide-ui` doc's §4.1
states for its own `keymap.rs`.

### 2.1 `KeymapOverlay`

```rust
pub type Chord = (crossterm::event::KeyModifiers, crossterm::event::KeyCode);

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KeymapOverlay {
    /// id -> `Some(encoded chord)` (bound) or `Some(None)`'s serialized
    /// form (explicitly unbound) or absent (falls through to the
    /// command's own static default). Stored as `BTreeMap<String,
    /// Option<String>>`, not `BTreeMap<String, Option<Chord>>`: `Chord`
    /// wraps `crossterm::event::{KeyModifiers, KeyCode}`, neither of
    /// which derives `serde::Serialize`/`Deserialize` in this crate's
    /// build (crossterm's `serde` feature isn't enabled anywhere in the
    /// workspace) -- rather than turning on a new feature flag on an
    /// already-approved dependency for this alone, `encode_chord`/
    /// `decode_chord` (§2.2) give a small, dependency-free, fully-tested
    /// codec, and only their `String` output needs to round-trip through
    /// `serde_json` (already a direct dependency, `state.rs`/
    /// `project_state.rs`'s own precedent).
    overrides: std::collections::BTreeMap<String, Option<String>>,
}

impl KeymapOverlay {
    /// `overrides.get(id)` decoded, else the command's own static
    /// `binding` field looked up in `commands()`. Unknown `id` -> `None`.
    pub fn effective_binding(&self, id: &str) -> Option<Chord>;

    /// Explicit assignment -- `Some(chord)` to bind, `None` to unbind.
    pub fn set_override(&mut self, id: &str, binding: Option<Chord>);

    /// Removes `id`'s override, falling back to its static default again.
    pub fn reset(&mut self, id: &str);

    pub fn reset_all(&mut self);

    pub fn is_customized(&self, id: &str) -> bool;

    /// Every other registered command whose effective binding equals
    /// `proposed`. Call before committing an assignment; non-empty does
    /// not block it (same as `ide-ui`'s own `conflicts`) -- see §3.2's
    /// first-match dispatch rule for why a shared chord still behaves
    /// predictably.
    pub fn conflicts(&self, id: &str, proposed: Chord) -> Vec<&'static str>;

    /// First command (registry order) whose effective binding equals
    /// `(modifiers, code)`, mapped to its `Action` -- the overlay-aware
    /// replacement for `commands::binding_for` at every real dispatch
    /// site (§3.2).
    pub fn action_for(&self, modifiers: crossterm::event::KeyModifiers, code: crossterm::event::KeyCode) -> Option<crate::commands::Action>;
}
```

`commands::binding_for` itself is untouched — it keeps meaning "this
command's compile-time default," which is exactly what `effective_binding`
falls back to and what every existing `commands.rs` test already verifies.
Nothing about the default table changes in this phase.

### 2.2 Chord codec and label

```rust
pub fn label(chord: Chord) -> String; // e.g. "Ctrl+Shift+G", "F3", "Esc"

fn encode_chord(chord: Chord) -> String; // e.g. "ctrl+shift+char:g"
fn decode_chord(s: &str) -> Option<Chord>;
```

Covers every `KeyCode` variant reachable from a real key press under this
crate's own keyboard-enhancement setup (`lib.rs`'s
`DISAMBIGUATE_ESCAPE_CODES` opt-in, *not*
`REPORT_ALL_KEYS_AS_ESCAPE_CODES`) -- `KeyCode::Media`/`KeyCode::Modifier`
require the flag this crate never requests, so `encode_chord` maps both to
an inert `"unsupported"` marker `decode_chord` never produces, rather than
making the function fallible for two variants that cannot occur here.
Modifiers covered: `CONTROL`/`ALT`/`SHIFT` only, joined lowercase with
`+`, matching the only three this crate's own `commands()` table ever
constructs (confirmed by reading it: every `binding` is built from
`KeyModifiers::{NONE,CONTROL,ALT,SHIFT}` and unions of those three).

### 2.3 Persistence

```rust
pub fn load() -> KeymapOverlay;
pub fn save(overlay: &KeymapOverlay);
```

Own file, `~/.config/ide-tui/keymap.json`, **not** folded into
`state.rs`'s `PersistedState`: `lib.rs`'s `main` reconstructs and
unconditionally re-`save`s a whole fresh `PersistedState { last_project:
Some(resolved_root) }` on every successful launch (`tui-persist-last-
project.md`) — a `keymap` field living in that same struct would need
`..Default::default()` or similar at that call site, silently discarding
any customization on every single run. Same best-effort load/save contract
as `state.rs`'s own functions (missing file / malformed JSON / no
resolvable `$HOME` all degrade to `KeymapOverlay::default()` on load, and
every `save` failure mode is silently swallowed) — persistence here is a
convenience, never a requirement to run, exactly matching `state.rs`'s own
stated reasoning.

### 2.4 `App`/`commands.rs` additions

`App` gains `keymap: keymap::KeymapOverlay` (loaded via `keymap::load()`
in `App::new`, alongside `nav_state`'s own load) and `keymap_popup:
Option<KeymapPopupState>`:

```rust
pub(crate) struct KeymapPopupState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    /// `Some(id)` while the *next* raw key event this popup receives is
    /// captured as `id`'s new binding instead of being interpreted as
    /// list navigation or search input.
    pub(crate) capturing: Option<&'static str>,
}
```

`commands.rs` gains three ids:
- `FindAction` (`Action::OpenPalette` — named `OpenPalette`, not
  `FindAction`, only to dodge clippy's `enum_variant_names` lint on an
  `Action` variant ending in `Action`; the id string itself still matches
  `ide-ui`'s own registry entry) — folds the existing hardcoded
  `Ctrl+Shift+A` → `open_palette()` special case in `handle_key` into a
  real registry entry with a real default binding, so the one command
  that was previously *not* overlay-aware becomes overlay-aware like
  everything else. Deliberate, in-scope inclusion, not a stray addition:
  a keymap-customization phase that left its own palette-launch shortcut
  as the one thing a user still couldn't rebind would be a foreseeable,
  avoidable gap in exactly the feature this phase delivers. `ide-ui`
  itself already registers this exact id (`crates/ui/src/command.rs`) for
  the identical action, so the name isn't invented either.
- `ToggleKeymapSettings` (`Action::ToggleKeymapSettings`) — opens/closes
  the popup (§2.5). Palette-only, no default binding: `ide-ui`'s own
  Keymap window is opened by a toolbar button, never a keybinding, so
  there is no existing binding to translate (same reasoning already
  established for `ToggleGitPanel`/`ToggleTodoPanel`).
- `ResetAllKeybindings` (`Action::ResetAllKeybindings`) — `self.keymap
  .reset_all()` + persist + `notify`. Palette-only; `ide-ui`'s "Reset All"
  is a settings-window button with no keybinding either.

`Action::Exit` keeps being the match's final, catch-all-adjacent arm; the
three new arms sit alongside the other palette-only actions.

### 2.5 Dispatch and the popup

`handle_key`'s hardcoded `Ctrl+Shift+A` check and its `binding_for(key)`
call (both immediately before the `Focus`-based fallback) are replaced by
one call: `self.keymap.action_for(key.modifiers, key.code)`. A new
`if self.keymap_popup.is_some() { return self.handle_keymap_popup_key(key); }`
joins the existing chain of per-overlay early returns (`self.palette
.is_some()`, `self.find.is_some()`, … `self.git_panel.is_some()`),
positioned last among them, immediately before the (now overlay-routed)
generic dispatch — same relative position the removed hardcoded check
used to occupy. `close_all_overlays` gains `self.keymap_popup = None;`.

`keymap_popup_rows(&self) -> Vec<&'static Command>`: every `commands()`
entry whose `title`, `id`, or effective-binding label (case-insensitively)
contains the popup's `query` — empty query returns every command, same
shape `recent_files_rows`/the palette's own filter already establish.
This is a direct port of `ide-ui`'s own §3.6 ("search by pressed
combination" is served by filtering on the label's *text*, not a second
live-capture mode — that was already `ide-ui`'s own choice, not a cut
made here).

`handle_keymap_popup_key`: `Esc` closes; `Up`/`Down` move the selection
(clamped to `keymap_popup_rows().len()`); typed chars (no `Ctrl`) extend
`query` and reset `selected` to `0`; `Backspace` shrinks it; `Enter` on a
row enters capture mode (`capturing = Some(id)`); `Delete` on a row calls
`self.keymap.reset(id)` + persist + `notify`. While `capturing` is
`Some(id)`, every key routes to `handle_keymap_capture_key(id, key)`
instead: `Esc` cancels (clears `capturing`, no assignment — Esc can never
itself become a *newly assigned* binding through this UI, even though it
remains reachable as an existing *default*, `CollapseSelections`'s own
plain `Esc` — a deliberate, narrow simplification: every other popup in
this crate already treats bare `Esc` as "close/cancel," and preserving
that one universal meaning is worth more than making the exact bytes for
"escape" itself rebindable through the capture flow); any other key
becomes `(key.modifiers, key.code)`, computes `self.keymap.conflicts(id,
chord)`, calls `set_override(id, Some(chord))`, persists, clears
`capturing`, and `notify()`s the result (naming any conflicting ids if
non-empty) — no confirm step (§1.1's last bullet).

## 3. Behaviour

### 3.1 Effective binding resolution

`effective_binding(id)`: an explicit override (bound or unbound) always
wins; otherwise the command's own static `binding`. `commands::
binding_for` is unaffected and still answers "what does this command do
by default" — used by `KeymapOverlay` itself as that fallback, and still
directly exercised by every pre-existing `commands.rs` test.

### 3.2 Dispatch through the overlay

Same invariant `keymap.md` §3.1 establishes for `ide-ui`: `action_for`
returns the **first** command (registry order) whose effective binding
matches, not every match. Under this phase alone this never differs from
today's behaviour (no user has customized anything yet, and the static
table already has no two commands sharing a chord — `commands.rs`'s own
`no_two_bound_commands_share_the_same_chord` test proves it) — it only
starts to matter once a user's override makes two ids share a chord,
which `conflicts` warns about but never forbids.

### 3.3 Capture is synchronous, not multi-frame

Unlike `ide-ui`'s `poll_keymap_capture` (runs once per frame while a
window stays open across many frames), this crate's capture resolves
inside the single `handle_key` call that receives the next keystroke after
`Enter` — there is no per-frame polling loop to hook into the way
`poll_lsp`/`poll_watcher` do, since nothing here needs to survive across
multiple idle frames waiting for input the terminal already delivers
synchronously via `crossterm::event::read()`.

### 3.4 Reset and Reset All

`reset(id)` drops the override, falling back to the static default.
`reset_all()` drops every override. Both are followed by `keymap::save`
at their call sites (§2.5) — persistence is the caller's job, not baked
into the overlay's own mutators, matching `project_state.rs`'s existing
split between pure state mutation and explicit `save` calls at each call
site that changes something durable.

## 4. Constraints

1. `keymap.rs` has no `App` dependency beyond `commands.rs`'s own public
   surface — same boundary `commands.rs` itself holds towards `app.rs`.
2. `KeymapOverlay`'s mutators never touch disk themselves (§3.4) — every
   call site in `app.rs` that mutates `self.keymap` explicitly calls
   `keymap::save` right after, the same way `project_state`'s call sites
   already do for `self.nav_state`.
3. `conflicts` warns, never blocks (§2.1/§3.2) — `set_override` always
   succeeds regardless of collisions.
4. `action_for`'s first-match rule is what keeps a collision's runtime
   dispatch predictable, exactly as `keymap.md` §3.1 reasons for `ide-ui`.

## 5. Examples

Rebind Save from `Ctrl+S` to `Ctrl+Shift+S`: open the Keymap popup
(`ToggleKeymapSettings`, palette-only), find the "Save" row, `Enter` to
capture, press `Ctrl+Shift+S`. `conflicts("SaveAll", (CONTROL|SHIFT,
Char('s')))` returns `[]`; the override commits immediately and a
notification confirms it. From the next keystroke, `Ctrl+S` no longer
saves and `Ctrl+Shift+S` does; `Ctrl+S` itself becomes unbound only if the
user separately unbinds it (rebinding one command's chord never touches
another command's own binding).

Rebind Find Action itself (`Ctrl+Shift+A` → something else): fully
supported — `FindAction` is a real registry entry like any other (§2.4),
not the hardcoded special case it used to be.

Typing "ctrl+s" into the popup's search field filters to every command
whose effective binding's label contains that text (`SaveAll`, plus
anything else a user has since bound to a `Ctrl+S`-containing chord).

## 6. Dependencies / integration / tests

No new dependency — `serde`/`serde_json` are already direct dependencies
(`state.rs`/`project_state.rs`); crossterm's `serde` feature is
deliberately **not** turned on (§2.1). Diff scope: `crates/tui/src/{app,
commands,keymap,lib,ui}.rs` (new `keymap.rs`; `lib.rs` only for the new
`mod keymap;` line), this doc, `docs/roadmap.md`. Not security-sensitive
per `CLAUDE.md`'s list — no subprocess, no path handling, pure in-memory
state plus a JSON file at a fixed per-user config path (the same
non-sensitive shape `state.rs`/`project_state.rs` already are); `hacker`
is skipped for this phase.

Tests: `encode_chord`/`decode_chord` round-trip every reachable `KeyCode`
variant (including the `Media`/`Modifier` "unsupported, never decodes"
case) and every modifier combination this crate actually constructs;
`label` renders the worked examples above; `KeymapOverlay::
effective_binding`/`set_override`/`reset`/`reset_all`/`is_customized`/
`conflicts`/`action_for` each get direct unit coverage including the
first-match-wins case; `load`/`save` round-trip through a tempdir-backed
path and degrade correctly on missing/malformed input (mirrors `state.rs`'s
own test shape exactly). `app.rs`: `FindAction`'s default binding still
opens the palette; `ToggleKeymapSettings`/`ResetAllKeybindings` wiring;
`handle_keymap_popup_key`'s full key routing including capture mode,
`Esc`-during-capture cancelling instead of binding, `Delete` resetting a
row, and a full rebind-then-dispatch round trip proving the *next* key
press after a rebind actually reaches the new action.

## Revision notes

1. `Action::FindAction` was renamed to `Action::OpenPalette` during
   implementation: clippy's `enum_variant_names` lint fires on a variant
   whose name ends with its enum's own name (`Action::FindAction` inside
   `enum Action`). The `"FindAction"` *id string* is unchanged and still
   matches `ide-ui`'s own registry entry for the identical action — only
   the Rust identifier differs.
2. `commands::binding_for` (the pre-existing static-default-by-key
   lookup) became unreachable from any non-test call site once
   `handle_key` switched to `self.keymap.action_for(...)`. Per this
   project's own established precedent for this exact situation
   (`keymap.md`'s own history with `KeyChord::ctrl()` on the `ide-ui`
   side): delete if the code is genuinely obsolete and not a named future
   deliverable, don't suppress. `binding_for` fell in the "genuinely
   obsolete as production code, but still the clearest way to test the
   static table in isolation" category, so it was kept but demoted to
   `#[cfg(test)]` rather than either deleted (would have required
   rewriting ~20 existing tests to a less direct assertion shape) or
   kept as dead production code with a `#[allow(dead_code)]` (which
   would misrepresent it as a deliberate future primitive, which it
   isn't).
