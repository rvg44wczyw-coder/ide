# TUI Formatting (T38)

## 1. Purpose

Ports `docs/features/formatting.md` (A9, `ide-ui`) into `ide-tui`: **Reformat
Code** and **Format on Save**, both driven by the already-merged
`ide_lsp::LspRequest::Format`/`LspEvent::FormatReady` wire protocol. Neither
`ide-core` nor `ide-lsp` gains anything new — this is entirely a
`crates/tui/**` port of a bridge/app-state shape `ide-ui` already
established, the same "zero new `ide-core` API" pattern
`tui-search-and-replace-in-path.md` (T37) and every prior LSP-feature TUI
port already followed.

### 1.1 Deliberate divergences from `formatting.md`

- **No `Option<PathBuf>` guard on the active tab's path.** `ide-ui`'s
  `trigger_reformat_code` is a no-op with "no active tab or no path
  (untitled buffer)" — `ide-tui`'s `OpenBuffer::path` is a plain `PathBuf`,
  never `Option<PathBuf>` (`ide-tui` has no untitled-buffer concept at
  all; every open tab already corresponds to a real file on disk). The
  TUI's guard is therefore simpler: no-op only when `self.active_tab` is
  `None`.
- **`format_on_save` persists through `crate::state::PersistedState`
  (`docs/features/tui-persist-last-project.md`), not `eframe::Storage`.**
  `ide-tui` has no `eframe` dependency; `state.rs`'s existing
  load-on-startup/save-on-exit JSON file at a per-user config path is the
  established substitute (its own module doc already says as much for
  `last_project`). `PersistedState` gains one more field,
  `format_on_save: bool` (`#[serde(default)]` so an existing state file
  without the field deserializes to `false`, not a hard error).
- **`save_tab_at(&mut self, idx: usize)` is a new extraction, not already
  present.** `ide-ui` already had `save_tab_at` before `formatting.md`
  landed (`code-actions.md`'s own apply pipeline needed it first);
  `ide-tui`'s save path (`trigger_save_active`, `app.rs`) has never needed
  a by-index variant before this feature, since every prior TUI feature
  that writes to disk either targets the file the user is actively
  editing synchronously (no round trip in between) or goes through
  `apply_workspace_edit`/`apply_file_edits`'s own disk/buffer partitioning
  (T37, code actions, rename) rather than the interactive save path. This
  phase extracts `save_tab_at` out of `trigger_save_active` for the same
  reason `ide-ui` needed it: Format on Save's follow-up save must target
  the tab that was actually reformatted, not whatever tab is active by
  the time the (at-least-one-frame-later) `FormatReady` response lands.
- **Applying the formatting edit reuses `apply_workspace_edit`
  (`tui-search-and-replace-in-path.md` §2.2/§2.3), not a direct
  `Buffer::apply` call.** `ide-ui`'s `handle_format_ready` calls
  `self.tabs[idx].buffer.apply(...)` directly because `ide-ui` had no
  generic disk/buffer-partitioning apply path until `code-actions.md`
  introduced one specifically for multi-file `WorkspaceEdit`s, and
  `formatting.md` §3.4 argues at length for why formatting's single-file
  edit doesn't need that generality. `ide-tui` already has
  `apply_workspace_edit`/`apply_file_edits` (T37) doing exactly that
  partitioning for every other `ide_lsp::WorkspaceEdit`-shaped result in
  this crate (code actions, rename) — reusing it here is strictly less
  code than adding a second, narrower apply path, and behaves identically
  for this feature's actual shape: the formatted file is always the
  active tab's own open buffer (§4's invariant, unchanged from `ide-ui`'s),
  so `apply_workspace_edit`'s disk branch is simply never exercised in
  practice, exactly as `ide-ui`'s narrower direct-apply already assumed.

Everything else — the wire protocol, capability negotiation, response
handling, the Format-on-Save timing argument (§3.4 below) — is unchanged
from `formatting.md` and not re-litigated here beyond what differs.

Not security-sensitive per `CLAUDE.md`'s declared list: this diff touches
none of `crates/lsp/**` (the `Format`/`FormatRange`/`FormatReady` wire
types and capability negotiation already exist, unchanged, from `ide-ui`'s
A9 round — `rust-lsp-dev` is not part of this run), and it invokes no
external formatter subprocess of its own — every formatting result still
comes from whatever language server is already connected over the
existing LSP request/response path, the identical reasoning
`formatting.md` §6 already gives for why `ide-ui`'s own half of this
feature doesn't trigger `hacker` either. `crates/tui/src/app.rs` and
`crates/tui/src/lsp_bridge.rs` are not on `CLAUDE.md`'s declared list in
general (unlike `crates/ui/src/lsp_bridge.rs`, named specifically for its
now-configurable LSP *launch command* — this phase touches no launch-
command code, only a request/response pair over an already-started
connection) — `hacker` is skipped for this role, per the same
independent-recheck-against-the-actual-diff discipline `formatting.md` §6
already establishes rather than assumed by default.

This diff *does* add a third call site into `apply_file_edits`, which
`CLAUDE.md` explicitly names as security-sensitive alongside
`confirm_replace_in_path_preview` (T37) — worth addressing directly rather
than silently missing, since it's the one already-declared-sensitive
function this feature actually touches. The reason that listing doesn't
extend to this new caller: `CLAUDE.md`'s own rationale for flagging
`apply_file_edits` is specifically about *glob/regex-driven candidate-file
selection* — Replace in Path builds its `WorkspaceEdit` from an
open-ended, user-typed-pattern-matched set of files, which is the actual
trust boundary being guarded. Reformat Code's `WorkspaceEdit` always names
exactly one file — the active tab's own already-open path (§4's
invariant), chosen by tab selection, never by pattern-matching a candidate
set. There is no glob, no regex, no expandable file set anywhere in this
feature's call into `apply_workspace_edit`/`apply_file_edits` — the risk
that motivated the original listing structurally doesn't apply to this
caller. `hacker` stays skipped on that basis, not by omission.

