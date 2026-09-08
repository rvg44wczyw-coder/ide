# GUI: AI Orchestration — Hybrid Chat + FIM + Task Routing (roadmap G9, new)

## 1. Purpose

`ide-ai` (`crates/ai`) and `ide-sanitizer` (`crates/sanitizer`) were built in
T49 (`docs/features/tui-ai-hybrid-fallback.md`) as **frontend-independent**
libraries specifically so a GUI integration would be thin — `crates/ai/src/
lib.rs`'s own crate doc comment says so in as many words: *"Frontend-
independent: TUI-first, GUI later."* T55
(`docs/features/tui-ai-task-routing.md`) added per-role provider/model
routing plus an opt-in classifier on top, entirely inside `ide-ai`. Neither
ever landed in `ide-ui`: `crates/ui/Cargo.toml` has no dependency on
`ide-ai` or `ide-sanitizer` at all today (verified), and `ide-ui`'s only AI
integration is `claude_panel.rs`/`claude_terminal.rs`, both of which shell
out to the external `claude` CLI — a completely different mechanism (no
direct provider HTTP calls, no sanitizer, no routing). This doc is "GUI
later," the promised second half.

**This is additive, not a replacement.** `claude_panel.rs`/
`claude_terminal.rs` are untouched by this doc and keep working exactly as
they do today — a separate `ToolWindow::Claude` overlay shelling out to a
local `claude` CLI process. The feature this doc adds is a second,
independent AI surface: a normal bottom-dock tab (`BottomView::Ai`,
alongside `CustomActions`/`Todo`/`Log`) that talks directly to
Ollama/Gemini/Groq/GitHub Models via `ide-ai`'s HTTP client, with the same
sanitizer-gated cloud egress and hybrid fallback chain the TUI already has.
A user can have both open at once; they share no state.

**Scope: T49 + T55, not T56.** `AiConfig` (`crates/ai/src/project.rs`)
already carries an `agent_mode: PermissionMode` field added for T56
("Local agentic IDE assistant," `docs/features/tui-local-agent.md`) — the
tool-calling agent loop (`ide-agent`, `crates/tui/src/agent_panel.rs`).
That field is simply along for the ride in every `AiConfig::load` this doc
performs; **the agent loop itself is explicitly out of scope for this
run**. Porting T56 to the GUI (a much larger surface — tool execution,
approval prompts, the `RunShellCommand` allowlist) is a plausible follow-up
phase, not bundled here.

**Scope decision — hoist two functions into `ide-ai` instead of
duplicating them (recorded for reviewability).** `crates/tui/src/
ai_panel.rs` currently keeps `decide_threshold` and `mask_outgoing` as
`pub(crate)` functions, reused within `ide-tui` itself by `agent_panel.rs`
(`crates/tui/src/agent_panel.rs:43`, `use crate::ai_panel::{decide_threshold,
mask_outgoing};`). Both are pure, network-free, and touch only
`ide-ai`/`ide-sanitizer` types (`AiConfig`, `ProviderId`,
`ide_sanitizer::Sanitizer`) — nothing TUI-specific. Copying their bodies
into a new `crates/ui/src/ai_panel.rs` instead of sharing one
implementation would create exactly the kind of drift risk
`custom-actions.md` §4 already flagged for its own JSON-shape duplication
(a future change to the masking-threshold rule in one frontend silently
not applying to the other) — except here the stakes are the sanitizer
guarantee itself, not a config file format. This doc instead moves both
functions into `ide-ai` (§2.1) as `pub fn`s, and updates `ide-tui`'s
`ai_panel.rs`/`agent_panel.rs` to import them from there instead of the
local copies, which are deleted. This is a `rust-tui-dev`-owned,
behavior-preserving refactor (two functions relocated, call sites updated,
no logic change) that must land — and its tests stay green — before
`rust-ui-dev`'s new file can depend on the same functions. `settle` (the
per-frontend `Result<ProviderId, AiError>` → display-message-list adapter)
is **not** hoisted — it constructs each frontend's own `*DisplayMessage`
enum, which is genuinely frontend-specific, the same "thin adapter, not
shared logic" reasoning `custom-actions.md` used for its own
`spawn_streaming`.

**Branding**: no "Orbit" string anywhere (standing project rule, T41/T42/
Ember/G3/G8).

**Security posture, stated up front — `hacker` pass is mandatory.** This
feature sends project source (chat prompts, selections, whole files, FIM
context) to external cloud LLM endpoints over TLS, exactly the same surface
T49/T55 already declared sensitive for the TUI. Root `CLAUDE.md`'s
security-sensitive-paths list is updated as part of this doc's own landing
(§2.6) to add `crates/ui/src/ai_panel.rs`, and — closing a pre-existing gap
noticed while researching this doc — `crates/ai/**` and
`crates/sanitizer/**` themselves, which T49/T55 never actually added despite
both docs declaring `hacker` mandatory for the crates they created.

