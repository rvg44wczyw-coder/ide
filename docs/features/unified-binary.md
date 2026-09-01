# Unified `ide` binary: GUI by default, `--tui` for the terminal UI

## 1. Purpose

User request: "I want gui and tui to compile in one binary with startup
option --headless (--tui or whatever)". Before this, `ide` (the GUI) and
`ide-tui` (the TUI) were two entirely separate binaries with no shared
entry point -- a user had to know which one to run and type its exact
name. This makes `ide` itself the single artifact users install and run,
with the TUI reachable as a flag rather than a separate program to find.

## 2. Interface / API

### 2.1 `crates/tui/src/lib.rs` (new file)

```rust
pub fn main(root: PathBuf) -> ExitCode;
```

Everything `crates/tui/src/main.rs` used to do directly (terminal setup/
teardown, the panic hook, the event loop) moved here verbatim, behind one
public function. `crates/tui/src/main.rs` is now a four-line CLI wrapper:
parse `argv[1]` as an optional project directory (unchanged behavior),
call `ide_tui::main(root)`. The standalone `ide-tui` binary still builds
and behaves identically to before -- this is a pure extraction, not a
behavior change to that binary.

### 2.2 `crates/ui/src/lib.rs` (new file)

```rust
pub fn run() -> eframe::Result<()>;
```

The GUI's previous `main()` body, moved here verbatim behind one public
function, for the same reason.

### 2.3 `crates/ui/src/main.rs` (rewritten)

```rust
fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--tui") => ide_tui::main(/* argv[2] or cwd */),
        Some(other)   => { /* error, exit 1 */ }
        None          => ide_ui::run(),
    }
}
```

`ide` (this binary) is the unified entry point. No arguments: GUI.
`--tui [project-dir]`: TUI, `project-dir` defaulting to the current
directory exactly like the standalone `ide-tui` binary's own
argument-defaulting already did. Any other first argument: a one-line
error to stderr and exit code 1 -- no silent fallback to the GUI on a
typo.

### 2.4 `crates/ui/Cargo.toml`

New dependency: `ide-tui = { path = "../tui" }`.

## 3. Behaviour

1. `ide` with no arguments launches the GUI, unchanged from before.
2. `ide --tui` launches the TUI against the current directory.
3. `ide --tui ~/code/some-project` launches the TUI against that
   directory -- identical to `ide-tui ~/code/some-project`.
4. `ide-tui` (the standalone binary) is untouched and still works exactly
   as before; it is not deprecated or removed by this change.
5. `ide --anything-else` prints `ide: unrecognized argument '--anything
   -else' (expected \`--tui [project-dir]\`, or no arguments for the GUI)`
   to stderr and exits 1.

## 4. Constraints & invariants

- **One-directional dependency only.** `ide-ui` now depends on `ide-tui`;
  `ide-tui` has no dependency on `ide-ui` and never will (CLAUDE.md's
  workspace-layout note explains the merge-order consequence for
  `dev-chain`). `ide-tui`'s own crate boundary, ownership
  (`rust-tui-dev`), and everything inside `crates/tui/**` are otherwise
  completely unaffected -- this is additive at the edge, not a redesign.
- **No shared process state between the two frontends.** `--tui` doesn't
  hand off an already-open GUI session into the TUI or vice versa; each
  invocation of `ide` picks exactly one frontend for its whole lifetime.
  A future "switch frontend without restarting" feature is out of scope
  here and would be its own doc.
- **Argument parsing stays deliberately minimal.** No `clap`/argument-
  parsing crate added -- two `match` arms on `argv[1]` is the entire
  surface, and `CLAUDE.md`'s dependency table already requires asking
  before adding one; this doesn't need it.
- **Not on `CLAUDE.md`'s security-sensitive path list.** No subprocess,
  no PTY, no user-configurable command construction -- this only changes
  which of two already-existing, already-reviewed code paths a CLI flag
  selects.

## 5. Examples

```bash
$ ide                          # GUI
$ ide --tui                    # TUI, current directory
$ ide --tui ~/code/my-project  # TUI, that project
$ ide-tui ~/code/my-project    # unchanged: same effect, standalone binary
$ ide --bogus
ide: unrecognized argument '--bogus' (expected `--tui [project-dir]`, or no arguments for the GUI)
$ echo $?
1
```

## 6. Dependencies & integration points

New intra-workspace dependency edge `ide-ui → ide-tui` (path dependency,
no new external crate). Touches `crates/tui/{src/lib.rs (new), src/main.rs}`
and `crates/ui/{src/lib.rs (new), src/main.rs, Cargo.toml}`. See §4 for
why no `hacker` pass is needed.

## 7. Diagrams

None -- a two-arm `match` on one CLI argument has no meaningful
sequence/component structure beyond what §2/§3 already state directly.

## Revision notes

Implemented directly in response to a live user request. Both `main.rs`
extractions (`lib.rs`'s `pub fn main`/`pub fn run`) are byte-identical
moves of the previous `main()` bodies -- verified by running both the
standalone `ide-tui` binary's existing test suite (unaffected, since its
modules moved wholesale into the lib target) and a manual smoke test of
`ide --tui <dir>` before merge. Self-reviewed: no controversial findings
-- the one-directional dependency (§4) was the one real design choice in
this batch, and is stated plainly above rather than presented as free.
