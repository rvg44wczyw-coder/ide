//! User-declared named external commands, invocable from the Custom
//! Actions dock tab (`docs/features/tui-custom-actions.md`, `T42`).
//! Persisted per-project via `ide_core::project_settings`'s `CustomActions`
//! slot (`.ide/custom_actions.json`), the per-project counterpart to
//! `state.rs`'s global `~/.config/ide-tui/state.json` -- same split
//! `project_state.rs` already establishes for navigation state.

use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};

use ide_core::project_settings::{self, ProjectSettingsFile};
use serde::{Deserialize, Serialize};

use crate::subprocess::{self, StreamEvent};

/// Hard cap on how many actions `load` will hand back, mirroring
/// `docker_panel.rs`'s `MAX_DOCKER_LIST_ITEMS`/`k8s_panel.rs`'s
/// `MAX_K8S_LIST_ITEMS` (500) -- `.ide/custom_actions.json` is untrusted
/// input the moment it can arrive via a cloned repository (`docs/features/
/// tui-custom-actions.md` §4), and without this cap a crafted file with an
/// extreme action count would make `render_custom_actions_panel`/
/// `render_manage_actions_popup` rebuild an unbounded `Vec<ListItem>`
/// every single frame the Custom Actions dock tab or Manage popup is open
/// -- confirmed live (`docs/security-findings/
/// tui-custom-actions-2026-09-05.md`, finding 1) to cost ~100ms/frame at
/// 1,000,000 actions, i.e. a real, sustained UI hang, not a one-time cost.
/// Applied at *load* time (unlike `project_state.rs`'s `MAX_RECENT_FILES`,
/// enforced only at write time) since this file can be populated by
/// something other than this feature's own write path.
const MAX_CUSTOM_ACTIONS: usize = 500;

/// Which edge of the screen a `CustomAction` is bound to (`docs/features/
/// tui-custom-actions-edge-slots.md` §2.1, T47) -- the mockup's own
/// vocabulary, replacing `T42`'s single flat list. `Default` is `Bottom`,
/// the least surprising landing spot for a freshly-created action (where
/// `T42`'s one-and-only list used to live). `Ribbon` (`docs/features/
/// tui-key-hint-ribbon.md` §2.1, T48) is the persistent bottom key-hint
/// ribbon's own bind point -- mouse-only like `Top`/`Outline`, added last
/// in `ALL`/`next()`'s cycle since `Bottom` was already the established
/// default before this variant existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) enum ActionSlot {
    Top,
    Tree,
    Outline,
    #[default]
    Bottom,
    Ribbon,
}

impl ActionSlot {
    pub(crate) const ALL: [ActionSlot; 5] = [
        ActionSlot::Top,
        ActionSlot::Tree,
        ActionSlot::Outline,
        ActionSlot::Bottom,
        ActionSlot::Ribbon,
    ];

    pub(crate) fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn index(self) -> usize {
        match self {
            ActionSlot::Top => 0,
            ActionSlot::Tree => 1,
            ActionSlot::Outline => 2,
            ActionSlot::Bottom => 3,
            ActionSlot::Ribbon => 4,
        }
    }
}

/// What running a `CustomAction` actually does (`docs/features/
/// tui-custom-actions-edge-slots.md` §2.1/§3.2, T47). `External` is `T42`'s
/// original (and only) shape, unchanged. `Builtin` references an existing
/// `Command::id` -- **not** a second, parallel list of runnable things --
/// so running one never spawns a subprocess; `App::run_custom_action`
/// resolves the id against `commands()` and calls the exact same
/// `run_action` every keybinding/palette/colon-command row already calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub(crate) enum CustomActionKind {
    External { command: String, args: Vec<String> },
    Builtin { command_id: String },
}

/// One user-declared custom action. `args` (when `kind` is `External`) is
/// already a real argv (split once, at save time, in `App::
/// confirm_action_form`) -- never re-split from a raw string at run time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CustomAction {
    pub(crate) name: String,
    pub(crate) slot: ActionSlot,
    pub(crate) kind: CustomActionKind,
}

/// The `.ide/custom_actions.json` payload shape -- a struct, not a bare
/// `Vec<CustomAction>`, so a future field doesn't need a breaking
/// top-level shape change.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CustomActionsFile {
    pub(crate) actions: Vec<CustomAction>,
}

/// Best-effort load: missing file, malformed JSON, or a `.ide/` symlink
/// escape all collapse to an empty list -- the same fail-open posture
/// `project_state::load`/`state::load` already establish. Never blocks
/// `App::new`.
pub(crate) fn load(project_root: &Path) -> Vec<CustomAction> {
    let mut actions = project_settings::read::<CustomActionsFile>(
        project_root,
        ProjectSettingsFile::CustomActions,
    )
    .ok()
    .flatten()
    .unwrap_or_default()
    .actions;
    actions.truncate(MAX_CUSTOM_ACTIONS);
    actions
}

