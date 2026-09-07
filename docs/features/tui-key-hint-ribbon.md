# T48: TUI Persistent Key-Hint Ribbon

## 1. Purpose

Fifth and final run of the `T44`-`T48` series (`docs/roadmap.md` §11,
the `ide-tui`-only reversal of §9's IntelliJ-style docking in favor of the
`Orbit TUI.dc.html` mockup's full-screen navigation model). Delivers a
piece of that mockup explicitly deferred twice already: `T42`'s
`tui-custom-actions.md` §3.4 states plainly that "`ide-tui` has no
persistent bottom key-hint ribbon distinct from its status line," and
`T47`'s edge-slot rework (`tui-custom-actions-edge-slots.md`) covers only
the mockup's four screen-edge bind points (`Top`/`Tree`/`Outline`/
`Bottom`), not its separate, always-visible ribbon row with its own
dashed `[+]` bind affordance. Both prerequisites this needed — a
screen-navigation shell to anchor a genuinely persistent chrome row
(`T44`) and a generalized per-slot custom-action bind mechanism (`T47`)
— now exist, which is why this run is last in the series.

Two parts, both additive:

1. A new persistent row, visible under every `AppScreen` (`Editor`/
   `Git`/`Run`/`Keys` alike — unlike the `Bottom` dock tab, which only
   exists on the `Editor` screen), showing a small **fixed** set of
   key-hints: existing commands' *current effective* bindings, read
   live from `commands()`/`App::keymap` — never invented, never
   hardcoded strings duplicating what the keymap already knows.
2. A `[+]` affordance in that same row that opens the existing Manage
   Custom Actions popup (`T42`/`T47`) directly in add-form mode, pre-
   seeded to a new fifth `ActionSlot::Ribbon` — the mockup's own bind
   point for the ribbon, reusing `T47`'s already-generic slot mechanism
   rather than inventing a parallel one.

## 2. Interface

### 2.1 `ActionSlot` gains a fifth variant (`custom_actions.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) enum ActionSlot {
    Top,
    Tree,
    Outline,
    #[default]
    Bottom,
    Ribbon,
}
```

`ALL` becomes `[Top, Tree, Outline, Bottom, Ribbon]` (5 elements);
`index()` gains a `Ribbon => 4` arm; `next()` is unchanged (already
defined generically over `ALL`, `T47`) and now cycles all five. `T47`'s
Manage-popup `Slot` form field (`Space` cycles `form_slot` via
`ActionSlot::next()`) reaches `Ribbon` for free — no new form-field
plumbing needed, exactly the "reusing `T47`'s mechanism" this run is
scoped to. `CustomActionsPanel::selected` widens from `[usize; 4]` to
`[usize; 5]` to stay indexable by every `ActionSlot::index()` value;
`Ribbon` is mouse-only (§3.1) so nothing actually drives its cursor in
practice, but the array must still have a slot for it to keep
`selected(Ribbon)` panic-free (defensive, mirrors why `Top`/`Outline`'s
cursors already sit unused in the same array since `T47`).

### 2.2 `App::key_hint_rows` (`app.rs`)

```rust
/// Curated, fixed set of existing `Command::id`s shown as ribbon hints --
/// never an invented binding, only ever what the live keymap already
/// reports for a command that already exists in `commands()`.
const HINT_COMMAND_IDS: [&str; 6] =
    ["SaveAll", "Undo", "Redo", "Find", "GoToFile", "FindAction"];

