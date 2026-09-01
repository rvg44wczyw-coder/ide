# Claude panel + Claude terminal (ide-tui)

## 1. Purpose

Port of the already-merged, already-`hacker`-reviewed `claude-terminal.md`
(`ide-ui`'s G7-adjacent Batch C) plus `ide-ui`'s existing one-shot
`claude_panel.rs`, both into `ide-tui`. Neither has ever existed in this
crate (`T26` was `❌`; grepping this crate's own source for "claude" turns
up nothing but comments referencing `CLAUDE.md`). Two things, both gated
behind one new toggleable panel:

- **Chat**: `claude -p <prompt>` run once per submitted prompt on a
  background thread, reply appended to a scrollback history. Direct,
  almost line-for-line port of `crates/ui/src/claude_panel.rs` — that
  file has **zero** `egui` dependency (`std::process`/`std::thread`/
  `std::sync::mpsc` only), so it needs no framework-specific rewrite at
  all beyond its own module doc comment.
- **Terminal**: one or more real, interactive `claude` CLI sessions, each
  a `portable-pty`-backed child process with a hand-rolled bounded ANSI/
  CSI interpreter (`TerminalGrid`) rendering into a scrollback+viewport
  grid. Direct port of `crates/ui/src/claude_terminal.rs`'s pure logic
  (`AnsiColor`/`Cell`/`TerminalGrid`/`PtySession`/`ClaudeTerminalTab`/
  `ClaudeTerminalPanel` — none of that depends on `egui` either, only
  `AnsiColor::xterm_rgb`'s return type and the free-standing
  `key_event_to_bytes`/`terminal_tab_egui_id` functions are egui-specific
  and need a terminal-native replacement, below).

Per the plan this reuses (`whimsical-mapping-toast.md` Batch C, already
agreed with the user, already implemented once for `ide-ui`): account/
subscription switching is not managed by the IDE — a tab just inherits
whatever shell environment/`claude` login is active when it's spawned.
No IDE-side credential handling, same as `ide-ui`'s version.

## 1.1 Scope cuts and TUI-specific deviations

- **No native folder picker.** `ide-ui`'s "+`" opens
  `rfd::FileDialog::pick_folder()`; nothing equivalent exists in
  `ide-tui` (no GUI toolkit). A new terminal tab's directory is instead
  typed into a small text prompt (same shape as T23's `NewScratchFile`
  popup: type, `Enter` confirms, `Esc` cancels) — left blank, it defaults
  to `project_root`. Validated with `.is_dir()` before spawning, same as
  `PtySession::spawn` already independently re-validates in `ide-ui`.
- **No "Copy" button.** `ide-ui`'s custom-painted `egui` canvas has no OS-
  level text selection of its own, so a "Copy All → clipboard" button was
  worth adding there. `ide-tui` renders into a *real* terminal — the
  user's own terminal emulator already provides native click-drag text
  selection and copy over whatever `ratatui` draws, for free. Adding a
  second, redundant copy mechanism (and a new clipboard dependency this
  crate doesn't otherwise need) isn't worth it here; cut.
- **No scrollback UI.** `render_cargo_panel` (`docs/features/
  tui-cargo-panel.md` §4) already established "no scroll-back in v1: the
  panel shows only the tail of a growing output log that fits the
  screen" for this crate's build/test output panel. The Claude Terminal
  view follows the same precedent: `TerminalGrid` still *maintains* a
  `scrollback` buffer internally (needed for §4.3's resize semantics
  below), but rendering only ever shows `visible_rows()` — no
  `PageUp`/`PageDown`, no "stuck to bottom" tracking. A real terminal
  emulator's own scrollback (the user's outer terminal) already covers
  scrolling back through what's been printed, the same reasoning as the
  no-Copy-button cut above.
- **No mac/other keymap split, no palette registration for panel-local
  keys.** Exactly like `cargo_panel.rs`'s six subcommand letters
  (`b`/`r`/`t`/`c`/`l`/`f`), none of which are `Action`/`Command` registry
  entries at all — this panel's internal navigation (`Tab`/`Shift+Tab`
  cycle view, `Ctrl+N` new terminal tab, `Ctrl+W` close terminal tab,
  `Enter`/`Shift+Esc` enter/leave raw PTY focus) is pure local dispatch
  inside `handle_claude_panel_key`, undocumented in `commands.rs`,
  because — like Cargo Panel's letters — the action doesn't exist outside
  this one gated context, so it isn't rebindable/palette-visible material
  either. Only the panel's own open/close (`Action::ToggleClaudePanel`)
  is a real registry entry, palette-only (no default binding), matching
  `ToggleCargoPanel`/`ToggleGitPanel`/`ToggleTodoPanel`'s own precedent
  exactly (none of those four has a tracked JetBrains keymap entry
  either).