## 2. Interface / API

### 2.1 `ide-lsp`

No changes. `LspRequest::Format`/`FormatRange`, `LspEvent::FormatReady`,
and the `document_formatting_provider`/`document_range_formatting_provider`
capability flags on `ConnectionState` (`formatting.md` §2.1/§3.2) are
reused exactly as `ide-ui` already uses them — a second, independent
caller of an unchanged public API, the same relationship T37's
`ide-tui`-side diff already has to `ide_core::search_in_path`.

### 2.2 `ide-core`

No changes.

### 2.3 `ide-tui`

```rust
// crates/tui/src/lsp_bridge.rs -- additions to the existing LspBridge,
// mirroring every other request_*/*_ready pair already in this struct
// (docs/features/tui-code-actions-and-rename.md §2.3 established the
// shape this follows).
pub(crate) struct LspBridge {
    // ... existing fields ...

    /// The outcome of the most recently sent, not-yet-superseded
    /// `Format`/`FormatRange` query -- replaced wholesale on each
    /// `LspEvent::FormatReady`, cleared at send-time and on
    /// stop/`ServerExited` (same convention `workspace_edit`/`code_
    /// actions` already follow in this struct).
    format_edit: Option<ide_lsp::WorkspaceEdit>,
    /// The path `format_edit` answers -- mirrors `workspace_edit_label`'s
    /// staleness-guard role for every other per-path response in this
    /// struct.
    format_path: Option<PathBuf>,
    /// True for exactly one frame, the one in which a `FormatReady` event
    /// was drained -- reset to `false` at the top of every `poll()` call,
    /// same one-frame-true edge every other `*_ready` flag in this struct
    /// already establishes.
    pub(crate) format_ready: bool,
}

