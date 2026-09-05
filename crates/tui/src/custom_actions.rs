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

/// One user-declared, named external command. `args` is already a real
/// argv (split once, at save time, in `App::confirm_action_form`) -- never
/// re-split from a raw string at run time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CustomAction {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
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
    pub(crate) selected: usize,
    /// A **clone** of the action currently running, not an index into
    /// `actions` -- deleting or reordering the definition mid-run (via the
    /// Manage popup, independent of this dock tab) must never invalidate
    /// an in-flight run or panic on re-index. Mirrors `CargoPanel::
    /// running: Option<CargoCommand>` (there `Copy`; here `Clone`, since
    /// `CustomAction` holds `String`s).
    pub(crate) running: Option<CustomAction>,
    pub(crate) output: Vec<String>,
    /// `pub(crate)`, not private, only so `App::new`'s `CustomActionsPanel
    /// { actions: ..., ..Default::default() }` struct-update syntax can see
    /// every field from `app.rs` -- never set directly outside this module.
    pub(crate) rx: Option<Receiver<StreamEvent>>,
}

impl CustomActionsPanel {
    /// No-op if `running.is_some()` (v1 scope: at most one in flight,
    /// mirrors `CargoPanel::run`) or if `actions` is empty or `selected`
    /// is out of range.
    pub(crate) fn run_selected(&mut self, project_root: &Path) {
        if self.running.is_some() {
            return;
        }
        let Some(action) = self.actions.get(self.selected).cloned() else {
            return;
        };
        self.output.clear();
        self.rx = Some(subprocess::spawn_streaming(
            &action.command,
            &action.args,
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
        CustomAction {
            name: name.to_string(),
            command: "cargo".to_string(),
            args: vec!["test".to_string()],
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
    fn run_selected_on_an_empty_list_is_a_noop() {
        let mut panel = CustomActionsPanel::default();
        panel.run_selected(Path::new("."));
        assert!(panel.running.is_none());
    }

    #[test]
    fn run_selected_while_already_running_is_a_noop() {
        let mut panel = CustomActionsPanel {
            actions: vec![sample_action("a"), sample_action("b")],
            selected: 1,
            running: Some(sample_action("a")),
            output: vec!["existing".to_string()],
            rx: None,
        };
        panel.run_selected(Path::new("."));
        assert_eq!(panel.running, Some(sample_action("a")));
        assert_eq!(panel.output, vec!["existing".to_string()]);
    }

    #[test]
    fn poll_with_nothing_running_is_a_noop() {
        let mut panel = CustomActionsPanel::default();
        panel.poll();
        assert!(panel.output.is_empty());
        assert!(panel.running.is_none());
    }

    #[test]
    fn run_and_poll_streams_stdout_and_stderr_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = CustomActionsPanel {
            actions: vec![CustomAction {
                name: "Stream".to_string(),
                command: fixture("streaming_output.sh"),
                args: Vec::new(),
            }],
            selected: 0,
            running: None,
            output: Vec::new(),
            rx: None,
        };
        panel.run_selected(dir.path());
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
        let mut panel = CustomActionsPanel {
            actions: vec![CustomAction {
                name: "Stream".to_string(),
                command: fixture("streaming_output.sh"),
                args: Vec::new(),
            }],
            selected: 0,
            running: None,
            output: Vec::new(),
            rx: None,
        };
        panel.run_selected(dir.path());
        let running_before = panel.running.clone();
        assert!(running_before.is_some());

        // Simulates the Manage popup deleting the only defined action
        // while it's running -- `running` is a clone, so this must not
        // disturb it (`docs/features/tui-custom-actions.md` §2.3/§3.1).
        panel.actions.clear();

        wait_until(|| {
            panel.poll();
            panel.running.is_none()
        });

        assert!(panel.output.contains(&"line1".to_string()));
        assert!(panel.actions.is_empty());
    }
}
