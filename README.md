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

## Building and running

```bash
# GUI (default)
cargo run -p ide-ui --bin ide

# Terminal UI, via the same binary
cargo run -p ide-ui --bin ide -- --tui <project-directory>

# Terminal UI, standalone binary (equivalent, still built and works on its own)
cargo run -p ide-tui --bin ide-tui -- <project-directory>
```

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