/// `(title, current effective binding label)` for every `HINT_COMMAND_
/// IDS` entry that both exists in `commands()` and currently has an
/// effective binding (the user may have unbound it via Keymap Settings,
/// `T22`) -- in that case the hint is simply omitted, never rendered as
/// a dash placeholder (unlike `render_keys_screen`'s reference list,
/// this is a *hint* row, not a reference; an unbound hint has nothing
/// useful to hint at).
pub(crate) fn key_hint_rows(&self) -> Vec<(&'static str, String)> { .. }
```

### 2.3 New `App` method: `open_new_ribbon_action_form`

```rust
/// The ribbon's own `[+]` affordance (§3.2): opens Manage Custom Actions
/// directly in add-form mode, `form_slot` pre-seeded to `Ribbon` --
/// skips list mode since there's nothing to review yet, mirroring how
/// the list-mode `n` key already jumps straight to the form.
fn open_new_ribbon_action_form(&mut self) { .. }
```

No new `Command`/`Action` registry entry: a keyboard-only user can
already bind a `Ribbon`-slot action today, via the existing
`ManageCustomActions` command and the `Slot` field's `Space`-cycle
(§2.1) — `[+]` is purely a mouse convenience mirroring the mockup, the
same "click affordance on top of already-complete keyboard access"
relationship `Top`/`Outline`'s click regions already have to the same
popup (`T47` §3.1).

### 2.4 `ui.rs`

- `EDITOR_CHROME_ROWS`: `6` → `7` (one new persistent outer row; see
  §3.1 — this is a deliberate, explicit change to the constant, the kind
  its own doc comment warns to grep for before merging, not an
  oversight).
- `render()`'s outermost `Layout` (currently 3 rows: tab bar / body /
  status) gains a fourth `Constraint::Length(1)` row for the ribbon,
  rendered immediately after `render_status`.
- New `render_key_hint_ribbon(frame, app, area, hits)`.
- `HitMap` gains two fields: `ribbon_action_hits: Vec<(Rect,
  CustomAction)>` (click-to-run, mirrors `top_action_hits`/
  `outline_action_hits` exactly) and `ribbon_add_hit: Option<Rect>` (the
  `[+]` region — a single optional `Rect`, not a `Vec`, since there is
  always exactly one `[+]`, never zero or many).

### 2.5 `handle_mouse_click` (`app.rs`)

Two new hit-test blocks, same shape as `T47`'s `top_action_hits`/
`outline_action_hits` loops: a `ribbon_action_hits` loop calling
`run_custom_action(action.clone())`, and a check on `ribbon_add_hit`
calling `open_new_ribbon_action_form()`.

## 3. Behaviour

### 3.1 Layout: a new row, not a reused one — and why that's the exception

Every slot `T47` added reused an *existing* reserved row (§3.1 of that
doc); this run is the one deliberate exception, because nothing existing
fits: the ribbon must be visible on **every** `AppScreen` (`Git`/`Run`/
`Keys` included), while the status bar (already every-screen-visible)
already carries real content (find-bar status, active buffer path,
unread/problem-count badges) that a six-hint-wide ribbon would crowd
out, and the `Bottom` dock tab only exists inside the `Editor` screen's
own layout. Adding the row is the direct, explicit trigger the `EDITOR_
CHROME_ROWS` doc comment describes: bump the constant, grep its one call
site (`lib.rs`'s `set_editor_viewport_rows` subtraction), confirm nothing
else depends on the outer `render()` `Layout`'s row count. Both were done
as part of this run.

### 3.2 Ribbon content, left to right

1. Fixed hints (§2.2), left-aligned, `"{binding} {title}"` per entry
   (e.g. `"Ctrl+s Save"` — `crate::keymap::label` renders a bound
   `KeyCode::Char` verbatim, case included, not upper-cased), two spaces
   between entries — same spacing convention `render_screen_tabs`/
   `render_tab_strip` already use.
2. Right-aligned (via the same width-then-place technique `T47`'s
   `append_right_aligned_actions` already established for `Top`/
   `Outline`, generalized here to also place a non-`CustomAction` `[+]`
   marker at the end of the same right-aligned group): every
   `actions_for_slot(Ribbon)` entry as a clickable `[Name]` label
   (click → `run_custom_action`, exactly `Top`/`Outline`'s own
   dispatch), followed by the literal `[+]` affordance.

On a terminal too narrow to fit both groups, the left group is simply
truncated by `ratatui`'s own line-rendering (the same "no special
overflow handling" reality every other single-row strip in this file
already has — `render_screen_tabs`, `render_tab_strip`) — not a new
failure mode this run introduces.

### 3.3 Security: no new subprocess surface

`custom_actions.rs` is a `CLAUDE.md`-declared security-sensitive file;
this run's only change to it is a fifth enum discriminant and a widened
fixed-size array. `Ribbon`-slot actions run through the exact same
`App::run_custom_action` (`T47` §2.4) as every other slot: `Builtin`
resolves by exact `Command::id` match against `commands()` and calls
`run_action`; `External` calls `CustomActionsPanel::run`, unchanged
subprocess/argv discipline. Nothing about *which* slot an action is
bound to changes how it runs — this run adds a fifth place a
already-vetted dispatch can be triggered *from*, not a new way to
trigger it.

## 4. Constraints & invariants

- `HINT_COMMAND_IDS` entries must all exist in `commands()` with a
  non-`None` default `binding` — enforced by a test, so a future
  rename/removal of one of these ids fails the build loudly rather than
  silently rendering a shorter ribbon.
- The ribbon never renders a placeholder for an unbound hint (§2.2) —
  it can legitimately show fewer than 6 entries if the user has unbound
  one of the curated commands via Keymap Settings.
- `ActionSlot::ALL` order (`Top, Tree, Outline, Bottom, Ribbon`) is the
  order `Space` cycles through on the Manage-popup `Slot` field —
  `Ribbon` is reached last, one `Space` press past `Bottom` (today's
  default slot for a freshly-created action).

## 5. Examples

Binding an existing command into the ribbon via the mouse: click `[+]`
at the ribbon's right edge → Manage Custom Actions opens directly in
add-form mode, `Slot` already reading `Ribbon` → type a name, `Tab` to
`Kind`, `Space` to `Builtin`, `Tab` to `Command`, type `ToggleTodoPanel`,
`Enter` to save. The action now appears as a clickable `[Name]` label
just left of `[+]` in the ribbon, on every screen; clicking it toggles
the TODO panel exactly as the palette row of the same name would.

## 6. Dependencies & integration points

- `commands()`/`Command`/`App::keymap`/`crate::keymap::label` — read-only,
  unchanged; this run's only new caller of `keymap::label` outside
  `render_keys_screen`.
- `T47`'s `ActionSlot`/`CustomActionsPanel::actions_for_slot`/
  `App::run_custom_action`/`ManageActionsPopupState` — extended (fifth
  variant, widened array), not replaced.
- `lib.rs`'s `set_editor_viewport_rows(term_size.height.saturating_sub
  (EDITOR_CHROME_ROWS))` — the one call site that must keep working
  after `EDITOR_CHROME_ROWS`'s bump; no code change needed there since it
  already reads the constant fresh, but it is the reason the bump is
  safe to make in isolation.
