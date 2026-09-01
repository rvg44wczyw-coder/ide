# `ide-tui`: Go to Declaration, Find Usages, and in-app notifications

## 1. Purpose

`ide-tui` had zero `ide-lsp` integration -- no dependency on the crate at
all, confirmed by grep before this batch started. A user directly asked
("does in tui working go to declaration/usages?"), and the two features
share almost all their infrastructure (a running language-server client,
buffer-lifecycle notifications, a one-or-many result picker), so this
batch adds both together rather than as two separate runs.

While building it, the same user pushed back on using the existing ambient
`status()` line for the query outcomes ("we should not just send status
somewhere -- i'd prefer notifications... not macos notifications - just
internal app notifications... with list and possibility to clear/mark all
as read"). That became this batch's third piece: a small in-app
notification log, independent of `App::status()`.

## 2. Interface / API

### 2.1 `src/lsp_bridge.rs` (new file)

```rust
pub(crate) struct LspBridge { /* client, server_error, goto*, references* */ }

impl LspBridge {
    pub(crate) fn is_running(&self) -> bool;
    pub(crate) fn start_with_command(&mut self, project_root: &Path, command: &str);
    pub(crate) fn send(&self, request: LspRequest);
    pub(crate) fn go_to_definition(&mut self, path: &Path, position: Position);
    pub(crate) fn find_references(&mut self, path: &Path, position: Position);
    pub(crate) fn poll(&mut self) -> bool;
}
```

A deliberately small subset of `crates/ui/src/lsp_bridge.rs`'s shape (same
`finding_*`/`*_ready` flag pair, clear-at-send, replace-wholesale-on-
response, `ServerExited` clears everything): only `Goto`/`References`
requests are ever sent, so `poll()`'s `match` only handles `Goto`,
`References`, and `ServerExited` -- every other `LspEvent` variant
(`Diagnostics`, `Hover`, `InlayHint`, `SemanticTokens`, ...) falls into a
documented wildcard arm, since this crate never sends the requests that
would produce them (§4).

### 2.2 `src/app.rs`

```rust
pub(crate) struct GotoState { pub(crate) title: &'static str, pub(crate) results: Vec<Location>, pub(crate) selected: usize }
pub(crate) struct Notification { pub(crate) message: String, pub(crate) read: bool }

impl App {
    pub fn poll_lsp(&mut self);                    // called once per frame by main.rs
    pub(crate) fn unread_notification_count(&self) -> usize;
}
```

`App` gains `lsp: LspBridge`, `goto: Option<GotoState>`,
`notifications: Vec<Notification>`, `notifications_open: bool`.
`App::new` detects the project's language via `ide_core::detect_language`
(with an always-empty `custom` slice -- `ide-tui` has no language-settings
UI, §4) and starts `lsp` synchronously if one matched, the same
synchronous-`start_with_command`-in-the-constructor shape
`crates/ui/src/app.rs`'s `IdeApp::load_project` already uses.

