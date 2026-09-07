# T45: TUI colon-command mode

## 1. Purpose

Second run in the `docs/roadmap.md` §11 series (reversing §9's docking
model for `ide-tui` only, in favor of the `Orbit TUI.dc.html` mockup's
screen-navigation model — T44 shipped the screen shell this run builds
on). The mockup shows a persistent `:` command line at the bottom of the
screen: typing `:` opens a one-line prompt, filtered against the app's
command set, Enter runs the top/selected match. This run adds that
prompt as a second way to reach any `Command` in the registry, alongside
the existing full-screen command palette (`Ctrl+Shift+A`) — lower
friction for a single quick command, without displacing the palette.

**Non-goal**: the mockup's own further step, argument syntax
(`:bind bottom run cargo test`), is explicitly out of scope here — it
depends on the per-pane-edge bind-slot data model T47 introduces, which
doesn't exist yet. This run's colon line only ever matches and runs a
whole `Command`, exactly like the palette does today, with no argument
parsing at all.

**Design conflict surfaced during doc drafting, resolved by the user
directly (`AskUserQuestion`)**: the mockup triggers colon-command mode
with a bare, unmodified `:` keypress. `ide-tui`'s Editor screen is a real
text buffer, and `:` is an ordinary character typed constantly in real
code (Rust type annotations, Python dict/slice syntax, ternaries, YAML,
JSON, etc.) — binding it globally would make it impossible to type a
literal colon while editing, a far more severe version of the same
"real text buffer" concern that already ruled out bare digit-key screen
switching in T44's own doc. The user chose **"Only when not
text-editing"**: `:` triggers colon-command mode everywhere *except*
while the actual editor caret is live (`active_screen == Editor &&
focus == Focus::Editor`) — see §3.1 for the exact condition and why
every other screen/focus combination is safe.

## 2. Interface

### 2.1 `app.rs`

New state struct, deliberately identical in shape to the existing
`PaletteState` (`app.rs`, near `open_palette`) — same query/filtered/
selected fields, same reason to exist as a separate type from
`PaletteState` rather than reusing it: `close_all_overlays`/
`any_popup_open` gate on distinct `Option` fields per overlay throughout
this file, and colon-command and the palette are visually and
positionally distinct (bottom-anchored line vs. centered popup) even
though they share a filter/execute model:

```rust
pub(crate) struct ColonCommandState {
    pub(crate) query: String,
    pub(crate) filtered: Vec<&'static Command>,
    pub(crate) selected: usize,
}
```

New `App` field: `colon_command: Option<ColonCommandState>` (next to
`palette: Option<PaletteState>`), initialized `None` in `App::new`.

**Shared filter helper** (new, small, free function — not a method,
since it closes over nothing but the query string): both `refilter_
palette` (existing) and the new `refilter_colon_command` currently
duplicate the identical one-line predicate
(`commands().iter().filter(|c| c.title.to_lowercase().contains(&query)).collect()`).
Extract it once:

```rust
fn filter_commands_by_title(query: &str) -> Vec<&'static Command> {
    let query = query.to_lowercase();
    commands()
        .iter()
        .filter(|c| c.title.to_lowercase().contains(&query))
        .collect()
}
```

`refilter_palette` is updated to call this helper instead of repeating
the filter inline (a small, safe refactor of existing code — verify its
current exact body first, since the doc's citation may have drifted;
the resulting behavior must be byte-for-byte identical, this is a pure
extraction, not a behavior change). `refilter_colon_command` (new) calls
the same helper.

New methods, mirroring `open_palette`/`handle_palette_key`/
`refilter_palette` exactly in shape:

```rust
fn open_colon_command(&mut self) {
    self.colon_command = Some(ColonCommandState {
        query: String::new(),
        filtered: filter_commands_by_title(""),
        selected: 0,
    });
}

fn handle_colon_command_key(&mut self, key: KeyEvent) -> LoopSignal {
    let Some(state) = self.colon_command.as_mut() else {
        return LoopSignal::Continue;
    };
    match key.code {
        KeyCode::Esc => {
            self.colon_command = None;
        }
        KeyCode::Up => {
            if state.selected > 0 {
                state.selected -= 1;
            }
        }
        KeyCode::Down => {
            if state.selected + 1 < state.filtered.len() {
                state.selected += 1;
            }
        }
        KeyCode::Enter => {
            let action = state.filtered.get(state.selected).map(|c| c.action);
            self.colon_command = None;
            if let Some(action) = action {
                return self.run_action(action);
            }
        }
        KeyCode::Backspace => {
            state.query.pop();
            self.refilter_colon_command();
        }
        KeyCode::Char(c) => {
            state.query.push(c);
            self.refilter_colon_command();
        }
        _ => {}
    }
    LoopSignal::Continue
}

fn refilter_colon_command(&mut self) {
    let Some(state) = self.colon_command.as_mut() else {
        return;
    };
    state.filtered = filter_commands_by_title(&state.query);
    state.selected = 0;
}
```

