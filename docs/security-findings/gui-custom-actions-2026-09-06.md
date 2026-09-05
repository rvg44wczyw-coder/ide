# Security review: GUI Custom Actions (roadmap G8)

## Scope

Reviewed worktree `/Users/ivs/rust/ide-worktrees/rust-ui-dev-gui-custom-actions`,
branch `rust-ui-dev/gui-custom-actions`, commit `a385938`, against
`docs/features/custom-actions.md`. Files: `crates/ui/src/custom_actions.rs`
(new), `crates/ui/src/app.rs` (new methods/fields), `crates/ui/src/app/
render.rs` (new render functions), `crates/ui/src/command.rs`,
`crates/ui/src/app/menu.rs`.

This is the GUI port of `ide-tui`'s already-shipped T42
(`docs/features/tui-custom-actions.md`), which already went through its own
`hacker` pass (`docs/security-findings/tui-custom-actions-2026-09-05.md`)
and had one finding (unbounded action-count DoS), fixed there and carried
into this port from its first draft.

**Attack-surface categories applicable**: Subprocess execution (user-typed
Name/Command/Args → `std::process::Command`), DoS (untrusted
`.ide/custom_actions.json` from a cloned repo). **Ruled out**:
MITM/Replay/Downgrade/KeyConfusion/Timing/WeakRandomness — no network
protocol, no crypto, no key material anywhere in this diff. Metadata
leakage — the subprocess argv is exactly what the user themselves typed
into the Manage popup, not a secret being exposed to another party.
Path traversal — `current_dir` is always `ide_core::Project::root()`, never
built from typed text; the `.ide/custom_actions.json` file path itself goes
through `ide_core::project_settings` (already reviewed separately, not
re-litigated here).

**Live tests actually run**: `cargo test -p ide-ui --lib custom_actions::`
with `--nocapture`, all 12 tests, including the truncation test (writes a
real 550-action oversized JSON file to a real tempdir, loads it through the
real `load()` function, asserts truncation to exactly `MAX_CUSTOM_ACTIONS`
= 500) and the delete-mid-run test (spawns a real subprocess via the
`streaming_output.sh` fixture, deletes the definition from `actions` while
it's still running, asserts the run completes normally with the expected
output and no panic). Everything else below is code-analysis (tracing call
sites by hand), since `custom_actions.rs`'s `pub(crate)` items aren't
reachable from an external test harness and this skill's rules forbid
adding a new test file to the repo to work around that — noted explicitly
rather than silently passing this off as more "live" than it is.

## Findings

