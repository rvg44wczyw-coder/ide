# T59 — Keys screen becomes rebindable

## 1. Purpose

`T44` (`docs/features/tui-screen-navigation.md`) introduced the `Keys`
screen and explicitly deferred its editing capability: that doc's own
`render()`-dispatch section says `Keys` gets *"a plain read-only reference
list ... no edit UI, no per-pane-edge bind slots yet **(T47)**"*, and the
`keys_screen_scroll` field's own doc comment (`app.rs:1018-1022`) restates
the same thing — *"this content has no selectable rows (T44 kept it
read-only-reference)"*. `T47`, when it actually shipped
(`docs/roadmap.md`'s own T47 row), turned out to be a different feature —
Custom Actions' `Builtin`/`External` tag and per-edge bind slots — and
never touched the Keys screen at all. The promise made in `T44`'s row was
silently never delivered, and `T48`'s roadmap row closes the `T44`–`T48`
series calling it "полностью завершена" (fully complete) without anyone
having caught the gap. `T58`'s own doc (`tui-settings-consolidation.md`
§3.4) already flagged this explicitly while scoping a different, smaller
feature: *"Whether `Keys` and the Keymap Settings popup should eventually
merge (one already-flagged, not-yet-built idea being T47's 'editable Keys
screen') is a decision for whichever future run actually builds that."*
This doc is that run.

**What changes:** the Keys screen (`AppScreen::Keys`) gains the same two
mutating actions the Keymap Settings popup (`T22`) already has — `Enter`
rebinds the highlighted command, `Delete` resets it to its default — so a
user browsing the full-screen reference list can act on what they're
looking at, instead of having to close it, reopen the palette, and search
for the same command by name in the (smaller, filtered) popup. Both
surfaces keep existing side by side, not merged into one: the Keymap
Settings popup remains the fast, search-first way to jump straight to one
command among ~108 by typing part of its name; the Keys screen becomes the
browse-first way to look at the whole list and act on the row you're
already looking at. Neither replaces the other — this mirrors real
IDEs having both a scrollable keymap settings page and a fast rebind
search, not picking one.

## 2. Interface / API

### 2.1 `ide-core` / `ide-lsp`

None.

### 2.2 `crates/tui/src/app.rs`

**Field replacement.** `keys_screen_scroll: u16` (`app.rs:1018-1023`) is
replaced by two fields, mirroring `KeymapPopupState`'s `selected`/
`capturing` shape but without a `query` (§1 — no search on this screen):

```rust
/// Highlighted row index into `keymap_popup_rows()` (unfiltered on this
/// screen -- no `query`, see §1). Row order is `commands()`'s own
/// registry order, the same order `keymap_popup_rows()` already returns
/// it in when its `query` is empty.
pub(crate) keys_screen_selected: usize,
/// `Some(id)` while the *next* raw key event is captured as `id`'s new
/// binding, mirroring `KeymapPopupState::capturing`'s exact contract
/// (`docs/features/tui-keymap.md` §2.4) but scoped to this screen instead
/// of the popup.
pub(crate) keys_screen_capturing: Option<&'static str>,
```

`App::new`/`App::new_with_state_path` initialize both (`app.rs:1372`'s
`keys_screen_scroll: 0,` becomes `keys_screen_selected: 0,
keys_screen_capturing: None,`).

**New methods**, direct siblings of `start_keymap_capture`/
`reset_selected_keymap_binding`/`handle_keymap_capture_key`
(`app.rs:9845-9904`) with the same three-method shape, reading/writing the
two new fields instead of `self.keymap_popup`. Deliberately **not**
generalized into one shared implementation the popup and this screen both
call through — the two duplicate bodies are ~15 lines each and read
`self.keymap_popup`/`self.keys_screen_*` respectively, which are two
different `Option` shapes (`KeymapPopupState` carries `query`/`selected`/
`capturing` together; the screen has no `query` and no reason to bundle
`selected`/`capturing` into a struct when both are plain fields already).
A shared abstraction here would mean either giving the screen a pointless
`query: ()`-shaped struct or introducing a trait purely to unify two call
sites — this project's own convention (`CLAUDE.md`: *"Three similar lines
is better than a premature abstraction"*) says duplicate the ~45 lines
rather than build that:

```rust
fn start_keys_screen_capture(&mut self) {
    let id = self.keymap_popup_rows().get(self.keys_screen_selected).map(|c| c.id);
    if let Some(id) = id {
        self.keys_screen_capturing = Some(id);
    }
}

/// `Delete` on a row: identical contract to `reset_selected_keymap_
/// binding` (`docs/features/tui-keymap.md` §2.5/§3.4).
fn reset_selected_keys_screen_binding(&mut self) {
    let id = self.keymap_popup_rows().get(self.keys_screen_selected).map(|c| c.id);
    if let Some(id) = id {
        self.keymap.reset(id);
        self.persist_keymap();
        self.notify(format!("Reset \"{id}\" to its default binding."));
    }
}

/// The next raw key event while `keys_screen_capturing` is `Some(id)`.
/// `Esc` cancels without assigning anything and **without** leaving the
/// Keys screen (see §2.3's dispatch-ordering note -- this is the whole
/// reason this handler must be reached before T44's generic
/// Esc-returns-to-Editor rule); any other key becomes `id`'s new binding
/// immediately, no confirm step (same contract as `handle_keymap_capture_
/// key`).
fn handle_keys_screen_capture_key(&mut self, key: KeyEvent) -> LoopSignal {
    let Some(id) = self.keys_screen_capturing else {
        return LoopSignal::Continue;
    };
    if key.code == KeyCode::Esc {
        self.keys_screen_capturing = None;
        return LoopSignal::Continue;
    }
    let chord = (key.modifiers, key.code);
    let conflicts = self.keymap.conflicts(id, chord);
    self.keymap.set_override(id, Some(chord));
    self.persist_keymap();
    self.keys_screen_capturing = None;
    if conflicts.is_empty() {
        self.notify(format!("\"{id}\" is now bound to {}.", keymap::label(chord)));
    } else {
        self.notify(format!(
            "\"{id}\" is now bound to {} (shared with {}).",
            keymap::label(chord),
            conflicts.join(", ")
        ));
    }
    LoopSignal::Continue
}
```

### 2.3 `handle_key` dispatch changes (`app.rs`)

**New pre-empting check**, inserted immediately after the existing Agent
dock-tab Esc-interception block ends (`app.rs:6867`) and — critically —
**before** T44's generic Esc-returns-to-Editor rule (`app.rs:6877-6883`):

```rust
// `docs/features/tui-keys-screen-rebind.md` §2.3, T59 -- must sit before
// T44's generic Esc-returns-to-Editor rule immediately below: while
// capturing a new binding on the Keys screen, Esc must cancel the
// capture and stay on this screen, not kick the user back to Editor
// mid-capture the way an unrelated bare Esc on this screen otherwise
// would.
if self.active_screen == AppScreen::Keys && self.keys_screen_capturing.is_some() {
    return self.handle_keys_screen_capture_key(key);
}
```

Without this ordering, pressing `Esc` to cancel an in-progress capture
would instead be caught by T44's own rule two checks later (`active_screen
!= Editor && !any_popup_open()` — a capture in progress doesn't count as
a popup, so that condition is true) and silently switch the user back to
`Editor` with the capture left dangling in `Some`, so the *next* key
they press anywhere would be wrongly consumed as the pending binding.

**Existing guard rewritten** (`app.rs:6936-6958`, the block whose own
comment currently says *"read-only reference screen"*):

```rust
// `docs/features/tui-keys-screen-rebind.md` §2.3, T59 -- browse mode
// only; capture mode is handled by the pre-empting check above, which
// always returns before reaching here while `keys_screen_capturing` is
// `Some`.
if self.active_screen == AppScreen::Keys {
    let len = self.keymap_popup_rows().len();
    match key.code {
        KeyCode::Up => {
            self.keys_screen_selected = self.keys_screen_selected.saturating_sub(1);
        }
        KeyCode::Down => {
            self.keys_screen_selected = (self.keys_screen_selected + 1).min(len.saturating_sub(1));
        }
        KeyCode::PageUp => {
            self.keys_screen_selected = self.keys_screen_selected.saturating_sub(10);
        }
        KeyCode::PageDown => {
            self.keys_screen_selected = (self.keys_screen_selected + 10).min(len.saturating_sub(1));
        }
        KeyCode::Enter => self.start_keys_screen_capture(),
        KeyCode::Delete => self.reset_selected_keys_screen_binding(),
        _ => {}
    }
    return LoopSignal::Continue;
}
```

`Up`/`Down`/`PageUp`/`PageDown` clamp without wraparound, same convention
`handle_theme_popup_key`/`handle_keymap_popup_key` already use (no
wrap-style popup in this crate's overlay set). `10` for
`PageUp`/`PageDown` is not a new constant — it's the exact page size the
`keys_screen_scroll` code this replaces already used
(`app.rs:6950`/`6953` pre-`T59`), carried forward unchanged.

**Mouse wheel needs no code change.** `handle_mouse_scroll`'s existing
Keys-screen comment and dispatch (`app.rs:7397-7401`) already routes a
wheel notch into `handle_key` as a synthetic `KeyCode::Up`/`Down` event —
since that goes through the same rewritten guard above, wheel scrolling
continues to work unmodified, now moving `keys_screen_selected` (with the
list auto-scrolling to keep it visible, §2.4) instead of the old raw
offset.

**Mouse *clicks* need one new guard.** `any_true_popup_open`
(`app.rs:6975-6996`) already includes `self.keymap_popup.is_some()`/
`self.theme_popup.is_some()` (`app.rs:7001-7002`), which is what makes
`handle_mouse_click` (`app.rs:7118-7133`, returns early on
`self.any_true_popup_open()`) refuse every click — including a screen-tab
click — for as long as either settings popup is open, capturing a chord
or not. `keys_screen_capturing` needs the identical protection: without
it, `Enter`-to-start-capture on the Keys screen followed by a click on
the always-visible "Editor" tab (T44's persistent tab bar, unaffected by
this doc) switches screens with the capture still `Some(id)` — inert
while away from the Keys screen (the §2.3 pre-empting check only fires
when `active_screen == AppScreen::Keys`), then silently re-arms the next
time the user returns via `GoToKeysScreen`, capturing whatever key they
next press (e.g. an ordinary `Down` navigation) as `id`'s new binding
with no indication anything unusual happened. `any_true_popup_open` gains
one more disjunct:

```rust
|| (self.active_screen == AppScreen::Keys && self.keys_screen_capturing.is_some())
```

placed alongside the existing `keymap_popup`/`theme_popup` checks — same
rank, same reasoning: a capture in progress, wherever it lives, blocks
every mouse-driven action exactly the way an open popup already does.
This is the only change needed anywhere in the mouse path; `handle_mouse_
click`'s own early-return on `any_true_popup_open()` then covers the Keys
screen for free, the same way it already covers both settings popups
today.

### 2.4 `crates/tui/src/ui.rs`: `render_keys_screen` rewritten

Current version (`ui.rs:1868-1893`) manually slices `rows[start..]` from a
plain scroll offset and renders every row identically (no highlight, no
customized marker, no capturing indicator) via a bare `List`. New version
mirrors `render_keymap_popup`'s row-building exactly (`ui.rs:1814-1866`),
reusing `render_scrollable_list` (`ui.rs:388-399`, already shared with
both settings popups) so `ratatui::widgets::ListState` handles keeping
`keys_screen_selected` in view automatically — no manual windowing math
left in this function at all:

```rust
fn render_keys_screen(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.keymap_popup_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let style = if i == app.keys_screen_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let binding = app
                .keymap
                .effective_binding(cmd.id)
                .map(crate::keymap::label)
                .unwrap_or_else(|| "\u{2014}".to_string());
            let customized = if app.keymap.is_customized(cmd.id) { "*" } else { "" };
            let text = if Some(cmd.id) == app.keys_screen_capturing {
                format!("{}  [Press a key... Esc to cancel]", cmd.title)
            } else {
                format!("{}{customized}  {binding}", cmd.title)
            };
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();

    let block = Block::default().borders(Borders::ALL).title(
        "Keys  (Enter: rebind, Delete: reset, Up/Down/PgUp/PgDn: navigate, Esc: back to editor)",
    );
    render_scrollable_list(frame, items, block, area, app.keys_screen_selected);
}
```

The title string drops "reference only -- use the Keymap command to
rebind" (no longer true) and states the screen's own new controls
directly, the same shape `render_keymap_popup`'s/`render_theme_popup`'s
titles already use for their own controls.

## 3. Behaviour

### 3.1 Browse mode

`Up`/`Down` move `keys_screen_selected` one row; `PageUp`/`PageDown` move
it ten rows; both clamp at `[0, keymap_popup_rows().len() - 1]`, no
wraparound. `Enter` starts capture on the highlighted row (§3.2). `Delete`
resets the highlighted row's binding to its default and shows a
notification, identical wording to the popup's own reset notification.
Any other key falls through unconsumed to `_ => {}` and the guard still
returns `LoopSignal::Continue` — exactly as today, no editor-buffer leak
(the guard's whole reason for existing per T44 §3.4 is preserved
unchanged).

### 3.2 Capture mode

`Enter` sets `keys_screen_capturing = Some(id)` for the highlighted row.
While `Some`, every subsequent key is intercepted by the new pre-empting
check in §2.3 before it can reach *any* other dispatch path (including
T44's generic Esc rule) — `Esc` cancels (clears `capturing`, no
assignment, stays on the Keys screen); any other key/chord becomes the
new binding immediately (no confirm step, matching `tui-keymap.md`
§1.1's last bullet — this crate's capture has never had a two-step
propose/confirm flow anywhere, and this screen doesn't invent one either).
A conflict with another command's effective binding is reported in the
same notification, never blocked — identical to `KeymapOverlay::
conflicts`' existing "warns, never blocks" contract (`tui-keymap.md`
§2.1/§3.2), reused unchanged; this screen introduces no new conflict
policy.

### 3.3 Persistence

Both mutations (`Enter`-then-a-key, `Delete`) call `self.persist_keymap()`
— the exact same method, writing to the exact same file
(`~/.config/ide-tui/keymap.json`, or `keymap_path_override` under test)
the Keymap Settings popup's own capture/reset paths already write to.
There is exactly one `KeymapOverlay` (`self.keymap`) and exactly one file
it round-trips through; this screen is a second *editor* of that same
state, not a second copy of it. A binding changed from the Keys screen is
immediately reflected in the Keymap Settings popup's own display next
time it's opened (and vice versa) — both surfaces read `self.keymap`
live, there is nothing to synchronize.

### 3.4 Relationship to the Keymap Settings popup (unchanged)

`ToggleKeymapSettings` (`T22`) keeps working exactly as it does today —
opens the popup, independent `query`/`selected`/`capturing` state,
search-filtered rows. Opening the popup while the Keys screen is active
does not touch `keys_screen_selected`/`keys_screen_capturing`, and vice
versa — the two are entirely independent pieces of state that happen to
mutate the same underlying `self.keymap`. `T58`'s `Left`/`Right`
page-switch between the Theme and Keymap popups is unaffected — this doc
adds no third page to that pair, and does not make the Keys screen part
of it (§1 states why: it's a screen, not a popup, and stays that way).

## 4. Constraints & invariants

- `keymap_popup_rows()` itself is unchanged — still returns every
  `commands()` entry unfiltered when called with no popup `query` in
  scope, which is always true from this screen's call sites (it never
  reads `self.keymap_popup`'s `query`, only its own `keys_screen_*`
  fields).
- `keys_screen_selected`'s clamp bound (`len.saturating_sub(1)`) is
  recomputed from `keymap_popup_rows().len()` on every `Down`/`PageDown`
  press, not cached — `commands()`'s registry is a fixed compile-time
  table in this crate today (no runtime-added commands), so `len` cannot
  actually change between frames, but recomputing costs nothing and
  avoids the guard silently going stale if that ever changes.
- No new persisted state and no schema change to `keymap.json` — this
  doc only adds a second in-memory call path to the same existing
  `KeymapOverlay::set_override`/`reset`, both already fully specified by
  `tui-keymap.md`.
- Not on `CLAUDE.md`'s security-sensitive-paths list: no subprocess, no
  new file path, no network — same trust shape `tui-keymap.md`/
  `tui-theme.md`/`tui-settings-consolidation.md` already established for
  this exact pair of files (`app.rs`/`ui.rs`) and the same
  `~/.config/ide-tui/keymap.json` path. `hacker` is not required.

## 5. Examples

Rebind `Save` from the Keys screen without ever opening the popup:

```rust
let mut app = App::new(project_root)?;
app.run_action(Action::GoToKeysScreen);
assert_eq!(app.active_screen, AppScreen::Keys);
// "Save" (`SaveAll`) is commands()'s first registered entry.
assert_eq!(app.keys_screen_selected, 0);

app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
assert_eq!(app.keys_screen_capturing, Some("SaveAll"));

app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL | KeyModifiers::SHIFT));