Note the asymmetry from `handle_palette_key`: colon-command's `Char(c)`
arm pushes *every* character typed, including a second `:` — there is no
special-case "typing `:` again does something different" in this run
(the mockup doesn't show one either). The *first* `:` triggers
`open_colon_command` and is never itself added to `query` (query starts
empty); a second `:` press has no special case and is pushed like any
other character, so it becomes a one-character query — which happens to
substring-match "Custom Actions: Manage" (the one command title
containing a literal colon), not "matching nothing" as originally
assumed here — still harmless, just not literally empty-result. See the
regression test `colon_command_typing_a_second_colon_is_a_harmless_
literal_query_char`.

**New helper, used by the trigger condition below and reusable by any
future run needing the same "is the caret live" question**:

```rust
fn is_text_editing_focused(&self) -> bool {
    self.active_screen == AppScreen::Editor && self.focus == Focus::Editor
}
```

### 2.2 `handle_key`'s dispatch chain

Two changes, both verified against the *current* (post-T44) dispatch
order in `app.rs` before implementing — that order already changed once
this session (T44's own round-2 `rev` fix moved the Git-panel gate), so
re-check line numbers/positions against source rather than trusting any
citation here to still be exact.

**1. New popup-priority check**, added to the existing chain of early
`if self.<field>.is_some() { return self.handle_<x>_key(key); }` checks
(the same rank `self.palette.is_some()` already occupies — place it
immediately next to that check, order between the two doesn't matter
since they're mutually exclusive by construction, see §3.3):

```rust
if self.colon_command.is_some() {
    return self.handle_colon_command_key(key);
}
```

**2. New trigger check**, placed at the *same rank* T44's generic
Esc-returns-to-Editor rule occupies — after every existing popup-`Option`
check above, but *before* the global keymap lookup
(`self.keymap.action_for(...)`). This rank is load-bearing, not
incidental (§3.2 explains why in detail): every screen/focus this
trigger must fire for (Run, Keys, Editor+LeftDock, Editor+BottomDock) has
its *own* catch-all handling that runs later in the chain and would
otherwise consume `:` silently before this check ever ran.

```rust
if key.code == KeyCode::Char(':')
    && !self.is_text_editing_focused()
    && !self.any_popup_open()
{
    self.open_colon_command();
    return LoopSignal::Continue;
}
```

`!self.any_popup_open()` guards the same way it already does for T44's
Esc rule — if some *other* overlay happens to be open (a Docker confirm
dialog, say), that overlay's own key handling already claimed the key
several checks earlier in the chain, so this line is reachable at all
only when no popup owns input. It is not reachable while
`active_screen == Git` either (the Git-panel check, now `active_screen
== AppScreen::Git`, sits even earlier — dead code there in the same
"expected, not a bug" sense T44's Esc rule already established for that
screen, since typing `:` into a draft commit message or a branch filter
inside the Git screen must remain literal text entry, not a colon-command
trigger).

### 2.3 `close_all_overlays`/`any_popup_open`

Both existing functions get one new line each, in the same alphabetical-
ish position other `Option`-gated overlay fields already occupy (find
the exact current insertion point by locating `self.palette`'s own line
in each — colon-command is the palette's closest sibling in shape):

```rust
// close_all_overlays
self.colon_command = None;
```

```rust
// any_popup_open
|| self.colon_command.is_some()
```

### 2.4 `ui.rs`

New function, positioned near `render_palette` (its closest sibling):

```rust
fn render_colon_command(frame: &mut Frame, app: &App, area: Rect);
```

Unlike `render_palette`'s centered popup, this is **bottom-anchored**,
matching the mockup's own "command line" placement and giving this
feature a real visual identity distinct from "the palette, again":
full width (minus a 1-column margin each side, same margin convention
`render_git_panel`'s popup inset already uses elsewhere in this file),
height = up to `COLON_COMMAND_VISIBLE_ROWS` (new constant, `6` — smaller
than the palette's `12`, since this is meant for a quick single command,
not browsing the whole registry) plus 2 for the border, positioned with
its bottom edge one row above `area`'s own bottom edge (i.e. directly
above the status bar row `render()` already reserves) rather than
vertically centered. Row/list rendering (styled `ListItem` per filtered
command, `REVERSED` on the selected row, `ListState` for scroll-follow)
mirrors `render_palette`'s body exactly; only the title differs
(`format!(": {}", state.query)`, no `Find Action:` prefix — the leading
`:` glyph itself is the whole visual cue this is a colon command, not a
second palette).

Wire the call into `render()`'s existing popup-overlay block, at the
same rank `render_palette`'s own call already occupies (order between
the two doesn't matter, they're mutually exclusive):

```rust
if app.colon_command.is_some() {
    render_colon_command(frame, app, size);
}
```

## 3. Behaviour

### 3.1 Where `:` is safe to intercept, verified exhaustively

`is_text_editing_focused()` is `true` only for `active_screen == Editor
&& focus == Focus::Editor` — the one and only context in this crate
where a bare printable `Char(':')` keystroke means "insert a colon into
a real file's content." Every other reachable context was checked
directly against source before writing this doc, not assumed:

- **`active_screen == Editor && focus == Focus::LeftDock`** (tree
  navigation): `handle_left_dock_key` dispatches on structural keys
  (`Up`/`Down`/`Left`/`Right`/`Enter`/search-filter chars — but the tree's
  own type-ahead filter, if it has one, is itself a `find`-shaped `Option`
  overlay already excluded by `any_popup_open()`, not inline `Focus::
  LeftDock` handling); no plain-text-content interpretation of `:`
  exists here.
- **`active_screen == Editor && focus == Focus::BottomDock`**: every
  inline (non-popup) `BottomDockTab` handler was checked —
  `handle_cargo_panel_key` (`b`/`r`/`t`/`c`/`l`/`f` shortcuts, `_ => {}`
  catch-all), `handle_custom_actions_panel_key` (`Up`/`Down`/`Enter`
  only), `handle_docker_panel_key`/`handle_k8s_panel_key`/
  `handle_problems_key`/`handle_git_log_dock_key` (selection/navigation
  shortcuts, no raw text fields). Every dock tab's actual text-entry UI
  (Custom Actions' Add/Edit form, Docker/K8s's confirm/scale-input
  prompts) lives in a separate `Option`-gated popup
  (`manage_actions_popup`, `docker.confirm`, `k8s.scale_input`, etc.),
  already excluded by the `any_popup_open()` guard several ranks earlier
  in the chain — this trigger genuinely never reaches those.
- **`active_screen == Run`**: `handle_cargo_panel_key`'s own
  `_ => {}` catch-all (same function as above) confirms no text-content
  interpretation.
- **`active_screen == Keys`**: read-only reference screen, consumes every
  key as a no-op (T44 §3.4) — no text content at all.
- **`active_screen == Git`**: not reached by this trigger at all, as
  explained in §2.2 — `handle_git_panel_key` claims every key first,
  including while its own commit-message/branch-filter text fields are
  focused, where a literal `:` must remain typeable.

### 3.2 Dispatch-rank verification (the specific mistake T44's own review
round caught twice already for this exact shape of check)

Placing this trigger *after* the global keymap lookup would make it
**unreachable on the Run and Keys screens** — not because `:` collides
with an existing global binding (it doesn't, confirmed: `grep -rn
"Char(':')" crates/tui/src/` returns nothing today), but because both
screens' own catch-all handlers (`handle_cargo_panel_key`'s `_ => {}`,
the Keys screen's unconditional consume-and-`Continue` guard) run
*before* `Focus`-based dispatch ever would, and — per T44's own §3.3 —
those two checks themselves sit *after* the global keymap lookup. If
this new trigger were placed after the lookup too, it would need to sit
*before* those two Run/Keys checks specifically to ever fire on those
screens, which is a fragile, order-dependent way to get the same
guarantee that "before the lookup, right next to the Esc rule" gets for
free and uniformly. Placing it *before* the lookup (as specified in
§2.2) sidesteps this entirely: it's checked once, before any
screen-specific catch-all has a chance to run, for every screen/focus
combination this trigger applies to.

### 3.3 Mutual exclusion with the palette

`colon_command` and `palette` are never both `Some` at once: opening
either always goes through `open_colon_command`/`open_palette`
independently (no shared "open an overlay" helper calls
`close_all_overlays` first for either — verify this against the current
`open_palette` body; if it does call `close_all_overlays`, `open_colon_
command` must match that convention exactly, not diverge from it), and
the trigger conditions for opening each are themselves mutually
exclusive (`:` only opens colon-command; `Ctrl+Shift+A` only opens the
palette) — a user cannot type a key sequence that opens both.

### 3.4 What Enter actually runs

Identical semantics to the palette's own `Enter` handling: runs
`self.run_action(action)` for the currently-selected filtered command,
where `action` is the *first* matching command if the user never
touched `Up`/`Down` (`selected` starts at `0`). No confirmation step, no
argument parsing (see §1's stated non-goal) — pressing Enter on an empty
query with a non-empty `commands()` list runs whatever the alphabetically
-or-registration-order-first command happens to be, exactly matching
today's palette behavior with an empty query (not a new risk this run
introduces).

## 4. Constraints & invariants

- `ColonCommandState` and `PaletteState` stay two separate types with
  duplicated shape, not one shared type — see §2.1's reasoning
  (distinct `Option` fields for `close_all_overlays`/`any_popup_open`
  bookkeeping, distinct rendering).
- `filter_commands_by_title` is the single source of truth for "does
  this command match this query" — both the palette and colon-command
  call it; do not let a future edit special-case one without the other,
  or they silently diverge in what "matches" means.
- The trigger check's rank (§2.2/§3.2) is exactly as load-bearing as
  T44's Esc rule and Run/Keys checks were — do not move it without
  re-deriving why it must sit before the global keymap lookup.
- No new keybinding is registered in `commands.rs` — `:` is a raw,
  hardcoded dispatch-chain check (same category as `Esc`'s generic rule
  in T44, not a rebindable `Command`), since it is conditional on
  `!is_text_editing_focused()` in a way the flat keymap-lookup model
  doesn't represent. `Action::OpenPalette` remains the one and only
  rebindable way to reach a filtered command list; colon-command is a
  second, non-rebindable, always-`:` shortcut layered on top, exactly
  the same relationship `Esc`'s hardcoded `CollapseSelections` global
  binding already has alongside T44's *also*-hardcoded generic
  Esc-returns-to-Editor rule (one is a real `Command`, one is chain-only
  logic, and both coexist today without conflict).

## 5. Examples

```
On the Editor screen, cursor in a Rust file, focus on the buffer:
  Type "let x: i32 = 1;"
  -> Every character, including both colons, is inserted literally.
     is_text_editing_focused() is true here; the trigger never fires.

On the Editor screen, focus on the left-dock tree (Ctrl+T):
  Press ':'
  -> Colon-command line opens at the bottom, above the status bar.
  Type "ref" -> filtered list narrows to "Refactor This" and any other
     command whose title contains "ref".
  Press Enter -> runs the top filtered match, closes the line.

On the Run screen, a Cargo build is streaming output:
  Press ':' -> colon-command line opens (the build keeps running
     unaffected -- switching overlays never touches self.cargo, same
     invariant T44 established for screen switches).
  Press Esc -> closes the line, back to the Run screen, build still
     streaming.

On the Git screen, editing a draft commit message:
  Type "fix: correct off-by-one"
  -> The colon is inserted into the draft message literally --
     handle_git_panel_key claims the key before this trigger is ever
     reached (see §2.2/§3.1).
```

## 6. Dependencies & integration points

- `commands.rs`'s existing `Command`/`commands()` registry — reused
  verbatim, no new commands registered for this run.
- `app.rs`'s existing `PaletteState`/`open_palette`/`handle_palette_key`/
  `refilter_palette` — mirrored, not modified, except for `refilter_
  palette`'s pure-extraction refactor onto the new shared helper (§2.1).
- `app.rs`'s existing `close_all_overlays`/`any_popup_open`/`handle_key`'s
  dispatch chain (per T44's own §3.5 rank precedent) — each gets exactly
  one new line/check, per §2.2/§2.3.
- `ui.rs`'s existing `render_palette`/`PALETTE_VISIBLE_ROWS` — mirrored
  layout logic, bottom-anchored instead of centered (§2.4).