## 2. Interface / API

### 2.1 `ide-ai` (`crates/ai`, first role, `rust-tui-dev` — small refactor)

Move (not duplicate) two functions from `crates/tui/src/ai_panel.rs` into
`crates/ai/src/lib.rs`, made `pub` (both are currently `pub(crate)` there):

```rust
/// §3.3's masking rule: any cloud provider in the enabled chain forces the
/// (tighter) cloud threshold; a local-only chain masks at the local
/// threshold only when `sanitize_local` is set; local with sanitizing off
/// sends raw. Returns `None` for the only unmasked case.
pub fn decide_threshold(config: &AiConfig, order: &[ProviderId]) -> Option<f64> {
    let has_cloud = order.iter().any(|id| !matches!(id, ProviderId::OllamaLocal));
    if has_cloud {
        Some(config.cloud_sanitize_threshold)
    } else if config.sanitize_local {
        Some(config.local_sanitize_threshold)
    } else {
        None
    }
}

/// The outbound half of a request: sanitize `payload` when a threshold
/// applies, keeping the roundtrip map so the reply can be restored later;
/// pass through untouched when unmasked.
pub fn mask_outgoing(
    payload: String,
    threshold: Option<f64>,
) -> (String, Option<std::collections::HashMap<String, String>>) {
    let mut sanitizer = ide_sanitizer::Sanitizer::new();
    match threshold {
        Some(t) => {
            let out = sanitizer.mask_with_threshold(&payload, t);
            (out.masked, Some(ide_sanitizer::as_map(&sanitizer)))
        }
        None => (payload, None),
    }
}
```

Bodies are copied verbatim (no logic change) from the current
`crates/tui/src/ai_panel.rs:263-294`. `ide-ai` already depends on
`ide-sanitizer` (feature `sanitizer`, default on) for this exact purpose —
no new dependency.

`crates/tui/src/ai_panel.rs`: delete the local `decide_threshold`/
`mask_outgoing` definitions, replace with `use ide_ai::{decide_threshold,
mask_outgoing, /* existing imports */};`. `crates/tui/src/agent_panel.rs:43`'s
own `use crate::ai_panel::{decide_threshold, mask_outgoing};` becomes `use
ide_ai::{decide_threshold, mask_outgoing};`. Every existing test in both
files that exercises these two functions moves with them (or is replaced by
equivalent tests added to `crates/ai/src/tests.rs` — either is acceptable,
`rust-tui-dev`'s call, as long as coverage doesn't regress). No behavior
change anywhere; `cargo test --workspace` must stay green with identical
pass/fail outcomes.

### 2.2 New file `crates/ui/src/ai_panel.rs` (second role, `rust-ui-dev`)

Same shapes as `crates/tui/src/ai_panel.rs`'s `AiDisplayMessage`/
`AiContext`/`PreparedRequest`, with one deliberate deviation from the TUI
version, explained below.