Private helpers: `trigger_go_to_declaration`/`trigger_find_usages`
(`Ctrl+B`/`Ctrl+U` entry points), `handle_goto_results` (dispatches a
`Goto`/`References` response to a direct jump, the picker, or a "no
results" notification), `open_location` (opens the target path, converts
its LSP `Position` to a byte offset, sets the selection and top-aligns the
scroll -- the same shape `tui-find.md`'s `jump_to_match` already
established), `lsp_query_target` (active buffer's path + cursor position,
mirroring `crates/ui/src/app.rs`'s `find_usages_target`),
`sync_lsp_did_change` (sends `DidChange` with the active buffer's full
text), `notify`/`toggle_notifications`/`handle_notifications_key`.

### 2.3 `src/commands.rs`

Three new `Command`s:

| id | title | binding |
|---|---|---|
| `GoToDeclaration` | Go to Declaration | `Ctrl+B` |
| `FindUsages` | Find Usages | `Ctrl+U` |
| `ToggleNotifications` | Notifications | none (palette-only) |

`Ctrl+B` is `ide-ui`'s `⌘B` Ctrl-translated, per this crate's standing
rule. `Ctrl+U` is a deliberate departure: `ide-ui`'s Find Usages binding
is `⌥F7` (Option, not Cmd), and this crate has no established convention
yet for translating an Option-held JetBrains binding into a terminal
chord (every existing binding here started from a Cmd chord) -- rather
than invent one, `Ctrl+U` (an unused, unambiguous `Ctrl+<letter>`, no
`Shift` involved so it needs no Kitty-protocol disambiguation either) is
picked the same way `ToggleProjectToolWindow`'s `Ctrl+T` was: CLAUDE.md's
"never invent a binding" rule is about not guessing a *JetBrains* binding
that doesn't exist, and both `⌥F7` and the Kitty-only-reliable `⌘⌥F7`
usages-popup binding are real JetBrains bindings that just don't have a
safe literal terminal translation yet. `ToggleNotifications` has no
JetBrains equivalent to translate at all (a purely `ide-tui`-internal
concept), so per CLAUDE.md it's registered with no default binding,
reachable only through the palette -- the same treatment `Exit` already
gets.

### 2.4 Notifications (`src/app.rs` + `src/ui.rs`)

An in-app log, **not** a desktop/OS notification. `App::notify` appends
an unread entry; nothing ever auto-expires or auto-caps it (§4). The
panel (`Ctrl+Shift+A` → "Notifications" → Enter) is a modal overlay, same
interception tier as the Goto picker/palette/find bar:

- `Esc` closes the panel (does not clear anything).
- `c` clears every notification.
- `r` marks every notification read (bulk only -- no per-entry toggle).

`render_status` (`ui.rs`) appends a `[N unread]` suffix to the existing
status line when `unread_notification_count() > 0`, so unread activity is
visible without opening the panel.

## 3. Behaviour

1. `App::new` opens the project, scans the tree, and -- if
   `Cargo.toml` sits at the root -- starts `rust-analyzer` via `lsp`
   (silently: no LSP for a non-Rust project, same as `ide-ui`).
2. Opening a file (`open_or_focus_tab`) sends `DidOpen`; closing a tab
   (`close_active_tab`) sends `DidClose`; every text-mutating key in the
   editor (typed chars, Enter/Tab/Backspace/Delete when they actually
   delete something, Undo/Redo, and Replace/Replace All) sends `DidChange`
   with the buffer's full current text -- no incremental sync, matching
   `LspRequest::DidChange`'s only supported shape.
3. `Ctrl+B` with the cursor on a symbol: if no server is running, pushes
   an unread "No language server running." notification and stops. If the
   position converts and a server is running, sends `Goto{kind:
   Definition}`; `main.rs`'s per-iteration `App::poll_lsp` call drains the
   response next tick and:
   - **zero** results: pushes "Declaration: no results.".
   - **one** result: pushes "Declaration: jumped to `<file>`:`<line>`."
     and opens it directly, no picker.
   - **many** results: pushes "Declaration: `<n>` results." and opens the
     Goto picker (`Up`/`Down`/`Enter`/`Esc`).
4. `Ctrl+U` is identical, using `References` instead of `Goto`, titled
   "Usages".
5. A crashed/exited server (`LspEvent::ServerExited`) clears `lsp`'s
   client and query state; the next `Ctrl+B`/`Ctrl+U` reports "No language
   server running." rather than hanging.

## 4. Constraints & invariants

- **One language, no custom configuration.** `ide-tui` has no
  language-settings UI, so `detect_language`'s `custom` argument is always
  `&[]` -- only the built-in Rust/`rust-analyzer` config can ever match.
  This also means the language-server *command* is never user-configurable
  in this crate today, unlike `crates/ui/src/lsp_bridge.rs` (which
  CLAUDE.md lists as security-sensitive specifically because its command
  comes from user-typed settings). `crates/tui/src/lsp_bridge.rs` is **not**
  on that list for the same reason -- it always spawns the literal
  `"rust-analyzer"` -- but the moment a future batch adds per-project
  language configuration here, this file needs to join it.
- **No diagnostics, hover, inlay hints, semantic tokens, code actions,
  formatting, or rename in this bridge.** `LspBridge::poll` only reacts to
  `Goto`/`References`/`ServerExited`; every other event is intentionally
  dropped. Building any of those is a separate, later batch, not scope
  creep into this one.
- **`DidChange` sends the whole buffer on every edit, unthrottled.** No
  debounce/idle-frame batching the way a production LSP client normally
  would -- acceptable for `ide-tui`'s scope today (small local files,
  `rust-analyzer` already has to reparse to serve the immediately-following
  `Goto`/`References` query anyway); revisit if this becomes a measured
  problem.
