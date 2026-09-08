# TUI top menu bar with submenus

## 1. Purpose

Direct user request: "I want a full menu with submenus on top for all our
actions" (confirmed scope: `ide-tui`). `ide-tui` has no menu-bar concept
today -- the only ways to reach a command are its keybinding, the command
palette (`FindAction`, all 106 commands flat, fuzzy-filtered), and
per-screen chrome (screen tabs, dock tabs, the T48 key-hint ribbon). This
adds a **persistent, always-visible, in-app-rendered menu bar** (there is
no OS-native menu bar available to a terminal app, unlike `ide-ui`'s
`native-menu-bar.md`, which uses a real macOS menu via `muda`) with
dropdown submenus, organizing **every one of the 106 commands in
`commands()`** -- not a curated subset like `ide-ui`'s own menu
deliberately chose, since the user asked for "all our actions" explicitly.

**Interaction model**, decided directly with the user via `AskUserQuestion`
before design:
- Always-visible bar; click a menu name or press `Alt+<mnemonic letter>`
  to open its dropdown (not a two-step F10-then-mnemonic gesture).
- Lives in a **new row above** the existing screen-tab row -- the screen
  tabs (Editor/Git/Run/Keys) are unchanged in every other respect, just
  shifted down one row.

## 2. Interface

### 2.1 `crates/tui/src/commands.rs` -- the menu table

```rust
/// One dropdown entry: either a command by id, or a titled flyout
/// submenu of further command ids (one level deep -- no menu in this
/// table nests a `Submenu` inside another `Submenu`).
pub enum MenuEntry {
    Item(&'static str),
    Submenu(&'static str, &'static [&'static str]),
}

pub struct MenuGroup {
    pub title: &'static str,
    /// Case-insensitive; must be a character literally present in
    /// `title` (checked by `menu_mnemonic_is_present_in_its_own_title`).
    /// `Alt+<mnemonic>` opens this menu directly, from anywhere.
    pub mnemonic: char,
    pub entries: &'static [MenuEntry],
}

pub fn menu_groups() -> &'static [MenuGroup];
```

