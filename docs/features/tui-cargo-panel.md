# `ide-tui`: Cargo panel (T10)

## 1. Purpose

Second item of the TUI-parity backlog (`docs/roadmap.md` §10, driven by
the user's "all ide-gui features must exist in tui version"), picking up
exactly where `tui-problems.md` (`T9`) left off -- that doc's §4
explicitly named this as "the natural place" for build diagnostics later.
Mirrors `ide-ui`'s `cargo_panel.rs`: shell out to `cargo build`/`run`/
`test`/`check`/`clippy`/`fmt`, stream stdout+stderr line by line as the
process runs.

## 2. Interface / API

### 2.1 `src/cargo_panel.rs` (new file)

```rust
pub(crate) enum CargoCommand { Build, Run, Test, Check, Clippy, Fmt }

#[derive(Default)]
pub(crate) struct CargoPanel {
    pub(crate) output: Vec<String>,
    pub(crate) running: Option<CargoCommand>,
}

impl CargoPanel {
    pub(crate) fn run(&mut self, project_root: &Path, command: CargoCommand);
    pub(crate) fn poll(&mut self);
}
```

A near-verbatim port of `ide-ui/src/cargo_panel.rs`'s `CargoPanel`/
`CargoCommand`/`spawn_streaming`/`run_and_stream`/`stream_lines` --
identical background-thread-plus-`mpsc::channel` streaming shape, same
`Command::new("cargo").arg(subcommand).current_dir(project_root)`
construction (single fixed literal per `CargoCommand`, never a
user-supplied string), same "spawn a thread per line-stream (stdout,
stderr), join both, then send `Done`" structure. Duplicated rather than
shared: `ide-tui` has no dependency on `ide-ui` and `crates/core` isn't
the right home for a subprocess-shelling panel, so this is its own
self-contained copy, the same way `ide-tui`'s LSP bridge duplicates
shape (not code) from `ide-ui`'s rather than sharing a crate.

### 2.2 `src/app.rs`

```rust
impl App {
    pub fn poll_cargo(&mut self); // called once per frame by lib.rs's run loop
}
```

`App` gains `cargo: CargoPanel` (always live, like `lsp: LspBridge` --
not an `Option`, since running/output tracking has no "doesn't exist yet"
state) and `cargo_panel_open: bool` (visibility only, same split
`notifications`/`notifications_open` already uses: the data keeps
accumulating in the background regardless of whether the panel is on
screen). `close_all_overlays` gains `self.cargo_panel_open = false;`,
extending the existing three-way mutual exclusion (Goto/Notifications/
Problems) to four.

### 2.3 `src/commands.rs`

