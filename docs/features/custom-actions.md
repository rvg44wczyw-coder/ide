# GUI Custom Actions (roadmap G8, new)

## 1. Purpose

Second half of the `claude.ai/design` `Fleet-like IDE.dc.html` import — the
GUI counterpart of `ide-tui`'s already-shipped `T42`
(`docs/features/tui-custom-actions.md`); the first half (colour palette)
shipped as roadmap G3 (`docs/features/themes.md`).

**Scope decision, made explicitly by the user (`AskUserQuestion`,
2026-09-05), not assumed.** A full read of the mockup's Settings screen
shows its actual "panel actions" model is **not** T42's shape: a
`PANEL ACTIONS` grid lists the Top/Left/Right/Bottom edges, each showing
its currently-bound *built-in* chrome items (e.g. Top: "Search ·
Workspace · Presence · Terminal · Right panel"; Left: "Files · Version
control · Debug · Terminal · Settings"), with copy reading "Every panel
edge exposes an action slot. Drag an action between edges or add a new
one — the layout is the extension point." That is a drag-and-drop,
relocatable-toolbar redesign of `ide-ui`'s currently-fixed chrome
(`intellij-shell.md`/`B2b`) — a much larger, more invasive feature than
T42 ever was for the TUI. Given three options (mirror T42's narrower
"user-declared external command" model; build the full relocatable
toolbar; a no-drag hybrid), the user chose **"Mirror T42 literally"** —
this doc implements exactly that: a small panel of user-declared named
external commands, run from a dock tab and the command palette. The
mockup's drag-and-drop four-edge relocation of *built-in* chrome items is
explicitly **out of scope** and not attempted here.

**Roadmap slot.** This wasn't part of the original Track A–G plan (unlike
G3's already-designated "themes" phase) — it's a net-new phase, `G8`
(Track G, «платформа»), added the same way T41/T42 were added fresh to
§10 when the Orbit TUI import created work nobody had pre-planned.

**Shares its storage with `ide-tui`, for free.** `ide_core::project_settings
::ProjectSettingsFile::CustomActions` (`.ide/custom_actions.json`) already
exists — created by `rust-core-dev` for T42, and its own doc comment
explicitly anticipated a second frontend user ("content-named, not
frontend-named, same convention `Navigation`'s own doc comment already
establishes since ide-tui is that variant's first user too"). No
`ide-core` work is needed this run: this doc's `CustomAction`/
`CustomActionsFile` struct shapes are defined identically (same field
names/types) to `crates/tui/src/custom_actions.rs`'s so the JSON file
round-trips between both frontends — an action added in the GUI is
runnable from the TUI's Custom Actions dock tab on the same project, and
vice versa, the next time either frontend loads project settings (§3).

**Branding**, same rule as T41/T42/Ember: no "Orbit" string anywhere in
code, UI text, or identifiers.

**Security-sensitive — `hacker` is required before merge.** Same class of
surface `crates/ui/src/cargo_panel.rs` already is: subprocess argument
vector and `current_dir` both come from user-typed config
(`docs/features/tui-custom-actions.md`'s own hacker pass found and fixed a
real DoS on the TUI side of this exact storage format — §4 carries the
proactive fix forward from day one instead of waiting to rediscover it).
Root `CLAUDE.md`'s declared security-sensitive-paths list is updated as
part of this doc's own landing (§2.5) to name
`crates/ui/src/custom_actions.rs` explicitly, mirroring the existing
`cargo_panel.rs` bullet — this is an orchestrator-level edit to a
project-level file, not `rust-ui-dev`'s to make.

## 2. Interface / API

### 2.1 New file `crates/ui/src/custom_actions.rs`

```rust
pub(crate) struct CustomAction {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
}

pub(crate) struct CustomActionsFile {
    pub(crate) actions: Vec<CustomAction>,
}

const MAX_CUSTOM_ACTIONS: usize = 500;

pub(crate) fn load(project_root: &Path) -> Vec<CustomAction>;
pub(crate) fn save(project_root: &Path, actions: &[CustomAction]);

pub(crate) enum StreamEvent { Line(String), Done }

pub(crate) struct CustomActionsPanel {
    pub(crate) actions: Vec<CustomAction>,
    pub(crate) selected: usize,
    pub(crate) running: Option<CustomAction>,
    pub(crate) output: Vec<String>,
    pub(crate) rx: Option<Receiver<StreamEvent>>,
}

impl CustomActionsPanel {
    pub(crate) fn run_selected(&mut self, project_root: &Path);
    /// Returns `true` if `output`/`running` changed this call -- same
    /// signature as `CargoPanel::poll`, not `ide-tui`'s `poll(&mut self)`
    /// (no return): the caller uses it to decide whether to
    /// `ctx.request_repaint()`, exactly like `cargo.poll()`'s existing
    /// call site (`app/render.rs`).
    pub(crate) fn poll(&mut self) -> bool;
}
```

`CustomAction`/`CustomActionsFile`'s field names/types (`name: String`,
`command: String`, `args: Vec<String>`) are **identical** to
`crates/tui/src/custom_actions.rs`'s — this is the constraint that keeps
`.ide/custom_actions.json` byte-compatible between frontends (§4).

`load`/`save` are the exact same best-effort, fail-open semantics
`crates/tui/src/custom_actions.rs::load`/`save` already established:
missing file, malformed JSON, or a symlink-escape all collapse to an
empty list on `load`; every failure is swallowed on `save`. `load` applies
`MAX_CUSTOM_ACTIONS = 500` via `.truncate(...)` **from this doc's first
draft**, not discovered later by `hacker` — `tui-custom-actions.md`'s own
hacker pass already proved the concrete DoS (a synthetic 1,000,000-action/
~59MB file costing ~100ms/frame to re-render while the tab is open) against
this exact file format, and that finding transfers directly since both
frontends read the same untrusted-once-cloned file.

`run_selected`: no-op if `running.is_some()` (at most one action running
at a time, same v1 scope `CargoPanel`/T42 both already accepted — not
re-argued here, see T42's own `[controversial]` note in `docs/roadmap.md`'s
T42 row) or if `actions` is empty or `selected` is out of range. Clears
`output`, spawns via this module's own `spawn_streaming` (below), sets
`running` to a **clone** of the selected action, not an index — the exact
same invariant `tui-custom-actions.md` §2.3 established, for the same
reason: deleting the running action's definition via the Manage popup
mid-run must not corrupt or panic the in-flight run (§3).

```rust
fn spawn_streaming(program: &str, args: &[String], project_root: &Path) -> Receiver<StreamEvent>;
```

`ide-ui` has no shared `subprocess.rs` the way `ide-tui` does (`ide-tui`'s
`custom_actions.rs` reuses `subprocess::spawn_streaming` verbatim, written
once for `docker_panel.rs`/`k8s_panel.rs`). This doc's `spawn_streaming` is
new code, but not a new *pattern*: it is `cargo_panel.rs`'s own
`run_and_stream`/`stream_lines`/`StreamEvent` generalized from a single
fixed `subcommand: &str` argv element to an arbitrary `args: &[String]`
slice, changing exactly one call (`Command::new(program).arg(subcommand)`
→ `Command::new(program).args(args)`) and nothing else — same
not-found/failed-to-run message wording
(`"{program} not found on PATH"` / `"failed to run {program}: {e}"`), same
stdout+stderr-interleaved-by-completion-order streaming via two joined
reader threads.

### 2.2 `crates/ui/src/app.rs`

`BottomView` (currently `Problems`/`CargoOutput`/`Usages`/`Search`/
`Debug`) gains a sixth variant, `CustomActions`.

```rust
#[derive(Default)]
pub struct CustomActionsPopupState {
    pub open: bool,
    pub new_name: String,
    pub new_command: String,
    pub new_args: String,
    /// `Some(i)` while editing `actions[i]` in place (populated by
    /// clicking Edit on a row); `None` means Create appends instead of
    /// overwriting. Unlike `tui-custom-actions.md`'s `ActionFormField`
    /// Tab-cycling enum, egui renders Name/Command/Args as three always-
    /// visible `TextEdit` widgets simultaneously (mouse-driven, not modal
    /// keyboard input) -- no field-cycling state is needed at all, the
    /// one place this doc's design is simpler than its TUI counterpart's.
    ///
    /// Valid whenever it's checked, but **not invariantly valid across
    /// every mutation of `custom_actions.actions`** -- specifically,
    /// `load_project_settings` replacing the whole list on a project
    /// switch does *not* itself clear or adjust this field (see the
    /// `load_project_settings` note below, which is where that reset
    /// actually lives instead). Within a single project, delete
    /// (`delete_custom_action`) clears/adjusts it in the same step, same
    /// as `tui-custom-actions.md` §2.3/§4's analogous invariant.
    pub editing_index: Option<usize>,
    /// Set by `confirm_custom_action_form` on validation failure (empty
    /// name/command), rendered as a `colored_label` above the form the
    /// same way `language_settings_error` is rendered for
    /// `add_custom_language` (`app.rs:3662` -- the real, existing
    /// precedent for this validate-then-reject shape in this crate;
    /// there is no `confirm_debug_adapter_config` function in `ide-ui`,
    /// an earlier draft of this doc cited one that doesn't exist).
    /// Cleared on any successful `confirm_custom_action_form` call and
    /// when the popup is (re)opened via `open_custom_actions_popup`.
    pub error: Option<String>,
}
```

`IdeApp` gains `custom_actions: CustomActionsPanel` and
`custom_actions_popup: CustomActionsPopupState` fields (both `Default`).

`load_project_settings` gains two lines, alongside the existing
`custom_languages`/`format_on_save`/etc. reads: `self.custom_actions.actions
= crate::custom_actions::load(root);` **and**
`self.custom_actions_popup = CustomActionsPopupState::default();`. The
second line matters: `editing_index`, when `Some`, is an index into
`custom_actions.actions`, and nothing about opening the Manage popup
prevents the user from also switching projects while it's still open (this
crate has no `close_all_overlays`-style catch-all — see the note later in
this section — so nothing else would close it for us). Without this reset, editing row 3 of a
5-action project, then switching to a project with fewer actions while the
popup stays open, then clicking Save, indexes `actions[3]` on the new
project's (shorter) list and panics. Resetting the whole popup state on
every project load — rather than only patching `editing_index` — is
deliberately blunt: it also discards an in-progress unsaved Name/Command/
Args draft on project switch, which is an acceptable (and arguably
correct — the draft belonged to the old project) cost for closing off the
panic path entirely instead of trying to prove a narrower patch is safe
under every future edit to this state. **Deliberately does not** reset
`running`/`output`/`rx` on project switch — mirrors `CargoPanel`'s own
existing untouched-across-project-switch treatment exactly (`self.cargo`
is never reset in `load_project`/`open_project` either); see §3. These are
different fields with different reasoning: `running`/`output`/`rx` track a
already-in-flight subprocess with no dangling-index risk, `editing_index`
is a raw index into a list that just got swapped out from under it.