- **Notifications never auto-expire or cap.** The user explicitly asked
  for a persistent list they manage (`clear`/`mark all read`), not a
  timed toast -- so there's no `Instant`/TTL tracking anywhere, and no
  upper bound on `Vec<Notification>` length. A pathological flood (e.g.
  rapid repeated queries) grows the vector unboundedly; not addressed
  here since nothing in this batch's actual usage pattern (one entry per
  user-initiated query) can realistically trigger it.
- **`c`/`r` inside the notifications panel are plain, unmodified letters.**
  Safe because this panel has no text field to type into (unlike find's
  query, which is exactly why `handle_find_key` has to carefully exclude
  `Ctrl`-held letters from its typing arm) -- every key while the panel is
  open is either one of these two letters, `Esc`, or ignored.
- **Mutual exclusion with the Goto picker only, not with find/palette.**
  Opening the notifications panel force-closes an open Goto picker
  (`toggle_notifications`); it does not and cannot open while
  find/palette are open, since `handle_key`'s existing `self.find.is_some()`/
  `self.palette.is_some()` checks already run first and intercept
  everything before the palette's own `ToggleNotifications` selection
  could ever fire.

## 5. Examples

```
$ ide-tui ~/code/my-rust-project
```

Opens `src/main.rs`, cursor on a function call. `Ctrl+B` jumps straight to
its definition if `rust-analyzer` resolves exactly one location; `Ctrl+U`
on the function's own name opens the Usages picker if it's called from
more than one place. `Ctrl+Shift+A` → "Notif" → `Enter` opens the log to
review what just happened; `c` inside it clears the log, `r` marks it all
read (visible immediately as the `[N unread]` status-bar suffix
disappearing).

## 6. Dependencies & integration points

New workspace dependency edge: `crates/tui/Cargo.toml` now depends on
`ide-lsp` (path dependency, already a workspace member -- no new external
crate). Touches `crates/tui/src/{main,app,commands,ui}.rs` and adds
`crates/tui/src/lsp_bridge.rs`. Not on CLAUDE.md's security-sensitive list
under its current scope (§4) -- no `hacker` pass run for this batch.

## 7. Diagrams

None. The request/response flow is exactly `crates/ui/src/lsp_bridge.rs`'s
already-diagrammed-elsewhere shape at a fraction of the surface, and the
notification/picker interactions are small enough modal-key-handling logic
(same shape as the find bar and palette, neither of which has a diagram
either) that a sequence diagram would restate §3's bullet list without
adding information.

## Revision notes

Implemented directly in response to a live user question ("does in tui
working go to declaration/usages?"), then extended mid-implementation by
direct user feedback on the first draft (which used `App::status()` for
query outcomes) into the dedicated notification log described in §2.4 --
this doc was written after both pieces were implemented and tested, the
same "written post-implementation" pattern `tui-scroll-follows-cursor.md`
used earlier in this session, for the same reason (fast-moving direct
user steering rather than a stale-before-merge risk). Self-reviewed
(code-review checklist + devil's-advocate pass) before merge:
- The unbounded `Vec<Notification>` growth (§4) was considered and is an
  accepted v1 trade-off given the user's explicit request for a persistent,
  manually-managed list rather than an auto-expiring toast -- not an
  oversight.
- `Ctrl+U` for Find Usages (§2.3) is the one binding decision in this
  batch that isn't a mechanical Cmd-to-Ctrl translation of an existing
  `ide-ui` binding; flagged explicitly above rather than presented as
  equally settled as the others.
No other controversial findings.