impl LspBridge {
    /// `tab_size`/`insert_spaces` come from the caller's already-resolved
    /// `ide_core::IndentUnit` (`OpenBuffer::indent`, computed per-tab from
    /// `.editorconfig` -- `tui-line-commands-and-editorconfig.md` §3.6) --
    /// `ide-lsp` has no dependency on `ide-core` and cannot resolve this
    /// itself.
    ///
    /// Unlike every other `request_*` method on this type, a missing
    /// client is **not** a silent no-op: it immediately sets
    /// `format_ready = true`, `format_edit = None`,
    /// `format_path = Some(path.to_path_buf())`, entirely inside
    /// `LspBridge` -- the same observable outcome an unsupported-
    /// capability response produces one layer down (`formatting.md`
    /// §3.2), so every caller (including `maybe_trigger_format_on_save`'s
    /// bookkeeping, §2.3 below) can rely on "calling this always
    /// eventually sets `format_ready`" without checking
    /// `LspBridge::is_running()` first. Identical guarantee to `ide-ui`'s
    /// own `request_format` (`formatting.md` §2.3).
    pub(crate) fn request_format(&mut self, path: &Path, tab_size: u32, insert_spaces: bool);
    /// Same, for a range -- no `ide-tui` caller in this phase, for the
    /// same reason `ide-ui` has none (`formatting.md` §1: no "current
    /// selection" range plumbed out to app state for any LSP feature to
    /// consume yet). Kept for wire-level parity and so a future range-
    /// aware feature has it ready to call.
    #[allow(dead_code)]
    pub(crate) fn request_format_range(&mut self, path: &Path, range: Range, tab_size: u32, insert_spaces: bool);
}
```

`App` (in `app.rs`) gains:

- `format_on_save: bool` — loaded from `crate::state::PersistedState` in
  `App::new` (alongside `last_project`) and written back to it on every
  successful `crate::state::save` call site this crate already has
  (`docs/features/tui-persist-last-project.md`'s existing save trigger —
  see that doc for exactly when `state::save` is called; this field rides
  along unconditionally, not on its own separate trigger).
- `format_on_save_target: Option<PathBuf>` — set immediately after
  `trigger_save_active` succeeds and format-on-save fires a follow-up
  `Format` request (§3.4); cleared once that request's `FormatReady` has
  been applied and re-saved, or superseded by a second save before the
  first one's response arrived. Safe to set unconditionally whenever the
  branch fires, mirroring `ide-ui`'s own note: `request_format` is now
  guaranteed to eventually produce a `format_ready` outcome either way, so
  this can never be left set forever.
- `request_format_for(&mut self, idx: usize)` — the shared by-index
  primitive both `trigger_reformat_code` and `maybe_trigger_format_on_save`
  call, so idx-explicitness (below) isn't undone by delegating back to an
  active-tab-based entry point. Resolves `self.tabs[idx].indent` into
  `(tab_size, insert_spaces)` (`insert_spaces = matches!(unit.style,
  IndentStyle::Spaces)`, `tab_size = unit.width as u32`) and calls
  `self.lsp.request_format(&self.tabs[idx].path, tab_size, insert_spaces)`.
- `trigger_reformat_code(&mut self)` — `Ctrl+Alt+L`'s entry point
  (`Action::ReformatCode`). No-op with `self.active_tab.is_none()` (§1.1 —
  no separate "no path" case); otherwise `self.request_format_for(idx)`
  for `idx = self.active_tab.unwrap()`.
- `save_tab_at(&mut self, idx: usize)` — extracted from the body of
  `trigger_save_active` (§1.1): every `self.active_buffer()`/
  `self.active_buffer_mut()`/`self.active_tab` reference inside the
  extracted body becomes `self.tabs.get(idx)`/`self.tabs.get_mut(idx)`/
  `idx` directly, with one deliberate exception: line 6100's `if let
  Some(idx) = self.active_tab { self.refresh_blame_if_on(idx); }` becomes
  an unconditional `self.refresh_blame_if_on(idx);` — blame refresh is
  cheap, idempotent, and harmless to run ahead of time for a tab the user
  isn't currently looking at (it simply means blame is already fresh
  if/when they switch back to it), so there's no reason to gate it on
  `idx == self.active_tab` once the function takes an explicit `idx`. For
  the *existing* caller (idx is always `self.active_tab` there), this is
  behavior-identical to today; the new caller (Format on Save's follow-up
  resave, possibly targeting a non-active tab) gets the same treatment
  rather than a special-cased skip.
  `trigger_save_active` becomes a thin wrapper:
  ```rust
  fn trigger_save_active(&mut self) {
      let Some(idx) = self.active_tab else { return; };
      self.save_tab_at(idx);
      self.maybe_trigger_format_on_save(idx);
  }
  ```
  — no behavior change for its existing caller (`Action::SaveActive`).
- `maybe_trigger_format_on_save(&mut self, idx: usize)` — called from
  `trigger_save_active`'s wrapper (above), after `save_tab_at(idx)` has
  already fully completed, **taking `idx` as an explicit parameter rather
  than re-reading `self.active_tab`** — the save that just happened was
  for tab `idx` specifically; re-deriving it from `self.active_tab` would
  happen to give the same answer here (nothing runs between `save_tab_at`
  and this call that could change `self.active_tab`), but an explicit
  parameter doesn't rely on that being true, and matches this feature's
  own recurring principle (§2.3's `handle_format_ready`, §4) of never
  trusting "the active tab" for an operation that must target a specific,
  already-known tab. The wrapper calling `maybe_trigger_format_on_save`
  rather than `save_tab_at` itself is what prevents the Format-on-Save
  follow-up resave (`handle_format_ready`'s own `save_tab_at` call, below)
  from recursively triggering a second format request. No-op unless
  `self.format_on_save` (§1.1 — every tab has a path, so this is the only
  guard needed). Calls `self.request_format_for(idx)` directly — **not**
  `trigger_reformat_code()`, which resolves against `self.active_tab`
  rather than the passed-in `idx`; going through it here would quietly
  reintroduce the exact "trust the active tab" dependency taking `idx`
  explicitly was meant to remove — and records
  `format_on_save_target = Some(self.tabs[idx].path.clone())`.
- `handle_format_ready(&mut self)` — called once per frame from
  `poll_lsp`, immediately after the existing `handle_rename_ready()` call.
  No-op unless `self.lsp.format_ready` (cleared by `poll()` regardless).
  Applies the response through `apply_workspace_edit`/`apply_file_edits`
  without a separate open-tab pre-check of its own — §4's invariant
  guarantees a matching tab exists unless it was closed in the interim, and
  for that TOCTOU case `apply_workspace_edit`/`apply_file_edits` (§1.1/§4)
  fail *safe*, not silent: with no open tab left for the response's path,
  they route through the disk-write branch and write the file directly,
  the same fail-safe behavior §4's Path provenance bullet already
  describes. With `Some(edit)`, calls
  `self.apply_workspace_edit(edit, "Reformat Code")` (§1.1 — reuses T37's
  existing partition-and-apply path rather than a direct `Buffer::apply`),
  surfacing any `Err` through `self.status` exactly like
  `handle_rename_ready`'s own apply call already does. On success, if
  `self.format_on_save_target` matches the response's path, additionally
  looks the tab back up by that path (indices can shift between the
  original save and this response landing) and calls `self.save_tab_at(idx)`
  on it — never `trigger_save_active()`/`self.active_tab`, so a tab switch
  in the interim can never write the wrong tab to disk. `format_on_save_
  target` is cleared whenever it matches this
  response's path, regardless of outcome (`Some`/`None`/apply error).

`commands.rs` gains two entries:

```rust
Command {
    id: "ReformatCode",
    title: "Reformat Code",
    // `⌘⌥L` translated -- same Cmd->Ctrl substitution already applied to
    // e.g. `ToggleBlockComment`'s `⌘⌥/` -> `Ctrl+Alt+/`.
    binding: Some((KeyModifiers::CONTROL.union(KeyModifiers::ALT), KeyCode::Char('l'))),
    action: Action::ReformatCode,
},
Command {
    id: "ToggleFormatOnSave",
    title: "Toggle Format on Save",
    // No JetBrains default keymap entry for this exact toggle (per
    // `docs/roadmap.md` §5.2's rule and `formatting.md`'s own identical
    // conclusion for `ide-ui`) -- palette-only.
    binding: None,
    action: Action::ToggleFormatOnSave,
},
```

`Action` gains `ReformatCode` and `ToggleFormatOnSave` variants. Unlike
`ide-ui`, `ide-tui` has no `is_command_enabled`-style command-gating
mechanism at all — every command is always selectable from the palette,
and an active-tab-scoped action simply no-ops internally when it doesn't
apply. `Action::ReformatCode` follows that existing convention:
`trigger_reformat_code` is a no-op when `self.active_tab.is_none()`, the
same self-guard every other active-tab-scoped action in `app.rs` already
uses, rather than this feature introducing a new palette-gating concept
just for two commands. `Action::ToggleFormatOnSave` needs no gating either
way — it's always available, like every other session-wide toggle this
crate has.

`state.rs`'s `PersistedState` gains:

```rust
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedState {
    pub last_project: Option<PathBuf>,
    #[serde(default)]
    pub format_on_save: bool,
}
```

## 3. Behaviour

### 3.1–3.3: wire protocol, capability negotiation, response handling

Unchanged from `formatting.md` §3.1–§3.3 — `ide-tui` sends the exact same
`LspRequest::Format`/`FormatRange` shape and receives the exact same
`LspEvent::FormatReady`, through the same `ide_lsp::LspClient` every other
`ide-tui` LSP feature already drives. No new behavior to describe at the
wire layer; see that doc's sections for the full detail (JSON param
shape, `null`/`[]` → `None` conversion, capability fail-closed rules,
`pending_format_id` supersession).

### 3.4 Applying the edit, and Format on Save's timing

**Reformat Code** (manual): `handle_format_ready` calls
`self.apply_workspace_edit(edit, "Reformat Code")` (§1.1). One more undo
step, tab marked dirty, not auto-saved — identical discipline to every
other in-editor LSP-driven edit in this crate (code actions, rename).

**Format on Save**: `trigger_save_active`'s existing synchronous save
(via the newly-extracted `save_tab_at`) runs to completion first,
completely unchanged — nothing in this phase adds latency, blocking, or a
new failure mode to `Ctrl+S` itself. Only *after* it succeeds does
`maybe_trigger_format_on_save` fire a `Format` request and record
`format_on_save_target`. When the result lands — necessarily at least one
frame later, a real (if usually fast) round trip to a subprocess —
`handle_format_ready` applies it and calls `save_tab_at` a second time,
targeting the specific tab that was reformatted (looked up by index from
`self.lsp.format_path`) rather than whatever tab happens to be active at
that moment.

This means the file on disk is briefly *unformatted* between the visible
`Ctrl+S` and the silent follow-up save — the identical, deliberately
accepted trade-off `formatting.md` §3.4 argues for at length (deferring
the disk write until the formatting round trip completes would make
`Ctrl+S` depend on a subprocess that could hang, with no existing timeout
mechanism to bound it). Not re-argued here beyond restating the
conclusion: a save that might hang is worse than a file that's correctly
saved, then reformatted and saved again a moment later.

`format_on_save_target` and `Format`/`FormatRange`'s single shared
pending-id slot mean a manual Reformat Code and a save-triggered one can,
in a narrow window, answer each other — identical race, identical
"harmless at most one extra save" resolution `formatting.md` §3.4 already
describes for `ide-ui`; unaffected by the `apply_workspace_edit` vs.
direct-`Buffer::apply` difference (§1.1), since both apply exactly one
`WorkspaceEdit` to exactly one already-open tab either way.

## 4. Constraints & invariants

- **Path provenance.** `request_format`/`request_format_range`'s `path`
  comes only from an already-open tab's own buffer — via
  `trigger_reformat_code` (manual, the active tab) or
  `maybe_trigger_format_on_save`'s follow-up (the tab that was just saved,
  by index — §2.3). Never called for a file with no open tab — the
  invariant `handle_format_ready`'s apply path depends on (§1.1: unlike
  `ide-ui`, there is no "open tab with no path" case to additionally rule
  out). This invariant is not just assumed silently: even if a future
  change to this code broke it, `apply_workspace_edit`/`apply_file_edits`
  (§1.1) fails *safe*, not loudly-wrong — a formatting edit for a path
  with no matching open tab would simply route through the disk-write
  branch (`ide_core::apply_workspace_edit_to_disk`, already hardened) and
  write the file directly, rather than panicking on an invalid tab index
  the way a direct `self.tabs[idx].buffer.apply(...)` call would need to.
  This is the concrete reason reusing the generic apply path (§1.1) is
  preferred over a narrower direct-`Buffer::apply` call here: the
  invariant becomes something the code degrades gracefully around if it's
  ever wrong, not just something a reader has to trust.
- **A formatting response is trusted without path validation**, same
  narrower trust model `formatting.md` §4 already states: no server-
  supplied path in the response, only the request's own path, echoed back
  implicitly by construction.
- **Capability negotiation fails closed** — unchanged, `ide-lsp`-owned
  (§2.1), reused as-is.
- **Format on Save never delays or risks the underlying save.** The
  synchronous save (via `save_tab_at`) always runs to completion,
  unchanged, before any formatting request is sent (§3.4) — this ordering
  is the whole reason the feature needs no new timeout machinery, same
  reasoning `formatting.md` §4 already gives.
- **The format-on-save follow-up save is always by-index (`save_tab_at`),
  never `trigger_save_active`/`self.active_tab`-based.**
  `handle_format_ready` runs on whatever frame a response happens to land
  on, arbitrarily long after the user's `Ctrl+S` — the active tab may
  have changed in the interim.
- **`request_format`/`request_format_range` always resolve.** A missing
  client is not a silent no-op (§2.3) — `format_ready` is guaranteed to
  become `true` on the next relevant poll regardless of whether a client
  is running, which is what makes it safe to set `format_on_save_target`
  unconditionally in `maybe_trigger_format_on_save`.
- **`maybe_trigger_format_on_save` is only ever called from
  `trigger_save_active`'s wrapper, never from `save_tab_at` itself.** This
  is what prevents the Format-on-Save follow-up resave (which also calls
  `save_tab_at`) from recursively triggering a second format request —
  a divergence from `ide-ui`'s equivalent only in *where* the hook lives
  (tail of the by-index-agnostic wrapper vs. tail of `ide-ui`'s
  `try_save_active`, which never needed this split since `ide-ui`'s
  `save_tab_at` was never itself the Format-on-Save resave's caller's
  wrapper — see §1.1's note on why `ide-ui` already had `save_tab_at`
  before this feature for an unrelated reason).
- **`ide-core` gains no new dependency, `ide-lsp` gains none on
  `ide-core`, `ide-tui` gains none beyond what T15/T37/code-actions/rename
  already established.** `tab_size`/`insert_spaces` cross the
  `ide-lsp`/`ide-core` boundary as plain `u32`/`bool`, unchanged.

## 5. Examples

**Manual Reformat Code:**

```rust
// Caret anywhere in the active tab; server negotiated
// documentFormattingProvider: true.
app.trigger_reformat_code();
// -> LspRequest::Format { path, tab_size: 4, insert_spaces: true }
// ... a frame or so later ...
// LspEvent::FormatReady { path, edit: Some(edit) }