- **`Ctrl+N`/`Ctrl+W` for tab management are a considered choice, not
  arbitrary.** IntelliJ's own Terminal tool window binds ⌘T/⌘W to New Tab/
  Close Tab specifically while the terminal has focus — this project's
  established mac-Cmd→terminal-Ctrl translation (every other `T`-phase
  bound a JetBrains `⌘`-chord to the `Ctrl`-equivalent) carries that over
  directly. These shadow the *global* `Ctrl+T`("Project")/`Ctrl+W`("Close
  Tab") bindings by chord, but never collide at runtime: `handle_key`'s
  dispatch chain checks `claude_panel_open` and returns early, so the
  global keymap lookup is never reached while this panel is open — the
  same "same chord, different meaning depending on mode" pattern this
  crate already has everywhere (`Esc` alone means at least six different
  things depending on which overlay is open).
- **The raw-PTY-focus / chrome-mode split is new, with no
  `ide-ui`/JetBrains precedent — a TUI-only necessity.** `ide-ui`'s
  terminal forwards every key (`Escape`→`\x1b`, `Tab`→`\t`, any
  `Ctrl`+letter→its C0 byte) *unconditionally* whenever a terminal tab has
  mouse-click focus, because leaving that focus is just clicking
  somewhere else with the mouse. `ide-tui` has no mouse, so an
  unconditional full-forward would make the panel impossible to leave by
  keyboard (`Ctrl+C`/`Escape`/`Tab` are all real, load-bearing terminal
  input that must reach the child, not be interceptable). Solved with an
  explicit two-mode split, `ClaudeView::Terminal(idx)` plus a
  `terminal_focus: bool`: **chrome mode** (`terminal_focus == false`) is
  where `Tab`/`Shift+Tab`/`Ctrl+N`/`Ctrl+W`/`Esc` are intercepted by the
  panel itself and `Enter` switches into **raw focus**; **raw focus**
  (`terminal_focus == true`) forwards every key to the PTY except
  `Shift+Esc`, which switches back to chrome mode without forwarding
  anything. `Shift+Esc` is JetBrains' own real, cross-keymap
  `HideActiveWindow` binding ("Hide Active Tool Window") — reused here for
  its closest TUI-reachable equivalent action, defocusing this tool
  window's content back to its own chrome, since there's no mouse-click-
  elsewhere to fall back on. `Chat` view has no such split: its text box
  is a single flat field (same shape as `NewScratchFile`'s), so it's
  always "focused" for typing and never needs `Tab`/`Ctrl`-letter
  passthrough at all.

## 2. Interface

Two new files.

### 2.1 `crates/tui/src/claude_panel.rs` (Chat)

Identical public surface to `crates/ui/src/claude_panel.rs`:
`ClaudeMessage { User(String), Assistant(String), Error(String) }`,
`ClaudePanel { pub input: String, pub history: Vec<ClaudeMessage>, .. }`
with `submit(&mut self, prompt: String)`, `is_in_flight(&self) -> bool`,
`poll(&mut self) -> bool` (call once per frame; `true` means history
changed). `Default` uses the real `run_claude_cli` runner (shells out to
`claude -p`, prompt piped via stdin exactly as `ide-ui`'s version does —
see that file's own doc comment on why stdin-not-argv); tests substitute
a fake `Runner = fn(&str) -> Result<String, String>`.

### 2.2 `crates/tui/src/claude_terminal.rs` (Terminal)

```rust
pub const TERMINAL_SCROLLBACK_LIMIT: usize = 2000;

pub enum AnsiColor { Default, Black, Red, .. /* same 16 variants */ }
impl AnsiColor {
    /// `None` for `Default` (caller renders with no explicit fg/bg --
    /// see §3.3, this crate has no theme to fall back on either).
    pub fn xterm_rgb(self) -> Option<ratatui::style::Color>;
}

pub struct Cell { pub ch: char, pub fg: AnsiColor, pub bg: AnsiColor, pub bold: bool }

pub struct TerminalGrid { /* identical to ide-ui's: viewport, scrollback,
    cursor, parser state, partial-UTF-8 buffer */ }
impl TerminalGrid {
    pub fn new(rows: usize, cols: usize) -> Self;
    pub fn feed(&mut self, bytes: &[u8]);
    pub fn resize(&mut self, rows: usize, cols: usize); // §4.3, unchanged
    pub fn rows(&self) -> usize;
    pub fn cols(&self) -> usize;
    pub fn cursor(&self) -> (usize, usize);
    pub fn visible_rows(&self) -> &[Vec<Cell>];
    pub fn scrollback_rows(&self) -> &VecDeque<Vec<Cell>>; // maintained, unrendered (§1.1)
    pub fn plain_text(&self) -> String; // unused by any v1 call site, kept for parity + tests
}

pub struct PtySession { /* identical: writer, reader-thread channel, master, child */ }
impl PtySession {
    pub fn spawn(cwd: &Path, rows: u16, cols: u16) -> Result<Self, String>;
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    pub fn resize(&self, rows: u16, cols: u16);
    fn poll(&mut self) -> Vec<PtyEvent>;
}
impl Drop for PtySession { fn drop(&mut self); } // kills the child

pub struct ClaudeTerminalTab {
    pub id: u64,
    pub cwd: PathBuf,
    pub title: String,
    pub exited: bool,
    grid: TerminalGrid,
    pty: Option<PtySession>,
}
impl ClaudeTerminalTab {
    pub fn grid(&self) -> &TerminalGrid;
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    pub fn resize(&mut self, rows: u16, cols: u16);
}

#[derive(Default)]
pub struct ClaudeTerminalPanel { tabs: Vec<ClaudeTerminalTab>, pub active: Option<usize>, next_id: u64 }
impl ClaudeTerminalPanel {
    pub fn open_tab(&mut self, cwd: PathBuf, rows: u16, cols: u16); // never fails, §3.1 below
    pub fn close_tab(&mut self, index: usize);
    pub fn tabs(&self) -> &[ClaudeTerminalTab];
    pub fn poll(&mut self) -> bool; // call once per frame regardless of panel visibility, §4.2
    pub fn active_tab(&self) -> Option<&ClaudeTerminalTab>;
    pub fn active_tab_mut(&mut self) -> Option<&mut ClaudeTerminalTab>;
}

/// Replaces `ide-ui`'s `egui::Event`-based version (§1.1's raw-focus
/// deviation): translates one `crossterm` key event into the bytes a
/// real terminal would send, or `None` for a key that isn't forwarded
/// (this module has no opinion on *when* it's called -- `app.rs`'s
/// `terminal_focus` gate decides that, §3.4).
pub fn key_event_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>>;
```

`MAX_GRID_DIMENSION = 4096` and `MAX_CSI_PARAMS = 32` are carried over
unchanged from `ide-ui`'s post-`hacker`-pass hardening (`docs/security-
findings/rust-ui-dev-claude-terminal-2026-08-25.md` finding 5, plus the
CSI-params bound that same pass's write-up documents) — both apply
identically here, nothing about the DoS reasoning is GUI-specific.

## 3. Behaviour

### 3.1 Panel toggle, view, and tab lifecycle

`Action::ToggleClaudePanel` (palette-only, no default binding) opens/
closes `App.claude_panel_open: bool`, same "closing hides, never resets"
convention as `toggle_cargo_panel`/`toggle_search_panel`. `App` also
tracks `claude_view: ClaudeView` (`Chat` or `Terminal(usize)`, an index
into `claude_terminals.tabs()`) and `claude_terminal_focus: bool` (§1.1),
both persisting across toggle the same way.

`Ctrl+N` (from either view) opens a `NewClaudeTerminalState { name: String
}` text prompt (own popup, nested on top of the already-open panel --
checked first inside `handle_claude_panel_key` rather than at `handle_key`'s
top level, since it's only ever reachable while the panel is already
open): `Esc` cancels; `Enter` resolves the typed string via
`resolve_claude_terminal_dir` (blank ⇒ `project_root`; relative ⇒ joined
onto `project_root`; absolute ⇒ used as-is -- a pure, separately-tested
helper) and calls `claude_terminals.open_tab(dir, rows, cols)`
unconditionally (`rows`/`cols` from the panel's last-known char-grid
size, §3.3). Deliberately **not** re-validated with `.is_dir()` at this
layer first: `open_tab` itself never fails (§2.2) — `PtySession::spawn`
failing (e.g. a nonexistent directory, or `claude` not on `PATH`) still
creates a tab, `exited: true`, the error text fed into its `TerminalGrid`
(`ide-ui`'s exact behaviour, unchanged) — a second, cruder check here
would only produce a worse-UX duplicate of that same error path.

`Ctrl+W` closes the active terminal tab (`Terminal(idx)` only — a no-op in
`Chat` view, nothing to close); `close_tab`'s existing index-adjustment
rules (§2.2) apply, and if no terminal tabs remain afterward, `claude_view`
resets to `Chat`.

`Tab`/`Shift+Tab` (`BackTab`) cycle `claude_view` forward/backward through
`Chat, Terminal(0), Terminal(1), .., Chat, ..` (empty terminal tabs list ⇒
`Tab`/`Shift+Tab` are a no-op, always `Chat`). Switching view always resets
`claude_terminal_focus = false`.

`Esc` in `Chat` view, or in `Terminal(idx)` chrome mode
(`claude_terminal_focus == false`), closes the whole panel
(`claude_panel_open = false`) — same convention `cargo_panel`/
`new_scratch_file`/every other overlay in this crate already uses.

### 3.2 Chat view key handling

Same shape as `NewClaudeTerminalState`'s text field: `Backspace` pops a
char from `claude.input`; `Enter` calls `claude.submit(std::mem::take(&mut
claude.input))` (no-op on blank/whitespace-only input, per `submit`'s own
existing contract); any `Char(c)` without `CONTROL` appends to
`claude.input`. `Ctrl+N`/`Ctrl+W`/`Tab`/`Shift+Tab`/`Esc` are checked
*before* this catch-all (exact ordering `new_scratch_file`'s own handler
already established for its Esc/Backspace/Enter/Char dispatch).

### 3.3 Terminal view: sizing and rendering

Same "no scrollback UI" v1 as `render_cargo_panel` (§1.1): only
`visible_rows()` renders, sized to the popup's available rect the same
way `render_cargo_panel`'s own `height`/`width` are computed from `area`.
Whenever the computed `rows`/`cols` differ from the active tab's current
`grid.rows()`/`grid.cols()`, `ClaudeTerminalTab::resize` is called once
that frame (mirrors `ide-ui`'s own per-frame size-comparison check).

Each visible row becomes one `ratatui::text::Line`, built by coalescing
runs of adjacers `Cell`s sharing the same `(fg, bg, bold)` into one
`ratatui::text::Span` (same rationale `ide-ui`'s doc gives: don't pay
per-character layout cost on a screen that can fully repaint every
frame). `AnsiColor::Default` maps to a `Style` with no explicit fg/bg set
at all (inherits the outer real terminal's own colors — simpler than
`ide-ui`'s theme-token fallback, and this crate has no theme to fall back
to regardless, per T21's roadmap note); the other 16 map through
`xterm_rgb`'s fixed RGB constants via `Color::Rgb(r, g, b)`. The cell at
`grid.cursor()` renders with fg/bg swapped (`Modifier::REVERSED`, the
same modifier every other "selected row" indicator in `ui.rs` already
uses) whenever `claude_terminal_focus` is true for the active tab — an
unfocused (chrome-mode) tab shows no cursor swap, so a plain glance at
the panel tells you whether typing would currently reach the child.

### 3.4 Keyboard input while a terminal tab has raw focus

Every `KeyEvent` `handle_claude_panel_key` receives while
`claude_terminal_focus` is true goes through `claude_terminal::
key_event_to_bytes` first; `Some(bytes)` is written to the active tab
immediately via `ClaudeTerminalTab::write` and the event is consumed
(never falls through to chrome-mode dispatch). The one exception checked
*before* this: `(SHIFT, KeyCode::Esc)` — i.e. `Shift+Esc` — always exits
to chrome mode (§1.1) regardless of what `key_event_to_bytes` would have
produced for it (plain `Esc` alone *is* forwarded as `0x1b`, matching a
real terminal; only the `Shift`-qualified chord is intercepted). Any
`Ctrl`+letter maps to its standard C0 control byte
(`(letter.to_ascii_uppercase() as u8) - b'A' + 1`) exactly like `ide-ui`'s
version — `Ctrl+C` (interrupt), `Ctrl+D` (EOF), `Ctrl+L` (clear), etc. all
reach the child, not this app's own bindings.

## 4. Constraints and invariants

### 4.1 Security-sensitive (mandatory `hacker` pass)

Identical surface `CLAUDE.md` already names for `ide-ui`'s version and
extends generically to any PTY-spawning panel: program is the fixed
literal `"claude"` (never a shell, never user-editable, unlike
`lsp_bridge.rs`'s configurable language-server command); `cwd` comes from
a user-typed path in this crate (not a native picker, §1.1) but
`PtySession::spawn` still independently re-validates `.is_dir()` before
spawning, same defense-in-depth `ide-ui`'s version already has for its
own picker-sourced `cwd`; environment inherited unmodified, never logged;
PTY output is `claude`'s own bytes, not attacker-controlled the way a
network peer's would be, but the ANSI parser must still never panic/hang
on malformed sequences (`hacker` should stress this with deliberately
malformed input, same as the original pass did).

### 4.2 Resource cleanup

Same as `ide-ui`: `Drop for PtySession` kills the child; holds for
`close_tab` and for `App` dropping its `claude_terminals: ClaudeTerminalPanel`
normally at process exit — nothing leaks a `PtySession` out of `tabs`.
`ClaudeTerminalPanel::poll()` must be called every frame *unconditionally*
in `lib.rs`'s `run` loop (alongside `poll_cargo`/`poll_search`/etc.), never
gated on `claude_panel_open` — `ide-ui`'s own `hacker` pass already found
and fixed exactly this DoS shape (an ungated channel filling at ~119 MB/s)
when its `poll()` was only reachable through the visible-panel render
path; this port must not reintroduce that mistake.

### 4.3 Resize does not reflow

Unchanged from `ide-ui`: bottom-anchored rows (shrink drops oldest first,
grow adds blank rows at top), left-aligned columns, dropped rows on
shrink are *not* pushed into scrollback and are gone for good on a
subsequent grow. See `crates/ui/src/claude_terminal.rs`'s own worked
example (`docs/features/claude-terminal.md` §5.4) for the exact behaviour
— ported byte-for-byte, this doc doesn't repeat it.

### 4.4 A tab survives its process exiting

Unchanged: `exited = true`, content stays visible, no auto-close. The tab
strip (§3.1's rendering, a header line above the grid) dims/marks an
exited tab's title — rendering detail, not pinned down further here.

### 4.5 Char-boundary safety

Unchanged: `TerminalGrid` buffers a trailing incomplete UTF-8 sequence
across `feed()` calls.

## 5. Examples

```rust
// Chat:
let mut chat = ClaudePanel::default();
chat.submit("hello".to_string());
if chat.poll() { /* history changed, redraw */ }

// Terminal:
let mut panel = ClaudeTerminalPanel::default();
panel.open_tab(PathBuf::from("/tmp/proj"), 24, 80);
if panel.poll() { /* a tab's grid changed, redraw */ }
if let Some(tab) = panel.active_tab_mut() {
    tab.write(b"g")?;
    tab.write(b"\r")?;
}

// Keyboard translation (crossterm, not egui):
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
assert_eq!(claude_terminal::key_event_to_bytes(ctrl_d), Some(vec![0x04]));
```

## 6. Dependencies & integration points

- `crates/tui/Cargo.toml`: add `portable-pty = "0.9.0"` (already
  pre-approved in `CLAUDE.md`'s dependency table for phase F2, already
  used by `ide-ui` for this same feature — the row's "For" column already
  reads "integrated terminal", broad enough to cover a second frontend's
  use of the identical crate for the identical purpose).
- `crates/tui/src/lib.rs`: `mod claude_panel; mod claude_terminal;`, plus
  `app.claude_terminals.poll()` added to the unconditional per-frame
  section of `run` (§4.2), and `app.claude.poll()` alongside it (same
  unconditional-poll reasoning applies to the Chat side too, even though
  it has no comparable DoS shape — consistency with every other polled
  subsystem already in that function).
- `crates/tui/src/app.rs`: new fields (`claude: ClaudePanel`,
  `claude_terminals: ClaudeTerminalPanel`, `claude_view: ClaudeView`,
  `claude_terminal_focus: bool`, `claude_panel_open: bool`,
  `new_claude_terminal: Option<NewClaudeTerminalState>`), `Action::
  ToggleClaudePanel` + `toggle_claude_panel`/`handle_claude_panel_key`/
  `handle_new_claude_terminal_key`, gated in `handle_key`'s dispatch chain
  the same way every other overlay already is.
- `crates/tui/src/commands.rs`: one new `Command` entry
  (`ToggleClaudePanel`, `binding: None`).
- `crates/tui/src/ui.rs`: `render_claude_panel` (tab strip + Chat history/
  input or the active terminal tab's grid) and
  `render_new_claude_terminal_prompt` (same shape as
  `render_new_scratch_file_prompt`).
- Security-sensitive per `CLAUDE.md`'s existing generic "any panel that
  spawns a PTY" bullet — **mandatory `hacker` pass** before merge, same as
  `ide-ui`'s original.
- Single role: `rust-tui-dev`. No `ide-core`/`ide-lsp` changes.

## Revision notes

- `ClaudeTerminalTab`/`ClaudeTerminalPanel` dropped `ide-ui`'s `id: u64`/
  `next_id: u64` entirely: `ide-tui` dispatches to Claude Terminal tabs by
  plain `Vec` index (`ClaudeView::Terminal(usize)`), so there's no
  `egui::Id`-shaped need for a stable identifier independent of position.
  Documented directly on the struct rather than carried over as dead code.
- `handle_claude_terminal_raw_key`'s first draft called a nonexistent
  `self.claude_terminals.tabs_mut()`; `ClaudeTerminalPanel` only exposes
  `tabs()` (immutable) and `active_tab_mut()`. Fixed by routing through
  `active_tab_mut()`, which in turn required `cycle_claude_view` to keep
  `claude_terminals.active` in sync with `claude_view`'s `Terminal(idx)`
  whenever it changes — the first draft updated `claude_view` alone and
  left `claude_terminals.active` stale. Both are now updated together in
  `cycle_claude_view`.
- `confirm_new_claude_terminal`'s first draft duplicated `open_tab`'s own
  graceful error handling with a redundant `dir.is_dir()` pre-check.
  Removed in favor of a pure, separately unit-tested
  `resolve_claude_terminal_dir` helper — `open_tab` never fails (an
  invalid directory just produces an `exited: true` tab whose grid shows
  the error), so re-validating before calling it was dead logic that
  disagreed with the doc's own §3.1 contract.
- Test suite is careful to never let a test actually spawn the real
  `claude` CLI: every `app.rs` test that opens a Claude Terminal tab
  targets a nonexistent path (`/does/not/exist/N`), exercising the same
  graceful `exited: true` path `claude_terminal.rs`'s own test suite
  already relies on. `ClaudePanel::with_runner` was bumped from private to
  `pub(crate)` so the one chat-submission test
  (`claude_chat_enter_submits_and_clears_input`) can inject a fake runner
  instead — unlike `cargo_panel.rs`'s tests, which safely spawn real,
  fast, free `cargo` subcommands, spawning real `claude` in a test suite
  would cost real tokens/API calls on every run.
- Self-performed `hacker`-style adversarial pass (no Agent delegation, per
  this project's standing convention): added a 20,000-random-byte fuzz
  test (`0x1b` overrepresented so the CSI/OSC state machine is actually
  exercised, across four grid sizes) and a live process-cleanup test that
  spawns a real `sleep 100` child and confirms via `kill -0` that `Drop
  for PtySession` actually terminates it at the OS level — both pass.
  Findings doc: `docs/security-findings/tui-claude-panel-2026-08-27.md`,
  verdict Clean, one informational note (Claude Terminal `cwd` isn't
  confined to the project root — a deliberate, accepted design property
  matching `ide-ui`'s own unrestricted folder-picker precedent, not a new
  gap).
- Final coverage: `claude_panel.rs` 92.02% lines, `claude_terminal.rs`
  91.50% lines (both comfortably above the 80% floor).
