# TUI: persist last-opened project (T21)

## 1. Purpose

`ide-tui`/`ide --tui` today always requires the caller to already know
which directory to open: an explicit `[project-dir]` argument, or (if
omitted) the current working directory at launch — there is no memory of
what was open last time. `ide-ui` already solved the analogous problem via
`eframe::Storage`'s `LAST_PROJECT_STORAGE_KEY` (`crates/ui/src/app.rs`):
remember the last successfully-opened project root globally (not
per-project), and reopen it automatically when the app starts with nothing
else to go on.

This phase ports that one piece of `ide-ui`'s persistence story to
`ide-tui`. The roadmap entry (`docs/roadmap.md`, T21) frames it as "last
project, theme — по аналогии с `eframe::Storage`"; the theme half does not
apply here and is deliberately out of scope — the roadmap's own `Themes`
row already rules it out for this crate ("терминал — другая среда
рендеринга"; `ide-tui`'s colors are `highlight.rs`'s fixed palette, not a
switchable `egui::Visuals::light()`/`dark()` the way `ide-ui`'s is). There
is nothing analogous to persist. `ide-tui` also has no per-project
`.ide/preferences.json` equivalent (restored tabs, format-on-save toggle,
etc.) — that is a separate, larger feature this doc does not attempt;
scope here is exactly the one global "which directory did I open last"
fact.

## 1.1 Why not `eframe::Storage` itself

`eframe::Storage` is an `eframe`-specific mechanism tied to the `egui`
window's own persistence path; `ide-tui` has no `eframe`/`egui` dependency
at all and never will (`crates/tui/**` is a `ratatui`/`crossterm`
frontend). The equivalent primitive here is a small JSON file at a
per-user, per-application config path — the same *kind* of storage
(global, keyed by application identity, survives across every project you
open), just backed by `std::fs` instead of `eframe`'s platform storage
backend.

## 2. Interface

### 2.1 New module: `crates/tui/src/state.rs`

```rust
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedState {
    pub last_project: Option<PathBuf>,
}

/// Best-effort load: a missing file, malformed JSON, or an unresolvable
/// home directory all yield `PersistedState::default()` rather than an
/// error -- there is no user-facing error channel this early (the
/// terminal isn't even set up yet), and a fresh/broken state file must
/// never block startup.
pub fn load() -> PersistedState;

/// Best-effort save: creates the parent directory if needed; any failure
/// (permission denied, read-only filesystem, no resolvable home
/// directory) is silently swallowed. Persistence is a convenience, never
/// a requirement for `ide-tui` to run.
pub fn save(state: &PersistedState);
```

A private `state_file_path() -> Option<PathBuf>` resolves `$HOME` (falling
back to `$USERPROFILE` for Windows) joined with
`.config/ide-tui/state.json`; returns `None` if neither environment
variable is set, which `load`/`save` both treat as "nothing to do."

### 2.2 `crates/tui/src/lib.rs`

`pub fn main(root: PathBuf) -> ExitCode` becomes:

```rust
pub fn main(root: Option<PathBuf>) -> ExitCode
```

Resolution order for the actual directory to open, computed once at the
top of `main`:

1. `root` if the caller passed one explicitly (an explicit CLI argument
   always wins).
2. Otherwise, `state::load().last_project`, but only if that path
   `.is_dir()` right now — a remembered path for a directory that was
   since deleted or renamed is silently discarded, not surfaced as an
   error.
3. Otherwise, `std::env::current_dir()`, falling back to `PathBuf::from(".")`
   on failure — unchanged from today's behaviour.

Once `App::new(resolved_root.clone())` **succeeds**, `main` calls
`state::save(&state::PersistedState { last_project: Some(resolved_root) })`
before entering the terminal loop. Saving only after a successful open is
deliberate (§4.2) — a bad path never overwrites a previously-good
remembered one.

### 2.3 `crates/tui/src/main.rs` / `crates/ui/src/main.rs`

Both CLI wrappers simplify their argument parsing to stop applying their
own `current_dir()` fallback, since that decision now lives centrally in
`ide_tui::main`:

```rust
// crates/tui/src/main.rs
let root = std::env::args().nth(1).map(PathBuf::from);
ide_tui::main(root)
```

```rust
// crates/ui/src/main.rs, inside the `Some("--tui")` arm
let root = args.next().map(PathBuf::from);
ide_tui::main(root)
```

Both files already only ever called `ide_tui::main` with the fully-resolved
path; this removes the duplicated fallback from each and centralizes it in
one place both binaries call. Consistent with `unified-binary.md`'s
existing framing of `crates/ui/src/main.rs` as "a thin wrapper, not a
reason to pull `rust-tui-dev` into a run that doesn't otherwise touch
`crates/tui/**`" — this run touches both because the shared signature
changed, so per `CLAUDE.md`'s workspace-layout note, `rust-tui-dev`'s
change (the signature itself) merges before `rust-ui-dev`'s (updating the
call site to match).

## 3. Behaviour

- No CLI arg, valid remembered path → reopens it.
- No CLI arg, remembered path no longer exists → falls back to
  `current_dir()`, exactly as if nothing had ever been remembered.
- No CLI arg, nothing ever remembered (fresh install, or the state file
  was never successfully written) → `current_dir()`, unchanged from
  today.
- Explicit CLI arg → always wins, and — once the project opens
  successfully — becomes the new remembered path for the next arg-less
  run.
