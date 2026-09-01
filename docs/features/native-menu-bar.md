# Native macOS Menu Bar

## 1. Purpose

`ide-ui` has no menu bar at all today — everything is reachable only
through the command palette (`Find Action`, `⌘⇧A`) and per-command
keybindings. This phase adds a **real, OS-native macOS menu bar** (the
strip at the top of the screen, not an in-app `egui` menu) that mirrors a
subset of the command registry (`crates/ui/src/command.rs`) as
`App name / File / Edit / View / Go / Window` menus — every item invokes
an existing `CommandAction` through the existing `IdeApp::run_command`
dispatcher. **No new action is invented anywhere in this phase** — the
menu is a second way to reach commands that already exist, matching
CLAUDE.md's keyboard-shortcuts rule ("never invent a binding") applied to
menu items: nothing here does anything the palette couldn't already do.

**macOS only, this phase.** The uses's own scoping decision for this
polish batch ("Menus (#4): real native macOS menu bar only, not an in-app
egui menu bar"). Windows/Linux keep working exactly as today, with no menu
bar and no `muda` dependency compiled in at all (§6) — this doc does not
attempt cross-platform parity.

**Two deliberate scope cuts from the six menus originally sketched
(`File/Edit/View/Go/Window/Help`):**

- **No `Help` menu.** Everything muda could put there beyond an About
  panel belongs in the app-name menu instead (real macOS convention, see
  §3.2), and the registry has no help/documentation/report-issue command
  to hang a `Help` menu item on. Inventing placeholder content to fill an
  otherwise-empty menu would violate the "no invented actions" rule above
  — omitted until a real command exists for it.
- **No `Run`/`Build` menu.** The registry's six `Build`-category commands
  (`CargoBuild`/`CargoRun`/`CargoTest`/`CargoCheck`/`CargoClippy`/
  `CargoFmt`) stay reachable via the existing cargo panel, toolbar, and
  palette only. A `Run` menu is a reasonable future addition but wasn't
  part of what this batch scoped, and `MENU_GROUPS`'s data-driven shape
  (§2.1) adds one with no further design changes whenever that's wanted.

## 2. Interface / API

### 2.1 `crates/ui/src/app/menu.rs` (new file, submodule of `app`)

A submodule of `app` — not a new top-level sibling module — specifically
so it can call `IdeApp::run_command`, which is a private `fn` on `IdeApp`
visible to descendant modules of `app` but not to an unrelated sibling
module (the same reason `app/render.rs`, which already calls
`self.run_command(...)`, is structured as `app::render` rather than a
free-standing `render.rs`).

```rust
/// One native menu (`Menu Item > Sub Item`), built entirely from
/// `Command::id`s already in the registry (`crate::command::commands()`)
/// -- this table is the *only* place that decides native-menu-bar
/// grouping, and it is deliberately a different grouping than
/// `Command::category` (`command.rs`'s grouping is for the command
/// palette's own sections, e.g. tool-window toggles are `category:
/// "Window"` there because that's where the palette lists them, but a
/// real macOS app puts tool-window toggles under View and reserves
/// Window for actual window/tab management -- see §3.2's table for the
/// exact per-menu item list).
pub(super) struct MenuGroup {
    pub title: &'static str,
    /// In display order. `None` renders as a separator.
    pub items: &'static [Option<&'static str>],
}

/// Pure, no `muda`/AppKit dependency -- the menu structure as data, so
/// `menu_groups_reference_only_real_commands` (§4) can check every id
/// against `crate::command::commands()` without ever constructing a real
/// `muda::Menu`. `install_native_menu` (below) is the only caller that
/// turns this into OS calls.
pub(super) fn menu_groups() -> &'static [MenuGroup];

/// Builds the real `muda::Menu` from `menu_groups()` and attaches it via
/// `Menu::init_for_nsapp()` (§3.1). Idempotent -- safe to call more than
/// once (each call rebuilds and re-attaches), though `IdeApp` only calls
/// it once (§3.1). No-op stub compiled in on non-macOS (§6).
pub(super) fn install_native_menu();