New methods, mirroring `GitPanel::open_worktrees_popup`'s naming and
`add_custom_language`'s (`app.rs:3662`) validate-then-apply,
visible-error-on-failure pattern rather than inventing a new one:

- `open_custom_actions_popup(&mut self)` — `self.custom_actions_popup =
  CustomActionsPopupState { open: true, ..Default::default() };`. No
  project-root parameter needed (unlike `open_worktrees_popup`, which
  eagerly lists worktrees from disk) since `custom_actions.actions` is
  already the live in-memory list this popup renders directly, kept
  current by `load_project_settings`/`confirm_custom_action_form` — there
  is nothing to separately load. (This also clears any stale `error` from
  a previous session with the popup, same as every other field.)
- `start_editing_custom_action(&mut self, index: usize)` — populates
  `new_name`/`new_command`/`new_args` (args rejoined with single spaces,
  same lossy hand-edited-round-trip limitation
  `tui-custom-actions.md`'s Revision notes already documented and
  accepted for the TUI side) from `custom_actions.actions[index]`, sets
  `editing_index = Some(index)`. No-op if `index` is out of range.
- `confirm_custom_action_form(&mut self)` — trims `new_name`/`new_command`
  and, if either is empty, sets `self.custom_actions_popup.error =
  Some("Custom action name cannot be empty.".to_string())` (or the
  command-empty equivalent) and returns, leaving the form open with its
  text intact — the same visible-error shape `add_custom_language`
  (`app.rs:3662`) already uses via its own `language_settings_error`
  field, and the same outcome (form stays open, user is told why) TUI's
  own `confirm_action_form` gets via `self.notify(...)`. On success:
  splits `new_args` on whitespace into `Vec<String>`; if
  `editing_index.is_some()`, overwrites that entry, else pushes a new
  one; calls `crate::custom_actions::save`; clears the three text fields,
  `editing_index`, and `error` (popup stays open, matching
  `render_worktrees_popup`'s own post-Create behaviour).
- `delete_custom_action(&mut self, index: usize)` — removes immediately,
  no confirmation step (same as `tui-custom-actions.md`'s own explicit
  "no confirm" choice for this exact action); calls
  `crate::custom_actions::save`; if `editing_index == Some(index)`, clears
  it (the invariant above).

`run_command`/`is_command_enabled` gain two arms each, mirroring
`GitWorktrees`'s exact existing shape:

```rust
// is_command_enabled
CommandAction::ManageCustomActions => self.project.is_some(),
CommandAction::ToggleCustomActionsToolWindow => self.project.is_some(),

// run_command
CommandAction::ManageCustomActions => self.open_custom_actions_popup(),
CommandAction::ToggleCustomActionsToolWindow => {
    self.toggle_bottom_tool_window(BottomView::CustomActions)
}
```

No `close_all_overlays`/`any_popup_open`-style catch-all exists in
`ide-ui` (that's a TUI-only pattern for dispatching Esc across modal
keyboard-driven popups) — `egui::Window`'s own close button and the
existing per-popup `open` flag are the whole dismiss mechanism, same as
`WorktreesPopupState`/`BranchesPopupState` already use. Nothing else needs
wiring.

### 2.3 `crates/ui/src/command.rs`

Two new `CommandAction` variants and two new `Command` entries, `binding:
None` on both — no reference-IDE precedent for either action, same
reasoning `GitWorktrees`/`ToggleClaudeToolWindow` already state for
themselves:

```rust
Command {
    id: "ManageCustomActions",
    title: "Manage Custom Actions...",
    category: "Run",
    binding: None,
    action: CommandAction::ManageCustomActions,
},
Command {
    id: "ToggleCustomActionsToolWindow",
    title: "Custom Actions",
    category: "Window",
    binding: None,
    action: CommandAction::ToggleCustomActionsToolWindow,
},
```

`category: "Run"` for the Manage command — **not**, as an earlier draft of
this doc incorrectly claimed, because it matches Cargo's commands (those
are actually all filed under `category: "Build"`, verified against
`command.rs`: `CargoBuild`/`CargoRun`/`CargoTest`/`CargoCheck`/
`CargoClippy`/`CargoFmt` are all `"Build"`). The real existing occupants of
`"Run"` are the debug-session actions (`Debug`/`ResumeProgram`/`StepOver`/
`StepInto`/`StepOut`/`ToggleLineBreakpoint`/`StopDebugging`/
`PauseProgram`) — `"Run"` is chosen here because "declare and execute
something" is closer to that category's actual theme (starting/driving an
external execution) than to `"Build"`'s (Cargo's fixed build/lint
subcommands specifically). `category: "Window"` for the toggle, matching
every other `ToggleXToolWindow` entry.