assert!(app.keys_screen_capturing.is_none());
assert!(app.keymap.is_customized("SaveAll"));
assert_eq!(app.active_screen, AppScreen::Keys); // never left this screen
```

Cancelling a capture leaves the screen, not the binding, untouched:

```rust
app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
let id = /* whatever row 1's id is */;
assert_eq!(app.keys_screen_capturing, Some(id));

app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

assert!(app.keys_screen_capturing.is_none());
assert!(!app.keymap.is_customized(id));
assert_eq!(app.active_screen, AppScreen::Keys); // Esc cancelled, didn't navigate away
```

A bare `Esc` with no capture in progress still returns to `Editor`,
unchanged from today:

```rust
app.run_action(Action::GoToKeysScreen);
app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
assert_eq!(app.active_screen, AppScreen::Editor);
```

## 6. Dependencies / integration points / tests

No new dependency. Diff scope: `crates/tui/src/{app,ui}.rs` only, plus
this doc and `docs/roadmap.md`.

Two existing tests are renamed/rewritten to the new selection model
(both currently assert on `keys_screen_scroll`, which no longer exists):
`keys_screen_scrolls_via_up_down_page_keys` (`app.rs:15803-15824`) →
asserts `keys_screen_selected` moves by 1/10 with the same saturating
clamp, at the new upper bound (`keymap_popup_rows().len() - 1`, not
"never observed to hit a real ceiling" the way the old open-ended scroll
offset was); `wheel_scroll_over_the_keys_screen_moves_keys_screen_scroll`
(`app.rs:23644-23660`) → same mechanical rename to `keys_screen_selected`,
behavior unchanged (still just synthesizes `Up`/`Down` through
`handle_key`, §2.3).

New tests, mirroring the equivalent `theme_popup_*`/`keymap_popup_*`
groups' shape: `Enter` on the Keys screen starts capture on the
highlighted row's id; a captured key rebinds it, persists, and clears
`capturing` (round-trip through `state_path`-equivalent
`keymap_path_override`, same tempdir-backed pattern
`capturing_a_new_chord_rebinds_and_a_later_key_press_dispatches_the_new_
action` already uses); `Esc` during capture clears `capturing` without
mutating `self.keymap` and without changing `active_screen`; `Esc` with no
capture in progress still returns to `Editor` (regression guard for
§2.3's ordering requirement — the one behavior this doc must not break);
`Delete` resets the highlighted row and notifies; `Down`/`PageDown` clamp
at the real row-count ceiling (`keymap_popup_rows().len() - 1`), not an
arbitrary fixed number; a binding changed on the Keys screen is visible
through `self.keymap.effective_binding` from the Keymap Settings popup's
own `keymap_popup_rows()`-backed rendering afterward (proves §3.3's "one
shared state" claim, not two independent copies); a synthetic mouse click
on the screen-tab bar (or any other `any_true_popup_open()`-gated mouse
target) while `keys_screen_capturing` is `Some` is ignored — `active_screen`
stays `Keys` and `keys_screen_capturing` stays unchanged (regression test
for §2.3's mouse-click guard).

## 7. Diagram

Skipped — the two new state machines (browse ↔ capture on the Keys
screen; the `Esc`-ordering rule in §2.3) are each fully described by one
short paragraph and are structurally identical to the Keymap Settings
popup's own already-shipped, already-undiagrammed capture flow
(`tui-keymap.md` §2.5/§3.3) — a new diagram wouldn't add clarity beyond
that precedent plus this doc's text.

## Revision notes

`rev` (doc review, round 1) found one real, blocking gap: §2.2/§2.3
originally added `keys_screen_capturing` with no mouse-click protection,
unlike both existing settings popups (`keymap_popup`/`theme_popup`),
whose presence in `any_true_popup_open` already blocks every mouse-driven
action, capturing or not. Without the equivalent guard, starting a
capture and then clicking a screen tab would leave `keys_screen_capturing`
dangling `Some`, silently re-arming and swallowing an unrelated later
keystroke as a stray rebind the next time the user returned to the Keys
screen. Fixed: `any_true_popup_open` gains a new disjunct covering
`active_screen == AppScreen::Keys && keys_screen_capturing.is_some()`
(§2.3), with a regression test added to §6's list. The devil's-advocate
pass separately considered whether the three near-duplicate capture
methods (§2.2) should share code with the popup's own three — concluded
no, matching this project's own established "duplicate a few lines over
inventing an abstraction for two call sites" convention; not changed.