/// Drains `muda::MenuEvent::receiver()` (a global channel, not tied to
/// any particular `Menu` -- §3.1) and dispatches at most one event per
/// call via `IdeApp::run_command`. Returns whether an event was handled,
/// the same `bool`-return-means-"repaint"/"something happened" shape
/// `LspBridge::poll`/`CargoPanel::poll`/`ClaudeTerminals::poll` already
/// use. No-op, always returns `false`, on non-macOS.
impl IdeApp {
    pub(super) fn poll_menu_events(&mut self, ctx: &egui::Context) -> bool;
}
```

### 2.2 `crates/ui/src/app.rs` / `app/render.rs` (small additions)

- `IdeApp::new` (`app.rs`, where `cc: &eframe::CreationContext` is
  available) calls `menu::install_native_menu()` once, unconditionally on
  macOS (§3.1) — no field/flag needed on `IdeApp` itself, since the menu
  is entirely OS-owned state after this call, not something `IdeApp`
  needs to hold a handle to (muda's `MenuEvent::receiver()` is a free
  function, not a method on the `Menu` value — nothing to store).
- `IdeApp::ui` (`app/render.rs`, the `eframe::App` impl) gets one more
  line alongside the existing per-frame poll calls
  (`self.lsp.poll()`/`self.cargo.poll()`/`self.claude_terminals.poll()`,
  all immediately after `let ctx = ui.ctx().clone();`):
  `if self.poll_menu_events(&ctx) { ctx.request_repaint(); }` — same
  shape, same place, same repaint-on-change convention every other
  background/OS event source in this file already follows.

## 3. Behaviour

### 3.1 Lifecycle

`menu::install_native_menu()` is called exactly once, from `IdeApp::new`,
which runs inside the closure `eframe::run_native` passes to its window-
setup callback — by the time this closure runs, `cc.egui_ctx` and the
underlying native window already exist, which is the latest-and-therefore-
safest point in this app's existing startup sequence to attach a menu
before the first frame is ever drawn.

**Confirmed working**, empirically, against this exact `eframe`/`egui`
0.36.1 pair: `muda`'s own `init_for_nsapp()` is a direct, synchronous
`NSApplication::sharedApplication(mtm).setMainMenu(Some(menu))` call (read
from the crate's own source, not assumed). A standalone spike (`Menu`
with one `File > Save All` item, `init_for_nsapp()` called once from
inside the first `ui()` frame) was launched directly by a human and
clicked by hand — the menu appeared and clicking "Save All" produced a
real `MenuEvent { id: MenuId("save_all") }` on `MenuEvent::receiver()`.
(Two earlier attempts to verify this via `osascript`/screenshot from an
automated agent shell falsely showed no menu at all — a real human launch
and a real human click were what actually settled it; an automated
recheck of the same binary is not a reliable substitute for this kind of
OS-integration check.) `rust-ui-dev` should still build and manually
confirm the skeleton once (§2.1's `install_native_menu`/
`poll_menu_events`, wired to at least one real command) before writing
out all of §3.2's grouping, simply as ordinary incremental development —
not because the mechanism itself is still in doubt.

### 3.2 Menu content

Every item below is `MenuItem::with_id(command.id, command.title, true,
None)` (§3.3 on the `None` accelerator), built by walking `menu_groups()`
and resolving each `Some(id)` against `crate::command::commands()`
(panicking at startup on a lookup miss is correct here, not a silent
fallback — an id typo in `menu_groups()` is a programmer error caught by
`menu_groups_reference_only_real_commands`, §4, long before it ships).
`None` entries become `PredefinedMenuItem::separator()`.

The always-first, bold app-name menu (titled by macOS itself, not
something this code names) holds the items real macOS conventions put
there — About, Preferences-equivalents, and the app-lifecycle predefined
items muda provides natively (no `CommandAction` needed for any of these,
they're not registry entries, `PredefinedMenuItem` is a real OS-standard
element, not an invented action):

| Item | Source |
|---|---|
| About ide | `PredefinedMenuItem::about(Some("ide"), None)` |
| *(separator)* | |
| Languages… | `ShowLanguageSettings` |
| Keymap… | `ShowKeymapSettings` |
| *(separator)* | `PredefinedMenuItem::services(None)` |
| *(separator)* | `PredefinedMenuItem::hide(None)`, `hide_others(None)`, `show_all(None)` |
| *(separator)* | `PredefinedMenuItem::quit(None)` |

`menu_groups()`'s six named menus:

| Menu | Items (in order, `—` = separator) |
|---|---|
| **File** | `SaveAll`, `RefreshTree` |
| **Edit** | `Undo`, `Redo` — `Find`, `Replace`, `FindNext`, `FindPrevious` — `CollapseFold`, `ExpandFold`, `CollapseAllFolds`, `ExpandAllFolds` — `ReformatCode`, `ToggleFormatOnSave` — `Rename` |
| **View** | `ToggleTheme`, `ToggleZenMode` — `ToggleProjectToolWindow`, `ToggleFindToolWindow`, `ToggleRunToolWindow`, `ToggleProblemsToolWindow`, `ToggleVcsToolWindow`, `ToggleClaudeToolWindow` |
| **Go** | `GoToFile`, `GoToClass`, `GoToSymbol`, `GoToLine` — `GoToDeclaration`, `GoToImplementation`, `GoToTypeDeclaration` — `NavigateBack`, `NavigateForward` — `FindUsages`, `ShowUsages`, `FindInPath`, `FindAction` — `QuickDocumentation`, `ShowIntentionActions` — `ToggleSmartMode` |
| **Window** | `NextTab`, `PreviousTab`, `CloseTab` — *(separator, then)* `PredefinedMenuItem::minimize(None)`, `PredefinedMenuItem::fullscreen(None)` |

**`Undo`/`Redo` are `MenuItem::with_id("Undo", …)`/`with_id("Redo", …)`
dispatching to this app's own `CommandAction::Undo`/`Redo` (the buffer's
own undo stack) — never `PredefinedMenuItem::undo()`/`redo()`.** Those
predefined items drive macOS's native `NSTextField`/`NSUndoManager`
undo, which this app's custom `Buffer` type doesn't participate in; using
them here would silently do nothing (or the wrong thing) instead of what
`⌘Z` already does today.

### 3.3 Dispatch and accelerators

`poll_menu_events` does `MenuEvent::receiver().try_recv()`, and on `Ok`,
looks up `crate::command::commands().iter().find(|c| event.id() == c.id)`
(valid via `MenuId`'s `PartialEq<&str>` impl — no string allocation
needed for the comparison) and calls `self.run_command(cmd.action, ctx)`
if found. A miss (an id from a `PredefinedMenuItem` this code doesn't
otherwise handle, e.g. About/Quit/Hide/Minimize, all of which macOS
handles entirely itself without this app ever seeing an event for them)
is silently ignored, not an error — `PredefinedMenuItem`s are exactly the
items that never reach application code at all.

**No accelerator glyphs shown on menu items in this phase** (`None`
passed everywhere in §3.2's table) — a deliberate cut, not an oversight:

- Keyboard shortcuts already work today via `handle_shortcuts`
  (`command.rs`'s `Binding`/`KeyChord`, resolved per-frame from
  `egui::InputState`) — the menu doesn't need to duplicate them for the
  shortcuts to keep functioning exactly as they do now.
- If a menu item *did* carry a native `Accelerator`, macOS would route
  that keystroke through the OS menu system, which is a **second,
  independent path** to the same `run_command` call `handle_shortcuts`
  already triggers — without care to ensure exactly one of the two fires,
  a single keystroke could dispatch a command twice in one frame.
  Avoiding that requires either suppressing `handle_shortcuts`'s own
  matching `KeyChord` when a native accelerator is set (extra
  synchronization state, one more thing that can drift out of sync
  between two independent binding tables), or converting every
  `egui::Key`/`Modifiers` combination in the registry to muda's
  `accelerator::Code`/`Modifiers` types accurately (a full second mapping
  table with its own edge cases). Both are real, doable work for a
  future phase; this one ships the simpler, unambiguous version first
  (menu items are click-only triggers) and leaves accelerator display as
  an explicit, separately-scoped follow-up.

### 3.4 Platform scoping

Everything in `menu.rs` is behind `#[cfg(target_os = "macos")]` at the
function level, not the module level — `install_native_menu` and
`poll_menu_events` exist and compile on every platform (so `app.rs`/
`app/render.rs` need no `#[cfg]` of their own at the call sites), but on
non-macOS both are trivial no-ops (`install_native_menu` does nothing;
`poll_menu_events` returns `false` immediately) and `muda` itself is a
macOS-only `Cargo.toml` dependency (§6) — so non-macOS builds pull in
neither the crate nor any AppKit-specific code path, matching this
project's existing "small, pure-Rust dependency set" discipline of not
compiling in platform code nothing on that platform will ever run.