### 2.4 `crates/ui/src/app/render.rs`

The bottom-panel tab row (currently five `render_boxed_tab` calls ending
in `Debug`) gains a sixth, `"Custom Actions"`, and the trailing `match
self.bottom_view { ... }` gains `BottomView::CustomActions =>
self.render_custom_actions_panel(ui)`.

New `render_custom_actions_panel(&mut self, ui: &mut egui::Ui)`: a
click-to-select list (row click sets `self.custom_actions.selected`), a
Run button above the list (disabled while `running.is_some()`, calling
`self.custom_actions.run_selected(root)` — only reachable when
`self.project.is_some()`), an output area below mirroring
`render_cargo_output`'s shape, and an empty-state placeholder when
`actions` is empty. Error output lines (`"... not found on PATH"`/
`"failed to run ..."`) are styled via
`self.theme.tokens().color.danger` — **never** the empty-list placeholder,
the exact same styling-scope mistake `tui-custom-actions.md`'s own code
review caught and fixed on the TUI side, called out here so it isn't
repeated.

New `render_custom_actions_popup(&mut self, ctx: &egui::Context)`, mirroring
`render_worktrees_popup`'s exact structure: `egui::Window::new("Custom
Actions")`, a scrollable list of existing actions (each row: name —
command + args, an Edit button calling `start_editing_custom_action`, a
Delete button calling `delete_custom_action`), a separator, then three
`egui::TextEdit::singleline` fields (Name/Command/Args) bound to
`custom_actions_popup.new_name`/`new_command`/`new_args`, a button
labelled `"Save"` when `editing_index.is_some()` else `"Create"`, calling
`confirm_custom_action_form`, and — mirroring `render_worktrees_popup`'s
own `if let Some(err) = &self.git.worktrees_popup.error { ui.colored_label
(self.theme.tokens().color.danger, err); }` block verbatim — an equivalent
block reading `custom_actions_popup.error`. Wired into the same per-frame
"render every open popup" call site `render_worktrees_popup` already is.