/// Best-effort save -- swallows every failure the same way
/// `project_state::save` does; losing a just-added action to a permission
/// error is a worse UX than a silent no-op, but never worth crashing an
/// editing session over.
pub(crate) fn save(project_root: &Path, actions: &[CustomAction]) {
    let _ = project_settings::write(
        project_root,
        ProjectSettingsFile::CustomActions,
        &CustomActionsFile {
            actions: actions.to_vec(),
        },
    );
}

/// The Custom Actions dock tab's always-alive state (`docs/features/
/// tui-tool-window-docking.md` §2.1 pattern) -- `actions` is the one
/// in-memory copy the Manage popup also reads/writes directly; the dock
/// tab and the popup are two views over the same `Vec`, not two copies
/// that need syncing.
#[derive(Default)]
pub(crate) struct CustomActionsPanel {
    pub(crate) actions: Vec<CustomAction>,
    /// One selection cursor per `ActionSlot` (`docs/features/
    /// tui-custom-actions-edge-slots.md` §2.2, T47), indexed by
    /// `ActionSlot::index()` -- was a single `selected: usize` before this
    /// doc; each slot's own filtered view (`actions_for_slot`) can have a
    /// different length, so one shared cursor would mean different things
    /// in each. Widened to `[usize; 5]` for `Ribbon` (`docs/features/
    /// tui-key-hint-ribbon.md` §2.1, T48) -- `Ribbon` is mouse-only, so
    /// nothing actually drives that slot's cursor, but the array must stay
    /// indexable by every `ActionSlot::index()` value to keep `selected`
    /// panic-free.
    pub(crate) selected: [usize; 5],
    /// A **clone** of the action currently running, not an index into
    /// `actions` -- deleting or reordering the definition mid-run (via the
    /// Manage popup, independent of this dock tab) must never invalidate
    /// an in-flight run or panic on re-index. Mirrors `CargoPanel::
    /// running: Option<CargoCommand>` (there `Copy`; here `Clone`, since
    /// `CustomAction` holds `String`s).
    pub(crate) running: Option<CustomAction>,
    pub(crate) output: Vec<String>,
    /// Lines scrolled back from the live tail of `output` (`docs/features/
    /// tui-panel-pane-scroll.md` §2.2, T53) -- same tail-anchored shape as
    /// `CargoPanel::output_scroll`, reset to `0` by `run` the same way.
    pub(crate) output_scroll: u16,
    /// `pub(crate)`, not private, only so `App::new`'s `CustomActionsPanel
    /// { actions: ..., ..Default::default() }` struct-update syntax can see
    /// every field from `app.rs` -- never set directly outside this module.
    pub(crate) rx: Option<Receiver<StreamEvent>>,
}

impl CustomActionsPanel {
    /// Every action bound to `slot`, in `actions`' own order -- owned
    /// clones, not references: these lists are tiny (bounded by
    /// `MAX_CUSTOM_ACTIONS` across *all* slots combined) and every caller
    /// needs an owned value anyway (to hand to `run` or to `App::
    /// run_custom_action`), so cloning here avoids fighting borrow
    /// lifetimes at every call site for no real cost.
    pub(crate) fn actions_for_slot(&self, slot: ActionSlot) -> Vec<CustomAction> {
        self.actions
            .iter()
            .filter(|a| a.slot == slot)
            .cloned()
            .collect()
    }

    pub(crate) fn selected(&self, slot: ActionSlot) -> usize {
        self.selected[slot.index()]
    }

    /// Moves `slot`'s own cursor by `delta` (`+1`/`-1`), clamped to
    /// `[0, actions_for_slot(slot).len().saturating_sub(1)]` -- same
    /// clamp-not-wrap convention every other list cursor in this crate
    /// uses (`GoToFileState::selected`, `PaletteState::selected`, ...).
    pub(crate) fn move_selection(&mut self, slot: ActionSlot, delta: i32) {
        let len = self.actions_for_slot(slot).len();
        let current = self.selected(slot) as i32;
        let next = (current + delta).clamp(0, len.saturating_sub(1) as i32);
        self.selected[slot.index()] = next as usize;
    }