- A state file that fails to load (malformed JSON, permission error) is
  treated exactly like no state file at all; a state file that fails to
  *save* leaves the previous run's remembered path in place, or is simply
  a no-op if none exists yet.

## 4. Constraints & invariants

1. `load`/`save` never panic — every I/O and (de)serialization failure is
   caught and treated as "nothing to remember" / "nothing to do",
   respectively.
2. A path is only ever persisted **after** `App::new` on it succeeds —
   never eagerly on every launch attempt. This is the one behavioural
   difference from `ide-ui`'s equivalent (which saves whenever the user
   picks a project via the UI, i.e. also always a successful open) worth
   calling out explicitly: `ide-tui`'s only "pick a project" entry point
   *is* process startup, so "successful open" is the only gate available.
3. The state file lives at a fixed per-user path, entirely outside any
   project directory — never confused with, and never conflicting with,
   a project's own contents.
4. The state file's only content is a single optional path. No
   credentials, no per-project settings, nothing else — keeps this phase's
   security surface minimal (a local, user-owned config file with a path
   string in it).
5. Windows path support is best-effort (`$USERPROFILE` fallback) since
   this project's actual CI/dev environment is macOS, but the resolution
   code must not be macOS/Linux-only in an obviously broken way.

## 5. Examples

```rust
// First run ever, nothing remembered, no arg:
ide_tui::main(None);
// -> current_dir(), same as pre-T21 behaviour. If the project opens,
//    state.json now remembers current_dir().

// Second run, no arg, same machine:
ide_tui::main(None);
// -> reopens the directory remembered above.

// Explicit override:
ide_tui::main(Some(PathBuf::from("/tmp/other-project")));
// -> opens /tmp/other-project regardless of what was remembered; on
//    success, /tmp/other-project becomes the new remembered path.
```

## 6. Dependencies & integration points

- New direct dependency in `crates/tui/Cargo.toml`: `serde` (with
  `derive`) and `serde_json` — both already part of this project's base,
  project-wide-approved dependency set per `CLAUDE.md` (used today by
  `ide-core`/`ide-lsp`/`ide-ui`); this is the first time `ide-tui` itself
  depends on them directly, not a new dependency decision.
- `crates/tui/src/lib.rs` (`main`'s signature change), `crates/tui/src/
  main.rs`, `crates/ui/src/main.rs` (call-site update to match), new
  `crates/tui/src/state.rs`.
- No `ide-core`/`ide-lsp` change of any kind.
- Not security-sensitive per `CLAUDE.md`'s list: no subprocess, no
  external CLI, no user-chosen-directory-as-project-root validation logic
  (that remains entirely `ide-core::Project`'s job, unchanged — this phase
  only decides *which* path string to hand to it, and only reads it back
  from a file this project's own code wrote). `hacker` pass not expected.

Tests required:
1. `state::load` on a fresh/nonexistent file returns `PersistedState::default()`.
2. `state::save` then `state::load` round-trips a `PersistedState` with `Some(path)`.
3. `state::load` on malformed JSON content returns `PersistedState::default()` rather than erroring.
4. `state::save` creates the parent directory if it doesn't exist yet.
5. `ide_tui::main`'s resolution order is exercisable as a pure function
   separate from the terminal-owning `main` itself (`resolve_root(explicit:
   Option<PathBuf>, remembered: Option<PathBuf>, cwd: PathBuf) -> PathBuf`
   extracted specifically so this logic is unit-testable without a real
   terminal or real environment variables): explicit wins over remembered
   wins over cwd; a remembered path that doesn't exist on disk is skipped
   in favor of cwd.

## 7. Revision notes

Self-review round (inline, no `hacker` pass — no security-sensitive path
touched):

1. A remembered path passing the `.is_dir()` check is not a full guarantee
   `App::new` will actually succeed on it (e.g. permissions changed
   underneath it since it was last opened). In that case `ide-tui` reports
   the error and exits, exactly like today's behaviour for a bad *explicit*
   CLI argument — this phase doesn't introduce a new failure mode, only a
   new (rare) way to reach the existing one for a no-argument invocation
   that used to unconditionally succeed against `cwd`. Accepted as-is:
   `ide-tui` has no "welcome screen" fallback the way `ide-ui` does, so
   there's nowhere softer to degrade to; matching the existing explicit-arg
   behaviour is the most consistent choice available.
2. `state::load`/`state::save`'s test coverage deliberately never mutates
   the real `$HOME`/`$USERPROFILE` environment variable (racy against
   parallel test threads, and `save` writing to a real developer's actual
   `~/.config/ide-tui/state.json` during `cargo test` would be a genuinely
   bad side effect) — `load_from`/`save_to` are split out as the testable,
   tempdir-pointed core, and only the pure `state_file_path_from_home` join
   logic plus a read-only smoke test of `load()`/`state_file_path()`
   against the real environment cover the public wrapper functions
   themselves.
3. `crates/tui/src/lib.rs` and `crates/tui/src/main.rs`/`crates/ui/src/
   main.rs` are touched by this run even though the feature is centered on
   `state.rs`, because `main`'s signature itself changed
   (`PathBuf` → `Option<PathBuf>`) to let both callers stop duplicating the
   `current_dir()` fallback individually — flagged here since `CLAUDE.md`'s
   role table calls `crates/ui/src/main.rs` out as `rust-ui-dev`-owned and
   this diff crosses that boundary; per the workspace-layout note, the
   `rust-tui-dev` half (the signature change) is what forces the
   `rust-ui-dev` half (the call-site update) to exist at all.
