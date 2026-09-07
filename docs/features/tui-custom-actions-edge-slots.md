# T47: TUI Custom Actions — Builtin/External Tag + Per-Edge Bind Slots

## 1. Purpose

`docs/roadmap.md` §11's `T44`-`T48` series adopts the `Orbit TUI.dc.html`
mockup's navigation model. `T47` is the fourth run, and the direct answer
to the user's original complaint that started the whole series: "custom
actions must go also from prepared list of our actions" — `T42`'s Custom
Actions (`docs/features/tui-custom-actions.md`) can currently only run an
external shell command the user types by hand; there is no way to bind a
custom-action slot to one of the IDE's own already-registered commands
(`commands()`, the same registry the palette/colon-command/keymap all
search).

Two independent changes, both in `crates/tui/src/custom_actions.rs`/
`app.rs`/`ui.rs`:

1. `CustomAction` gains a `kind` tag: `External { command, args }` (today's
   only shape, unchanged) or `Builtin { command_id }` — a reference into
   the existing `Command::id` registry, **not** a second, parallel list of
   runnable things. Running a `Builtin` action never spawns a subprocess;
   it calls `App::run_action` with that command's own `Action`, the exact
   same call every keybinding/palette-row/colon-command-row already makes.
2. The flat global action list + single `BottomDockTab::CustomActions`
   dock tab is replaced by binding each action to one of four edge slots —
   `Top`, `Tree`, `Outline`, `Bottom` — the mockup's own vocabulary. Each
   slot shows only the actions bound to it.

## 2. Interface