```rust
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use ide_ai::{
    classify_task_role, decide_threshold, mask_outgoing, resolve_role_route, AiConfig, AiError,
    ChatMessage, DefaultRouter, ProviderId, Router, TaskRole,
};
use ide_sanitizer::restore_originals;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiDisplayMessage {
    User(String),
    Assistant(String),
    StreamingDelta(String),
    ProviderServing(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub enum AiContext {
    Selection(String),
    WholeFile(String),
    None,
}

pub struct PreparedRequest {
    payload: String,
    order: Vec<ProviderId>,
    threshold: Option<f64>,
    config: AiConfig,
    tx: Sender<AiDisplayMessage>,
}

type AiRunner = fn(PreparedRequest);

pub struct AiPanel {
    pub input: String,
    pub history: Vec<AiDisplayMessage>,
    pub sanitized: bool,
    pub(crate) provider: Option<String>,
    streaming: bool,
    rx: Option<Receiver<AiDisplayMessage>>,
    runner: AiRunner,
}

impl Default for AiPanel {
    fn default() -> Self { Self::with_runner(run_request) }
}

impl AiPanel {
    fn with_runner(runner: AiRunner) -> Self { /* all fields zeroed/empty */ }

    pub fn is_in_flight(&self) -> bool { self.rx.is_some() }

    /// Manual recovery backstop, identical contract to `ide-tui`'s own
    /// `cancel` (`crates/tui/src/ai_panel.rs:134`): drops the receiver so
    /// `poll` stops waiting on it and a fresh `submit` can start
    /// immediately. The orphaned background thread keeps running until its
    /// own transport timeout or a failed `tx.send`.
    pub fn cancel(&mut self) { self.rx = None; self.streaming = false; }

    fn compose(&self, prompt: String, context: AiContext) -> Option<String> { /* identical to TUI */ }

    /// **Deviation from `ide-tui`'s `AiPanel`**: takes `project_root: &Path`
    /// as a parameter instead of storing a `root: PathBuf` field set once at
    /// construction. `ide-tui`'s `App::new` runs once per process (no
    /// mid-session project switching), so storing `root` on the panel is
    /// safe there; `ide-ui` opens/switches/creates projects throughout a
    /// session (`load_project_settings`, doc §2.3 below), and this crate's
    /// established convention for exactly this situation is to *not* cache
    /// a project root on a panel struct at all — `CargoPanel::run` and
    /// `CustomActionsPanel::run_selected` both take `project_root: &Path`
    /// per call (`crates/ui/src/cargo_panel.rs:54`,
    /// `crates/ui/src/custom_actions.rs`) rather than storing it. This
    /// doc's `AiPanel` follows that existing convention rather than
    /// inventing a third pattern.
    fn prepare(
        &mut self,
        prompt: String,
        context: AiContext,
        project_root: &Path,
    ) -> Option<PreparedRequest> {
        let payload = self.compose(prompt, context)?;
        if self.is_in_flight() { return None; }
        self.history.push(AiDisplayMessage::User(payload.clone()));
        let config = AiConfig::load(project_root);
        let order = config.enabled_providers();
        let threshold = decide_threshold(&config, &order);
        self.sanitized = threshold.is_some();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.streaming = false;
        Some(PreparedRequest { payload, order, threshold, config, tx })
    }

    /// Same no-queue, no-op-while-in-flight v1 scope as `ClaudePanel::submit`
    /// (`crates/ui/src/claude_panel.rs`) and `CargoPanel::run`/
    /// `CustomActionsPanel::run_selected` — silent no-op, no status message,
    /// matching this crate's existing precedent for "an action is already
    /// running" rather than inventing a notification mechanism `ide-ui` has
    /// no equivalent of (§3).
    pub fn submit(&mut self, prompt: String, context: AiContext, project_root: &Path) {
        let Some(prepared) = self.prepare(prompt, context, project_root) else { return };
        let runner = self.runner;
        thread::spawn(move || runner(prepared));
    }

    /// Identical contract to `ide-tui`'s own `poll`
    /// (`crates/tui/src/ai_panel.rs:193`): drains the channel, returns
    /// `true` if `history`/status changed.
    pub fn poll(&mut self) -> bool { /* identical to TUI's poll/ingest */ }
}

fn settle(
    accumulated: String,
    map: &Option<HashMap<String, String>>,
    outcome: Result<ProviderId, AiError>,
    role_label: Option<&str>,
) -> Vec<AiDisplayMessage> { /* identical to TUI's settle, same signature */ }

fn run_request(prepared: PreparedRequest) { /* identical to TUI's run_request:
    mask_outgoing → classify_task_role (if auto_route) → resolve_role_route →
    DefaultRouter::chat → forward StreamingDelta live → settle */ }
```

`ingest` (the per-message `history` update) is copied unchanged from
`crates/tui/src/ai_panel.rs:216-252` — it only touches `history`/
`streaming`, no TUI-specific state.

### 2.3 `crates/ui/src/app.rs` (`rust-ui-dev`)

`BottomView` (`app.rs:208`, currently nine variants ending in `Log`) gains a
tenth, `Ai`:

```rust
pub enum BottomView {
    Problems,
    CargoOutput,
    Usages,
    Search,
    Debug,
    CustomActions,
    Todo,
    Log,
    /// AI Orchestration chat: hybrid local/cloud provider chat via
    /// `ide-ai` (`docs/features/gui-ai-orchestration.md`, G9) -- distinct
    /// from the `ToolWindow::Claude` overlay, which shells to the external
    /// `claude` CLI and is untouched by this feature.
    Ai,
}
```

`IdeApp` (`app.rs:839`) gains:

```rust
ai: crate::ai_panel::AiPanel,
/// FIM autocomplete's in-flight request (doc §3.4). `None` when idle.
fim_rx: Option<std::sync::mpsc::Receiver<Result<String, ide_ai::AiError>>>,
/// `(path, byte offset)` recorded at request time -- copied, never read
/// live, mirroring `AiContext`'s own "copied at submit time" invariant
/// (§3.3 of `tui-ai-hybrid-fallback.md`). Consumed by `poll_fim` when the
/// result arrives; a path mismatch against the now-active tab drops the
/// insertion (mirrors `ide-tui`'s own `apply_fim_insert`,
/// `crates/tui/src/app.rs:1736`, "a tab switch mid-request drops the
/// stale insertion").
fim_target: Option<(PathBuf, usize)>,
```