    /// Spawns `action`'s subprocess. No-op if already `running.is_some()`
    /// (v1 scope: at most one in flight, mirrors `CargoPanel::run`) --
    /// **or** if `action.kind` is `Builtin`: that is a caller bug, not a
    /// reachable user-facing state (`App::run_custom_action` is
    /// responsible for routing `Builtin` through `run_action` before ever
    /// reaching here, `docs/features/tui-custom-actions-edge-slots.md`
    /// §3.2), so this mirrors this crate's existing "defensive no-op on a
    /// should-never-happen state" convention rather than `panic!`.
    pub(crate) fn run(&mut self, project_root: &Path, action: CustomAction) {
        if self.running.is_some() {
            return;
        }
        let CustomActionKind::External { command, args } = &action.kind else {
            return;
        };
        self.output.clear();
        self.output_scroll = 0;
        self.rx = Some(subprocess::spawn_streaming(
            command,
            args,
            Some(project_root),
        ));
        self.running = Some(action);
    }

    /// Identical shape to `CargoPanel::poll` -- call once per loop
    /// iteration regardless of dock visibility, so a running action keeps
    /// streaming even while the dock tab isn't the visible one.
    pub(crate) fn poll(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(StreamEvent::Line(line)) => self.output.push(line),
                Ok(StreamEvent::Done) => {
                    self.running = None;
                    self.rx = None;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.running = None;
                    self.rx = None;
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn wait_until<F: FnMut() -> bool>(mut condition: F) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "condition did not become true in time"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn sample_action(name: &str) -> CustomAction {
        external_action(name, ActionSlot::Bottom)
    }

    fn external_action(name: &str, slot: ActionSlot) -> CustomAction {
        CustomAction {
            name: name.to_string(),
            slot,
            kind: CustomActionKind::External {
                command: "cargo".to_string(),
                args: vec!["test".to_string()],
            },
        }
    }

    fn builtin_action(name: &str, slot: ActionSlot, command_id: &str) -> CustomAction {
        CustomAction {
            name: name.to_string(),
            slot,
            kind: CustomActionKind::Builtin {
                command_id: command_id.to_string(),
            },
        }
    }

    #[test]
    fn load_from_a_fresh_project_returns_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), Vec::new());
    }

    #[test]
    fn save_then_load_round_trips_the_actions() {
        let dir = tempfile::tempdir().unwrap();
        let actions = vec![sample_action("Run tests"), sample_action("Run clippy")];
        save(dir.path(), &actions);
        assert_eq!(load(dir.path()), actions);
    }

    #[test]
    fn load_truncates_a_maliciously_oversized_file_to_max_custom_actions() {
        let dir = tempfile::tempdir().unwrap();
        let ide_dir = dir.path().join(".ide");
        std::fs::create_dir_all(&ide_dir).unwrap();
        let oversized = CustomActionsFile {
            actions: (0..MAX_CUSTOM_ACTIONS + 50)
                .map(|i| sample_action(&format!("action-{i}")))
                .collect(),
        };
        std::fs::write(
            ide_dir.join("custom_actions.json"),
            serde_json::to_vec(&oversized).unwrap(),
        )
        .unwrap();

        let loaded = load(dir.path());
        assert_eq!(loaded.len(), MAX_CUSTOM_ACTIONS);
        assert_eq!(loaded[0].name, "action-0");
    }

    #[test]
    fn load_on_malformed_json_returns_an_empty_list_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let ide_dir = dir.path().join(".ide");
        std::fs::create_dir_all(&ide_dir).unwrap();
        std::fs::write(ide_dir.join("custom_actions.json"), "{ not json").unwrap();
        assert_eq!(load(dir.path()), Vec::new());
    }

    #[test]
    fn run_while_already_running_is_a_noop() {
        let mut panel = CustomActionsPanel {
            actions: vec![sample_action("a"), sample_action("b")],
            selected: [0; 5],
            running: Some(sample_action("a")),
            output: vec!["existing".to_string()],
            output_scroll: 0,
            rx: None,
        };
        panel.run(Path::new("."), sample_action("b"));
        assert_eq!(panel.running, Some(sample_action("a")));
        assert_eq!(panel.output, vec!["existing".to_string()]);
    }

    #[test]
    fn run_resets_output_scroll_to_zero() {
        let mut panel = CustomActionsPanel {
            output_scroll: 7,
            ..Default::default()
        };
        panel.run(Path::new("."), sample_action("a"));
        assert_eq!(panel.output_scroll, 0);
    }

    #[test]
    fn run_on_a_builtin_action_is_a_noop_caller_bug_guard() {
        // `App::run_custom_action` is responsible for never calling `run`
        // with a `Builtin` action -- this just confirms the defensive
        // no-op holds if that invariant is ever violated.
        let mut panel = CustomActionsPanel::default();
        panel.run(
            Path::new("."),
            builtin_action("b", ActionSlot::Bottom, "Exit"),
        );
        assert!(panel.running.is_none());
        assert!(panel.rx.is_none());
    }

    #[test]
    fn actions_for_slot_filters_by_slot() {
        let panel = CustomActionsPanel {
            actions: vec![
                external_action("top1", ActionSlot::Top),
                external_action("bottom1", ActionSlot::Bottom),
                external_action("top2", ActionSlot::Top),
            ],
            ..Default::default()
        };
        let top = panel.actions_for_slot(ActionSlot::Top);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].name, "top1");
        assert_eq!(top[1].name, "top2");
        assert_eq!(panel.actions_for_slot(ActionSlot::Tree).len(), 0);
    }

    #[test]
    fn move_selection_clamps_per_slot_independently() {
        let mut panel = CustomActionsPanel {
            actions: vec![
                external_action("top1", ActionSlot::Top),
                external_action("bottom1", ActionSlot::Bottom),
                external_action("bottom2", ActionSlot::Bottom),
            ],
            ..Default::default()
        };
        panel.move_selection(ActionSlot::Top, 1);
        assert_eq!(panel.selected(ActionSlot::Top), 0); // only 1 row, clamps
        panel.move_selection(ActionSlot::Bottom, 1);
        assert_eq!(panel.selected(ActionSlot::Bottom), 1);
        panel.move_selection(ActionSlot::Bottom, 1);
        assert_eq!(panel.selected(ActionSlot::Bottom), 1); // clamps at len-1
        panel.move_selection(ActionSlot::Bottom, -5);
        assert_eq!(panel.selected(ActionSlot::Bottom), 0);
    }

    #[test]
    fn action_slot_next_cycles_through_all_five_and_wraps() {
        assert_eq!(ActionSlot::Top.next(), ActionSlot::Tree);
        assert_eq!(ActionSlot::Tree.next(), ActionSlot::Outline);
        assert_eq!(ActionSlot::Outline.next(), ActionSlot::Bottom);
        assert_eq!(ActionSlot::Bottom.next(), ActionSlot::Ribbon);
        assert_eq!(ActionSlot::Ribbon.next(), ActionSlot::Top);
    }

    #[test]
    fn action_slot_all_lists_every_variant_once_in_next_order() {
        for slot in ActionSlot::ALL {
            assert_eq!(ActionSlot::ALL.iter().filter(|s| **s == slot).count(), 1);
        }
        for i in 0..ActionSlot::ALL.len() {
            let next_index = (i + 1) % ActionSlot::ALL.len();
            assert_eq!(ActionSlot::ALL[i].next(), ActionSlot::ALL[next_index]);
        }
    }

    #[test]
    fn poll_with_nothing_running_is_a_noop() {
        let mut panel = CustomActionsPanel::default();
        panel.poll();
        assert!(panel.output.is_empty());
        assert!(panel.running.is_none());
    }

    fn streaming_action(name: &str) -> CustomAction {
        CustomAction {
            name: name.to_string(),
            slot: ActionSlot::Bottom,
            kind: CustomActionKind::External {
                command: fixture("streaming_output.sh"),
                args: Vec::new(),
            },
        }
    }

    #[test]
    fn run_and_poll_streams_stdout_and_stderr_lines() {
        let dir = tempfile::tempdir().unwrap();
        let action = streaming_action("Stream");
        let mut panel = CustomActionsPanel {
            actions: vec![action.clone()],
            ..Default::default()
        };
        panel.run(dir.path(), action);
        assert!(panel.running.is_some());

        wait_until(|| {
            panel.poll();
            panel.running.is_none()
        });

        assert!(panel.output.contains(&"line1".to_string()));
        assert!(panel.output.contains(&"err1".to_string()));
        assert!(panel.output.contains(&"line2".to_string()));
    }

    #[test]
    fn deleting_the_running_actions_definition_does_not_affect_the_in_flight_run() {
        let dir = tempfile::tempdir().unwrap();
        let action = streaming_action("Stream");
        let mut panel = CustomActionsPanel {
            actions: vec![action.clone()],
            ..Default::default()
        };
        panel.run(dir.path(), action);
        let running_before = panel.running.clone();
        assert!(running_before.is_some());

        // Simulates the Manage popup deleting the only defined action
        // while it's running -- `running` is a clone, so this must not
        // disturb it (`docs/features/tui-custom-actions-edge-slots.md` §3.1).
        panel.actions.clear();

        wait_until(|| {
            panel.poll();
            panel.running.is_none()
        });

        assert!(panel.output.contains(&"line1".to_string()));
        assert!(panel.actions.is_empty());
    }
}