### 2.1 `ActionSlot`, `CustomActionKind`, `CustomAction` (`custom_actions.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) enum ActionSlot {
    Top,
    Tree,
    Outline,
    #[default]
    Bottom,
}
impl ActionSlot {
    pub(crate) const ALL: [ActionSlot; 4] = [Top, Tree, Outline, Bottom];
    pub(crate) fn next(self) -> Self { .. } // cycles ALL, wraps
    fn index(self) -> usize { .. }          // position in ALL, for the
                                             // panel's per-slot arrays
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub(crate) enum CustomActionKind {
    External { command: String, args: Vec<String> },
    Builtin { command_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CustomAction {
    pub(crate) name: String,
    pub(crate) slot: ActionSlot,
    pub(crate) kind: CustomActionKind,
}
```

`CustomActionsFile { actions: Vec<CustomAction> }` (the `.ide/
custom_actions.json` payload) keeps its own shape — only the element type
underneath changed. **Breaking, not migrated**: a `.ide/custom_actions.json`
written by `T42` (flat `name`/`command`/`args`, no `kind`/`slot`) fails to
deserialize into this new shape, and `load`'s existing `.ok().flatten()
.unwrap_or_default()` already collapses any deserialize error to an empty
list (§4) — the same fail-open posture this file already documents for a
missing file or malformed JSON, now also covering "valid JSON in the
previous version's shape." Accepted, not fixed with a migration: this
project has no established migration convention for `.ide/*.json`
elsewhere, and inventing one for a single, low-stakes, locally-editable
settings file is more machinery than the problem warrants.

### 2.2 `CustomActionsPanel` (`custom_actions.rs`)

```rust
#[derive(Default)]
pub(crate) struct CustomActionsPanel {
    pub(crate) actions: Vec<CustomAction>,
    /// One selection cursor per `ActionSlot`, indexed by `ActionSlot::
    /// index()` -- was a single `selected: usize` before this doc; each
    /// slot's own filtered view (`actions_for_slot`) needs its own cursor,
    /// since the four views can have different lengths and the same
    /// numeric position means different things in each.
    pub(crate) selected: [usize; 4],
    pub(crate) running: Option<CustomAction>,
    pub(crate) output: Vec<String>,
    pub(crate) rx: Option<Receiver<StreamEvent>>,
}

impl CustomActionsPanel {
    pub(crate) fn actions_for_slot(&self, slot: ActionSlot) -> Vec<CustomAction> {
        self.actions.iter().filter(|a| a.slot == slot).cloned().collect()
    }
    pub(crate) fn selected(&self, slot: ActionSlot) -> usize { .. }
    pub(crate) fn move_selection(&mut self, slot: ActionSlot, delta: i32) { .. }

    /// Spawns `action`'s subprocess (`External` only). A no-op — not a
    /// panic, not an error surfaced to the user — if `action.kind` is
    /// `Builtin`: that is a caller bug (`App` is responsible for routing
    /// `Builtin` through `run_action` before ever reaching here, §3.2),
    /// never a reachable user-facing state, so this mirrors this crate's
    /// existing "defensive no-op on a should-never-happen index/state"
    /// convention (`confirm_go_to_file` on an empty result set, etc.)
    /// rather than `unwrap`/`panic!`.
    pub(crate) fn run(&mut self, project_root: &Path, action: CustomAction) { .. }
    pub(crate) fn poll(&mut self) { .. } // unchanged
}
```

### 2.3 Manage popup: `FormKind`, `ActionFormField`, `ManageActionsPopupState` (`app.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FormKind {
    #[default]
    External,
    Builtin,
}

pub(crate) enum ActionFormField { Name, Kind, Command, Args, Slot } // Tab-cycled, this order

pub(crate) struct ManageActionsPopupState {
    pub(crate) selected: usize,
    pub(crate) adding: bool,
    pub(crate) editing_index: Option<usize>,
    pub(crate) add_field: ActionFormField,
    pub(crate) new_name: String,
    /// `External`: the program name. `Builtin`: the typed `Command::id`
    /// (case-sensitive exact match, validated at confirm time, §3.3) --
    /// one field reused for both, not two separate text fields, since
    /// exactly one is ever meaningful at a time depending on `form_kind`.
    pub(crate) new_command: String,
    /// Ignored (not cleared, not validated) when `form_kind == Builtin`.
    pub(crate) new_args: String,
    pub(crate) form_kind: FormKind,
    pub(crate) form_slot: crate::custom_actions::ActionSlot,
}
```

`Kind`/`Slot` fields don't accept typed text: `Space` toggles `form_kind`
(`Kind` field) or cycles `form_slot` (`Slot` field, via `ActionSlot::
next()`) — the same "no keyboard mnemonic, plain Tab-reachable + Space"
convention `SearchInPathField`'s own boolean fields already establish
(root `CLAUDE.md`'s keyboard-shortcuts section cites this exact
precedent). `Backspace`/`Char` are no-ops on `Kind`/`Slot` (matched
explicitly, not silently swallowed by falling through to the `Name`/
`Command`/`Args` arms) — mirrors `SearchInPathField`'s own boolean fields
ignoring text-entry keys.

### 2.4 New `App` methods

- `fn run_custom_action(&mut self, action: CustomAction)` — the shared
  dispatch point every slot's "run the thing" path (keyboard `Enter` and
  mouse click alike) goes through. `Builtin { command_id }`: looks up
  `commands().iter().find(|c| c.id == command_id)`; found → `self.
  run_action(cmd.action)` (the exact call every other command source
  makes); not found (the bound id was renamed/removed from the registry
  since this action was saved) → `self.notify(...)`, no panic, no
  subprocess. `External { command, args }`: `self.custom_actions.run
  (&self.project_root.clone(), action)`.
- `fn run_custom_action_in_slot(&mut self, slot: ActionSlot)` — keyboard
  entry point: resolves `slot`'s currently-selected row via `actions_for_
  slot`/`selected`, then calls `run_custom_action`. No-op if the slot is
  empty or the cursor is out of range (both defensive, not reachable
  through normal navigation, which already clamps).
- `Left`/`Right`-dock's `Actions` tab (`LeftDockTab::Actions`, §3.1) reuses
  the *existing* left-dock focus/navigation model — no new `Focus` variant.

No new `Command`/`Action` registry entries for opening/running a slot —
`ManageCustomActions`/`ToggleCustomActionsPanel` (`T42`'s own two commands)
are untouched; `ToggleCustomActionsPanel`'s title still just says "Custom
Actions" and now switches to the `Bottom`-slot dock tab specifically (§3.1).

## 3. Behaviour

### 3.1 Where each slot actually renders — reusing existing chrome, not new rows

Every slot piggybacks on a row/tab this crate's layout **already**
reserves, rather than adding a new fixed-height `Layout` row anywhere.
`ui.rs`'s own `EDITOR_CHROME_ROWS` doc comment already warns that this
crate's chrome-row accounting has no single source of truth and a change
here risks desyncing `main.rs`'s scroll-follow-cursor math — the fix
applied here is to need zero such changes, not to audit around them:

- **`Bottom`** — `BottomDockTab::CustomActions`, `T42`'s already-built dock
  tab (list + streamed-output pane), unchanged in appearance. Its list is
  now `actions_for_slot(Bottom)` instead of the whole flat list; its Enter
  key now calls `run_custom_action_in_slot(Bottom)` instead of directly
  spawning a subprocess (so a `Bottom`-bound `Builtin` action works too).
- **`Tree`** — a new fourth `LeftDockTab::Actions`, alongside `Files`/
  `Todos` (`render_dock_tab_strip`'s existing tab-strip mechanism, zero new
  layout code). Fully keyboard-navigable for free: it inherits
  `Focus::LeftDock`'s existing Tab-cycle-then-Up/Down/Enter model, the same
  as `Files`/`Todos` today.
- **`Top`** — appended, right-aligned, to `render_screen_tabs`'s existing
  one-row strip (`Constraint::Length(1)`, already reserved since `T44`) —
  after the `Editor`/`Git`/`Run`/`Keys` labels, not replacing them.
- **`Outline`** — appended after the breadcrumb trail on `render_
  breadcrumbs`'s existing reserved row (already unconditionally reserved
  since `tui-file-structure-and-breadcrumbs.md`, whether or not there are
  any breadcrumbs to show on a given frame).

**Accepted asymmetry, not an oversight**: `Top`/`Outline` are mouse-click-
only in this run — clicking a bound action's `[Name]` label runs it
(`HitMap::top_action_hits`/`outline_action_hits`, the same `Vec<(Rect, _)>`
+ `handle_mouse_click` loop shape `screen_tabs`/`tab_strip` already use);
neither gets a new keyboard-focus target. This crate already ships real
mouse support (`docs/features/tui-mouse-support.md`) as a first-class
interaction, not a fallback, so this isn't a degraded story for these two
slots — but giving them full keyboard parity would mean inventing a new
`Focus` variant (today's is the three-way `LeftDock`/`Editor`/`BottomDock`)
and a navigation model for two one-row strips, a much larger and riskier
change than this run's actual scope. `Tree` and `Bottom` — the two slots
the mockup itself calls out as substantial enough for a real panel/tab —
get full keyboard navigation because they ride an *existing* focus target
for free; `Top`/`Outline` don't have one to ride.

### 3.2 Builtin dispatch never grows the subprocess surface

`CustomActionsPanel::run` (§2.2) still only ever calls `subprocess::
spawn_streaming`, still only for `External`. A `Builtin` action reaching
`run_custom_action` (§2.4) is resolved by exact `Command::id` string match
against the existing, already-registered `commands()` table and then
handed to the existing `App::run_action` — the identical call every
keybinding, every palette row, every colon-command row, and (`T46`) every
unified-finder command row already makes. There is no new code path that
turns arbitrary persisted JSON into an executed command with a
capability the app didn't already grant that command through its normal
registry; the worst a maliciously-crafted `.ide/custom_actions.json` (a
cloned, untrusted repository) can do via a `Builtin` entry is invoke one
of the IDE's own existing actions early/without the user's own keypress —
no different in kind from what a malicious `.ide/custom_actions.json`
could already do with `T42`'s `MAX_CUSTOM_ACTIONS` cap and argument-vector
discipline for `External` (`docs/features/tui-custom-actions.md` §4);
neither variant is auto-run on load (`load` never calls `run`/`run_action`
itself, same as `T42`).

### 3.3 Form validation (`confirm_action_form`)

Unchanged for `External` (`name`/`command` non-empty, `args` whitespace-
split). New for `Builtin`: `name` non-empty (shared check), `new_command`
(the typed id) trimmed and non-empty, then looked up in `commands()` by
exact `id`; no match → `self.notify(format!("No command with id \"{id}\"."))`
and the form stays open with the typed text intact (mirrors every other
validation-failure branch in this method). `new_args` is read only when
`form_kind == External`. **Known, documented v1 limitation**: there is no
fuzzy/title-based picker for choosing a `Builtin` id — the user types the
literal id (visible in the existing Keymap popup's own rows, `T22`) rather
than searching by title the way `T46`'s unified finder or the palette
would let them. Building a dedicated picker sub-UI for one form field was
judged more machinery than this run's scope warrants; a future run can
swap this one field's key-handling for an inline fuzzy dropdown without
touching anything else in this doc.

## 4. Constraints & invariants

- `MAX_CUSTOM_ACTIONS` (500, `T42`) is unchanged and still applies to the
  *total* `actions` list across all four slots combined, not per-slot —
  same DoS rationale, unaffected by this doc's changes.
- A `Bottom`-slot `Builtin` action's "run" no longer touches `custom_
  actions.running`/`output`/`rx` at all (those are `External`-only,
  subprocess-shaped state) — the dock tab's output pane simply doesn't
  change when a `Builtin` row runs, and its `running` indicator never
  shows one. Not a bug: a `Builtin` action's actual visible effect is
  whatever the underlying `Action` does (e.g. opening a popup), which the
  rest of the UI already reflects.
- `ActionSlot`'s `Default` is `Bottom` — a freshly-created `CustomAction`
  (before the user picks a slot in the form) lands where `T42`'s single
  list used to live, the least surprising default.
- No migration path for pre-`T47` `.ide/custom_actions.json` files (§2.1)
  — a project that already had custom actions saved under `T42` loses them
  (an empty list, not an error) the first time this version reads that
  file, and must re-add them via the Manage popup.

## 5. Examples

Binding an existing command: open Manage (`ManageCustomActions`), `n` for
a new entry, type a name, `Tab` to `Kind`, `Space` to flip to `Builtin`,
`Tab` to `Command`, type `RefactorThis` (an existing `Command::id`), `Tab`
to `Slot`, `Space` until it reads `Top`, `Enter` to save. The action now
appears as a clickable `[Name]` label at the right edge of the top screen-
tab row; clicking it calls `run_action(Action::RefactorThis)` — the exact
menu `⌃T`/the palette's own "Refactor This" row would trigger.

## 6. Dependencies & integration points

- `commands()`/`Command`/`Action`/`App::run_action` (`commands.rs`) —
  unchanged, `Builtin` dispatch's only new caller.
- `render_dock_tab_strip`, `render_screen_tabs`, `render_breadcrumbs`,
  `HitMap`, `handle_mouse_click` (`ui.rs`/`app.rs`) — extended, not
  replaced (§3.1).
- `T45`'s command-title substring search / `T46`'s unified finder — not
  reused for the `Builtin` id picker (§3.3's accepted limitation); a
  candidate integration point for the future picker mentioned there.
