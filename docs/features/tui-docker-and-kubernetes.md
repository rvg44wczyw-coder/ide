# TUI Docker & Kubernetes panels

## 1. Purpose

Two new `ide-tui` overlay panels, shelling out to the `docker` and
`kubectl` CLIs the user already has installed and configured — the same
"drive the real tool the user already trusts, don't reimplement its API
client" approach `cargo_panel.rs` already established for `cargo`. Direct
user request: "я хочу работать с докером и кубернетес из TUI — нужны
дополнительные панели", TUI first, GUI (a mirrored `ide-ui` version)
explicitly deferred to a later run.

Scope, per the three decisions made via `AskUserQuestion` before this doc
was written:

- **Docker**: read-only listing (containers, images) plus lifecycle
  actions (start/stop/restart/remove a container), each lifecycle action
  gated behind a simple yes/no confirm popup.
- **Kubernetes**: read-only listing (pods, deployments, services) plus
  delete-pod/scale-deployment, each gated behind a **typed-name**
  confirmation (stronger than Docker's yes/no — a wrong kubectl context
  can reach a real, possibly-production cluster, not just the local
  daemon).
- **Kubernetes context/namespace**: the panel owns its own selection (via
  a picker reading `kubectl config get-contexts`/`kubectl get
  namespaces`) and passes `--context`/`--namespace` as explicit flags on
  every invocation — it **never** calls `kubectl config use-context`,
  which would mutate the user's global kubeconfig and affect every other
  terminal/tool using `kubectl` concurrently. This is a stricter
  implementation than what was described in the question the user
  answered ("let the panel switch context") — same user-visible effect
  (you can switch without leaving the IDE), no shared global side effect,
  strictly safer with no functional downside, so implemented this way
  directly rather than asked again.

### Scope cuts (v1)

- No live-follow logs (`docker logs -f` / `kubectl logs -f`) — one-shot
  `--tail 200` fetch, manually refreshable. Follow mode is a natural
  future extension once this v1's simpler request/response shape is
  proven; the same reasoning `tui-cargo-panel.md` gave for not building
  incremental-diff-aware anything in its own v1.
- No `docker exec`/`kubectl exec` (opens an interactive shell inside a
  container — a materially different, PTY-shaped feature, not a
  request/response CLI call; out of scope the same way `docker
  compose`/Helm/`kubectl apply` are).
- No image pull/build, no volume/network management, no Deployment/
  Service create/edit — v1 covers exactly the operations named in the
  three scoping decisions above and nothing else invented beyond them.
- No live background polling/auto-refresh — lists are fetched once on
  panel open and on an explicit manual refresh keypress, the same
  fetch-on-demand model `git_panel.rs`'s commit graph already uses
  (constant `docker`/`kubectl` polling every frame would be wasteful and,
  for a remote cluster, a real latency/cost concern).

## 2. Interface

### 2.1 `crates/tui/src/subprocess.rs` (new file)

A small shared helper both new panels use, generalizing
`cargo_panel.rs`'s `spawn_streaming`/`run_and_stream`/`stream_lines`
(which only takes one fixed subcommand string) to a real `args: &[String]`
argv — `cargo_panel.rs` itself is untouched, since duplicating its exact
shape a third time (once for Docker, once for Kubernetes) would be the
kind of repetition worth extracting, but touching an already-working,
already-tested file for an unrelated feature is not.

```rust
pub(crate) enum StreamEvent { Line(String), Done }

/// Spawns `program` with `args` (an explicit argv, never a shell string),
/// `current_dir` if given, off the calling thread. Streams stdout+stderr
/// line-by-line via the returned channel, `Done` once the process exits
/// (including "failed to spawn" / "not found on PATH", each reported as
/// one `Line` immediately followed by `Done` -- same convention
/// `cargo_panel.rs::run_and_stream` already established).
pub(crate) fn spawn_streaming(
    program: &str,
    args: &[String],
    current_dir: Option<&Path>,
) -> Receiver<StreamEvent>;

/// Spawns `program`/`args` off the calling thread the same way
/// `spawn_streaming` does, but the returned channel yields **exactly one**
/// message once the process exits: its combined stdout+stderr lines plus
/// exit success -- for callers that want a single `Vec<String>` result
/// rather than incremental lines (list-refresh, lifecycle/destructive
/// actions, describe). `poll()` drains at most one `(Vec<String>, bool)`
/// from this receiver per call, same shape as draining `StreamEvent`s.
pub(crate) fn spawn_to_completion(
    program: &str,
    args: &[String],
    current_dir: Option<&Path>,
) -> Receiver<(Vec<String>, bool)>;
```

