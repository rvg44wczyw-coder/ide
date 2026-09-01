//! Kubernetes pods/deployments/services panel (`docs/features/
//! tui-docker-and-kubernetes.md` §2.3/§3.1/§3.4/§3.5/§4) -- shells out to
//! `kubectl`. Context/namespace are this panel's own selection, passed as
//! explicit `--context`/`--namespace` flags on every invocation, never a
//! `kubectl config use-context` mutation of the user's global kubeconfig
//! (§1's design decision -- stricter than "let the panel switch context"
//! literally implies, deliberately, since a global mutation would affect
//! every other terminal/tool using `kubectl` concurrently).

use crate::subprocess::{spawn_streaming, spawn_to_completion, StreamEvent};
use std::sync::mpsc::Receiver;

/// Same rationale as `docker_panel::MAX_DOCKER_LIST_ITEMS` -- not a
/// security boundary, a real cluster can legitimately have more pods
/// than this, capped so this panel's list rendering stays bounded.
pub(crate) const MAX_K8S_LIST_ITEMS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum K8sTab {
    #[default]
    Pods,
    Deployments,
    Services,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct K8sPod {
    pub(crate) name: String,
    pub(crate) phase: String,
    pub(crate) restarts: u32,
    pub(crate) ready: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct K8sDeployment {
    pub(crate) name: String,
    pub(crate) ready: String,
    pub(crate) replicas: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct K8sService {
    pub(crate) name: String,
    pub(crate) service_type: String,
    pub(crate) cluster_ip: String,
}

/// The raw shape `kubectl get pods -o json` actually returns --
/// `PodList { items: [Pod { metadata, status }] }`, the stable core `v1`
/// API. Kept separate from `K8sPod` (the flattened, render-friendly
/// shape this panel actually stores) so parsing failures are localized to
/// this module rather than leaking Kubernetes' own JSON layout into the
/// rest of the panel.
#[derive(serde::Deserialize)]
struct RawList<T> {
    items: Vec<T>,
}

#[derive(serde::Deserialize)]
struct RawPod {
    metadata: RawMetadata,
    status: RawPodStatus,
}

#[derive(serde::Deserialize, Default)]
struct RawPodStatus {
    #[serde(default)]
    phase: String,
    #[serde(default)]
    #[serde(rename = "containerStatuses")]
    container_statuses: Vec<RawContainerStatus>,
}

#[derive(serde::Deserialize)]
struct RawContainerStatus {
    #[serde(default)]
    ready: bool,
    #[serde(default, rename = "restartCount")]
    restart_count: u32,
}

#[derive(serde::Deserialize)]
struct RawMetadata {
    name: String,
}

#[derive(serde::Deserialize)]
struct RawDeployment {
    metadata: RawMetadata,
    #[serde(default)]
    spec: RawDeploymentSpec,
    #[serde(default)]
    status: RawDeploymentStatus,
}

#[derive(serde::Deserialize, Default)]
struct RawDeploymentSpec {
    #[serde(default)]
    replicas: u32,
}

#[derive(serde::Deserialize, Default)]
struct RawDeploymentStatus {
    #[serde(default)]
    #[serde(rename = "readyReplicas")]
    ready_replicas: u32,
}

#[derive(serde::Deserialize)]
struct RawService {
    metadata: RawMetadata,
    spec: RawServiceSpec,
}

#[derive(serde::Deserialize)]
struct RawServiceSpec {
    #[serde(default = "default_service_type")]
    #[serde(rename = "type")]
    service_type: String,
    #[serde(default, rename = "clusterIP")]
    cluster_ip: String,
}

fn default_service_type() -> String {
    "ClusterIP".to_string()
}

fn pods_from_raw(raw: RawList<RawPod>) -> Vec<K8sPod> {
    raw.items
        .into_iter()
        .map(|pod| {
            let total = pod.status.container_statuses.len();
            let ready_count = pod
                .status
                .container_statuses
                .iter()
                .filter(|c| c.ready)
                .count();
            let restarts = pod
                .status
                .container_statuses
                .iter()
                .map(|c| c.restart_count)
                .sum();
            K8sPod {
                name: pod.metadata.name,
                phase: pod.status.phase,
                restarts,
                ready: format!("{ready_count}/{total}"),
            }
        })
        .collect()
}

fn deployments_from_raw(raw: RawList<RawDeployment>) -> Vec<K8sDeployment> {
    raw.items
        .into_iter()
        .map(|d| K8sDeployment {
            name: d.metadata.name,
            ready: format!("{}/{}", d.status.ready_replicas, d.spec.replicas),
            replicas: d.spec.replicas,
        })
        .collect()
}

fn services_from_raw(raw: RawList<RawService>) -> Vec<K8sService> {
    raw.items
        .into_iter()
        .map(|s| K8sService {
            name: s.metadata.name,
            service_type: s.spec.service_type,
            cluster_ip: s.spec.cluster_ip,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum K8sDestructive {
    DeletePod { name: String },
    ScaleDeployment { name: String, replicas: u32 },
}

impl K8sDestructive {
    fn target_name(&self) -> &str {
        match self {
            K8sDestructive::DeletePod { name } => name,
            K8sDestructive::ScaleDeployment { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct K8sConfirm {
    pub(crate) action: K8sDestructive,
    pub(crate) target_name: String,
    pub(crate) typed: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum K8sPicker {
    Context,
    Namespace,
}

enum InFlight {
    RefreshList,
    RefreshContexts,
    RefreshNamespaces,
    Logs,
    Describe,
    Destructive,
}

#[derive(Default)]
pub(crate) struct K8sPanel {
    pub(crate) tab: K8sTab,
    pub(crate) context: Option<String>,
    pub(crate) namespace: Option<String>,
    pub(crate) available_contexts: Vec<String>,
    pub(crate) available_namespaces: Vec<String>,
    pub(crate) picker: Option<K8sPicker>,
    pub(crate) pods: Vec<K8sPod>,
    pub(crate) deployments: Vec<K8sDeployment>,
    pub(crate) services: Vec<K8sService>,
    pub(crate) truncated: bool,
    pub(crate) selected: usize,
    in_flight: Option<InFlight>,
    stream_rx: Option<Receiver<StreamEvent>>,
    once_rx: Option<Receiver<(Vec<String>, bool)>>,
    pub(crate) logs: Vec<String>,
    pub(crate) logs_for: Option<String>,
    pub(crate) describe_output: Vec<String>,
    pub(crate) describe_for: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) confirm: Option<K8sConfirm>,
    pub(crate) scale_input: Option<String>,
    /// Set alongside `scale_input`/the pending `K8sConfirm` -- which
    /// deployment `confirm_scale_input` is building a confirm for.
    scale_target: Option<String>,
}

impl K8sPanel {
    /// The shared `--context`/`--namespace` flag prefix every `kubectl`
    /// invocation this panel makes gets appended to -- exposed
    /// `pub(crate)` purely so tests can assert the exact argv shape
    /// without a real subprocess spawn.
    pub(crate) fn context_namespace_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(context) = &self.context {
            args.push("--context".to_string());
            args.push(context.clone());
        }
        if let Some(namespace) = &self.namespace {
            args.push("--namespace".to_string());
            args.push(namespace.clone());
        }
        args
    }

    pub(crate) fn refresh(&mut self) {
        if self.in_flight.is_some() {
            return;
        }
        // Deliberately *not* cleared here -- see the identical comment on
        // `DockerPanel::refresh` (`docker_panel.rs`): whatever error is
        // showing (e.g. a failed delete/scale that triggered this very
        // refresh) must stay visible until this refresh's own result
        // lands.
        let resource = match self.tab {
            K8sTab::Pods => "pods",
            K8sTab::Deployments => "deployments",
            K8sTab::Services => "services",
        };
        let mut args = vec![
            "get".to_string(),
            resource.to_string(),
            "-o".to_string(),
            "json".to_string(),
        ];
        args.extend(self.context_namespace_args());
        self.in_flight = Some(InFlight::RefreshList);
        self.once_rx = Some(spawn_to_completion("kubectl", &args, None));
    }

    pub(crate) fn refresh_contexts(&mut self) {
        if self.in_flight.is_some() {
            return;
        }
        self.in_flight = Some(InFlight::RefreshContexts);
        let args = vec![
            "config".to_string(),
            "get-contexts".to_string(),
            "-o".to_string(),
            "name".to_string(),
        ];
        self.once_rx = Some(spawn_to_completion("kubectl", &args, None));
    }

    pub(crate) fn refresh_namespaces(&mut self) {
        if self.in_flight.is_some() {
            return;
        }
        self.in_flight = Some(InFlight::RefreshNamespaces);
        let mut args = vec![
            "get".to_string(),
            "namespaces".to_string(),
            "-o".to_string(),
            "name".to_string(),
        ];
        if let Some(context) = &self.context {
            args.push("--context".to_string());
            args.push(context.clone());
        }
        self.once_rx = Some(spawn_to_completion("kubectl", &args, None));
    }

    pub(crate) fn fetch_logs(&mut self, pod_name: &str) {
        if self.in_flight.is_some() {
            return;
        }
        self.in_flight = Some(InFlight::Logs);
        self.logs.clear();
        self.logs_for = Some(pod_name.to_string());
        let mut args = vec![
            "logs".to_string(),
            "--tail".to_string(),
            "200".to_string(),
            pod_name.to_string(),
        ];
        args.extend(self.context_namespace_args());
        self.stream_rx = Some(spawn_streaming("kubectl", &args, None));
    }

    pub(crate) fn fetch_describe(&mut self, kind: &str, name: &str) {
        if self.in_flight.is_some() {
            return;
        }
        self.in_flight = Some(InFlight::Describe);
        self.describe_output.clear();
        self.describe_for = Some(format!("{kind}/{name}"));
        let mut args = vec!["describe".to_string(), format!("{kind}/{name}")];
        args.extend(self.context_namespace_args());
        self.once_rx = Some(spawn_to_completion("kubectl", &args, None));
    }

    pub(crate) fn request_delete_pod(&mut self, name: String) {
        let action = K8sDestructive::DeletePod { name };
        self.confirm = Some(K8sConfirm {
            target_name: action.target_name().to_string(),
            action,
            typed: String::new(),
        });
    }

    pub(crate) fn request_scale_deployment(&mut self, name: String) {
        self.scale_target = Some(name);
        self.scale_input = Some(String::new());
    }

    pub(crate) fn push_scale_input_char(&mut self, c: char) {
        if let Some(input) = &mut self.scale_input {
            input.push(c);
        }
    }

    pub(crate) fn pop_scale_input_char(&mut self) {
        if let Some(input) = &mut self.scale_input {
            input.pop();
        }
    }

    /// Parses `scale_input` as a non-negative integer and, on success,
    /// opens the typed-name confirm for it; on failure (non-numeric,
    /// empty) this is a no-op and `scale_input` stays open for
    /// correction -- mirroring delete's own "wrong input doesn't
    /// silently reset" behavior (§3.4).
    pub(crate) fn confirm_scale_input(&mut self) {
        let (Some(input), Some(target)) = (&self.scale_input, &self.scale_target) else {
            return;
        };
        let Ok(replicas) = input.parse::<u32>() else {
            return;
        };
        let action = K8sDestructive::ScaleDeployment {
            name: target.clone(),
            replicas,
        };
        self.confirm = Some(K8sConfirm {
            target_name: action.target_name().to_string(),
            action,
            typed: String::new(),
        });
        self.scale_input = None;
        self.scale_target = None;
    }

    pub(crate) fn push_confirm_char(&mut self, c: char) {
        if let Some(confirm) = &mut self.confirm {
            confirm.typed.push(c);
        }
    }

    pub(crate) fn pop_confirm_char(&mut self) {
        if let Some(confirm) = &mut self.confirm {
            confirm.typed.pop();
        }
    }

    /// Only actually runs `kubectl` when `typed == target_name` exactly
    /// (byte-for-byte, not a prefix match) -- otherwise a no-op that
    /// leaves the popup open (§3.4).
    pub(crate) fn confirm_submit(&mut self) {
        let Some(confirm) = &self.confirm else {
            return;
        };
        if confirm.typed != confirm.target_name {
            return;
        }
        if self.in_flight.is_some() {
            return;
        }
        let confirm = self.confirm.take().unwrap();
        let mut args = destructive_args(&confirm.action);
        args.extend(self.context_namespace_args());
        self.in_flight = Some(InFlight::Destructive);
        self.once_rx = Some(spawn_to_completion("kubectl", &args, None));
    }

    pub(crate) fn confirm_cancel(&mut self) {
        self.confirm = None;
        self.scale_input = None;
        self.scale_target = None;
    }

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
                    let kind = self.in_flight.take();
                    self.once_rx = None;
                    self.apply_once_result(kind, lines, success);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.in_flight = None;
                    self.once_rx = None;
                }
            }
        }
    }

    fn apply_once_result(&mut self, kind: Option<InFlight>, lines: Vec<String>, success: bool) {
        match kind {
            Some(InFlight::RefreshList) => self.apply_refresh_result(lines, success),
            Some(InFlight::RefreshContexts) => {
                if success {
                    self.available_contexts = lines.into_iter().filter(|l| !l.is_empty()).collect();
                } else {
                    self.error = Some(lines.join("\n"));
                }
            }
            Some(InFlight::RefreshNamespaces) => {
                if success {
                    self.available_namespaces = lines
                        .into_iter()
                        .filter_map(|l| l.strip_prefix("namespace/").map(str::to_string))
                        .collect();
                } else {
                    self.error = Some(lines.join("\n"));
                }
            }
            Some(InFlight::Describe) => {
                if success {
                    self.describe_output = lines;
                } else {
                    self.error = Some(lines.join("\n"));
                }
            }
            Some(InFlight::Destructive) => {
                if !success {
                    self.error = Some(lines.join("\n"));
                }
                self.refresh();
            }
            Some(InFlight::Logs) | None => {}
        }
    }

    fn apply_refresh_result(&mut self, lines: Vec<String>, success: bool) {
        if !success {
            self.error = Some(lines.join("\n"));
            return;
        }
        let joined = lines.join("\n");
        self.error = None;
        self.truncated = false;
        self.selected = 0;
        match self.tab {
            K8sTab::Pods => match serde_json::from_str::<RawList<RawPod>>(&joined) {
                Ok(raw) => {
                    let mut pods = pods_from_raw(raw);
                    self.truncated = pods.len() > MAX_K8S_LIST_ITEMS;
                    pods.truncate(MAX_K8S_LIST_ITEMS);
                    self.pods = pods;
                }
                Err(e) => self.error = Some(format!("failed to parse pod list: {e}")),
            },
            K8sTab::Deployments => match serde_json::from_str::<RawList<RawDeployment>>(&joined) {
                Ok(raw) => {
                    let mut deployments = deployments_from_raw(raw);
                    self.truncated = deployments.len() > MAX_K8S_LIST_ITEMS;
                    deployments.truncate(MAX_K8S_LIST_ITEMS);
                    self.deployments = deployments;
                }
                Err(e) => self.error = Some(format!("failed to parse deployment list: {e}")),
            },
            K8sTab::Services => match serde_json::from_str::<RawList<RawService>>(&joined) {
                Ok(raw) => {
                    let mut services = services_from_raw(raw);
                    self.truncated = services.len() > MAX_K8S_LIST_ITEMS;
                    services.truncate(MAX_K8S_LIST_ITEMS);
                    self.services = services;
                }
                Err(e) => self.error = Some(format!("failed to parse service list: {e}")),
            },
        }
    }
}

/// Pure so the exact argv a destructive action sends is directly
/// testable without spawning a real process.
fn destructive_args(action: &K8sDestructive) -> Vec<String> {
    match action {
        K8sDestructive::DeletePod { name } => {
            vec!["delete".to_string(), "pod".to_string(), name.clone()]
        }
        K8sDestructive::ScaleDeployment { name, replicas } => vec![
            "scale".to_string(),
            "deployment".to_string(),
            name.clone(),
            format!("--replicas={replicas}"),
        ],
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
    fn default_tab_is_pods() {
        assert_eq!(K8sPanel::default().tab, K8sTab::Pods);
    }

    #[test]
    fn context_namespace_args_is_empty_with_neither_set() {
        assert!(K8sPanel::default().context_namespace_args().is_empty());
    }

    #[test]
    fn context_namespace_args_includes_only_context_when_namespace_unset() {
        let panel = K8sPanel {
            context: Some("prod".to_string()),
            ..Default::default()
        };
        assert_eq!(
            panel.context_namespace_args(),
            vec!["--context".to_string(), "prod".to_string()]
        );
    }

    #[test]
    fn context_namespace_args_includes_both_when_set() {
        let panel = K8sPanel {
            context: Some("prod".to_string()),
            namespace: Some("staging-ns".to_string()),
            ..Default::default()
        };
        assert_eq!(
            panel.context_namespace_args(),
            vec![
                "--context".to_string(),
                "prod".to_string(),
                "--namespace".to_string(),
                "staging-ns".to_string(),
            ]
        );
    }

    #[test]
    fn destructive_args_for_delete_pod() {
        let action = K8sDestructive::DeletePod {
            name: "worker-7f9c".to_string(),
        };
        assert_eq!(
            destructive_args(&action),
            vec![
                "delete".to_string(),
                "pod".to_string(),
                "worker-7f9c".to_string()
            ]
        );
    }

    #[test]
    fn destructive_args_for_scale_deployment() {
        let action = K8sDestructive::ScaleDeployment {
            name: "api-server".to_string(),
            replicas: 5,
        };
        assert_eq!(
            destructive_args(&action),
            vec![
                "scale".to_string(),
                "deployment".to_string(),
                "api-server".to_string(),
                "--replicas=5".to_string(),
            ]
        );
    }

    #[test]
    fn request_delete_pod_opens_a_confirm_with_empty_typed() {
        let mut panel = K8sPanel::default();
        panel.request_delete_pod("worker-7f9c".to_string());
        let confirm = panel.confirm.unwrap();
        assert_eq!(confirm.target_name, "worker-7f9c");
        assert_eq!(confirm.typed, "");
        assert_eq!(
            confirm.action,
            K8sDestructive::DeletePod {
                name: "worker-7f9c".to_string()
            }
        );
    }

    #[test]
    fn confirm_submit_with_a_prefix_match_does_not_run_anything() {
        let mut panel = K8sPanel::default();
        panel.request_delete_pod("worker-7f9c".to_string());
        panel.push_confirm_char('w');
        panel.push_confirm_char('o');
        panel.push_confirm_char('r');
        panel.confirm_submit();
        assert!(
            panel.confirm.is_some(),
            "popup must stay open on a non-match"
        );
        assert!(panel.in_flight.is_none());
    }

    #[test]
    fn confirm_submit_with_an_exact_match_runs_the_action() {
        let mut panel = K8sPanel::default();
        panel.request_delete_pod("worker-7f9c".to_string());
        for c in "worker-7f9c".chars() {
            panel.push_confirm_char(c);
        }
        panel.confirm_submit();
        assert!(panel.confirm.is_none());
        assert!(panel.in_flight.is_some());
        assert!(panel.once_rx.is_some());
    }

    #[test]
    fn pop_confirm_char_lets_a_wrong_keystroke_be_corrected() {
        let mut panel = K8sPanel::default();
        panel.request_delete_pod("abc".to_string());
        panel.push_confirm_char('a');
        panel.push_confirm_char('x'); // typo
        panel.pop_confirm_char();
        panel.push_confirm_char('b');
        panel.push_confirm_char('c');
        assert_eq!(panel.confirm.as_ref().unwrap().typed, "abc");
    }

    #[test]
    fn confirm_cancel_discards_everything_and_sends_nothing() {
        let mut panel = K8sPanel::default();
        panel.request_delete_pod("abc".to_string());
        panel.confirm_cancel();
        assert!(panel.confirm.is_none());
        assert!(panel.in_flight.is_none());
    }

    #[test]
    fn request_scale_deployment_opens_the_numeric_prompt_not_a_confirm() {
        let mut panel = K8sPanel::default();
        panel.request_scale_deployment("api-server".to_string());
        assert_eq!(panel.scale_input, Some(String::new()));
        assert!(panel.confirm.is_none());
    }

    #[test]
    fn confirm_scale_input_with_a_valid_number_opens_the_typed_confirm() {
        let mut panel = K8sPanel::default();
        panel.request_scale_deployment("api-server".to_string());
        panel.push_scale_input_char('5');
        panel.confirm_scale_input();
        assert!(panel.scale_input.is_none());
        let confirm = panel.confirm.unwrap();
        assert_eq!(
            confirm.action,
            K8sDestructive::ScaleDeployment {
                name: "api-server".to_string(),
                replicas: 5,
            }
        );
        assert_eq!(confirm.target_name, "api-server");
    }

    #[test]
    fn confirm_scale_input_with_non_numeric_input_stays_in_the_prompt() {
        let mut panel = K8sPanel::default();
        panel.request_scale_deployment("api-server".to_string());
        panel.push_scale_input_char('x');
        panel.confirm_scale_input();
        assert!(panel.confirm.is_none());
        assert_eq!(panel.scale_input, Some("x".to_string()));
    }

    #[test]
    fn confirm_scale_input_with_empty_input_stays_in_the_prompt() {
        let mut panel = K8sPanel::default();
        panel.request_scale_deployment("api-server".to_string());
        panel.confirm_scale_input();
        assert!(panel.confirm.is_none());
        assert!(panel.scale_input.is_some());
    }

    #[test]
    fn confirm_cancel_during_the_scale_numeric_prompt_discards_it() {
        let mut panel = K8sPanel::default();
        panel.request_scale_deployment("api-server".to_string());
        panel.push_scale_input_char('5');
        panel.confirm_cancel();
        assert!(panel.scale_input.is_none());
        assert!(panel.confirm.is_none());
    }

    #[test]
    fn pods_from_raw_flattens_ready_count_and_sums_restarts() {
        let json = r#"{"items":[{
            "metadata": {"name": "worker-7f9c"},
            "status": {
                "phase": "Running",
                "containerStatuses": [
                    {"ready": true, "restartCount": 2},
                    {"ready": false, "restartCount": 1}
                ]
            }
        }]}"#;
        let raw: RawList<RawPod> = serde_json::from_str(json).unwrap();
        let pods = pods_from_raw(raw);
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].name, "worker-7f9c");
        assert_eq!(pods[0].phase, "Running");
        assert_eq!(pods[0].ready, "1/2");
        assert_eq!(pods[0].restarts, 3);
    }

    #[test]
    fn pods_from_raw_handles_a_pod_with_no_container_statuses_yet() {
        // A pod still in `Pending` may have no containerStatuses at all --
        // must not panic/error, just report 0/0.
        let json = r#"{"items":[{
            "metadata": {"name": "pending-pod"},
            "status": {"phase": "Pending"}
        }]}"#;
        let raw: RawList<RawPod> = serde_json::from_str(json).unwrap();
        let pods = pods_from_raw(raw);
        assert_eq!(pods[0].ready, "0/0");
        assert_eq!(pods[0].restarts, 0);
    }

    #[test]
    fn deployments_from_raw_reads_ready_and_desired_replicas() {
        let json = r#"{"items":[{
            "metadata": {"name": "api-server"},
            "spec": {"replicas": 3},
            "status": {"readyReplicas": 2}
        }]}"#;
        let raw: RawList<RawDeployment> = serde_json::from_str(json).unwrap();
        let deployments = deployments_from_raw(raw);
        assert_eq!(deployments[0].ready, "2/3");
        assert_eq!(deployments[0].replicas, 3);
    }

    #[test]
    fn deployments_from_raw_handles_a_deployment_with_no_status_yet() {
        let json = r#"{"items":[{
            "metadata": {"name": "brand-new"},
            "spec": {"replicas": 1}
        }]}"#;
        let raw: RawList<RawDeployment> = serde_json::from_str(json).unwrap();
        let deployments = deployments_from_raw(raw);
        assert_eq!(deployments[0].ready, "0/1");
    }

    #[test]
    fn services_from_raw_reads_type_and_cluster_ip() {
        let json = r#"{"items":[{
            "metadata": {"name": "web-svc"},
            "spec": {"type": "LoadBalancer", "clusterIP": "10.0.0.1"}
        }]}"#;
        let raw: RawList<RawService> = serde_json::from_str(json).unwrap();
        let services = services_from_raw(raw);
        assert_eq!(services[0].name, "web-svc");
        assert_eq!(services[0].service_type, "LoadBalancer");
        assert_eq!(services[0].cluster_ip, "10.0.0.1");
    }

    #[test]
    fn services_from_raw_defaults_type_to_cluster_ip_when_omitted() {
        // A bare ClusterIP service's `spec.type` is often omitted
        // entirely by `kubectl` rather than explicitly set.
        let json = r#"{"items":[{
            "metadata": {"name": "internal-svc"},
            "spec": {"clusterIP": "10.0.0.2"}
        }]}"#;
        let raw: RawList<RawService> = serde_json::from_str(json).unwrap();
        let services = services_from_raw(raw);
        assert_eq!(services[0].service_type, "ClusterIP");
    }

    #[test]
    fn apply_refresh_result_with_malformed_json_sets_error_not_a_panic() {
        let mut panel = K8sPanel::default();
        panel.apply_refresh_result(vec!["not json".to_string()], true);
        assert!(panel.error.is_some());
    }

    #[test]
    fn apply_refresh_result_with_failure_surfaces_the_error() {
        let mut panel = K8sPanel::default();
        panel.apply_refresh_result(vec!["connection refused".to_string()], false);
        assert!(panel.error.is_some());
    }

    #[test]
    fn apply_refresh_result_truncates_pods_at_the_cap() {
        let mut panel = K8sPanel::default();
        let items: Vec<String> = (0..(MAX_K8S_LIST_ITEMS + 20))
            .map(|i| {
                format!(r#"{{"metadata":{{"name":"pod{i}"}},"status":{{"phase":"Running"}}}}"#)
            })
            .collect();
        let json = format!(r#"{{"items":[{}]}}"#, items.join(","));
        panel.apply_refresh_result(vec![json], true);
        assert_eq!(panel.pods.len(), MAX_K8S_LIST_ITEMS);
        assert!(panel.truncated);
    }

    #[test]
    fn refresh_contexts_populates_available_contexts_skipping_blank_lines() {
        let mut panel = K8sPanel::default();
        panel.apply_once_result(
            Some(InFlight::RefreshContexts),
            vec!["prod".to_string(), "".to_string(), "staging".to_string()],
            true,
        );
        assert_eq!(
            panel.available_contexts,
            vec!["prod".to_string(), "staging".to_string()]
        );
    }

    #[test]
    fn refresh_namespaces_strips_the_namespace_prefix() {
        let mut panel = K8sPanel::default();
        panel.apply_once_result(
            Some(InFlight::RefreshNamespaces),
            vec![
                "namespace/default".to_string(),
                "namespace/kube-system".to_string(),
            ],
            true,
        );
        assert_eq!(
            panel.available_namespaces,
            vec!["default".to_string(), "kube-system".to_string()]
        );
    }

    #[test]
    fn describe_result_populates_describe_output_on_success() {
        let mut panel = K8sPanel::default();
        panel.apply_once_result(
            Some(InFlight::Describe),
            vec![
                "Name: worker-7f9c".to_string(),
                "Status: Running".to_string(),
            ],
            true,
        );
        assert_eq!(
            panel.describe_output,
            vec![
                "Name: worker-7f9c".to_string(),
                "Status: Running".to_string()
            ]
        );
    }

    #[test]
    fn destructive_result_refreshes_the_list_and_surfaces_failure() {
        let mut panel = K8sPanel::default();
        panel.apply_once_result(
            Some(InFlight::Destructive),
            vec!["error: pod not found".to_string()],
            false,
        );
        assert!(panel.error.is_some());
        // `refresh()` was triggered -- confirmed by `in_flight` becoming
        // `Some` again (RefreshList) rather than staying `None`.
        assert!(panel.in_flight.is_some());
    }

    #[test]
    fn fetch_logs_streams_output_and_sets_logs_for() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = K8sPanel {
            in_flight: Some(InFlight::Logs),
            logs_for: Some("worker-7f9c".to_string()),
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
        assert_eq!(panel.logs_for, Some("worker-7f9c".to_string()));
    }

    #[test]
    fn refresh_while_already_in_flight_is_a_noop() {
        let mut panel = K8sPanel {
            in_flight: Some(InFlight::RefreshList),
            ..Default::default()
        };
        panel.refresh();
        assert!(panel.once_rx.is_none());
    }

    #[test]
    fn confirm_submit_while_something_else_in_flight_is_a_noop() {
        let mut panel = K8sPanel {
            in_flight: Some(InFlight::RefreshList),
            ..Default::default()
        };
        panel.request_delete_pod("abc".to_string());
        for c in "abc".chars() {
            panel.push_confirm_char(c);
        }
        panel.confirm_submit();
        // Stays pending -- nothing was sent, confirm is still there.
        assert!(panel.confirm.is_some());
    }

    #[test]
    fn poll_with_nothing_in_flight_is_a_noop() {
        let mut panel = K8sPanel::default();
        panel.poll();
        assert!(panel.pods.is_empty());
    }
}
