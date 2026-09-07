# Shared Terminal Core + General-Purpose Terminal (F2)

## 1. Problem

`claude_terminal.rs` is duplicated across GUI (1513 lines) and TUI (1569 lines) with 85%+ identical logic:
- ANSI/CSI parser state machine
- PTY session management
- Terminal grid data structure
- Tab/panel management

Additionally, both are hardcoded to spawn the `claude` CLI — not a general-purpose terminal.

## 2. Solution

Extract framework-independent code into `crates/core/src/terminal/`. Generalize to spawn any shell. Both GUI and TUI use shared core via thin wrappers.

## 3. Module Structure

```
crates/core/src/terminal/
├── mod.rs          — re-exports
├── grid.rs         — TerminalGrid, Cell, AnsiColor, ParserState (ANSI/CSI parser)
├── pty.rs          — PtySession, PtyEvent (PTY management)
├── keymap.rs       — TerminalKey trait, generic key_event_to_bytes
└── panel.rs        — TerminalTab, TerminalPanel (tab management)
```

## 4. What Gets Shared (~85% of current code)

| Component | Source Lines | Description |
|---|---|---|
| `AnsiColor` enum | 32-52 (both) | 17 color variants |
| `Cell` struct | 82-99 (both) | Character + fg/bg + bold |
| `ParserState` enum | 101-118 (both) | ANSI parser states |
| `TerminalGrid` | 116-484 (UI), 123-503 (TUI) | Grid data structure + all parser methods |
| `PtySession` | 520-620 (UI), 540-638 (TUI) | PTY spawn/read/write/resize/drop |
| `PtyEvent` | 514-517 (both) | Data/Exited events |
| `TerminalPanel` | 656-752 (UI), 680-776 (TUI) | Tab management |
| `standard_color`/`bright_color` | 486-512 (UI), 505-531 (TUI) | Color helpers |
| Constants | 13-30 (UI), 24-35 (TUI) | Scrollback limit, max grid, max CSI params |

## 5. Abstractions

### 5.1 Color Abstraction

`AnsiColor::xterm_rgb()` returns `(u8, u8, u8)` tuple instead of framework-specific types:
- GUI maps to `egui::Color32::from_rgb(r, g, b)`
- TUI maps to `ratatui::style::Color::Rgb(r, g, b)`

### 5.2 Input Abstraction

`TerminalKey` trait in `keymap.rs`:

```rust
pub trait TerminalKey {
    fn is_ctrl(&self) -> bool;
    fn letter_byte(&self) -> Option<u8>;
    fn is_enter(&self) -> bool;
    fn is_backspace(&self) -> bool;
    fn is_tab(&self) -> bool;
    fn is_escape(&self) -> bool;
    fn arrow_byte(&self) -> Option<u8>;
    fn home_end_byte(&self) -> Option<u8>;
    fn page_up_down_byte(&self) -> Option<u8>;
    fn function_key(&self) -> Option<u8>;
}
```

Each frontend implements this trait for its event type.

### 5.3 Tab Identity

`TerminalTab::id: Option<u64>` — optional stable ID for GUI focus tracking. TUI passes `None`.

## 6. Generalization

### 6.1 PtySession::spawn

Before (hardcoded):
```rust
pub fn spawn(cwd: &Path, rows: u16, cols: u16) -> Result<Self, String> {
    let mut cmd = CommandBuilder::new("claude");
    // ...
}
```

After (generalized):
```rust
pub fn spawn(
    command: &str,
    args: &[&str],
    cwd: &Path,
    rows: u16,
    cols: u16,
) -> Result<Self, String> {
    let mut cmd = CommandBuilder::new(command);
    for arg in args {
        cmd.arg(arg);
    }
    // ...
}
```

### 6.2 Default Shell

```rust
fn default_shell() -> String {
    std::env::var("SHELL")
        .unwrap_or_else(|_| "/bin/zsh".to_string())
}
```

## 7. GUI Changes

### 7.1 BottomView::Terminal

New variant in `crates/ui/src/app.rs`:
```rust
pub enum BottomView {
    // ... existing variants ...
    Terminal,
}
```

### 7.2 Terminal Command

New command in `crates/ui/src/command.rs`:
```rust
Command {
    id: "Terminal",
    title: "Terminal",
    category: "View",
    binding: Some(Binding::same(KeyChord::new(Key::F12).alt())),
    action: CommandAction::Terminal,
}
```

### 7.3 Terminal Tab Rendering

New function `render_terminal_panel()` in `crates/ui/src/app/render.rs`:
- Renders `TerminalPanel` tabs
- Forwards input events via `TerminalKey` impl for `egui::Event`
- Handles focus switching between tabs

## 8. TUI Changes

Replace `crates/tui/src/claude_terminal.rs` (1569 lines) with thin wrapper (~200 lines):
- Implement `TerminalKey` for `crossterm::event::KeyEvent`
- Use shared `TerminalGrid`, `PtySession`, `TerminalPanel`
- Keep existing rendering logic (ratatui-specific)

## 9. Roles

| Role | Task | Security |
|---|---|---|
| rust-core-dev | Extract shared module | — |
| rust-tui-dev | Update TUI | — |
| rust-ui-dev | Update GUI | — |
| hacker | Security review | Required (PTY spawning) |

## 10. Security

### 10.1 Attack Surface

- PTY spawning: command comes from user config (`$SHELL` or settings)
- Cwd: validated with `is_dir()` before spawn
- Input: raw bytes to PTY, no shell interpolation

### 10.2 Mitigations

- No `Command::new(shell_string)` — always `Command::new(program).args(args)`
- Cwd re-checked with `is_dir()` at spawn time
- Child process killed on `Drop`
- No command construction from untrusted input

### 10.3 Hacker Verdict

Security-sensitive paths touched:
- `crates/core/src/terminal/pty.rs` — PTY spawning
- `crates/ui/src/claude_terminal.rs` — command construction (if any)

Hacker pass required before merge.

## 11. Testing

### 11.1 Shared Core Tests

Move from both `claude_terminal.rs` files to `crates/core/src/terminal/`:
- ANSI parser tests (CSI sequences, SGR, cursor movement, erase)
- PTY spawn/resize/poll tests
- Tab management tests
- UTF-8 handling tests

### 11.2 New Tests

- `default_shell()` returns `$SHELL` or `/bin/zsh`
- `PtySession::spawn` with custom command
- `TerminalKey` trait implementations

### 11.3 Existing Tests

All existing GUI and TUI terminal tests must continue to pass.

## 12. Files Modified

| File | Change |
|---|---|
| `crates/core/src/terminal/` (new) | Shared terminal core |
| `crates/core/src/lib.rs` | Add `pub mod terminal` |
| `crates/core/Cargo.toml` | Add `portable-pty = "0.9.0"` |
| `crates/ui/src/claude_terminal.rs` | Thin wrapper over shared core |
| `crates/ui/src/app.rs` | Add `BottomView::Terminal` |
| `crates/ui/src/app/render.rs` | Add terminal tab + render |
| `crates/ui/src/command.rs` | Add `Terminal` command |
| `crates/tui/src/claude_terminal.rs` | Thin wrapper over shared core |
| `docs/roadmap.md` | Mark F2 ✅ |