Both `ai: AiPanel::default()`/`fim_rx: None`/`fim_target: None` added
alongside `claude: ClaudePanel::default()` at every `IdeApp` construction
site (`app.rs:1237` production `new`, `app.rs:5586` test helper — both
verified present).

`load_project_settings` (`app.rs:2021`) gains **no new lines** — unlike
`custom_actions`/`custom_actions_popup`, `AiPanel` has no project-scoped
list to reload (`AiConfig::load` is called fresh inside `prepare` on every
`submit`, not cached on the panel) and no popup state whose index could go
stale across a project switch. `self.ai.history` is **not** reset on
project switch, deliberately: this mirrors `self.cargo`/`self.claude`'s own
existing untouched-across-project-switch behavior (`custom-actions.md` §2.2
already established this precedent and its reasoning for this exact
crate). `fim_rx`/`fim_target` are likewise left untouched on project
switch — an in-flight FIM request either completes and is dropped by the
path-mismatch guard, or (same as any other in-flight background request in
this crate) keeps running harmlessly.

New methods:

- `current_ai_context(&self) -> crate::ai_panel::AiContext` — ports
  `ide-tui`'s `current_ai_context` (`crates/tui/src/app.rs:2833-2844`)
  verbatim in spirit, adapted to this crate's `Tab`/`active_tab` shape
  instead of TUI's `active_buffer()`:

  ```rust
  fn current_ai_context(&self) -> crate::ai_panel::AiContext {
      let Some(idx) = self.active_tab else {
          return crate::ai_panel::AiContext::None;
      };
      let buf = &self.tabs[idx].buffer;
      let text = buf.text().to_string();
      let selection = buf.text_buffer().selections().primary();
      if selection.is_empty() {
          crate::ai_panel::AiContext::WholeFile(text)
      } else {
          crate::ai_panel::AiContext::Selection(text[selection.start()..selection.end()].to_string())
      }
  }
  ```

  `Buffer::text_buffer()`/`TextBuffer::selections()`/`Selections::primary()`
  are `ide_core::text` types already shared by both frontends (verified:
  `crates/core/src/buffer.rs:89`, `crates/core/src/text/mod.rs:96`) — no
  `ide-core` change needed.

- `trigger_fim_autocomplete(&mut self)` — ports `ide-tui`'s
  `trigger_fim_autocomplete` (`crates/tui/src/app.rs:1749-1771`) to this
  crate's `Tab`/`active_cursor_offset` shape:

  ```rust
  fn trigger_fim_autocomplete(&mut self) {
      if self.fim_rx.is_some() {
          self.error = Some("FIM autocomplete already in progress".to_string());
          return;
      }
      let Some(idx) = self.active_tab else {
          self.error = Some("no active file to complete".to_string());
          return;
      };
      let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) else {
          self.error = Some("no active file to complete".to_string());
          return;
      };
      let text = self.tabs[idx].buffer.text().to_string();
      let offset = self.active_cursor_offset.unwrap_or(text.len());
      let (tx, rx) = std::sync::mpsc::channel();
      self.fim_rx = Some(rx);
      self.fim_target = Some((path, offset));
      let prefix = text[..offset].to_string();
      let suffix = text[offset..].to_string();
      std::thread::spawn(move || {
          let provider = ide_ai::Provider::from_id(ide_ai::ProviderId::OllamaLocal);
          let rt = tokio::runtime::Builder::new_current_thread()
              .enable_io().enable_time().build()
              .expect("current-thread tokio runtime builds");
          let response = rt.block_on(provider.complete_fim(&prefix, &suffix));
          let _ = tx.send(response);
      });
  }
  ```

  Ollama-only, exactly like the TUI (§1's scope: FIM has no cloud-provider
  contract — see `docs/features/tui-ai-hybrid-fallback.md` §2.3's
  `complete_fim`, unchanged by this doc).

- `poll_fim(&mut self) -> bool` — ports `ide-tui`'s `poll_fim`
  (`crates/tui/src/app.rs:1703-1727`) 1:1, substituting `self.error =
  Some(...)` for `self.notify(...)` (`ide-ui` has no `notify` mechanism;
  `self.error` is this crate's one-line status-bar field, already rendered
  at `app/render.rs:514`) and `self.apply_fim_insert` below for
  `apply_fim_insert`. Returns `true` when it consumed a result (insertion
  applied, or an error/disconnect was recorded) so the caller can
  `ctx.request_repaint()`.