1. **[InputValidation, Low]** `crates/ui/src/app.rs`: `delete_custom_action`
   only clears `custom_actions_popup.editing_index` when it **exactly**
   equals the deleted index (`if self.custom_actions_popup.editing_index ==
   Some(index)`). It doesn't account for `Vec::remove` shifting every
   later index down by one. Concrete scenario: actions = `[A, B, C]`.
   User clicks Edit on `C` (index 2) → `editing_index = Some(2)`, the form
   is populated with `C`'s values. Still in the same popup session (no
   reload in between), the user clicks Delete on `A` (index 0) — a
   different row, both buttons are simultaneously reachable in
   `render_custom_actions_popup`'s per-row Edit/Delete buttons.
   `delete_custom_action(0)` removes `A`; `actions` becomes `[B, C]`
   (length 2). `editing_index` stays `Some(2)` since `0 != 2`. If the user
   now clicks Save, `confirm_custom_action_form`'s match arm `Some(i) if i
   < self.custom_actions.actions.len()` evaluates `2 < 2 = false`, so it
   falls through to `_ => push` — **no panic** (the bounds check is
   correct and does its job), but the result is a silent logic error: the
   user believes they're overwriting `C` in place, and instead a brand
   new duplicate-ish entry is appended with `C`'s (possibly edited) values,
   while the real `C` (now at index 1) is left untouched. No crash, no data
   leak, no cross-project or cross-user impact — this is local single-user
   UI state — but it is a real, easily-reachable correctness bug via
   ordinary use (not just adversarial input), and it's worth fixing
   because "Save silently does the wrong thing" is a worse failure mode
   than a bounds-checked panic would have been. Verified by tracing the
   exact code path in `app.rs` (`delete_custom_action`,
   `confirm_custom_action_form`) and `render.rs`
   (`render_custom_actions_popup`'s independent per-row Edit/Delete
   buttons) — not caught by the existing test suite, which never exercises
   Edit-then-Delete-a-different-row-then-Save in one session. Suggested
   fix direction: either disable a row's Delete button while
   `editing_index.is_some()` (simplest — the TUI's own modal single-field
   popup makes this scenario structurally impossible there, which is
   presumably why T42 never needed this), or have `delete_custom_action`
   decrement `editing_index` by one whenever the deleted index is strictly
   less than it (mirrors how many list-editing UIs keep a selection stable
   across a deletion before it).

## Not re-litigated (inherited unchanged from T42, already accepted there)

- `crates/ui/src/custom_actions.rs::load` truncates to `MAX_CUSTOM_ACTIONS`
  = 500 **after** `project_settings::read` has already fully deserialized
  the file via `serde_json` — an attacker-controlled `.ide/
  custom_actions.json` could still cost parse-time/memory proportional to
  the file's actual size before truncation ever runs, not just
  render-time (which is what T42's own hacker pass measured and fixed:
  ~100ms/frame at 1,000,000 actions while the tab stays open). This
  parse-before-truncate structure is identical to `ide-tui`'s own already-
  reviewed `custom_actions.rs::load`, carried over unchanged by this GUI
  port rather than introduced by it — flagging for visibility, not as a
  new finding against this diff, since re-opening an already-accepted
  design decision from a prior review isn't this pass's job unless the
  port changed it (it didn't).
- The general risk profile of "a user-declared action can be an arbitrary
  binary with arbitrary args, running with the full privilege of the IDE
  process" is identical to `cargo_panel.rs`'s own already-accepted scope
  (per `CLAUDE.md`'s own bullet for that file) and isn't new here.

## Verified clean (live where noted, code-analysis otherwise)

- No shell anywhere: the only `Command::new` call site in the entire diff
  is `custom_actions.rs`'s `run_and_stream`, using `.args(args)` — grepped
  the full diff for `format!` calls and confirmed none build a command
  string; the new `app.rs` methods never call `Command::new` at all.
- `current_dir` traced end to end: `render_custom_actions_panel`'s Run
  button → `self.project.as_ref().map(|p| p.root().to_path_buf())` (an
  `ide-core` API, not derived from typed text) → `run_selected(root)` →
  `spawn_streaming(&action.command, &action.args, project_root)` →
  `.current_dir(project_root)`. Never the typed Command/Args/Name text.
- No side-effect execution: grepped every call site of `run_selected`/
  `spawn_streaming` across the diff — exactly one production call site
  (`render.rs`'s Run-button `.clicked()` block). `load_project_settings`'s
  new lines only call `crate::custom_actions::load` (a pure file read) and
  reset `custom_actions_popup` to `Default`; `open_custom_actions_popup`
  only sets struct fields.
- `MAX_CUSTOM_ACTIONS` truncation: live-verified via
  `load_truncates_a_maliciously_oversized_file_to_max_custom_actions`.
- `running: Option<CustomAction>` clone-not-index invariant: live-verified
  via `deleting_the_running_actions_definition_does_not_affect_the_in_
  flight_run` — a real subprocess kept streaming and completed normally
  after its definition was deleted from `actions`.
- Project-switch `editing_index` panic prevention: `load_project_settings`
  resets `custom_actions_popup` to `Default` wholesale (verified in code
  and via the existing `load_project_settings_resets_the_custom_actions_
  popup_on_project_switch` test), which sets `editing_index` to `None`.
  Additionally traced `confirm_custom_action_form`'s match arm (`Some(i)
  if i < actions.len()`) and confirmed it's bounds-checked regardless —
  even without the reset, this specific arm can't index out of bounds; the
  reset's actual job is preventing the *other* bug (silently overwriting
  the wrong project's array slot), which it does correctly.

## Verdict

Findings (Low).

`CHAIN_STEP step=hacker result=findings severity=Low doc="docs/security-findings/gui-custom-actions-2026-09-06.md"`