Both functions build the child with `Command::new(program).args(args)` —
no shell, ever, for the same reason `cargo_panel.rs`'s own doc comment
already states. `spawn_to_completion` is used by list-refresh (needs the
full output at once, off-thread already) and by lifecycle/destructive
actions (start/stop/restart/rm/delete/scale — short-lived, one-shot,
success/failure is all the caller needs); `spawn_streaming` is used by
logs (potentially large output, streamed as it arrives the same way
`cargo build`'s output is).

### 2.2 `crates/tui/src/docker_panel.rs` (new file)

```rust
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockerTab { Containers, Images }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockerLifecycleAction { Start, Stop, Restart, Remove }

impl DockerLifecycleAction {
    /// The literal `docker` subcommand -- `remove` maps to `"rm"`, every
    /// other variant is its own lowercased name.
    pub(crate) fn subcommand(self) -> &'static str;
}

#[derive(Default)]
pub(crate) struct DockerPanel {
    pub(crate) tab: DockerTab,
    pub(crate) containers: Vec<DockerContainer>,
    pub(crate) images: Vec<DockerImage>,
    /// Whether the most recent `refresh()` hit `MAX_DOCKER_LIST_ITEMS`
    /// and stopped early (§3.1/§4) -- read by the render side to show a
    /// "showing first 500…" note instead of silently under-reporting.
    pub(crate) truncated: bool,
    pub(crate) selected: usize,
    /// Set the instant a list-refresh, logs fetch, or lifecycle action is
    /// sent; cleared when its result is drained -- backs a "Loading…"
    /// indicator so a slow/hung daemon doesn't look identical to "done,
    /// zero containers" (`docs/features/tui-cargo-panel.md`'s `running`
    /// field is the closest existing precedent, though this panel's
    /// in-flight state additionally has to distinguish *which* kind of
    /// request is in flight -- see `InFlight` below).
    in_flight: Option<InFlight>,
    /// Exactly one of these is `Some` at a time, matching whichever
    /// `InFlight` variant is set -- `Logs` uses `stream_rx`
    /// (`spawn_streaming`), everything else uses `once_rx`
    /// (`spawn_to_completion`). Two separate `Option` fields rather than
    /// one enum-wrapped receiver so `poll()` doesn't need to downcast;
    /// `debug_assert!` in `poll()` confirms the invariant instead.
    stream_rx: Option<Receiver<StreamEvent>>,
    once_rx: Option<Receiver<(Vec<String>, bool)>>,
    pub(crate) logs: Vec<String>,
    pub(crate) logs_for: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) confirm: Option<DockerConfirm>,
}

enum InFlight { Refresh, Logs, Lifecycle(DockerLifecycleAction) }

/// A pending lifecycle action awaiting the user's yes/no answer --
/// nothing is sent to `docker` until `confirm_yes` runs.
pub(crate) struct DockerConfirm {
    pub(crate) action: DockerLifecycleAction,
    pub(crate) container_id: String,
    pub(crate) container_name: String,
}

impl DockerPanel {
    /// Kicks off a container-or-image list refresh (whichever `tab` is
    /// active) on a background thread. No-op if a request is already in
    /// flight -- v1 runs at most one `docker` invocation at a time, the
    /// same single-in-flight discipline `cargo_panel.rs::run` already
    /// uses, so a slow daemon can't pile up concurrent processes from
    /// repeated refresh keypresses.
    pub(crate) fn refresh(&mut self);

    /// Fetches `--tail 200` logs for `container_id`, streamed.
    pub(crate) fn fetch_logs(&mut self, container_id: &str);

    /// Opens the yes/no confirm popup -- does not run anything yet.
    pub(crate) fn request_lifecycle_action(
        &mut self,
        action: DockerLifecycleAction,
        container_id: String,
        container_name: String,
    );

    /// The confirm popup's "yes": actually runs `docker <subcommand>
    /// <id>`, then triggers a fresh `refresh()` once it completes so the
    /// list reflects the new state.
    pub(crate) fn confirm_yes(&mut self);

    /// The confirm popup's "no"/`Esc`: discards the pending action,
    /// sends nothing.
    pub(crate) fn confirm_no(&mut self);

    /// Call once per loop iteration while the panel is open (same
    /// "background work keeps streaming even while a panel is hidden"
    /// convention `cargo_panel.rs::poll` documents, though this panel's
    /// only caller only ever polls while open -- listing/logs have no
    /// reason to keep running once the user has navigated away, unlike
    /// a build the user explicitly started and may want to alt-tab away
    /// from).
    pub(crate) fn poll(&mut self);
}
```

### 2.3 `crates/tui/src/k8s_panel.rs` (new file)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum K8sTab { Pods, Deployments, Services }

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

/// What a destructive action targets -- `Delete` always a pod (v1 scope,
/// per the scoping decision), `Scale` always a deployment.
pub(crate) enum K8sDestructive {
    DeletePod { name: String },
    ScaleDeployment { name: String, replicas: u32 },
}

/// The typed-confirmation gate -- stronger than Docker's yes/no per the
/// scoping decision. `typed` accumulates what the user has entered so
/// far; the action only runs once `typed == target_name` exactly and the
/// user presses Enter (matching, not just prefix-matching, so a
/// three-character pod name can't be confirmed by three random
/// keystrokes that happen to prefix-match a longer name -- see §3.4).
pub(crate) struct K8sConfirm {
    pub(crate) action: K8sDestructive,
    pub(crate) target_name: String,
    pub(crate) typed: String,
}

#[derive(Default)]
pub(crate) struct K8sPanel {
    pub(crate) tab: K8sTab,
    pub(crate) context: Option<String>,
    pub(crate) namespace: Option<String>,
    /// Populated by `refresh_contexts`/`refresh_namespaces`, backing the
    /// picker popup -- not auto-fetched on every panel open (an extra
    /// `kubectl` round-trip the user doesn't always need), only when the
    /// picker is actually opened.
    pub(crate) available_contexts: Vec<String>,
    pub(crate) available_namespaces: Vec<String>,
    pub(crate) picker: Option<K8sPicker>,
    pub(crate) pods: Vec<K8sPod>,
    pub(crate) deployments: Vec<K8sDeployment>,
    pub(crate) services: Vec<K8sService>,
    /// Same meaning as `DockerPanel::truncated`, for whichever list
    /// `refresh()` last populated.
    pub(crate) truncated: bool,
    pub(crate) selected: usize,
    in_flight: Option<InFlight>,
    /// Same split as `DockerPanel`'s `stream_rx`/`once_rx` -- `Logs` uses
    /// `stream_rx`, every other `InFlight` variant uses `once_rx`.
    stream_rx: Option<Receiver<StreamEvent>>,
    once_rx: Option<Receiver<(Vec<String>, bool)>>,
    pub(crate) logs: Vec<String>,
    pub(crate) logs_for: Option<String>,
    pub(crate) describe_output: Vec<String>,
    pub(crate) describe_for: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) confirm: Option<K8sConfirm>,
    /// Only meaningful while a `ScaleDeployment` confirm is being built
    /// -- the replica-count text entry step that runs *before* the typed-
    /// name confirmation (§3.4).
    pub(crate) scale_input: Option<String>,
}

pub(crate) enum K8sPicker { Context, Namespace }

enum InFlight { RefreshList, RefreshContexts, RefreshNamespaces, Logs, Describe, Destructive }

impl K8sPanel {
    pub(crate) fn refresh(&mut self); // current tab's resource list
    pub(crate) fn refresh_contexts(&mut self);
    pub(crate) fn refresh_namespaces(&mut self);
    pub(crate) fn fetch_logs(&mut self, pod_name: &str);
    pub(crate) fn fetch_describe(&mut self, kind: &str, name: &str);

    /// Begins the delete-pod confirm flow directly (no intermediate
    /// numeric-input step, unlike scale).
    pub(crate) fn request_delete_pod(&mut self, name: String);
    /// Begins the scale flow's *first* step -- the replica-count prompt.
    /// `request_delete_pod`'s typed-name confirm only follows once a
    /// valid replica count has been entered (§3.4).
    pub(crate) fn request_scale_deployment(&mut self, name: String);
    /// Confirms the replica-count prompt (`scale_input`) and opens the
    /// typed-name confirm with `K8sDestructive::ScaleDeployment` -- no-op
    /// (stays in the prompt) if `scale_input` doesn't parse as a
    /// non-negative integer.
    pub(crate) fn confirm_scale_input(&mut self);

    /// Every keystroke into the confirm popup's typed-name field.
    pub(crate) fn push_confirm_char(&mut self, c: char);
    pub(crate) fn pop_confirm_char(&mut self);
    /// Only succeeds (actually runs the `kubectl` command) if
    /// `typed == target_name` exactly; otherwise a no-op that leaves the
    /// popup open so the user can keep correcting their input.
    pub(crate) fn confirm_submit(&mut self);
    pub(crate) fn confirm_cancel(&mut self);

    pub(crate) fn poll(&mut self);

    /// Every `kubectl` invocation's shared flag prefix -- `["--context",
    /// ctx]`/`["--namespace", ns]` appended only when `Some`, per the
    /// no-global-mutation design decision in §1. Exposed `pub(crate)`
    /// purely so its own unit tests can assert the exact argv shape
    /// without going through a real subprocess spawn.
    pub(crate) fn context_namespace_args(&self) -> Vec<String>;
}
```

### 2.4 `crates/tui/src/app.rs`

- `App` gains `docker_panel: Option<DockerPanel>`, `k8s_panel:
  Option<K8sPanel>` — same `Option<PanelState>` shape `git_panel`/
  `GitPanelState` already establishes, not a plain non-optional field
  with its own `open: bool`, so "is the panel open" and "the panel's
  state" can't drift apart.
- `Action::ToggleDockerPanel`, `Action::ToggleK8sPanel` — each closes
  every other overlay first (find/palette/other panels), mirroring
  `toggle_git_panel`'s existing `self.git_panel = None` siblings-clearing
  pattern, and opens with a fresh `default()` state plus an immediate
  `refresh()` call (so the list is already loading by the time the panel
  renders, not empty-until-the-user-presses-refresh).
- `handle_docker_panel_key`/`handle_k8s_panel_key`, checked in
  `handle_key`'s early-dispatch chain at the same priority tier
  `handle_git_panel_key` already occupies (after find/palette, before
  general editor keys) — exact ordering relative to `git_panel`'s own
  check doesn't matter since `toggle_*` already guarantees at most one of
  find/palette/git_panel/docker_panel/k8s_panel is ever open at once.
- Both `poll_*` calls added to the main loop's per-iteration poll step,
  alongside the existing `git.refresh`/`watcher`/`lsp` polls — only while
  the respective panel is `Some` (§1's "no background polling" scope cut:
  unlike `cargo_panel.rs`'s deliberately-keeps-running-when-hidden
  design, closing a Docker/K8s panel mid-request just drops the in-flight
  `Receiver`, silently discarding whatever arrives after — acceptable
  because, unlike a build the user explicitly started, listing/logs are
  cheap to re-request the next time the panel is reopened).

### 2.5 `crates/tui/src/commands.rs`

Two new palette-only entries, no default binding — same reasoning
`ToggleGitPanel`/`ToggleCargoPanel` already documented (no JetBrains
keymap binds a Docker or Kubernetes tool window in the mac keymap this
project tracks, and CLAUDE.md's "never invent a binding" rule applies
identically here):

```rust
Command { id: "ToggleDockerPanel", title: "Docker", binding: None, action: Action::ToggleDockerPanel },
Command { id: "ToggleK8sPanel", title: "Kubernetes", binding: None, action: Action::ToggleK8sPanel },
```

### 2.6 `crates/tui/src/ui.rs`

Two new render functions, `render_docker_panel`/`render_k8s_panel`,
called from the same top-level overlay-dispatch `match` that already
picks between `render_git_panel`/`render_cargo_panel`/etc. based on which
`Option` is `Some`. Layout: a left list (containers/images, or the active
K8s resource tab) plus a right detail pane (logs or describe output),
split the same proportions `render_git_panel`'s graph+diff split already
uses; the confirm popups render as a centered modal, the same shape the
existing rename/bookmark/etc. popups already use (`ui.rs` already has a
shared centered-popup layout helper — reused, not reinvented).

## 3. Behaviour

### 3.1 Container/pod list refresh

`refresh()` spawns the read (`docker ps -a --format '{{json .}}'` /
`docker images --format '{{json .}}'` / `kubectl get pods -o json
<context/namespace args>` / equivalent for deployments/services) via
`spawn_to_completion` on a background thread (list output is bounded and
arrives all at once — no reason to stream it line-by-line the way logs
are). Each line of Docker's JSON-lines output is parsed independently
with `serde_json::from_str`; a line that fails to parse is skipped, not
treated as a fatal error for the whole refresh (defensive against a
future Docker version adding a field this struct doesn't know about, or
a stray non-JSON line on stdout — the same permissive-parsing posture
`ide-lsp`'s bounded decoders take toward malformed *elements*, though
unlike those this isn't attacker-controlled input, just a defensive
parse). `kubectl`'s single JSON document is parsed as a whole
`{"items": [...]}` shape; a parse failure here **is** surfaced as
`self.error` (there's no meaningful "skip this line" for a single
document — a malformed whole document means the whole list is unusable
this refresh). Both parse paths stop accepting further items once
`MAX_DOCKER_LIST_ITEMS`/`MAX_K8S_LIST_ITEMS` is reached (§4) and set a
`truncated: bool` the render side reads to show the "showing first 500…"
note.

### 3.2 Logs

`fetch_logs`/`fetch_describe` use `spawn_streaming` (potentially large
output) and `spawn_to_completion` respectively (describe output is
bounded and the whole point is reading it once fully rendered, not
watching it arrive). Selecting a different container/pod while logs are
already showing replaces `logs`/`logs_for` wholesale on the next fetch,
the same "last query wins" convention `ide-ui`/`ide-tui`'s LSP-backed
panels already use elsewhere in this codebase.

### 3.3 Docker lifecycle confirm

`request_lifecycle_action` only ever sets `self.confirm = Some(..)` —
nothing runs until the popup's explicit yes. `confirm_yes` runs
`docker <subcommand> <container_id>` via `spawn_to_completion` on a
background thread (so a hung daemon can't freeze the main loop), then
`refresh()`s the list once it completes, success or failure either way
(a failed `stop` still means "check what actually happened," which a
refresh answers better than leaving stale state on screen).

### 3.4 Kubernetes typed-name confirm and the scale-input step

**Delete pod**: `request_delete_pod(name)` goes straight to
`K8sConfirm { action: DeletePod { name: name.clone() }, target_name:
name, typed: String::new() }`. Every character the user types is pushed
via `push_confirm_char`; `confirm_submit` only actually runs `kubectl
delete pod <name> <context/namespace args>` when `typed == target_name`
byte-for-byte — a partial or prefix match does nothing, leaving the popup
open with the user's (wrong) input still visible so they can see exactly
what didn't match and correct it, rather than silently clearing on a
failed attempt (which would look like nothing happened at all).

**Scale deployment** has one extra step first: `request_scale_deployment`
opens `scale_input = Some(String::new())`, a plain numeric text entry (no
`K8sConfirm` yet — there's nothing to confirm the *name* of until a valid
target replica count exists to state in the confirmation prompt).
`confirm_scale_input` parses `scale_input` as `u32`; on success it opens
the typed-name `K8sConfirm` with `action: ScaleDeployment { name,
replicas }` (the confirmation prompt's rendered text includes the parsed
replica count, e.g. "Type `my-deployment` to scale to 3 replicas"); on
failure (non-numeric, empty) it's a no-op — `scale_input` stays open for
correction, mirroring delete's own "wrong input doesn't silently reset"
behavior. `confirm_submit` for a `ScaleDeployment` runs `kubectl scale
deployment <name> --replicas=<n> <context/namespace args>`.

`confirm_cancel`/`Esc` at any point in either flow (the numeric prompt or
the typed-name popup) discards all pending state (`scale_input` and/or
`confirm`) and sends nothing — matching Docker's `confirm_no` and every
other cancelable popup already in this crate.

### 3.5 Context/namespace picker

`K8sPicker::Context`/`Namespace`, opened from within the K8s panel (a
dedicated keypress, not auto-shown), triggers `refresh_contexts`/
`refresh_namespaces` if the corresponding `available_*` list is still
empty, then renders a selectable list (same list-popup shape the confirm
overlays use). Selecting an entry sets `self.context`/`self.namespace`
and closes the picker — **does not** itself trigger a `kubectl config
use-context` call (§1); every subsequent `kubectl` invocation this panel
makes picks up the new value through `context_namespace_args()`. Picking
"no namespace filter" (a synthetic first entry, distinct from any real
namespace name) sets `self.namespace = None`, meaning `kubectl`'s own
default namespace behavior applies (whatever the *unmutated* kubeconfig's
current context's default namespace already is) — the panel never
invents its own "default" namespace string.

## 4. Constraints & invariants

- **No shell, ever.** Every `docker`/`kubectl` invocation goes through
  `Command::new(program).args(args)` via `subprocess.rs`'s two helpers —
  no format!-ed command string, matching every other subprocess-spawning
  surface in this codebase (`cargo_panel.rs`, `claude_terminal.rs`,
  `ide-lsp`'s server spawn).
- **Context/namespace are per-invocation flags, never a global kubeconfig
  mutation.** `context_namespace_args()` is the single source of truth
  for this — no code path in `k8s_panel.rs` may call `kubectl config
  use-context`/`kubectl config set-context` directly.
- **A destructive action never runs before its confirmation succeeds.**
  `DockerConfirm`/`K8sConfirm` existing in state is not itself
  permission to act — only `confirm_yes` (Docker) / `confirm_submit` with
  an exact `typed == target_name` match (Kubernetes) may call
  `spawn_to_completion` with a mutating subcommand (`start`/`stop`/
  `restart`/`rm`/`delete`/`scale`).
- **At most one `docker`/`kubectl` invocation per panel in flight at
  once** — `refresh`/`fetch_logs`/`fetch_describe`/lifecycle-and-
  destructive actions all check `in_flight`/`rx` first and no-op if
  something is already running, the same discipline
  `cargo_panel.rs::run` already established, preventing a burst of
  refresh keypresses (or an impatient repeated confirm) from piling up
  concurrent subprocesses.
- **Both binaries' absence is a normal, recoverable error state, not a
  crash.** `spawn_to_completion`/`spawn_streaming` already report "not
  found on PATH" as a `Line` + `Done` (§2.1) — surfaced as `self.error`,
  the panel stays usable (e.g. showing the error and letting the user
  retry once they've installed the tool) rather than the feature being
  unreachable if `docker`/`kubectl` isn't installed.
- **List size is capped at parse time.** `MAX_DOCKER_LIST_ITEMS`/
  `MAX_K8S_LIST_ITEMS = 500` each — not a security boundary the way
  `MAX_SEARCH_RESULTS`/`MAX_LOCATIONS_PER_MESSAGE` are elsewhere in this
  codebase (`docker`/`kubectl`'s output isn't attacker-controlled the way
  a malformed settings file or a malicious LSP response is; it's the
  user's own daemon/cluster), but a real cluster can legitimately have
  thousands of pods, and rendering an unbounded list into a fixed-height
  terminal list widget is a genuine, non-adversarial usability/perf
  concern on its own. Parsing stops once the cap is reached (the same
  early-stop-then-truncate shape this session's other bounded decoders
  already use) and the panel shows a "showing first 500 of possibly
  more — narrow with a filter" note rather than silently hiding the
  truncation.
- **Argument vectors, not shell strings, make container/pod names safe
  even if a name is adversarially crafted.** A container or pod name
  comes from `docker`/`kubectl`'s own output (or the user's typed
  confirmation), never from this panel's own generation — but because
  every invocation is `Command::args(&[...])`, not a formatted shell
  string, even a name containing shell metacharacters (`; rm -rf /`,
  `` `id` ``, etc. — technically disallowed by Docker/Kubernetes naming
  rules for real resources, but this panel doesn't rely on that rule
  holding) reaches the child process as one inert argv element, never
  interpreted. Same reasoning `cargo_panel.rs`'s own doc comment already
  gives for `subcommand`.
- **Closing a panel mid-action does not abort the underlying subprocess.**
  Dropping `stream_rx`/`once_rx` (on `Toggle*` closing the panel, or the
  app exiting) only drops *this panel's* receiving end — the spawned
  thread's `docker stop`/`kubectl delete`/etc. child process keeps
  running to completion regardless, and its eventual `send()` on the now-
  disconnected channel just fails silently. This is deliberate: killing
  an in-progress `stop`/`delete`/`scale` because the user alt-tabbed away
  from the panel would leave the container/cluster resource in a worse,
  less-predictable state than letting it finish.

## 5. Examples

- Opening the Docker panel (`ToggleDockerPanel`, palette) with three
  running containers: `refresh()` fires immediately, `in_flight =
  Some(Refresh)`; once `poll()` drains the completed `spawn_to_completion`
  result, `containers` populates and `in_flight` clears.
- Selecting a container and pressing the "stop" key:
  `request_lifecycle_action(Stop, id, name)` opens the yes/no popup;
  pressing "n"/`Esc` clears it with nothing sent; pressing "y" runs
  `confirm_yes`, which spawns `docker stop <id>`, and once it completes,
  automatically re-`refresh()`es so the container's `Status` column
  updates to "Exited…".
- Opening the K8s panel with `context = None`/`namespace = None`:
  `refresh()`'s `kubectl get pods -o json` (no `--context`/`--namespace`
  flags at all) uses whatever the ambient kubeconfig's current context
  already resolves to — identical behavior to running `kubectl get pods`
  in a real terminal right now.
- Deleting a pod named `worker-7f9c`: `request_delete_pod` opens the
  typed-name popup; typing `worker` and pressing Enter does nothing (no
  match, popup stays open showing "worker" so far); typing the remaining
  `-7f9c` and pressing Enter now matches exactly and runs `kubectl delete
  pod worker-7f9c <context/namespace args>`.
- Scaling `api-server` to 5 replicas: `request_scale_deployment` opens the
  numeric prompt; typing `5` and Enter (`confirm_scale_input`, parses OK)
  opens the typed-name popup reading "Type `api-server` to scale to 5
  replicas"; typing `api-server` and Enter runs `kubectl scale deployment
  api-server --replicas=5 <context/namespace args>`.

## 6. Dependencies & integration points

- No new crate dependencies — `serde`/`serde_json` are already present in
  `crates/tui/Cargo.toml` (used by `state.rs`/`keymap.rs`'s own
  persistence), reused here for Docker's line-delimited JSON and
  `kubectl`'s single-document JSON.
- No `ide-core`/`ide-lsp`/`ide-dap` involvement — this is a pure
  `ide-tui`-local feature, the same shape `cargo_panel.rs`/
  `claude_terminal.rs` already are (no core crate has any concept of
  containers or clusters).
- No `ide-ui` changes in this run — the user explicitly deferred the GUI
  mirror of this feature to a later run, TUI-first.
- Both `docker`/`kubectl` are expected to already be installed and
  configured (existing `kubeconfig`, Docker daemon reachable) by the user
  — this feature doesn't install, configure, or validate either tool
  beyond "did the spawn succeed" (§4's not-found handling).

## Revision notes

**Self-review (before implementation)** caught four gaps in the first
draft:

1. **Threading inconsistency**: the original draft described `refresh()`/
   lifecycle actions as running `run_to_completion` "on a background
   thread" while defining that function as synchronous with no channel —
   no actual mechanism got the result back to the polling main thread.
   Fixed by replacing it with `spawn_to_completion`, symmetric to
   `spawn_streaming` (spawns its own thread, returns a `Receiver`).
2. **Receiver type mismatch**: `DockerPanel`/`K8sPanel` each had one `rx`
   field typed `Receiver<StreamEvent>`, but logs need that type while
   refresh/lifecycle/describe need `Receiver<(Vec<String>, bool)>` from
   the fix above — one field can't hold either. Fixed by splitting into
   `stream_rx`/`once_rx`, exactly one `Some` at a time per `InFlight`.
3. **No cap on list size**: `docker ps -a`/`kubectl get pods` on a large
   real environment could return thousands of entries with nothing
   capping how many this panel parses/renders. Not a security boundary
   (the daemon/cluster is the user's own, not attacker-controlled) but a
   genuine usability/perf concern on its own — fixed by adding
   `MAX_DOCKER_LIST_ITEMS`/`MAX_K8S_LIST_ITEMS = 500` with a `truncated`
   flag surfaced to the render side, matching this codebase's established
   bounded-list convention even though the threat model motivating it
   here is different (real scale, not adversarial input).
4. **Unstated behavior for closing a panel mid-action**: added an explicit
   invariant (§4) that dropping a panel's receiver doesn't abort the
   underlying `docker`/`kubectl` child process — worth stating outright
   rather than leaving a reviewer to wonder whether closing the panel
   mid-`delete` aborts it (it doesn't, deliberately).

Also flagged for follow-up, not fixed in this doc: `CLAUDE.md`'s
security-sensitive-paths list should gain `crates/tui/src/docker_panel.rs`
and `crates/tui/src/k8s_panel.rs` once this lands — same subprocess-
argument-construction surface `cargo_panel.rs` is already listed for. Will
be added alongside the implementation commit, not as a doc-only change
ahead of code that doesn't exist yet.