app.poll_lsp(); // drains lsp.poll(), calls handle_format_ready()
// edit.edits[0].path == the active tab's path -> apply_workspace_edit
// routes it to the buffer-edit branch (the tab is open). One more undo
// step, tab is dirty. Not auto-saved.
```

**Already-formatted file:**

```rust
app.trigger_reformat_code();
// Server returns [] -- file needs no changes.
// LspEvent::FormatReady { path, edit: None }
app.poll_lsp();
// No-op: nothing to apply, nothing to undo, tab dirty state unchanged.
```

**Format on Save:**

```rust
app.format_on_save = true;
app.trigger_save_active();
// 1. save_tab_at(idx) runs and succeeds, exactly as if format_on_save
//    were false -- file is now on disk, unformatted.
// 2. maybe_trigger_format_on_save(idx): format_on_save_target =
//    Some(path); request_format_for(idx) fires (not
//    trigger_reformat_code() -- idx is already known, no need to
//    re-derive it from self.active_tab).

// ... a frame or so later ...
// LspEvent::FormatReady { path, edit: Some(edit) }
app.poll_lsp();
// Tab located by format_path -> apply_workspace_edit(edit, ...) --
// buffer now formatted, dirty. format_on_save_target matches that same
// tab's path -> save_tab_at(idx) runs again, silently, on that specific
// tab (even if the user switched to a different active tab meanwhile).
// File on disk is now formatted; format_on_save_target cleared.
```

**Format on Save with no LSP client running** (editing a file outside any
project, or the server hasn't started):

```rust
app.format_on_save = true;
app.trigger_save_active();
// 1. save_tab_at(idx) runs and succeeds, unaffected by format_on_save.
// 2. format_on_save_target = Some(path); maybe_trigger_format_on_save's
//    request_format_for(idx) calls LspBridge::request_format, which
//    finds self.client.is_none() and immediately (same call, no round
//    trip) sets format_ready = true, format_edit = None,
//    format_path = Some(path).