## 4. Constraints & invariants

- **Every `Some(id)` in every `MenuGroup` in `menu_groups()` must name a
  real `Command::id`** — enforced by a test
  (`menu_groups_reference_only_real_commands`) that walks every group and
  asserts `crate::command::commands().iter().any(|c| c.id == id)` for
  each one, independent of ever constructing a real `muda::Menu`.
- **No `CommandAction` is invented for this phase.** Every dispatchable
  menu item maps to an id that already exists in `commands()` before this
  phase starts; `PredefinedMenuItem`s (About/Quit/Hide/Services/Minimize/
  Fullscreen/separators) are the only items with no `CommandAction`
  behind them, and they're OS-standard behavior muda/AppKit implements
  natively, not new application logic.
- **Exactly one dispatch path per keystroke.** §3.3's "no accelerators"
  cut exists specifically to preserve this — `handle_shortcuts` remains
  the only path from a keystroke to `run_command`; `poll_menu_events` is
  the only path from a menu *click* to `run_command`; nothing overlaps.
- **`install_native_menu` runs once, at startup, not per-frame** — unlike
  `poll_menu_events`, which by construction (draining a channel) is safe
  to call every frame, rebuilding the whole native menu every frame would
  be wasted OS-call overhead for a menu whose content (§3.2's table) never
  changes at runtime in this phase (no per-tab or per-project menu items).
- **§3.1's verification gate is a hard implementation-order requirement,
  not a suggestion** — the rest of this doc's design is only worth
  building once that's confirmed.

## 5. Examples

**Startup:**

```rust
// app.rs, IdeApp::new
menu::install_native_menu(); // attaches the real macOS menu bar once
```

**One frame, a user clicks "Save All" in the File menu:**

```rust
// app/render.rs, IdeApp::ui
let ctx = ui.ctx().clone();
if self.poll_menu_events(&ctx) {
    ctx.request_repaint();
}
// poll_menu_events, internally:
//   MenuEvent { id: MenuId("SaveAll"), .. } <- MenuEvent::receiver().try_recv()
//   commands().iter().find(|c| c.id == "SaveAll") -> Some(Command { action: CommandAction::SaveAll, .. })
//   self.run_command(CommandAction::SaveAll, &ctx) -- identical to what
//   the command palette does for the same id today.
```

**A predefined item (Quit) is chosen:** no `MenuEvent` for it ever reaches
`poll_menu_events` — AppKit terminates the app itself, the same as
`⌘Q` already does via the OS today (unrelated to this app's own
`request_quit`/`should_quit` close-confirmation flow, which only runs for
window-close, not the native Quit menu item, in this phase).

## 6. Dependencies & integration points

- New dependency, **macOS-only**: `muda = "0.19"`, added to
  `crates/ui/Cargo.toml` under `[target.'cfg(target_os = "macos")'.dependencies]`
  (a new table — `Cargo.toml` currently has none), and to CLAUDE.md's
  dependency table (phase: "polish batch D", for: "native macOS menu
  bar"). Not added to the unconditional `[dependencies]` table, so it
  never gets pulled into a Linux/Windows build (§3.4).
- Integrates with the existing command registry (`crates/ui/src/
  command.rs`) read-only — `menu_groups()` never constructs a
  `CommandAction` itself, only looks up ids already there.
- Integrates with `IdeApp::run_command` (`app.rs`), the same dispatch
  point the command palette (`app/render.rs`) already uses — this phase
  adds a second caller, not a second dispatcher.
- No `crates/core`/`crates/lsp` involvement; single role, `rust-ui-dev`.
- Not in CLAUDE.md's security-sensitive-paths list (no subprocess, no
  network I/O, no file-path handling from untrusted input) — no `hacker`
  pass expected for this role, though `git diff --name-only` should be
  re-checked once the diff is final, per that list's own standing rule.

## 7. Diagram

Skipped — this phase is a single OS callback (`MenuEvent` → one
`run_command` call) layered onto an existing dispatch point, not a
multi-step protocol; §3's prose and §5's worked examples cover the one
control-flow path completely.

## Revision notes

- §3.1: `rev`'s doc review approved this doc with §3.1's mechanism marked
  as an unresolved risk gated behind a mandatory first-implementation-step
  verification. That verification has since happened — a human launched
  the spike binary directly and clicked its menu item by hand, confirming
  `init_for_nsapp()` called from inside the first `ui()` frame does attach
  a working native menu. §3.1 is updated to state this as confirmed rather
  than open; two automated recheck attempts (`osascript`, a screenshot) run
  from an agent shell in between had falsely suggested it didn't work —
  noted in case a future phase needs the same kind of OS-integration check
  and hits the same false negative from automation.