Eleven groups, in bar order: **File, Edit, View, Navigate, Code, Refactor,
Run, Git, Tools, Window, Help**. This grouping is deliberately a
*different* taxonomy from `ide-ui`'s own `native-menu-bar.md` §2.1
`MENU_GROUPS` table (that one is a hand-picked subset with two whole
menus cut; this one is exhaustive and TUI-specific -- e.g. `Debug` lives
as a `Run > Debug` submenu here since `ide-tui` has no separate `Tools`
convention for it yet, `Docker`/`Kubernetes`/`Custom Actions`/`Claude`/`AI`
land in `Tools` since they're external-integration panels, not editor
views). Full assignment (all 106 ids, verified programmatically against
`commands()` before writing this doc -- see §4's completeness invariant):

| Menu | Direct items | Submenus |
|---|---|---|
| File | SaveAll, ReloadFromDisk, DismissExternalChange, NewScratchFile, ScratchFiles, Exit | -- |
| Edit | Undo, Redo, Find, Replace, FindInPath, ReplaceInPath, ToggleCase, DuplicateLines, DeleteLines, JoinLines, JumpToMatchingBracket | **Move**: MoveLinesUp, MoveLinesDown, MoveStatementsUp, MoveStatementsDown; **Comment**: ToggleLineComment, ToggleBlockComment; **Selection**: ExtendSelection, ShrinkSelection, AddNextOccurrence, UnselectOccurrence, SelectAllOccurrences, CollapseSelections |
| View | ToggleProjectToolWindow, ToggleNotifications, ToggleProblems, ToggleTodoPanel, ShowBookmarks, ToggleBookmark, QuickDocumentation, ShowIntentionActions | **Folding**: CollapseFold, ExpandFold, CollapseAllFolds, ExpandAllFolds |
| Navigate | GoToDeclaration, FindUsages, GoToFile, GoToSymbol, FileStructure, RecentFiles, NavigateBack, NavigateForward | -- |
| Code | GenerateMenu, ImplementMethods, OverrideMethods, CreateTest, OptimizeImports, Rename, ReformatCode, ToggleFormatOnSave, TriggerFimAutocomplete | -- |
| Refactor | RefactorThis, ExtractVariable, ExtractMethod, ExtractConstant, ExtractField, Inline | -- |
| Run | GoToRunScreen, ToggleCargoPanel | **Debug**: Debug, ResumeProgram, StepOver, StepInto, StepOut, ToggleLineBreakpoint, StopDebugging, PauseProgram, ConfigureDebugAdapter, ToggleDebugPanel |
| Git | ToggleGitPanel, GitBranches, GitWorktrees, ShowFileHistory, ToggleBlameAnnotations, ShowBlameForCurrentLine, Fetch, Pull, Push, ToggleClonePanel | -- |
| Tools | ManageCustomActions, ToggleCustomActionsPanel, ToggleDockerPanel, ToggleK8sPanel, ClaudePanel, AiPanel | -- |
| Window | NextTab, PreviousTab, CloseTab, GoToEditorScreen, GoToKeysScreen, ToggleLeftDock, ToggleBottomDock, ToggleBottomDockFocus, GrowFocusedDock, ShrinkFocusedDock | -- |
| Help | FindAction, ToggleKeymapSettings, ToggleThemeSettings, ResetAllKeybindings | -- |

Mnemonics: File=**F**, Edit=**E**, View=**V**, Navigate=**N**, Code=**C**,
Refactor=**R**, Run=**u** (second letter -- `R` is Refactor's), Git=**G**,
Tools=**T**, Window=**W**, Help=**H**. All eleven distinct. These are a
**UI navigation gesture, not a command keybinding** -- the same category
as this crate's existing raw `Tab`/arrow-key/`Enter`/`Esc` popup
navigation that never goes through `commands()`/the keymap overlay, so
CLAUDE.md's "never invent a binding, use the JetBrains one verbatim" rule
(which governs *command* keybindings) does not apply here; there is no
JetBrains menu-bar mnemonic table to translate from in the first place
(JetBrains' own menu bar is native-OS on macOS).

### 2.2 `crates/tui/src/app.rs` -- state

```rust
#[derive(Default)]
pub(crate) struct MenuBarState {
    /// Index into `menu_groups()`. `Some` = the bar's dropdown is open.
    pub(crate) open: Option<usize>,
    /// Selected row within the open menu's `entries`.
    pub(crate) selected: usize,
    /// `Some(i)` = a submenu flyout is open, `i` selects within its
    /// command-id slice. `None` = no flyout (dropdown-only, or a
    /// non-`Submenu` entry is highlighted).
    pub(crate) submenu_selected: Option<usize>,
}
```

New `App` field: `pub(crate) menu_bar: MenuBarState`.

New methods:

```rust
fn open_menu_bar(&mut self, index: usize);
fn close_menu_bar(&mut self);
fn handle_menu_bar_key(&mut self, key: KeyEvent) -> LoopSignal;
fn handle_menu_bar_click(&mut self, point: (u16, u16), hits: &ui::HitMap);
fn run_action_by_id(&mut self, id: &str) -> LoopSignal; // looks `id` up in `commands()` and runs it
```

No separate `menu_mnemonic_index` helper -- the `Alt+<letter> -> menu_groups()`
index` lookup (`groups.iter().position(|g| g.mnemonic.eq_ignore_ascii_case(&c))`)
is inlined at both of its call sites (`handle_key`'s opening trigger,
`handle_menu_bar_key`'s "jump to another menu while already open" branch)
rather than factored into a shared function -- it's a one-line `position`
call at each site, and `handle_menu_bar_click` takes the already-extracted
`(u16, u16)` point rather than the full `MouseEvent`, matching how
`handle_mouse_click` itself extracts `point` once at its own top.

`close_all_overlays` gains `self.menu_bar = MenuBarState::default();`
(same reasoning as `colon_command`/`unified_finder`: no toggle-function
precedent guarantees it's already closed). `any_true_popup_open` gains
`|| self.menu_bar.open.is_some()`.

`open_menu_bar` mirrors `go_to_git_screen`'s save-and-restore shape
around `close_all_overlays` (which unconditionally clears `git_panel`):

```rust
fn open_menu_bar(&mut self, index: usize) {
    let existing_git_panel = self.git_panel.take();
    self.close_all_overlays();
    self.git_panel = existing_git_panel;
    self.menu_bar = MenuBarState {
        open: Some(index),
        ..MenuBarState::default()
    };
}
```

Without this, opening the menu bar while on the Git screen (its label is
clickable there, §3.3) would silently desync `active_screen == Git` from
`git_panel == None` -- the exact bug class `tui-screen-navigation.md`'s
round-1 review already found and fixed for `go_to_git_screen`/`go_to_run_
screen`; this reuses that fix rather than reintroducing the gap for a
third caller.

### 2.3 `crates/tui/src/ui.rs` -- rendering

New `HitMap` fields:

```rust
/// Top-level menu-bar label click regions, same shape as `screen_tabs`.
pub menu_bar_labels: Vec<(Rect, usize)>,
/// The open dropdown's own row regions, index into that menu's
/// `entries`. Empty whenever `menu_bar.open` is `None`.
pub menu_dropdown_items: Vec<(Rect, usize)>,
/// The open flyout's row regions, index into that submenu's command-id
/// slice. Empty whenever `menu_bar.submenu_selected` is `None`.
pub menu_submenu_items: Vec<(Rect, usize)>,
```

New functions: `render_menu_bar` (row 0, unconditional, every frame, every
`AppScreen` -- same "always reserved" status as the T48 ribbon row),
`render_menu_dropdown`/`render_menu_submenu` (popups, called near the end
of `render()` alongside every other `if app.X.is_some() { render_X(...) }`
block, gated on `app.menu_bar.open.is_some()`/`app.menu_bar.submenu_
selected.is_some()`).

`render()`'s outer `Layout` gains a row: `[Length(1), Length(1), Min(1),
Length(1), Length(1)]` (menu bar / screen tabs / body / status / ribbon),
`render_menu_bar` called before `render_screen_tabs`. `EDITOR_CHROME_ROWS`
7 -> 8 (T48's own precedent for bumping this constant when `render`'s
outer split gains a row; its doc comment gets one more sentence).

Dropdown anchoring: `render_menu_dropdown` reads `hits.menu_bar_labels
[index].0.x` (already populated this same frame by the earlier `render_
menu_bar` call) as its left edge, `y` = the screen-tab row's `y` (row 1,
directly under the bar). Width = longest `(title, binding-label)` pair
among its entries plus padding; height = entry count + 2 (border). Each
row's binding label comes from `self.keymap.effective_binding(id)` +
`keymap::label`, exactly `key_hint_rows`' own resolution (§2.5 of
`tui-key-hint-ribbon.md`) -- a `Submenu` row shows `▸` instead of a
binding, and is not itself resolvable to an `Action`. Flyout anchors to
the right edge of the dropdown, `y` = the highlighted `Submenu` row's own
`y`.

## 3. Behaviour

### 3.1 Opening

- `Alt+<mnemonic>`, checked early in `handle_key` (same rank as the T45
  colon-command trigger, T46 double-tap gesture -- before the global
  keymap lookup), gated on `!self.any_popup_open()`: opens that menu with
  `selected = 0`.
- Click on `hits.menu_bar_labels[i]`: same effect, checked at the same
  tier `screen_tabs` clicks already occupy in `handle_mouse_click` (after
  `any_true_popup_open()`, so a genuine popup still blocks it, but it
  works on the Git screen exactly like `screen_tabs` does -- §3.3).
  Clicking the *already-open* menu's own label closes it (toggle).

### 3.2 Once open -- keyboard

Dispatched by a new early check in `handle_key`
(`if self.menu_bar.open.is_some() { return self.handle_menu_bar_key(key); }`),
positioned before the `active_screen == AppScreen::Git` check so it still
works if the bar was opened by clicking its label while on the Git
screen:

| Key | No flyout open | Flyout open |
|---|---|---|
| `Alt+<mnemonic>` | Switch directly to that menu (`selected = 0`) | Same -- closes the flyout too |
| `Left`/`Right` | Move to the previous/next top-level menu, **wrapping** (`selected = 0`) | Close the flyout, return to the dropdown |
| `Up`/`Down` | Move `selected` within `entries`, **clamped** (no wrap) | Move within the flyout's items, clamped |
| `Enter` | `Item`: run it, close everything. `Submenu`: open the flyout (`submenu_selected = Some(0)`) | Run the highlighted item, close everything |
| `Esc` | Close the whole bar | Close just the flyout |

Left/Right between top-level siblings wraps (standard menu-bar
convention, mirrored from how a physical menu bar cycles); Up/Down within
a list clamps (this crate's established list-cursor convention, e.g.
`CustomActionsPanel::move_selection`) -- two different, both precedented,
conventions applied to the two different axes, not an inconsistency.

### 3.3 Once open -- mouse

`handle_mouse_click` gains a new first-tier check, positioned *before*
`any_true_popup_open()` (mirroring `tui-panel-pane-scroll.md` §3.1's
precedent of checking a specific state's own regions before the generic
popup-blocking gate):

```
if menu_bar.open.is_some():
    flyout item hit?    -> run it, close everything
    dropdown item hit?  -> Item: run + close; Submenu: open flyout
    another bar label?  -> switch to that menu
    same bar label?     -> close (toggle)
    anything else       -> close the bar, consume the click (no further action)
```

The "anything else closes the bar and does nothing further" branch is the
standard "click outside a menu dismisses it" convention -- deliberately
*not* falling through to also perform whatever that click would otherwise
have done (e.g. a stray click on the tree while a menu is open just
closes the menu, it doesn't also select a tree row), since one click should not plausibly do two unrelated things.

### 3.4 Everywhere else

Every existing popup-priority/`close_all_overlays`/`any_popup_open`
behavior is unchanged; the menu bar plugs into those exact same
mechanisms as every other overlay, not a parallel system.

## 4. Constraints & invariants

- **Completeness, enforced by test, not by review**: `menu_completeness_
  covers_every_command_exactly_once` flattens every `MenuEntry::Item`/
  `Submenu` id across `menu_groups()` and asserts the resulting set equals
  `commands().iter().map(|c| c.id)` exactly (same id count, no
  duplicates either side). A future 107th command that forgets a menu
  entry fails this test immediately, the same self-verifying-invariant
  style as `no_two_bound_commands_share_the_same_chord`.
- `menu_mnemonic_is_present_in_its_own_title`: every `MenuGroup::mnemonic`
  is (case-insensitively) a character of its own `title`.
- `menu_mnemonics_are_all_distinct`: no two groups share a mnemonic.
- No menu nests a `Submenu` inside another `Submenu` -- one flyout level,
  enforced by the type (`MenuEntry::Submenu` holds `&[&str]`, not
  `&[MenuEntry]`).
- Selecting any menu item runs it through the existing `run_action`
  dispatcher -- identical to the palette, identical to a keybinding.
  **No new `Action` variant, no new `Command` entry.** This is a third way
  to reach an existing action, not a new capability.
- Opening the menu bar never loses Git-screen state (§2.2's save/restore).

## 5. Examples

`Alt+G` from the Editor screen with nothing else open: `menu_bar.open =
Some(<Git's index>)`, dropdown renders under the "Git" label showing
ToggleGitPanel/GitBranches/.../ToggleClonePanel, each with its current
effective binding (or blank if unbound) to the right. `Down` four times
highlights "Show History for File"; `Enter` runs `Action::ShowFileHistory`
and closes the bar.

Clicking "Edit" then clicking "Move" (a `Submenu` row): a flyout
opens to the right of the Edit dropdown listing Move Line Up/Down/Move
Statement Up/Down; clicking "Move Statement Down" runs `Action::
MoveStatementsDown` and closes both the flyout and the dropdown.

Clicking anywhere in the tree while the Git menu's dropdown is open: the
dropdown closes; the tree click itself is **not** also applied (§3.3).

## 6. Dependencies & integration points

`crates/tui/src/{commands.rs,app.rs,ui.rs}` only. No `ide-core`/`ide-lsp`/
`ide-dap` changes, no new `Action`/`Command` entries. Reuses `keymap::
label`/`effective_binding` (T48), the `HitMap`/click-and-scroll-priority
patterns from `tui-panel-focus-and-scroll.md` (T51) and `tui-panel-pane-
scroll.md` (T53), and `go_to_git_screen`'s save/restore-around-`close_
all_overlays` pattern (T44 round-1 review fix).

## Revision notes

Self-review round (post-implementation, same session) caught two real
deviations from this doc, both fixed in place rather than left as known
gaps:

1. **§3.1/§3.3's "clicking the already-open menu's own label closes it
   (toggle)" was documented but not implemented** -- `handle_menu_bar_
   click`'s bar-label branch unconditionally reopened at `selected = 0`
   regardless of whether the clicked label was already the open one.
   Fixed: compares the clicked index against `menu_bar.open` and calls
   `close_menu_bar()` on a match. New regression test:
   `mouse_click_on_the_already_open_menus_own_label_closes_it`.
2. **§2.3's flyout anchoring ("`y` = the highlighted `Submenu` row's own
   `y`") was implemented as the dropdown box's top border instead** --
   every flyout rendered flush with the top of its dropdown regardless of
   which row was highlighted, visually misaligned for any menu where the
   `Submenu` isn't the first entry (e.g. Edit's Move/Comment/Selection,
   Run's Debug at index 2). Fixed: `render_menu_submenu` now looks up the
   highlighted row's actual `y` from `hits.menu_dropdown_items` (already
   populated by this frame's earlier `render_menu_dropdown` call) instead
   of reusing the dropdown's own top edge. Rendering-only, not caught by
   any test (`ui.rs` is excluded from the coverage target per this
   crate's convention) -- caught by re-reading the code against the doc's
   own wording, not by a failing test.

§2.2's `menu_mnemonic_index`/`handle_menu_bar_click(event: MouseEvent,
...)` signatures in the original draft above were never actually
implemented that way -- the real code inlines the mnemonic lookup at its
two call sites and takes an already-extracted `(u16, u16)` point instead
of the full `MouseEvent`. Functionally identical, arguably cleaner; the
snippet above has been updated to match the real code rather than the
other way around.