One new `Command`: `ToggleCargoPanel`, title "Cargo", **no default
binding** (palette-only, like `ToggleNotifications`). `ide-ui`'s own
binding for the nearest equivalent (the "Run" tool window, which shows
`CargoOutput`) is `⌘4` -- a `Cmd`+digit chord. Every prior `Cmd`+digit
translation this crate has faced (`ToggleProjectToolWindow`'s `⌘1`) hits
the same C0 byte collision noted in `commands.rs`'s own module doc
comment: masking a digit's low 5 bits lands on a letter's `Ctrl` byte
(`Ctrl+4`'s raw byte is identical to `Ctrl+T`, already bound to
`ToggleTreeFocus`). Unlike `ToggleProjectToolWindow`/`FindUsages`/
`ToggleProblems`, this one has no obvious mnemonic unused letter to
substitute in its place, so per `CLAUDE.md`'s own rule ("If it doesn't
[have a safe translation], register the command with no default
binding") it's left unbound, reachable from the command palette
(`Ctrl+Shift+A`) exactly like `ToggleNotifications` already is.

## 3. Behaviour

1. Opening the panel (via the palette) shows either the accumulated
   output of the last-run command, or, if nothing has run yet, a hint
   line: `No output yet -- press b/r/t/c/l/f to run a command.`
2. While the panel has focus (i.e. it's open), six plain, unmodified
   letters each start one `cargo` subcommand: `b` Build, `r` Run, `t`
   Test, `c` Check, `l` Clippy, `f` Fmt -- same no-`Ctrl`-needed shape
   `handle_notifications_key`'s `c`/`r` already established for a
   panel with no text query to type into. Starting a command clears
   `output` and streams new lines in as they arrive, same as `ide-ui`'s
   `CargoPanel::run`. `CargoPanel::run` itself is the sole guard against
   overlapping runs (no-op while `running.is_some()`) -- v1 runs at most
   one command at a time, identical to `ide-ui`.
3. `Esc` closes the panel but does **not** stop a running command --
   `poll_cargo` keeps draining the channel every frame regardless of
   `cargo_panel_open`, the same way `ide-ui`'s background thread keeps
   running whether or not `BottomView::CargoOutput` is the visible tab.
   Reopening the panel (via the palette) shows whatever has accumulated
   since, including a command that finished while the panel was closed.
4. No status-bar badge (unlike Problems' `[N problems]`) -- there's no
   compact "N of something" summary that makes sense for a stream of
   build output the way a diagnostic count does; the title bar shows
   `running` state instead (`cargo build  (running... Esc: close)` vs.
   the six-key hint line when idle).

## 4. Constraints & invariants

- **Fixed argument vector, no shell.** Exactly `ide-ui`'s existing
  invariant, restated here since this is new security-sensitive surface:
  `subcommand` is always one of the six literals `CargoCommand::
  subcommand()` returns -- never a string built from project content,
  file names, or any other untrusted input. No shell is invoked; `cargo`
  runs via `Command::new("cargo").arg(subcommand)`, two explicit argv
  elements.
- **`project_root` only, `current_dir` only.** No other arguments (no
  package selector, no extra flags) -- same v1 scope `ide-ui`'s panel
  already ships with. `project_root` comes from `App::project_root`,
  itself only ever set once, from `Project::open` in `App::new` (a path
  the user passed on the command line or defaulted to `cwd`) -- never
  from tree/editor/LSP-sourced data.
- **Mutual exclusion via `close_all_overlays`.** Opening the Cargo panel
  closes an open Goto picker, Notifications panel, or Problems panel and
  vice versa, extending `tui-problems.md` §4's three-way rule to four.
  `find`/`palette` are unaffected (different, outer interception tier).
- **Streaming survives the panel closing.** See Behaviour §3 -- this is
  the one place this crate's overlay convention (every other overlay's
  state is either fully live only while open, like Goto/Palette, or an
  always-live log like Notifications) picks the "always-live in the
  background" shape, because a build/test run can outlast how long a
  user wants the panel covering their editor.
- **No mouse, no scrollbar.** Same as every other picker in this crate.
  Long output only shows its tail (the last N lines that fit the popup's
  height) -- no manual scroll-back in v1, matching the fact that none of
  this crate's other overlays support scrolling either.

## 5. Examples

```
$ ide-tui ~/code/my-rust-project
```

`Ctrl+Shift+A` → type "Cargo" → Enter opens the panel; `t` runs
`cargo test` in `~/code/my-rust-project`, streaming output live; `Esc`
hides the panel while the test suite keeps running; reopening it via the
palette a minute later shows the completed output.

## 6. Dependencies & integration points

No new dependencies (`std::process`/`std::thread`/`std::sync::mpsc` only,
same as `ide-ui`'s panel). Touches `crates/tui/src/{cargo_panel.rs (new),
app,commands,ui,lib}.rs`.

**Security-sensitive.** `CLAUDE.md` already lists `crates/ui/src/
cargo_panel.rs` as security-sensitive (shells out to `cargo`); this is
its direct TUI-side counterpart, same surface, same reasoning.
`crates/tui/src/cargo_panel.rs` is added to `CLAUDE.md`'s
security-sensitive-paths list alongside its `ide-ui` sibling. A `hacker`
pass ran before merge (`docs/security-findings/
tui-cargo-panel-2026-08-26.md`): clean verdict, one Low-severity finding
(`CargoPanel::output` has no size cap -- inherited from `ide-ui`'s
already-merged panel, not newly introduced here), non-blocking.

## 7. Diagrams

None -- same request/stream/render shape `tui-problems.md` and
`ide-ui`'s own `cargo_panel.rs` already document in prose; no new
component boundary a diagram would clarify.

## Revision notes

Implemented as the second item of the TUI-parity backlog. The
"streaming survives the panel closing" behaviour (§3/§4) was considered
against the simpler alternative (kill the process on `Esc`) and rejected,
since it would silently discard a build/test run's result the moment a
user glances away, which `ide-ui`'s own tab-based panel never does either.
Self-reviewed with both a `rev`-style code/doc pass and a `hacker`-style
adversarial pass (done inline in the same session, not delegated --
`docs/security-findings/tui-cargo-panel-2026-08-26.md`): clean, one Low
finding (unbounded output growth, inherited from `ide-ui`'s already-merged
panel), two controversial-but-non-blocking notes (duplicated
streaming logic between the two crates, matching existing precedent; the
tail-only display's lack of scroll-back is a real usability gap worth a
future increment).
