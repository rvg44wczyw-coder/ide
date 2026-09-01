//! Docker containers/images panel (`docs/features/
//! tui-docker-and-kubernetes.md` §2.2/§3.1/§3.3/§3.4/§4) -- shells out to
//! the `docker` CLI the user already has installed, the same "drive the
//! real tool" approach `cargo_panel.rs` already established.

use crate::subprocess::{spawn_streaming, spawn_to_completion, StreamEvent};
use std::sync::mpsc::Receiver;

/// Not a security boundary (`docker`'s output is the user's own daemon,
/// not attacker-controlled) -- a real host can legitimately run/have
/// pulled more than this, and rendering an unbounded list into a
/// fixed-height terminal widget is a genuine usability concern on its own
/// (`docs/features/tui-docker-and-kubernetes.md` §4).
pub(crate) const MAX_DOCKER_LIST_ITEMS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct DockerContainer {
    #[serde(rename = "ID")]
    pub(crate) id: String,
    #[serde(rename = "Names")]
    pub(crate) names: String,
    #[serde(rename = "Image")]
    pub(crate) image: String,
    #[serde(rename = "Status")]
    pub(crate) status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct DockerImage {
    #[serde(rename = "ID")]
    pub(crate) id: String,
    #[serde(rename = "Repository")]
    pub(crate) repository: String,
    #[serde(rename = "Tag")]
    pub(crate) tag: String,
    #[serde(rename = "Size")]
    pub(crate) size: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DockerTab {
    #[default]
    Containers,
    Images,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockerLifecycleAction {
    Start,
    Stop,
    Restart,
    Remove,
}

impl DockerLifecycleAction {
    pub(crate) fn subcommand(self) -> &'static str {
        match self {
            DockerLifecycleAction::Start => "start",
            DockerLifecycleAction::Stop => "stop",
            DockerLifecycleAction::Restart => "restart",
            DockerLifecycleAction::Remove => "rm",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            DockerLifecycleAction::Start => "Start",
            DockerLifecycleAction::Stop => "Stop",
            DockerLifecycleAction::Restart => "Restart",
            DockerLifecycleAction::Remove => "Remove",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerConfirm {
    pub(crate) action: DockerLifecycleAction,
    pub(crate) container_id: String,
    pub(crate) container_name: String,
}

enum InFlight {
    Refresh,
    Logs,
    Lifecycle,
}

/// Pure so `confirm_yes`'s argv shape (subcommand, then the container id
/// as one single element -- never concatenated) is directly testable
/// without spawning a real process.
fn lifecycle_args(confirm: &DockerConfirm) -> Vec<String> {
    vec![
        confirm.action.subcommand().to_string(),
        confirm.container_id.clone(),
    ]
}

#[derive(Default)]
pub(crate) struct DockerPanel {
    pub(crate) tab: DockerTab,
    pub(crate) containers: Vec<DockerContainer>,
    pub(crate) images: Vec<DockerImage>,
    pub(crate) truncated: bool,
    pub(crate) selected: usize,
    in_flight: Option<InFlight>,
    stream_rx: Option<Receiver<StreamEvent>>,
    once_rx: Option<Receiver<(Vec<String>, bool)>>,
    pub(crate) logs: Vec<String>,
    pub(crate) logs_for: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) confirm: Option<DockerConfirm>,
}

impl DockerPanel {
    /// No-op if a request is already in flight -- at most one `docker`
    /// invocation from this panel runs at a time.
    pub(crate) fn refresh(&mut self) {
        if self.in_flight.is_some() {
            return;
        }
        // Deliberately *not* cleared here -- whatever error is currently
        // showing (e.g. a failed lifecycle action that triggered this
        // very refresh) must stay visible until this refresh's own
        // result actually lands (`apply_refresh_result` updates it then,
        // one way or the other), not vanish the instant a new request is
        // merely sent.
        let args = match self.tab {
            DockerTab::Containers => vec![
                "ps".to_string(),
                "-a".to_string(),
                "--format".to_string(),
                "{{json .}}".to_string(),
            ],
            DockerTab::Images => vec![
                "images".to_string(),
                "--format".to_string(),
                "{{json .}}".to_string(),
            ],
        };
        self.in_flight = Some(InFlight::Refresh);
        self.once_rx = Some(spawn_to_completion("docker", &args, None));
    }

    pub(crate) fn fetch_logs(&mut self, container_id: &str) {
        if self.in_flight.is_some() {
            return;
        }
        self.in_flight = Some(InFlight::Logs);
        self.logs.clear();
        self.logs_for = Some(container_id.to_string());
        let args = vec![
            "logs".to_string(),
            "--tail".to_string(),
            "200".to_string(),
            container_id.to_string(),
        ];
        self.stream_rx = Some(spawn_streaming("docker", &args, None));
    }

    pub(crate) fn request_lifecycle_action(
        &mut self,
        action: DockerLifecycleAction,
        container_id: String,
        container_name: String,
    ) {
        self.confirm = Some(DockerConfirm {
            action,
            container_id,
            container_name,
        });
    }

    pub(crate) fn confirm_no(&mut self) {
        self.confirm = None;
    }

    /// The confirm popup's "yes": actually runs `docker <subcommand>
    /// <id>` -- nothing was sent before this call.
    pub(crate) fn confirm_yes(&mut self) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        if self.in_flight.is_some() {
            // Extremely unlikely (would need another request to have
            // started in the single frame between the popup rendering
            // and this key handler running) but never silently drop the
            // pending action -- put it back and let the next attempt
            // retry once the in-flight request finishes.
            self.confirm = Some(confirm);
            return;
        }
        self.in_flight = Some(InFlight::Lifecycle);
        let args = lifecycle_args(&confirm);
        self.once_rx = Some(spawn_to_completion("docker", &args, None));
    }

    /// Call once per loop iteration while the panel is open.
    pub(crate) fn poll(&mut self) {
        if let Some(rx) = &self.stream_rx {
            loop {
                match rx.try_recv() {
                    Ok(StreamEvent::Line(line)) => self.logs.push(line),
                    Ok(StreamEvent::Done) => {
                        self.in_flight = None;
                        self.stream_rx = None;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.in_flight = None;
                        self.stream_rx = None;
                        break;
                    }
                }
            }
        }
        if let Some(rx) = &self.once_rx {
            match rx.try_recv() {
                Ok((lines, success)) => {
                    let was_refresh = matches!(self.in_flight, Some(InFlight::Refresh));
                    let was_lifecycle = matches!(self.in_flight, Some(InFlight::Lifecycle));
                    self.in_flight = None;
                    self.once_rx = None;
                    if was_refresh {
                        self.apply_refresh_result(lines, success);
                    } else if was_lifecycle {
                        if !success {
                            self.error = Some(lines.join("\n"));
                        }
                        self.refresh();
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.in_flight = None;
                    self.once_rx = None;
                }
            }
        }
    }

    fn apply_refresh_result(&mut self, lines: Vec<String>, success: bool) {
        if !success {
            self.error = Some(lines.join("\n"));
            return;
        }
        self.error = None;
        self.truncated = false;
        match self.tab {
            DockerTab::Containers => {
                self.containers.clear();
                for line in &lines {
                    if self.containers.len() >= MAX_DOCKER_LIST_ITEMS {
                        self.truncated = true;
                        break;
                    }
                    if let Ok(container) = serde_json::from_str::<DockerContainer>(line) {
                        self.containers.push(container);
                    }
                }
            }
            DockerTab::Images => {
                self.images.clear();
                for line in &lines {
                    if self.images.len() >= MAX_DOCKER_LIST_ITEMS {
                        self.truncated = true;
                        break;
                    }
                    if let Ok(image) = serde_json::from_str::<DockerImage>(line) {
                        self.images.push(image);
                    }
                }
            }
        }
        self.selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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

    #[test]
    fn default_tab_is_containers() {
        assert_eq!(DockerPanel::default().tab, DockerTab::Containers);
    }

    #[test]
    fn lifecycle_action_maps_to_the_expected_subcommand() {
        assert_eq!(DockerLifecycleAction::Start.subcommand(), "start");
        assert_eq!(DockerLifecycleAction::Stop.subcommand(), "stop");
        assert_eq!(DockerLifecycleAction::Restart.subcommand(), "restart");
        assert_eq!(DockerLifecycleAction::Remove.subcommand(), "rm");
    }

    #[test]
    fn request_lifecycle_action_only_opens_the_popup_sends_nothing() {
        let mut panel = DockerPanel::default();
        panel.request_lifecycle_action(
            DockerLifecycleAction::Stop,
            "abc123".to_string(),
            "web".to_string(),
        );
        assert!(panel.confirm.is_some());
        assert!(panel.in_flight.is_none());
        assert!(panel.once_rx.is_none());
    }

    #[test]
    fn confirm_no_discards_the_pending_action_without_running_anything() {
        let mut panel = DockerPanel::default();
        panel.request_lifecycle_action(
            DockerLifecycleAction::Remove,
            "abc123".to_string(),
            "web".to_string(),
        );
        panel.confirm_no();
        assert!(panel.confirm.is_none());
        assert!(panel.in_flight.is_none());
    }

    #[test]
    fn confirm_yes_with_no_pending_confirm_is_a_noop() {
        let mut panel = DockerPanel::default();
        panel.confirm_yes();
        assert!(panel.in_flight.is_none());
    }

    #[test]
    fn refresh_while_already_in_flight_is_a_noop() {
        let mut panel = DockerPanel {
            in_flight: Some(InFlight::Refresh),
            ..Default::default()
        };
        panel.refresh();
        assert!(panel.once_rx.is_none());
    }

    #[test]
    fn refresh_with_a_missing_docker_binary_surfaces_an_error() {
        let mut panel = DockerPanel {
            once_rx: Some(spawn_to_completion(
                "definitely-not-a-real-docker-xyz",
                &["ps".to_string()],
                None,
            )),
            in_flight: Some(InFlight::Refresh),
            ..Default::default()
        };
        wait_until(|| {
            panel.poll();
            panel.in_flight.is_none()
        });
        assert!(panel.error.is_some());
        assert!(panel.containers.is_empty());
    }

    #[test]
    fn apply_refresh_result_parses_container_json_lines_and_skips_malformed_ones() {
        let mut panel = DockerPanel::default();
        let lines = vec![
            r#"{"ID":"abc123","Names":"web","Image":"nginx","Status":"Up 2 hours"}"#.to_string(),
            "not json at all".to_string(),
            r#"{"ID":"def456","Names":"db","Image":"postgres","Status":"Exited (0)"}"#.to_string(),
        ];
        panel.apply_refresh_result(lines, true);
        assert_eq!(panel.containers.len(), 2);
        assert_eq!(panel.containers[0].names, "web");
        assert_eq!(panel.containers[1].names, "db");
        assert!(!panel.truncated);
    }

    #[test]
    fn apply_refresh_result_truncates_at_the_cap_and_sets_truncated() {
        let mut panel = DockerPanel::default();
        let lines: Vec<String> = (0..(MAX_DOCKER_LIST_ITEMS + 50))
            .map(|i| format!(r#"{{"ID":"{i}","Names":"c{i}","Image":"x","Status":"Up"}}"#))
            .collect();
        panel.apply_refresh_result(lines, true);
        assert_eq!(panel.containers.len(), MAX_DOCKER_LIST_ITEMS);
        assert!(panel.truncated);
    }

    #[test]
    fn apply_refresh_result_with_failure_sets_error_and_leaves_list_untouched() {
        let mut panel = DockerPanel {
            containers: vec![DockerContainer {
                id: "x".to_string(),
                names: "old".to_string(),
                image: "img".to_string(),
                status: "Up".to_string(),
            }],
            ..Default::default()
        };
        panel.apply_refresh_result(vec!["docker: connection refused".to_string()], false);
        assert!(panel.error.is_some());
        assert_eq!(panel.containers.len(), 1);
        assert_eq!(panel.containers[0].names, "old");
    }

    #[test]
    fn images_tab_parses_docker_image_json() {
        let mut panel = DockerPanel {
            tab: DockerTab::Images,
            ..Default::default()
        };
        let lines =
            vec![r#"{"ID":"img1","Repository":"nginx","Tag":"latest","Size":"142MB"}"#.to_string()];
        panel.apply_refresh_result(lines, true);
        assert_eq!(panel.images.len(), 1);
        assert_eq!(panel.images[0].repository, "nginx");
    }

    #[test]
    fn fetch_logs_streams_output_and_sets_logs_for() {
        let dir = tempfile::tempdir().unwrap();
        // Reuse the same `cat`-style stand-in convention this crate's
        // other panels already use for a real, always-spawnable process --
        // exercised through `poll()`/`stream_rx` directly rather than
        // `docker` itself (not installed in this environment).
        let mut panel = DockerPanel {
            in_flight: Some(InFlight::Logs),
            logs_for: Some("abc123".to_string()),
            stream_rx: Some(crate::subprocess::spawn_streaming(
                "echo",
                &["hello-logs".to_string()],
                Some(dir.path()),
            )),
            ..Default::default()
        };
        wait_until(|| {
            panel.poll();
            panel.in_flight.is_none()
        });
        assert_eq!(panel.logs, vec!["hello-logs".to_string()]);
        assert_eq!(panel.logs_for, Some("abc123".to_string()));
    }

    #[test]
    fn poll_with_nothing_in_flight_is_a_noop() {
        let mut panel = DockerPanel::default();
        panel.poll();
        assert!(panel.containers.is_empty());
        assert!(panel.logs.is_empty());
    }

    #[test]
    fn lifecycle_args_puts_subcommand_then_the_container_id_as_one_element() {
        let confirm = DockerConfirm {
            action: DockerLifecycleAction::Stop,
            // A container name/id containing shell metacharacters --
            // `lifecycle_args` must carry it through as one untouched
            // element, never concatenate/reformat it into anything a
            // shell would interpret differently.
            container_id: "abc; rm -rf /".to_string(),
            container_name: "web".to_string(),
        };
        assert_eq!(
            lifecycle_args(&confirm),
            vec!["stop".to_string(), "abc; rm -rf /".to_string()]
        );
    }

    #[test]
    fn lifecycle_args_maps_remove_to_rm() {
        let confirm = DockerConfirm {
            action: DockerLifecycleAction::Remove,
            container_id: "abc123".to_string(),
            container_name: "web".to_string(),
        };
        assert_eq!(
            lifecycle_args(&confirm),
            vec!["rm".to_string(), "abc123".to_string()]
        );
    }

    #[test]
    fn confirm_yes_clears_the_pending_confirm_and_starts_a_lifecycle_request() {
        let mut panel = DockerPanel {
            confirm: Some(DockerConfirm {
                action: DockerLifecycleAction::Stop,
                container_id: "abc123".to_string(),
                container_name: "web".to_string(),
            }),
            ..Default::default()
        };
        panel.confirm_yes();
        assert!(panel.confirm.is_none());
        assert!(panel.in_flight.is_some());
        assert!(panel.once_rx.is_some());
    }

    #[test]
    fn confirm_yes_while_something_else_is_in_flight_keeps_the_pending_confirm() {
        let mut panel = DockerPanel {
            in_flight: Some(InFlight::Refresh),
            ..Default::default()
        };
        panel.confirm = Some(DockerConfirm {
            action: DockerLifecycleAction::Stop,
            container_id: "abc123".to_string(),
            container_name: "web".to_string(),
        });
        panel.confirm_yes();
        assert!(panel.confirm.is_some());
    }
}