### 2.5 Root `CLAUDE.md` (project-level, orchestrator edit)

Adds one bullet to the existing "Security-sensitive paths" list, adjacent
to `crates/ui/src/cargo_panel.rs`'s entry:

> `crates/ui/src/custom_actions.rs` — shells out to a user-declared
> external command (name/command/args, persisted in
> `.ide/custom_actions.json`); same command-injection/argument-vector
> surface as `cargo_panel.rs`, plus (since T42's hacker pass already
> proved it on the identical file format read by `ide-tui`) an unbounded-
> action-count DoS surface from an untrusted cloned repository's
> `.ide/custom_actions.json`.

## 3. Behaviour

- Running an action never happens as a side effect of `load_project_
  settings`, `App::new`, or opening the Manage popup — only
  `run_selected`'s own call site (the Run button) executes anything.
- At most one action runs at a time; clicking Run while one is already in
  flight is a no-op (same v1 scope as `CargoPanel`/T42 — not re-argued
  here, T42's own `[controversial]` note in `docs/roadmap.md` already
  covers the tradeoff for both frontends since they share the model).
- Deleting the running action's definition via the Manage popup does not
  affect the in-flight run or its streamed output — `running` is a
  clone, not an index (§2.1). Requires a direct test, mirroring
  `tui-custom-actions.md`'s own single most important behavioural
  guarantee.
