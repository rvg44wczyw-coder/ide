# ide

A native, compiled Rust IDE. No JS/TS frontend anywhere: the GUI is built
with `egui`/`eframe` (immediate-mode, pure Rust), the terminal UI with
`ratatui`/`crossterm` — both compile into one native binary, `ide`, per
platform (macOS/Linux/Windows), sharing one core (`ide-core`, `ide-lsp`).

## Workspace layout

```
crates/core/   ide-core   -- editor buffer, project model, git
crates/lsp/    ide-lsp    -- language server client, built on ide-core
crates/tui/    ide-tui    -- ratatui/crossterm terminal UI (lib + standalone bin `ide-tui`)
crates/ui/     ide-ui     -- egui/eframe GUI (lib + the unified bin `ide`, depends on ide-tui for `--tui`)
```

## Prerequisites

- **Rust 1.97.0** — pinned by `rust-toolchain.toml`, so a plain `cargo
  build`/`cargo run` via `rustup` fetches and selects it automatically.
  Not floating `stable` on purpose: a brand-new lint or API change in a
  newer compiler could otherwise break this repo's build with no code
  change at all.
- **Git LFS** — the GUI's embedded font assets
  (`crates/ui/assets/fonts/*.ttf`) are stored as LFS objects. A clone or
  checkout without LFS set up leaves ~130-byte pointer files in their
  place, which fails a compile-time assertion in
  `crates/ui/src/theme/fonts.rs`.
- **A C/C++ compiler** — `ide-core`'s `git2` dependency vendors and
  compiles libgit2 + OpenSSL itself (no system libgit2/OpenSSL needed),
  which still needs a C/C++ toolchain to build: Xcode Command Line Tools
  on macOS, `build-essential` (or your distro's equivalent) on Linux, the
  MSVC Build Tools on Windows.

`make configure` installs/checks all three (see [Makefile
targets](#makefile-targets) below) — run it once after cloning:

```bash
git clone <this repo>
cd ide
make configure
```

## Building and running

```bash
# GUI (default)
cargo run -p ide-ui --bin ide
# or: make run

# Terminal UI, via the same binary
cargo run -p ide-ui --bin ide -- --tui <project-directory>
# or: make run-tui ARGS=<project-directory>

# Terminal UI, standalone binary (equivalent, still built and works on its own)
cargo run -p ide-tui --bin ide-tui -- <project-directory>

# GUI, opening a project directly (skips the multi-window restore prompt)
cargo run -p ide-ui --bin ide -- <project-directory>
```

A release build (`cargo build --release --workspace --bins`, or `make
release`) produces `target/release/ide` and `target/release/ide-tui`.
`make install` builds a release binary and installs it as `ide` into
`~/.cargo/bin` (on `PATH` for anyone using `rustup`), running `configure`
first.

### Prebuilt binaries

Every tagged release (`vX.Y.Z`) is built for macOS, Linux, and Windows by
CI and attached to that tag's [GitHub
Release](../../releases) as `ide-<platform>` and
`ide-tui-<platform>` — no local Rust toolchain needed if a prebuilt binary
covers your platform.

## Makefile targets

| Target | What it does |
|---|---|
| `make configure` | Installs the pinned Rust toolchain + `rustfmt`/`clippy`, sets up Git LFS and pulls the font assets, checks for a C/C++ compiler. Safe to re-run. |
| `make build` | Debug build of the whole workspace. |
| `make release` | Optimized build. |
| `make run` | Runs the GUI (`cargo run -p ide-ui`). |
| `make run-tui ARGS=<dir>` | Runs the TUI via the unified binary's `--tui` flag. |
| `make test` | Runs the workspace test suite. |
| `make fmt` / `make fmt-fix` | Checks / applies `cargo fmt`. |
| `make clippy` | Runs clippy with warnings denied. |
| `make check` (alias `make ci`) | `fmt` + `clippy` + `build` + `test`, in the same order CI runs them — reproduces a CI failure locally. |
| `make install` / `make uninstall` | Installs/uninstalls the `ide` binary via `cargo install`. |
| `make bench` / `make bench-mem` | CPU / peak-memory benchmarks (`docs/features/perf-baseline.md`). |
| `make clean` | `cargo clean`. |

## `ide-tui`: supported terminals

`ide-tui` runs in any terminal that speaks standard ANSI/VT input and
output — there's no hard minimum. But a handful of its keybindings
(everything using `Ctrl+Shift+<letter>`, e.g. Redo, Find Previous,
Replace All, Next/Previous Tab) need a terminal that supports the
[Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
(`CSI u` / "disambiguate escape codes"). Without it, a legacy terminal
has no way to tell `Ctrl+Shift+Z` apart from plain `Ctrl+Z` on the wire —
see `crates/tui/src/main.rs`'s and `crates/tui/src/commands.rs`'s own doc
comments for the full byte-level explanation.

`ide-tui` detects this automatically at startup
(`crossterm::terminal::supports_keyboard_enhancement`) and opts in when
available — there's nothing to configure. On a terminal without support,
every other feature (editing, tabs, find/replace, Go to Declaration/Find
Usages, notifications, tree navigation) works exactly the same; only the
`Ctrl+Shift+*` chords listed above are unreachable, with no crash and no
degraded rendering.

**Known to support the Kitty protocol** (full keybinding set):

| Terminal | Platform | Notes |
|---|---|---|
| [kitty](https://sw.kovidgoyal.net/kitty/) | macOS/Linux | reference implementation |
| [WezTerm](https://wezfurlong.org/wezterm/) | macOS/Linux/Windows | |
| [iTerm2](https://iterm2.com/) | macOS | 3.5+ |
| [Ghostty](https://ghostty.org/) | macOS/Linux | |
| [Alacritty](https://alacritty.org/) | macOS/Linux/Windows | recent releases |
| [foot](https://codeberg.org/dnkl/foot) | Linux (Wayland) | |
| [Contour](https://contour-terminal.org/) | macOS/Linux/Windows | |
| [Rio](https://raphamorim.io/rio/) | macOS/Linux/Windows | |

**Known not to support it yet** (`ide-tui` still runs, `Ctrl+Shift+*`
chords are inert — reach the same commands via `Ctrl+Shift+A`'s command
palette instead):

| Terminal | Notes |
|---|---|
| macOS `Terminal.app` | no Kitty protocol support |
| GNOME Terminal / other VTE-based terminals | no Kitty protocol support as of writing |
| `tmux` | passes it through only with `allow-passthrough` enabled, and only to a supporting outer terminal |
| GNU `screen` | no support |

This list isn't exhaustive and terminal support for the protocol is
actively growing — if in doubt, just run `ide-tui` and press
`Ctrl+Shift+A`: if the command palette opens, the full keybinding set is
active.