- `apply_fim_insert(&mut self, path: &Path, offset: usize, text: &str)` —
  ports `ide-tui`'s `apply_fim_insert` (`crates/tui/src/app.rs:1729-1741`):
  no-op on empty `text`; finds the tab whose `buffer.path() == Some(path)`
  (by path, not by the now-possibly-stale `active_tab` index — a tab could
  have been reordered or the active tab changed since the request was
  sent) and applies `ide_core::text::Transaction::insert(offset, text)` via
  `buffer.apply(...)`; no-op (silently drops the stale insertion) if no tab
  matches `path` any more (tab closed) — same "a tab switch mid-request
  drops the stale insertion" contract as the TUI. Does **not** re-validate
  `offset` against the buffer's current length beyond what `Transaction::
  insert`'s own bounds-checking already does — this is an accepted,
  unguarded v1 simplification carried over unchanged from the TUI (its own
  `apply_fim_insert` has the identical gap; not introduced by this port).

`run_command`/`is_command_enabled` gain matching arms:

```rust
// is_command_enabled
CommandAction::ToggleAiToolWindow => self.project.is_some(),
CommandAction::TriggerFimAutocomplete => {
    self.active_tab.is_some() && self.view_mode == ViewMode::Editor
}

// run_command
CommandAction::ToggleAiToolWindow => self.toggle_bottom_tool_window(BottomView::Ai),
CommandAction::TriggerFimAutocomplete => self.trigger_fim_autocomplete(),
```

`TriggerFimAutocomplete`'s gate mirrors `GoToLine`'s existing shape exactly
(`app.rs:4907-4909`, "an active tab in Editor view, nothing else") rather
than `RefactorThis`/`GenerateMenu`'s LSP-running gate (`app.rs:4918-4934`)
— FIM needs no language server at all, only a caret position.

### 2.4 `crates/ui/src/command.rs` (`rust-ui-dev`)

Two new `CommandAction` variants, two new `Command` entries, both
`binding: None` (same "no reference-IDE precedent" reasoning
`ToggleClaudeToolWindow`/`ManageCustomActions` already state for
themselves — `custom-actions.md` §2.3):

```rust
Command {
    id: "ToggleAiToolWindow",
    title: "AI Orchestration",
    category: "Window",
    binding: None,
    action: CommandAction::ToggleAiToolWindow,
},
Command {
    id: "TriggerFimAutocomplete",
    title: "AI: Complete at Cursor",
    category: "Refactor",
    binding: None,
    action: CommandAction::TriggerFimAutocomplete,
},
```

`category: "Window"` matches every other `ToggleXToolWindow` entry.
`category: "Refactor"` for the FIM trigger matches the closest existing
occupant of that category for a single-shot code-generating action
(`GenerateMenu`/`ImplementMethods`/`CreateTest`, all `"Refactor"` —
verified `command.rs:884-900`), not `"Build"` (Cargo's fixed subcommands)
or a new category invented for one command.

### 2.5 `crates/ui/src/app/render.rs` (`rust-ui-dev`)

The bottom-panel tab row (`render_bottom_panel`, `app/render.rs:4726`,
currently eight `render_boxed_tab` calls ending in `Log`) gains a ninth,
`"AI"`, and the trailing `match self.bottom_view { ... }` (`app/
render.rs:4820-4827`) gains `BottomView::Ai => self.render_ai_panel(ctx,
ui)`.

New `render_ai_panel(&mut self, ctx: &egui::Context, ui: &mut egui::Ui)`,
following `render_claude_chat`'s exact structure (`app/render.rs:3782-
3815`): a scrollable history list (`User`/`Assistant`/`StreamingDelta`-
merged-into-the-open-`Assistant`-entry/`Error` rows, error rows styled via
`self.theme.tokens().color.danger` same as `render_claude_chat`'s `danger`
local), a status line showing `self.ai.provider`/`self.ai.sanitized` when
set, and an input row:

```rust
ui.horizontal(|ui| {
    let response = ui.text_edit_singleline(&mut self.ai.input);
    let submitted = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if (ui.button("Send").clicked() || submitted) && !self.ai.input.trim().is_empty() {
        if let Some(root) = self.project.as_ref().map(|p| p.root().to_path_buf()) {
            let prompt = std::mem::take(&mut self.ai.input);
            let context = self.current_ai_context();
            self.ai.submit(prompt, context, &root);
        }
    }
});
```

matching `render_claude_chat`'s `self.claude.submit(prompt)` call site
(`app/render.rs:3804-3810`) and `render_custom_actions_panel`'s
`Option<PathBuf>`-then-unwrap project-root pattern (`app/render.rs:3465-
3474`).

`self.ai.poll()`/`self.poll_fim()` are added to the **unconditional**
per-frame poll section (`app/render.rs:4959-4964`, right after
`self.custom_actions.poll()`), not gated on `BottomView::Ai` being the
visible tab:

```rust
if self.ai.poll() {
    ctx.request_repaint();
}
if self.poll_fim() {
    ctx.request_repaint();
}
```

This deliberately follows `cargo`/`custom_actions`/`claude_terminals`'s
"drain every frame regardless of visibility" precedent, **not**
`render_claude_chat`'s own poll-only-while-visible pattern
(`self.claude.poll()` at `app/render.rs:3784`, inside the render function
itself). The reason: `ClaudePanel`'s channel carries at most one message
per request (whole-reply, not streamed), so leaving it undrained while the
tab is closed bounds to one queued message; `AiPanel`'s channel carries
many `StreamingDelta`s per reply (§3.2) and is otherwise structurally
identical to `claude_terminals`' PTY-output channel, whose own doc comment
(`app/render.rs:4965-4971`) explicitly warns this exact unbounded-growth
shape was already a real, fixed hacker finding for that panel
(`docs/security-findings/rust-ui-dev-claude-terminal-2026-08-25.md`,
finding 3) — polling unconditionally here from the start avoids
reintroducing the same class of bug.

### 2.6 `crates/ui/src/app/menu.rs` (`rust-ui-dev`)

`every_non_build_command_appears_in_the_native_menu_exactly_once`
(`menu.rs:331-346`) asserts every `CommandAction` with `category !=
"Build"` appears in exactly one `MenuGroup` (or `APP_MENU_ITEMS`). Both new
commands (§2.4) are non-`Build`, so both are required entries here — this
is a real, test-enforced integration point this crate's implementation has
already missed once before for an identically-shaped pair of new dock/
window commands (`custom-actions.md`'s own Revision notes describe finding
this gap during `rust-ui-dev`'s implementation for `ManageCustomActions`/
`ToggleCustomActionsToolWindow`, not catching it at doc time).

`"View"` group's `items` (`menu.rs:64-80`) gains `Some("ToggleAiToolWindow")`,
placed with the other `Some("ToggleXToolWindow")` entries (after
`Some("ToggleCustomActionsToolWindow")`, before `Some("ToggleTodoToolWindow")`
— alphabetically arbitrary, matching this list's existing lack of a strict
ordering rule beyond "toggles grouped together").

`"Edit"` group's `items` (`menu.rs:28-63`) gains
`Some("TriggerFimAutocomplete")`, placed after the `Some("GenerateMenu")`/
`Some("ImplementMethods")`/`Some("OverrideMethods")`/`Some("CreateTest")`/
`Some("OptimizeImports")` block — the closest existing group of
single-shot, code-generating actions, matching `command.rs`'s own
`category: "Refactor"` choice for this command (§2.4).

### 2.7 Root `CLAUDE.md` (project-level, orchestrator edit)

Adds three bullets to the existing "Security-sensitive paths" list:

> - `crates/ui/src/ai_panel.rs` — sends chat/selection/whole-file/FIM
>   context to local and cloud LLM providers over HTTP via `ide-ai`, the
>   GUI counterpart of `crates/tui/src/ai_panel.rs` (T49/T55). Same
>   sanitizer-gated cloud-egress surface, credential handling, and
>   streaming/roundtrip bounds already audited on the TUI side
>   (`docs/features/gui-ai-orchestration.md`).
> - `crates/ai/**` — the shared HTTP provider layer both frontends now
>   depend on directly (credential handling, cloud dispatch, the sanitizer
>   gate, the task-routing classifier). Declared `hacker`-mandatory by
>   both `tui-ai-hybrid-fallback.md` and `tui-ai-task-routing.md`, but
>   never actually added to this list until now — a pre-existing gap
>   closed as part of this doc's own landing.
> - `crates/sanitizer/**` — the masking guarantee every cloud dispatch in
>   `crates/ai/**` depends on. Same gap as above, closed here.

Also updates the six `AI-hybrid-fallback (TUI-first)` dependency-table rows
(`tokio`, `hyper`, `http-body-util`+`bytes`, `hyper-util`, `hyper-rustls`,
`syn`; `CLAUDE.md:297-302`) to drop the now-stale `(TUI-first)`
qualifier — these crates are consumed by `ide-ui` as of this feature too.
No new external dependency is being approved here: every one of these
crates is already in the workspace, already vetted, and `ide-ui`'s new
`Cargo.toml` lines (§6) only add path-dependencies on the already-existing
`ide-ai`/`ide-sanitizer` crates, which pull the rest in transitively.

## 3. Behaviour

- Running a request never happens as a side effect of `load_project_
  settings`, `IdeApp::new`, or opening the AI dock tab — only the Send
  button (or Enter) triggers `AiPanel::submit`.
- At most one chat request in flight at a time; a second Send while one is
  in flight is a silent no-op (§2.2) — same v1 scope as `ClaudePanel`/
  `CargoPanel`/`CustomActionsPanel`, not re-argued here.
- `current_ai_context()` is computed fresh at submit time from whatever
  the active tab's buffer and primary selection are *then* — never read
  again once the request is in flight (§3.3 of `tui-ai-hybrid-fallback.md`'s
  "copied at submit time" invariant, carried over unchanged).
- FIM: at most one in-flight FIM request at a time (`fim_rx.is_some()`
  gate, §2.3); the result is applied against whichever tab still has the
  recorded `path` open when it arrives, or silently dropped if that tab
  was closed. Unlike the chat path, FIM is **not** gated by `is_in_flight`
  on `AiPanel` — it's a fully independent request/response cycle with its
  own `fim_rx`/`fim_target` pair, so a chat request and a FIM request can
  be in flight at the same time (matches `ide-tui`, where `fim_rx` and
  `AiPanel`'s own `rx` are likewise independent fields).
- `self.ai.history` and `self.ai.sanitized`/`self.ai.provider` persist
  across a project switch (§2.3) — an open reply keeps streaming into the
  same history even if the user switches projects mid-request, identical
  to `CargoPanel`'s already-existing cross-project-switch behavior.
- Router, sanitizer, and classifier behavior (fallback order, thresholds,
  `auto_route`/`role_routes` resolution, timeouts, buffer caps) are
  **entirely unchanged from T49/T55** — this doc adds no new behavior to
  `ide-ai` beyond the §2.1 function relocation. `.ide/ai.json` is the same
  file, same schema, shared between `ide-tui` and `ide-ui` on the same
  project the same way `.ide/custom_actions.json` already is
  (`custom-actions.md` §3) — a role/provider config edited via one
  frontend (there is still no settings UI for either, per T49/T55's own
  cuts) takes effect in the other the next time that frontend reloads
  project settings.

## 4. Constraints and invariants

- **`hacker` pass mandatory** (§1). Verifies the sanitizer guarantee holds
  identically through the new GUI call path, that `crates/ui/src/
  ai_panel.rs` never dispatches a payload that skipped `mask_outgoing`
  when a threshold applies, and that the §2.1 function relocation didn't
  change any of T49/r6's or T55's already-audited behavior (timeouts,
  buffer caps, credential handling, `classifier_provider.enabled()`
  pre-check).
- **No behavior change from the §2.1 refactor.** `decide_threshold`/
  `mask_outgoing`'s bodies are moved verbatim; `rev` should diff them
  against the pre-move versions to confirm byte-identical logic, not just
  that tests still pass.
- **`fim_target` is a `(PathBuf, usize)`, never re-derived from the
  now-current buffer state at apply time** — the offset that was valid
  when the request was sent is used as-is; if the buffer changed shape in
  the meantime (more/fewer characters before that offset), the insertion
  can land in the wrong place. This is an accepted, unguarded v1
  simplification inherited unchanged from `ide-tui`'s own
  `apply_fim_insert` (§2.3) — not a new gap this doc introduces, and not
  silently fixed here without a corresponding TUI-side fix, which would be
  a separate, unrequested change.
- **No new persisted secrets.** Same credential model as T49: env-var-only
  (`GEMINI_API_KEY`/`GROQ_API_KEY`/`GITHUB_MODELS_TOKEN`), never written to
  `.ide/ai.json`, never logged.
- **`AiConfig`'s `agent_mode` field is read (via `AiConfig::load`) but
  never acted on** by anything this doc adds — no tool-execution path
  exists in `ide-ui` yet (§1's scope cut). A user who sets `agent_mode` in
  `.ide/ai.json` expecting GUI agent behavior gets none from this feature;
  that's out of scope, not a bug to mask with a warning this run.
- No "Orbit" branding string anywhere (§1).

## 5. Examples

```rust
// ai_panel.rs unit test shape, mirrors ide-tui's own
// prepare_pushes_user_message_and_chooses_config_driven_threshold.
let dir = tempfile::tempdir().unwrap();
let mut panel = AiPanel::with_runner(fake_runner_that_echoes_a_reply);
panel.submit("explain this".into(), AiContext::None, dir.path());
assert!(panel.is_in_flight());
wait_until(|| { panel.poll(); !panel.is_in_flight() });
assert!(panel
    .history
    .iter()
    .any(|m| matches!(m, AiDisplayMessage::Assistant(_))));
```

Manual flow (description): open the AI Orchestration dock tab (command
palette "AI Orchestration", or click its tab if already open) → type a
prompt → Send (or Enter) → reply streams in token-by-token → status line
shows which provider served it (and, if `auto_route` is on in
`.ide/ai.json`, the classified role, e.g. "Gemini (coding)"). Separately:
place the caret somewhere in an open file → run "AI: Complete at Cursor"
from the command palette → the local Ollama FIM model's completion is
inserted at the caret as a single undo step, independent of whether the AI
dock tab is even open.

## 6. Dependencies & integration points

**`crates/ui/Cargo.toml`** gains two path dependencies, matching
`crates/tui/Cargo.toml`'s existing lines exactly:

```toml
ide-ai = { path = "../ai" }
ide-sanitizer = { path = "../sanitizer" }
```

`tokio` (full features) is needed directly by `crates/ui/src/app.rs`'s
`trigger_fim_autocomplete` (builds its own current-thread runtime, mirroring
`ide-tui`'s `fim_runtime()`, `crates/tui/src/app.rs:1773-1779`) and by
`crates/ui/src/ai_panel.rs`'s `run_request` (same pattern as `crates/tui/
src/ai_panel.rs`'s own runtime-per-request-thread). `tokio` is already an
approved workspace dependency (§2.6); this adds it as a **direct**
dependency of `ide-ui` for the first time (previously only transitive via
`ide-tui`), so `crates/ui/Cargo.toml` gains:

```toml
tokio = { version = "1", features = ["full"] }
```

No other new external crates. `hyper`/`hyper-rustls`/`hyper-util`/
`http-body-util`/`bytes`/`syn`/`regex` all stay internal to `ide-ai`/
`ide-sanitizer` — `ide-ui` never touches them directly.

**Role merge order:**

1. `rust-tui-dev` — §2.1's function relocation (`ide-ai` gains `pub fn
   decide_threshold`/`mask_outgoing`; `ide-tui`'s `ai_panel.rs`/
   `agent_panel.rs` updated to import them). Small, behavior-preserving;
   `ide-tui`'s full test suite must stay green.
2. `rust-ui-dev` — everything else (§2.2-§2.6): new `crates/ui/src/
   ai_panel.rs`, `app.rs`/`command.rs`/`app/render.rs`/`app/menu.rs`
   wiring, `Cargo.toml`. Depends on step 1 (imports `ide_ai::
   decide_threshold`/`mask_outgoing`).
3. `hacker` — mandatory (§1, §4).

## 7. Diagram

Skipped — no new diagram needed. The sanitize → route → stream → restore
choreography (§3.2/§3.3 of `tui-ai-hybrid-fallback.md`) and the
classify → resolve → route flow (§3.1 of `tui-ai-task-routing.md`) are
**unchanged** by this doc; both are already illustrated in
`diagrams/tui-ai-hybrid-fallback-sequence.png` and
`diagrams/tui-ai-task-routing-sequence.png`. This doc's own additions
(§2.3's `current_ai_context`/FIM request-response cycle) are small enough
to be fully covered in prose, the same "too small to benefit" reasoning
`custom-actions.md` §7 already used for a similarly-scoped change.

## Revision notes

- **r1 (2026-09-08, doc-review):** two gaps found, both fixed in place:
  (1) missed integration point — `crates/ui/src/app/menu.rs`'s
  `every_non_build_command_appears_in_the_native_menu_exactly_once` test
  requires both new non-`Build` commands (`ToggleAiToolWindow`,
  `TriggerFimAutocomplete`) to appear in a `MenuGroup`; the doc had no
  §2.x for this at all, the same gap `custom-actions.md` already hit once
  during implementation for an identically-shaped pair of commands —
  added §2.6 specifying the exact insertions; (2) §2.3 cited the wrong
  line numbers for the two `IdeApp` construction sites (`app.rs:1224`/
  `5573`, guessed from the struct's start rather than the actual
  `claude: ClaudePanel::default()` lines) — corrected to the verified
  `app.rs:1237`/`5586`. One controversial finding (the `rust-tui-dev`
  hoist of `decide_threshold`/`mask_outgoing` into `ide-ai` vs. just
  duplicating them) was raised and judged already-addressed: §1 already
  states the tradeoff and the reasoning (shared sanitizer-boundary logic,
  not ordinary code reuse) rather than asserting it as free.