- `.ide/custom_actions.json` is shared, byte-compatible storage between
  `ide-ui` and `ide-tui` on the same project — an action added/edited in
  one frontend becomes visible in the other the next time *that* frontend
  reloads project settings (GUI: `load_project_settings`, called from
  `open_project`/`create_project`/`restore_last_project`; TUI: its own
  `App::new`, which only runs once per process since `ide-tui` doesn't
  support switching projects mid-session). Neither frontend file-watches
  this specific settings file for live cross-frontend sync — the same
  limitation `custom_languages`/`format_on_save` already have for
  `ide-ui` itself.
- Switching projects (`load_project_settings`) reloads `custom_actions.
  actions` from the new project's own `.ide/custom_actions.json`, but
  does **not** reset `running`/`output`/`rx` — an action started against
  project A keeps streaming and finishes even if the user switches to
  project B mid-run, exactly the existing behaviour `CargoPanel` already
  has (not a new gap this feature introduces; consistent with it rather
  than inventing a different rule).

## 4. Constraints & invariants

- **Never a shell.** `Command::new(&action.command).args(&action.args)` —
  the same non-shell argument-vector construction `cargo_panel.rs` already
  uses, generalized from one fixed subcommand to an arbitrary
  user-typed `Vec<String>`. No string concatenation into a shell command
  anywhere in `custom_actions.rs` or `app.rs`'s new methods.
- **`current_dir` is always `project_root`**, never derived from
  `command`/`args`/`name` text.