app.poll_lsp(); // same frame or the next -- no waiting
// No open-tab/edit mismatch possible: nothing to apply, nothing to
// re-save. format_on_save_target is still cleared -- it never lingers
// waiting for an event that was never going to arrive.
```

## 6. Dependencies & integration points

- No new external dependencies in any crate.
- Builds entirely on already-merged machinery: `ide-lsp`'s `Format`/
  `FormatRange`/`FormatReady` wire types (A9, `ide-ui`), `ide-tui`'s own
  `LspBridge` request/poll shape (established across every prior TUI LSP
  port), `apply_workspace_edit`/`apply_file_edits` (T37), and `state.rs`'s
  `PersistedState` (`tui-persist-last-project.md`).
- `ide-lsp`: unchanged.
- `ide-tui`: extends `lsp_bridge.rs`, `app.rs`, `commands.rs`, `state.rs`.
  Does not touch `ui.rs` at all — Reformat Code has no
  popup, no new rendered surface; its only UI-visible effects are the
  buffer changing and the existing `self.status`/notification channel for
  errors, exactly like `formatting.md` §6 already notes for `ide-ui`.
- Not security-sensitive (§1) — `hacker` is skipped for this role.

## 7. Diagram

Skipped — the sequence is byte-for-byte the same shape
`formatting.md` §7's diagram already shows (`ide-ui` node relabeled
`ide-tui`, `eframe::Storage` relabeled `state.rs`); a second diagram would
duplicate it without adding information.

## Revision notes

- §1: added a paragraph explicitly addressing `apply_file_edits`'s
  presence on `CLAUDE.md`'s declared security-sensitive list (`rev`
  finding — the original security-sensitivity analysis ruled out
  `crates/lsp/**` and `lsp_bridge.rs`-style command-injection concerns but
  never engaged with the one already-declared-sensitive function this
  feature actually calls into). Reasoned through why the risk that
  motivated that listing (glob/regex-driven candidate-file selection,
  T37's Replace in Path) doesn't transfer to this caller (a single,
  tab-selected path, never a pattern-matched set) — `hacker` stays skipped
  on that explicit basis, not by omission.
- §2.3: fixed a contradiction between the `trigger_save_active` wrapper's
  prose description and its own code snippet — the snippet didn't
  actually call `maybe_trigger_format_on_save` (`rev` finding). Resolving
  this surfaced a real design question: whether the follow-up format
  request should re-derive its target tab from `self.active_tab` or take
  it as an explicit parameter. Introduced `request_format_for(&mut self,
  idx: usize)` as a shared by-index primitive so `maybe_trigger_format_
  on_save` never has to go through the active-tab-based
  `trigger_reformat_code` to send its request — keeping the "never trust
  the active tab for a specific, already-known tab" discipline this
  feature otherwise applies consistently (§4).
- §2.3/§4: resolved an unstated ambiguity in the `save_tab_at` extraction
  (`rev` finding) — whether `refresh_blame_if_on` stays gated to
  `idx == self.active_tab` or fires unconditionally once `save_tab_at`
  takes an explicit `idx`. Decided unconditionally: blame refresh is
  cheap and idempotent, so refreshing it for a non-active tab (the
  Format-on-Save follow-up resave's case) is harmless, not a behavior
  change worth gating.
- §4: strengthened the path-provenance invariant with the concrete
  fail-safe argument for reusing `apply_workspace_edit`/`apply_file_edits`
  over a narrower direct-`Buffer::apply` call (raised as a `[controversial]`
  finding, not blocking, but worth resolving in the doc rather than left
  as an assumption) — if the invariant is ever violated, the generic apply
  path degrades to a disk write through an already-hardened function
  rather than panicking on an invalid index.
- §2.3: after implementation, `rev`'s code review (round 1) found this
  section's own text self-contradicted §4 on the closed-tab TOCTOU case —
  this section said "a complete no-op," §4 said the apply path writes to
  disk directly. Corrected this section to match §4 (the code, following
  §4, does write to disk in that case) rather than the reverse.
- §2.3: dropped the `is_command_enabled` claim (`rev` code-review finding)
  — `ide-tui` has no command-gating mechanism of any kind (verified: no
  `is_command_enabled` function anywhere in `crates/tui/**`), unlike
  `ide-ui` where this section's wording originated from. Replaced with an
  explanation of the actual, already-established convention this feature
  follows instead (an active-tab-scoped action self-guards internally).
- Implementation note (not a doc change): `rev`'s code review also found
  `handle_format_ready` silently discarded an `apply_workspace_edit`
  failure instead of surfacing it via `self.status`, unlike
  `handle_rename_ready`'s identical call. Fixed in the implementation
  (`crates/tui/src/app.rs`), not here — this section's own text already
  described the call correctly, it just wasn't implemented to match yet.