- **`MAX_CUSTOM_ACTIONS = 500`**, enforced in `load` (not just at the
  popup's own add path) — the same place `tui-custom-actions.md`'s hacker
  pass required it, since the file can be populated by something other
  than this feature's own write path (e.g. a cloned repository, or the
  TUI's own popup writing more than the GUI ever would through its UI).
- **`editing_index`, when `Some`, is always a valid index** into
  `custom_actions.actions`. Within a project, delete adjusts/clears it in
  the same step. Across a project switch, `load_project_settings` resets
  the entire `custom_actions_popup` (not just `editing_index`) rather than
  trying to re-validate the old index against the new list — see §2.2's
  `load_project_settings` note for why this needs its own explicit reset
  rather than being covered by the delete-time handling alone.
- **JSON field-shape compatibility with `ide-tui`'s `CustomAction`/
  `CustomActionsFile`** (`name`/`command`/`args: Vec<String>`) is a real,
  ongoing constraint — a future change to either frontend's struct shape
  must be mirrored in the other's, or `.ide/custom_actions.json` silently
  stops round-tripping between them. There is no version field or
  negotiation; this is accepted the same way T42 accepted its own lossy
  hand-edited-file join/split limitation, and should be revisited if a
  third consumer of this file ever appears.
- No "Orbit" branding string anywhere in code or UI text (§1).

## 5. Examples

```rust
// custom_actions.rs
let dir = tempfile::tempdir().unwrap();
let actions = vec![CustomAction {
    name: "Run tests".to_string(),
    command: "cargo".to_string(),
    args: vec!["test".to_string()],
}];
save(dir.path(), &actions);
assert_eq!(load(dir.path()), actions);
```

```rust
// CustomActionsPanel::run_selected + poll (mirrors cargo_panel.rs's own
// run_and_poll_streams_stdout_and_stderr_lines test shape)
let mut panel = CustomActionsPanel {
    actions: vec![CustomAction {
        name: "Stream".to_string(),
        command: fixture("streaming_output.sh"),
        args: Vec::new(),
    }],
    selected: 0,
    ..Default::default()
};
panel.run_selected(dir.path());
assert!(panel.running.is_some());
wait_until(|| { panel.poll(); panel.running.is_none() });
assert!(panel.output.contains(&"line1".to_string()));
```

Manage popup flow (description, mirrors `tui-custom-actions.md` §5's own
add/edit/delete walkthrough): open via the command palette
("Manage Custom Actions...") or the Custom Actions dock tab's own
entry point → type Name/Command/Args → Create → new row appears in the
list and in the dock tab below → click Edit on it → fields pre-fill →
change Args → Save (overwrites in place, `editing_index` was `Some`) →
click Delete on a different row → removed immediately, no confirm.

## 6. Dependencies

None new. `std::process`/`std::sync::mpsc`/`std::thread` exactly as
`cargo_panel.rs` already uses. Reuses `ide_core::project_settings::
ProjectSettingsFile::CustomActions`, already created by `rust-core-dev`
for T42 — confirmed present in `crates/core/src/project_settings.rs`, no
`ide-core` work needed this run.

## 7. Diagram

Skipped — one dock tab, one manage popup, one subprocess spawn path,
fully described in prose above; same "too small to benefit" reasoning
`themes.md`/likely `tui-custom-actions.md` already used for a
similarly-scoped change.

## Revision notes

Round 1 `rev` (documentation review) found two real gaps and one factual
error, all fixed in place above rather than in a new file:

1. §2.2 cited `confirm_debug_adapter_config` as the existing GUI precedent
   for silently rejecting invalid form input — that function does not
   exist anywhere in `crates/ui/src/`. The real precedent
   (`add_custom_language`, `app.rs:3662`) shows a visible error instead of
   rejecting silently, and this feature's own TUI half
   (`confirm_action_form`) does too via `self.notify(...)`. Fixed by
   adding `CustomActionsPopupState::error: Option<String>` (mirroring
   `WorktreesPopupState::error`), having `confirm_custom_action_form` set
   it on validation failure instead of silently returning, and having
   `render_custom_actions_popup` render it the same way
   `render_worktrees_popup` renders `worktrees_popup.error`.
2. The stated `editing_index`-always-valid invariant only accounted for
   `delete_custom_action`; it missed that `load_project_settings`
   wholesale-replaces `custom_actions.actions` on every project switch,
   which — with the Manage popup left open mid-edit, a state ide-ui has no
   mechanism to prevent since it has no `close_all_overlays` catch-all —
   left `editing_index` able to point past the end of the new project's
   (shorter) list, panicking on the next Save. Fixed by having
   `load_project_settings` reset the whole `custom_actions_popup` to
   `Default` on every project load, not just patch `editing_index`; §2.2
   and §4 both spell out why the reset needs to be that blunt.
3. §2.3's justification for `category: "Run"` claimed it matched Cargo's
   own commands' category — those are actually all `"Build"`, not `"Run"`
   (verified against `command.rs`). Fixed by correcting the justification
   to reference `"Run"`'s actual existing occupants (the debug-session
   actions) instead; the category choice itself (`"Run"`, not `"Build"`)
   is unchanged.
