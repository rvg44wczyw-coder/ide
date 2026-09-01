//! Interactive `claude` CLI sessions in PTY-backed tabs
//! (`docs/features/tui-claude-panel.md` §2.2). Distinct from
//! `claude_panel.rs`'s one-shot request/response wrapper, which this
//! doesn't touch. Ported from `crates/ui/src/claude_terminal.rs`'s pure
//! logic (`AnsiColor`/`Cell`/`TerminalGrid`/`PtySession`/
//! `ClaudeTerminalTab`/`ClaudeTerminalPanel`, none of which depend on
//! `egui`) -- only `AnsiColor::xterm_rgb`'s return type and
//! `key_event_to_bytes` are framework-specific and were rewritten for
//! `ratatui`/`crossterm` (§1.1 of the doc explains why the keyboard-
//! forwarding contract itself also had to change: no mouse means no
//! "click elsewhere to defocus", so `app.rs` gates when this module's
//! `key_event_to_bytes` is even called).

use ratatui::style::Color;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

pub const TERMINAL_SCROLLBACK_LIMIT: usize = 2000;

/// `TerminalGrid::new`/`resize` clamp `rows`/`cols` to this ceiling --
/// carried over unchanged from `ide-ui`'s post-`hacker`-pass hardening
/// (`docs/security-findings/rust-ui-dev-claude-terminal-2026-08-25.md`
/// finding 5): not practically reachable through real terminal-size
/// metrics, but free to close off.
const MAX_GRID_DIMENSION: usize = 4096;

/// Bounds `Vec<Option<u16>>` growth against an adversarial stream of
/// `;`-separated params in one unterminated CSI sequence.
const MAX_CSI_PARAMS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnsiColor {
    #[default]
    Default,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl AnsiColor {
    /// Fixed xterm-standard RGB values, independent of any IDE theme
    /// (this crate has none regardless -- `docs/roadmap.md`'s T21 entry).
    /// `None` for `Default`: the caller renders with no explicit fg/bg at
    /// all, inheriting the real outer terminal's own colors.
    pub fn xterm_rgb(self) -> Option<Color> {
        use AnsiColor::*;
        Some(match self {
            Default => return None,
            Black => Color::Rgb(0x00, 0x00, 0x00),
            Red => Color::Rgb(0xCD, 0x00, 0x00),
            Green => Color::Rgb(0x00, 0xCD, 0x00),
            Yellow => Color::Rgb(0xCD, 0xCD, 0x00),
            Blue => Color::Rgb(0x00, 0x00, 0xEE),
            Magenta => Color::Rgb(0xCD, 0x00, 0xCD),
            Cyan => Color::Rgb(0x00, 0xCD, 0xCD),
            White => Color::Rgb(0xE5, 0xE5, 0xE5),
            BrightBlack => Color::Rgb(0x7F, 0x7F, 0x7F),
            BrightRed => Color::Rgb(0xFF, 0x00, 0x00),
            BrightGreen => Color::Rgb(0x00, 0xFF, 0x00),
            BrightYellow => Color::Rgb(0xFF, 0xFF, 0x00),
            BrightBlue => Color::Rgb(0x5C, 0x5C, 0xFF),
            BrightMagenta => Color::Rgb(0xFF, 0x00, 0xFF),
            BrightCyan => Color::Rgb(0x00, 0xFF, 0xFF),
            BrightWhite => Color::Rgb(0xFF, 0xFF, 0xFF),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: AnsiColor,
    pub bg: AnsiColor,
    pub bold: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: AnsiColor::Default,
            bg: AnsiColor::Default,
            bold: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    Csi {
        params: Vec<Option<u16>>,
        current: Option<u16>,
        private: bool,
    },
    Osc,
    OscEscape,
}

/// Hand-rolled, bounded ANSI/CSI subset (`docs/features/
/// tui-claude-panel.md` §2.2) -- not a full xterm emulator. Pure, no I/O:
/// `feed()` is the only way in.
pub struct TerminalGrid {
    rows: usize,
    cols: usize,
    viewport: Vec<Vec<Cell>>,
    scrollback: VecDeque<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    cur_fg: AnsiColor,
    cur_bg: AnsiColor,
    cur_bold: bool,
    state: ParserState,
    partial_utf8: Vec<u8>,
}

impl TerminalGrid {
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.clamp(1, MAX_GRID_DIMENSION);
        let cols = cols.clamp(1, MAX_GRID_DIMENSION);
        TerminalGrid {
            rows,
            cols,
            viewport: vec![vec![Cell::default(); cols]; rows],
            scrollback: VecDeque::new(),
            cursor_row: 0,
            cursor_col: 0,
            cur_fg: AnsiColor::Default,
            cur_bg: AnsiColor::Default,
            cur_bold: false,
            state: ParserState::Ground,
            partial_utf8: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed_byte(b);
        }
    }

    /// Bottom-anchored for rows, left-aligned for columns (`docs/features/
    /// tui-claude-panel.md` §4.3): shrinking drops the oldest visible
    /// rows first, matching the direction scrollback already ages in, so
    /// the cursor's row always survives.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.clamp(1, MAX_GRID_DIMENSION);
        let cols = cols.clamp(1, MAX_GRID_DIMENSION);
        let mut new_viewport = vec![vec![Cell::default(); cols]; rows];

        let copy_rows = rows.min(self.rows);
        let copy_cols = cols.min(self.cols);
        let old_row_start = self.rows - copy_rows;
        let new_row_start = rows - copy_rows;
        for i in 0..copy_rows {
            new_viewport[new_row_start + i][..copy_cols]
                .clone_from_slice(&self.viewport[old_row_start + i][..copy_cols]);
        }

        self.viewport = new_viewport;
        self.rows = rows;
        self.cols = cols;
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    pub fn visible_rows(&self) -> &[Vec<Cell>] {
        &self.viewport
    }

    /// No non-test call site: this crate's v1 rendering never shows
    /// scrollback (`docs/features/tui-claude-panel.md` §1.1 -- the real
    /// outer terminal's own scrollback already covers it). Kept
    /// `#[cfg(test)]` rather than deleted, for the same reason
    /// `commands.rs`'s `binding_for` was demoted rather than removed: it's
    /// still the most direct way this crate's own tests verify scrolling-
    /// into-scrollback behaviour, matching `ide-ui`'s equivalent tests.
    #[cfg(test)]
    pub fn scrollback_rows(&self) -> &VecDeque<Vec<Cell>> {
        &self.scrollback
    }

    /// No non-test call site, same reasoning as `scrollback_rows` above --
    /// `ide-ui`'s "Copy All" button has no `ide-tui` equivalent (§1.1: the
    /// real outer terminal already provides native copy).
    #[cfg(test)]
    pub fn plain_text(&self) -> String {
        self.scrollback
            .iter()
            .chain(self.viewport.iter())
            .map(|row| {
                let mut s: String = row.iter().map(|c| c.ch).collect();
                while s.ends_with(' ') {
                    s.pop();
                }
                s
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn feed_byte(&mut self, b: u8) {
        if matches!(self.state, ParserState::Ground) && b >= 0x80 {
            self.feed_utf8_byte(b);
            return;
        }
        match &self.state {
            ParserState::Ground => self.feed_ground(b),
            ParserState::Escape => self.feed_escape(b),
            ParserState::Csi { .. } => self.feed_csi(b),
            ParserState::Osc => self.feed_osc(b),
            ParserState::OscEscape => self.feed_osc_escape(b),
        }
    }

    fn feed_utf8_byte(&mut self, b: u8) {
        self.partial_utf8.push(b);
        match std::str::from_utf8(&self.partial_utf8) {
            Ok(s) => {
                if let Some(ch) = s.chars().next() {
                    self.put_char(ch);
                }
                self.partial_utf8.clear();
            }
            Err(e) if e.error_len().is_none() => {
                // Incomplete but valid so far -- wait for more bytes,
                // possibly across the next `feed()` call.
            }
            Err(_) => {
                self.put_char(char::REPLACEMENT_CHARACTER);
                self.partial_utf8.clear();
            }
        }
    }

    fn feed_ground(&mut self, b: u8) {
        match b {
            0x1b => self.state = ParserState::Escape,
            b'\r' => self.cursor_col = 0,
            b'\n' => self.advance_row(),
            0x08 => self.cursor_col = self.cursor_col.saturating_sub(1),
            b'\t' => {
                let next_stop = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next_stop.min(self.cols - 1);
            }
            0x00..=0x1f => {} // BEL and other C0 controls: ignored
            _ => self.put_char(b as char),
        }
    }

    fn feed_escape(&mut self, b: u8) {
        match b {
            b'[' => {
                self.state = ParserState::Csi {
                    params: Vec::new(),
                    current: None,
                    private: false,
                }
            }
            b']' => self.state = ParserState::Osc,
            _ => self.state = ParserState::Ground, // any other ESC <byte>: swallowed
        }
    }

    fn feed_csi(&mut self, b: u8) {
        let ParserState::Csi {
            mut params,
            mut current,
            private,
        } = std::mem::replace(&mut self.state, ParserState::Ground)
        else {
            unreachable!("feed_csi only called while self.state is Csi");
        };
        match b {
            b'0'..=b'9' => {
                let d = (b - b'0') as u16;
                current = Some(current.unwrap_or(0).saturating_mul(10).saturating_add(d));
                self.state = ParserState::Csi {
                    params,
                    current,
                    private,
                };
            }
            b';' => {
                if params.len() < MAX_CSI_PARAMS {
                    params.push(current.take());
                }
                self.state = ParserState::Csi {
                    params,
                    current,
                    private,
                };
            }
            b'?' if params.is_empty() && current.is_none() => {
                self.state = ParserState::Csi {
                    params,
                    current,
                    private: true,
                };
            }
            0x40..=0x7e => {
                if params.len() < MAX_CSI_PARAMS {
                    params.push(current);
                }
                self.dispatch_csi(&params, private, b);
                // self.state is already Ground, left there by mem::replace.
            }
            _ => {
                // Unrecognized intermediate byte: ignored, stay in Csi.
                self.state = ParserState::Csi {
                    params,
                    current,
                    private,
                };
            }
        }
    }

    fn feed_osc(&mut self, b: u8) {
        match b {
            0x07 => self.state = ParserState::Ground,
            0x1b => self.state = ParserState::OscEscape,
            _ => {} // stay in Osc, consume & discard
        }
    }

    fn feed_osc_escape(&mut self, b: u8) {
        match b {
            b'\\' => self.state = ParserState::Ground, // ST terminator complete
            _ => self.state = ParserState::Osc,
        }
    }

    fn dispatch_csi(&mut self, params: &[Option<u16>], _private: bool, final_byte: u8) {
        let get = |i: usize, default: u16| params.get(i).copied().flatten().unwrap_or(default);
        match final_byte {
            b'm' => self.dispatch_sgr(params),
            b'H' | b'f' => {
                let row = get(0, 1).saturating_sub(1) as usize;
                let col = get(1, 1).saturating_sub(1) as usize;
                self.cursor_row = row.min(self.rows - 1);
                self.cursor_col = col.min(self.cols - 1);
            }
            b'A' => {
                let n = get(0, 1) as usize;
                self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            b'B' => {
                let n = get(0, 1) as usize;
                self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
            }
            b'C' => {
                let n = get(0, 1) as usize;
                self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
            }
            b'D' => {
                let n = get(0, 1) as usize;
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            b'J' => self.erase_display(get(0, 0)),
            b'K' => self.erase_line(get(0, 0)),
            _ => {}
        }
    }

    fn dispatch_sgr(&mut self, params: &[Option<u16>]) {
        if params.is_empty() {
            self.cur_fg = AnsiColor::Default;
            self.cur_bg = AnsiColor::Default;
            self.cur_bold = false;
            return;
        }
        for p in params {
            match p.unwrap_or(0) {
                0 => {
                    self.cur_fg = AnsiColor::Default;
                    self.cur_bg = AnsiColor::Default;
                    self.cur_bold = false;
                }
                1 => self.cur_bold = true,
                22 => self.cur_bold = false,
                code @ 30..=37 => self.cur_fg = standard_color(code - 30),
                code @ 90..=97 => self.cur_fg = bright_color(code - 90),
                39 => self.cur_fg = AnsiColor::Default,
                code @ 40..=47 => self.cur_bg = standard_color(code - 40),
                code @ 100..=107 => self.cur_bg = bright_color(code - 100),
                49 => self.cur_bg = AnsiColor::Default,
                _ => {}
            }
        }
    }

    fn erase_display(&mut self, mode: u16) {
        match mode {
            0 => {
                self.clear_line_from(self.cursor_row, self.cursor_col);
                for r in (self.cursor_row + 1)..self.rows {
                    self.clear_row(r);
                }
            }
            1 => {
                for r in 0..self.cursor_row {
                    self.clear_row(r);
                }
                self.clear_line_to(self.cursor_row, self.cursor_col);
            }
            2 => {
                for r in 0..self.rows {
                    self.clear_row(r);
                }
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        match mode {
            0 => self.clear_line_from(self.cursor_row, self.cursor_col),
            1 => self.clear_line_to(self.cursor_row, self.cursor_col),
            2 => self.clear_row(self.cursor_row),
            _ => {}
        }
    }

    fn clear_row(&mut self, row: usize) {
        self.viewport[row] = vec![Cell::default(); self.cols];
    }

    fn clear_line_from(&mut self, row: usize, col: usize) {
        for c in col..self.cols {
            self.viewport[row][c] = Cell::default();
        }
    }

    fn clear_line_to(&mut self, row: usize, col: usize) {
        for c in 0..=col.min(self.cols - 1) {
            self.viewport[row][c] = Cell::default();
        }
    }

    fn put_char(&mut self, ch: char) {
        self.viewport[self.cursor_row][self.cursor_col] = Cell {
            ch,
            fg: self.cur_fg,
            bg: self.cur_bg,
            bold: self.cur_bold,
        };
        self.cursor_col += 1;
        if self.cursor_col >= self.cols {
            self.cursor_col = 0;
            self.advance_row();
        }
    }

    fn advance_row(&mut self) {
        if self.cursor_row + 1 >= self.rows {
            self.scroll_up();
        } else {
            self.cursor_row += 1;
        }
    }

    fn scroll_up(&mut self) {
        let old_top = self.viewport.remove(0);
        self.scrollback.push_back(old_top);
        if self.scrollback.len() > TERMINAL_SCROLLBACK_LIMIT {
            self.scrollback.pop_front();
        }
        self.viewport.push(vec![Cell::default(); self.cols]);
    }
}

fn standard_color(n: u16) -> AnsiColor {
    match n {
        0 => AnsiColor::Black,
        1 => AnsiColor::Red,
        2 => AnsiColor::Green,
        3 => AnsiColor::Yellow,
        4 => AnsiColor::Blue,
        5 => AnsiColor::Magenta,
        6 => AnsiColor::Cyan,
        7 => AnsiColor::White,
        _ => AnsiColor::Default,
    }
}

fn bright_color(n: u16) -> AnsiColor {
    match n {
        0 => AnsiColor::BrightBlack,
        1 => AnsiColor::BrightRed,
        2 => AnsiColor::BrightGreen,
        3 => AnsiColor::BrightYellow,
        4 => AnsiColor::BrightBlue,
        5 => AnsiColor::BrightMagenta,
        6 => AnsiColor::BrightCyan,
        7 => AnsiColor::BrightWhite,
        _ => AnsiColor::Default,
    }
}

enum PtyEvent {
    Data(Vec<u8>),
    Exited,
}

/// One `claude` child process behind a PTY (`docs/features/
/// tui-claude-panel.md` §2.2).
pub struct PtySession {
    writer: Box<dyn Write + Send>,
    rx: Receiver<PtyEvent>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtySession {
    /// Spawns the fixed literal `"claude"` (no shell, no args) via
    /// `portable_pty::native_pty_system()`. `cwd` is re-checked with
    /// `is_dir()` here rather than trusted from the caller (the directory
    /// could be deleted/unmounted between the caller resolving it and
    /// this call). Environment is inherited unmodified --
    /// `CommandBuilder::new` already does this by default.
    pub fn spawn(cwd: &Path, rows: u16, cols: u16) -> Result<Self, String> {
        if !cwd.is_dir() {
            return Err(format!("{} is not a directory", cwd.display()));
        }
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;

        let mut cmd = CommandBuilder::new("claude");
        cmd.cwd(cwd);
        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                if io_err.kind() == std::io::ErrorKind::NotFound {
                    return "claude CLI not found on PATH".to_string();
                }
            }
            format!("failed to launch claude: {e}")
        })?;

        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(PtyEvent::Exited);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(PtyEvent::Data(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(PtyEvent::Exited);
                        break;
                    }
                }
            }
        });

        Ok(PtySession {
            writer,
            rx,
            master: pair.master,
            child,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn poll(&mut self) -> Vec<PtyEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

pub struct ClaudeTerminalTab {
    /// Shown in the panel's title bar while this tab is active (`docs/
    /// features/tui-claude-panel.md` §3.1) -- `ide-ui`'s equivalent also
    /// keeps a stable `id: u64` for `egui::Id`-based focus tracking, which
    /// has no `ide-tui` equivalent: every dispatch site here already
    /// addresses tabs by their plain `Vec` index (`ClaudeView::Terminal`),
    /// kept correct across closes by `close_tab`'s own index-adjustment
    /// rules, so a separate stable identity would be unused state.
    pub cwd: PathBuf,
    pub title: String,
    /// `true` once the child has exited; the tab stays open regardless
    /// (`docs/features/tui-claude-panel.md` §4.4).
    pub exited: bool,
    grid: TerminalGrid,
    /// `None` when `PtySession::spawn` failed -- the tab still exists
    /// (`exited: true`, the spawn error fed into `grid` as text) rather
    /// than being silently dropped.
    pty: Option<PtySession>,
}

impl ClaudeTerminalTab {
    pub fn grid(&self) -> &TerminalGrid {
        &self.grid
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match &mut self.pty {
            Some(pty) => pty.write(bytes),
            None => Ok(()), // exited tab: typing into it is a harmless no-op
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.grid.resize(rows as usize, cols as usize);
        if let Some(pty) = &self.pty {
            pty.resize(rows, cols);
        }
    }
}

#[derive(Default)]
pub struct ClaudeTerminalPanel {
    tabs: Vec<ClaudeTerminalTab>,
    pub active: Option<usize>,
}

impl ClaudeTerminalPanel {
    /// Always creates and selects a tab, even if `claude` couldn't be
    /// spawned -- a failed spawn is shown inline (`exited: true`, the
    /// error text fed into the tab's grid), never silently dropped.
    pub fn open_tab(&mut self, cwd: PathBuf, rows: u16, cols: u16) {
        let title = cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "claude".to_string());

        let mut grid = TerminalGrid::new(rows as usize, cols as usize);
        let (pty, exited) = match PtySession::spawn(&cwd, rows, cols) {
            Ok(pty) => (Some(pty), false),
            Err(err) => {
                grid.feed(err.as_bytes());
                (None, true)
            }
        };

        self.tabs.push(ClaudeTerminalTab {
            cwd,
            title,
            exited,
            grid,
            pty,
        });
        self.active = Some(self.tabs.len() - 1);
    }

    /// Removes the tab at `index` (dropping its `PtySession`, which kills
    /// the child if still running). If the closed tab was active, the
    /// newly active tab is the one now at the same index, the previous
    /// index if that was the last tab, or `None` if no tabs remain --
    /// never out of bounds. Closing a tab before the active one shifts
    /// `active` down by one so it still points at the same tab.
    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        self.active = match self.active {
            Some(active) if active == index => {
                if self.tabs.is_empty() {
                    None
                } else {
                    Some(active.min(self.tabs.len() - 1))
                }
            }
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
    }

    pub fn tabs(&self) -> &[ClaudeTerminalTab] {
        &self.tabs
    }

    /// Polls every tab's `PtySession` and feeds received bytes into that
    /// tab's `TerminalGrid`. Returns `true` if anything changed. Must be
    /// called every frame regardless of whether the panel is visible
    /// (`docs/features/tui-claude-panel.md` §4.2 -- `ide-ui`'s own
    /// `hacker` pass already found the DoS shape of gating this on panel
    /// visibility).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for tab in &mut self.tabs {
            let Some(pty) = &mut tab.pty else { continue };
            for event in pty.poll() {
                match event {
                    PtyEvent::Data(bytes) => {
                        tab.grid.feed(&bytes);
                        changed = true;
                    }
                    PtyEvent::Exited => {
                        tab.exited = true;
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    pub fn active_tab(&self) -> Option<&ClaudeTerminalTab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut ClaudeTerminalTab> {
        self.active.and_then(move |i| self.tabs.get_mut(i))
    }
}

fn ctrl_letter_byte(c: char) -> Option<u8> {
    if c.is_ascii_alphabetic() {
        Some(c.to_ascii_uppercase() as u8 - b'A' + 1)
    } else {
        None
    }
}

/// Translates one raw-focused key event into the bytes a real terminal
/// would send for it, or `None` for a key that isn't forwarded
/// (`docs/features/tui-claude-panel.md` §2.2/§3.4). Pure, no I/O. Callers
/// decide *when* this is even invoked -- `app.rs`'s `claude_terminal_focus`
/// gate, and its own `Shift+Esc` interception before ever reaching this
/// function (§1.1 of the doc: there is no mouse-click-elsewhere to
/// defocus with, unlike `ide-ui`'s equivalent).
pub fn key_event_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = key.code {
            if let Some(byte) = ctrl_letter_byte(c) {
                return Some(vec![byte]);
            }
        }
    }
    match key.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells_text(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect()
    }

    #[test]
    fn feeds_plain_ascii_text() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"hi");
        assert_eq!(cells_text(&grid.visible_rows()[0][0..2]), "hi");
        assert_eq!(grid.cursor(), (0, 2));
    }

    #[test]
    fn carriage_return_resets_column_not_row() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"abc\r12");
        assert_eq!(cells_text(&grid.visible_rows()[0][0..3]), "12c");
        assert_eq!(grid.cursor(), (0, 2));
    }

    #[test]
    fn line_feed_advances_row_without_touching_column() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"ab\n");
        assert_eq!(grid.cursor(), (1, 2));
    }

    #[test]
    fn line_feed_on_the_last_row_scrolls_into_scrollback() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(grid.scrollback_rows().len(), 1);
        assert_eq!(cells_text(&grid.scrollback_rows()[0][0..3]), "one");
        assert_eq!(cells_text(&grid.visible_rows()[0][0..3]), "two");
        assert_eq!(cells_text(&grid.visible_rows()[1][0..5]), "three");
    }

    #[test]
    fn backspace_moves_cursor_back_without_deleting() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"ab\x08c");
        assert_eq!(cells_text(&grid.visible_rows()[0][0..2]), "ac");
        assert_eq!(grid.cursor(), (0, 2));
    }

    #[test]
    fn backspace_at_column_zero_does_not_underflow() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x08\x08x");
        assert_eq!(grid.cursor(), (0, 1));
    }

    #[test]
    fn tab_advances_to_the_next_multiple_of_eight() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"a\t");
        assert_eq!(grid.cursor(), (0, 8));
    }

    #[test]
    fn tab_clamps_to_the_last_column() {
        let mut grid = TerminalGrid::new(3, 5);
        grid.feed(b"ab\t");
        assert_eq!(grid.cursor(), (0, 4));
    }

    #[test]
    fn bel_and_other_c0_controls_are_ignored() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"a\x07\x01\x02b");
        assert_eq!(cells_text(&grid.visible_rows()[0][0..2]), "ab");
    }

    #[test]
    fn wrapping_at_the_right_margin_advances_a_row() {
        let mut grid = TerminalGrid::new(3, 3);
        grid.feed(b"abcd");
        assert_eq!(cells_text(&grid.visible_rows()[0]), "abc");
        assert_eq!(cells_text(&grid.visible_rows()[1][0..1]), "d");
        assert_eq!(grid.cursor(), (1, 1));
    }

    #[test]
    fn sgr_sets_and_resets_fg_color() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b[32mhi\x1b[0m");
        assert_eq!(grid.visible_rows()[0][0].fg, AnsiColor::Green);
        assert_eq!(grid.visible_rows()[0][1].fg, AnsiColor::Green);
        assert_eq!(grid.cursor(), (0, 2));
        grid.feed(b"x");
        assert_eq!(grid.visible_rows()[0][2].fg, AnsiColor::Default);
    }

    #[test]
    fn sgr_bright_bg_and_bold() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b[1;101my");
        let cell = grid.visible_rows()[0][0];
        assert_eq!(cell.bg, AnsiColor::BrightRed);
        assert!(cell.bold);
    }

    #[test]
    fn sgr_22_clears_bold_without_touching_color() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b[1;32m\x1b[22mz");
        let cell = grid.visible_rows()[0][0];
        assert!(!cell.bold);
        assert_eq!(cell.fg, AnsiColor::Green);
    }

    #[test]
    fn sgr_39_and_49_reset_only_fg_or_bg() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b[31;44m\x1b[39mz");
        assert_eq!(grid.visible_rows()[0][0].fg, AnsiColor::Default);
        assert_eq!(grid.visible_rows()[0][0].bg, AnsiColor::Blue);
    }

    #[test]
    fn unknown_sgr_codes_are_ignored_not_errors() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b[38;5;200mz"); // 256-color, out of scope
        let cell = grid.visible_rows()[0][0];
        assert_eq!(cell.fg, AnsiColor::Default);
        assert_eq!(cell.ch, 'z');
    }

    #[test]
    fn split_escape_sequence_across_two_feed_calls() {
        let mut a = TerminalGrid::new(3, 10);
        a.feed(b"\x1b[32mhi\x1b[0m");

        let mut b = TerminalGrid::new(3, 10);
        b.feed(b"\x1b[3");
        b.feed(b"2mhi\x1b[0m");

        assert_eq!(a.visible_rows()[0][0], b.visible_rows()[0][0]);
        assert_eq!(a.visible_rows()[0][1], b.visible_rows()[0][1]);
        assert_eq!(a.cursor(), b.cursor());
    }

    #[test]
    fn cup_moves_cursor_to_one_indexed_position() {
        let mut grid = TerminalGrid::new(5, 10);
        grid.feed(b"\x1b[3;4H");
        assert_eq!(grid.cursor(), (2, 3));
    }

    #[test]
    fn cup_missing_params_default_to_top_left() {
        let mut grid = TerminalGrid::new(5, 10);
        grid.feed(b"\x1b[5;5H\x1b[H");
        assert_eq!(grid.cursor(), (0, 0));
    }

    #[test]
    fn cup_clamps_to_grid_bounds() {
        let mut grid = TerminalGrid::new(3, 3);
        grid.feed(b"\x1b[99;99H");
        assert_eq!(grid.cursor(), (2, 2));
    }

    #[test]
    fn cursor_up_down_forward_back() {
        let mut grid = TerminalGrid::new(5, 5);
        grid.feed(b"\x1b[3;3H");
        grid.feed(b"\x1b[1A");
        assert_eq!(grid.cursor(), (1, 2));
        grid.feed(b"\x1b[2B");
        assert_eq!(grid.cursor(), (3, 2));
        grid.feed(b"\x1b[2C");
        assert_eq!(grid.cursor(), (3, 4));
        grid.feed(b"\x1b[3D");
        assert_eq!(grid.cursor(), (3, 1));
    }

    #[test]
    fn cursor_moves_clamp_at_grid_edges() {
        let mut grid = TerminalGrid::new(3, 3);
        grid.feed(b"\x1b[99A");
        assert_eq!(grid.cursor(), (0, 0));
        grid.feed(b"\x1b[99C");
        assert_eq!(grid.cursor(), (0, 2));
        grid.feed(b"\x1b[99B");
        assert_eq!(grid.cursor(), (2, 2));
        grid.feed(b"\x1b[99D");
        assert_eq!(grid.cursor(), (2, 0));
    }

    #[test]
    fn ed_mode_0_erases_from_cursor_to_end_of_display() {
        let mut grid = TerminalGrid::new(2, 6);
        grid.feed(b"abcde\r\nfghij");
        grid.feed(b"\x1b[1;2H\x1b[0J");
        assert_eq!(cells_text(&grid.visible_rows()[0]), "a     ");
        assert_eq!(cells_text(&grid.visible_rows()[1]), "      ");
    }

    #[test]
    fn ed_mode_1_erases_from_start_to_cursor() {
        let mut grid = TerminalGrid::new(2, 6);
        grid.feed(b"abcde\r\nfghij");
        grid.feed(b"\x1b[2;2H\x1b[1J");
        assert_eq!(cells_text(&grid.visible_rows()[0]), "      ");
        assert_eq!(cells_text(&grid.visible_rows()[1]), "  hij ");
    }

    #[test]
    fn ed_mode_2_erases_whole_viewport_but_not_scrollback() {
        let mut grid = TerminalGrid::new(2, 6);
        grid.feed(b"one\r\ntwo\r\nabcde");
        assert_eq!(grid.scrollback_rows().len(), 1);
        grid.feed(b"\x1b[2J");
        assert_eq!(cells_text(&grid.visible_rows()[0]), "      ");
        assert_eq!(cells_text(&grid.visible_rows()[1]), "      ");
        assert_eq!(grid.scrollback_rows().len(), 1);
    }

    #[test]
    fn el_mode_0_1_2() {
        let mut grid = TerminalGrid::new(1, 6);
        grid.feed(b"abcde\x1b[1;3H\x1b[0K");
        assert_eq!(cells_text(&grid.visible_rows()[0]), "ab    ");

        let mut grid2 = TerminalGrid::new(1, 6);
        grid2.feed(b"abcde\x1b[1;3H\x1b[1K");
        assert_eq!(cells_text(&grid2.visible_rows()[0]), "   de ");

        let mut grid3 = TerminalGrid::new(1, 6);
        grid3.feed(b"abcde\x1b[2K");
        assert_eq!(cells_text(&grid3.visible_rows()[0]), "      ");
    }

    #[test]
    fn unrecognized_csi_final_byte_is_discarded_no_effect() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b[6nz");
        assert_eq!(grid.visible_rows()[0][0].ch, 'z');
        assert_eq!(grid.cursor(), (0, 1));
    }

    #[test]
    fn private_mode_params_are_recognized_and_discarded() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b[?1049hz");
        assert_eq!(grid.visible_rows()[0][0].ch, 'z');
    }

    #[test]
    fn osc_sequence_terminated_by_bel_is_swallowed() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b]0;window title\x07z");
        assert_eq!(grid.visible_rows()[0][0].ch, 'z');
    }

    #[test]
    fn osc_sequence_terminated_by_st_is_swallowed() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b]0;window title\x1b\\z");
        assert_eq!(grid.visible_rows()[0][0].ch, 'z');
    }

    #[test]
    fn other_escape_sequence_consumes_one_byte_and_resumes() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b=z");
        assert_eq!(grid.visible_rows()[0][0].ch, 'z');
    }

    #[test]
    fn multi_byte_utf8_character_decodes_correctly() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed("héllo".as_bytes());
        assert_eq!(cells_text(&grid.visible_rows()[0][0..5]), "héllo");
    }

    #[test]
    fn multi_byte_utf8_character_split_across_two_feed_calls() {
        let bytes = "é".as_bytes();
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(&bytes[0..1]);
        grid.feed(&bytes[1..2]);
        assert_eq!(grid.visible_rows()[0][0].ch, 'é');
        assert_eq!(grid.cursor(), (0, 1));
    }

    #[test]
    fn invalid_utf8_byte_becomes_replacement_character_not_a_panic() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(&[0xff, b'z']);
        assert_eq!(grid.visible_rows()[0][0].ch, char::REPLACEMENT_CHARACTER);
        assert_eq!(grid.visible_rows()[0][1].ch, 'z');
    }

    #[test]
    fn resize_shrink_is_bottom_anchored_and_does_not_touch_scrollback() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"one\r\ntwo\r\n");
        grid.resize(2, 10);
        assert_eq!(grid.rows(), 2);
        assert!(grid.scrollback_rows().is_empty());
        assert_eq!(cells_text(&grid.visible_rows()[0][0..3]), "two");
        assert_eq!(cells_text(&grid.visible_rows()[1][0..3]), "   ");

        grid.resize(3, 10);
        assert_eq!(cells_text(&grid.visible_rows()[0][0..3]), "   ");
        assert_eq!(cells_text(&grid.visible_rows()[1][0..3]), "two");
    }

    #[test]
    fn resize_grow_columns_pads_not_truncates() {
        let mut grid = TerminalGrid::new(2, 3);
        grid.feed(b"ab");
        grid.resize(2, 6);
        assert_eq!(cells_text(&grid.visible_rows()[0]), "ab    ");
    }

    #[test]
    fn resize_shrink_columns_truncates_on_the_right() {
        let mut grid = TerminalGrid::new(2, 5);
        grid.feed(b"abcde");
        grid.resize(2, 3);
        assert_eq!(cells_text(&grid.visible_rows()[0]), "abc");
    }

    #[test]
    fn new_clamps_pathologically_large_dimensions() {
        let grid = TerminalGrid::new(u16::MAX as usize, u16::MAX as usize);
        assert_eq!(grid.rows(), MAX_GRID_DIMENSION);
        assert_eq!(grid.cols(), MAX_GRID_DIMENSION);
    }

    #[test]
    fn resize_clamps_pathologically_large_dimensions() {
        let mut grid = TerminalGrid::new(3, 3);
        grid.resize(u16::MAX as usize, u16::MAX as usize);
        assert_eq!(grid.rows(), MAX_GRID_DIMENSION);
        assert_eq!(grid.cols(), MAX_GRID_DIMENSION);
    }

    #[test]
    fn plain_text_trims_trailing_spaces_and_joins_with_newline() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.feed(b"hi\r\nbye");
        assert_eq!(grid.plain_text(), "hi\nbye");
    }

    #[test]
    fn plain_text_includes_scrollback_before_viewport() {
        let mut grid = TerminalGrid::new(1, 5);
        grid.feed(b"one\r\ntwo");
        assert_eq!(grid.plain_text(), "one\ntwo");
    }

    #[test]
    fn oversized_csi_param_list_does_not_panic_or_hang() {
        let mut grid = TerminalGrid::new(3, 10);
        let mut seq = b"\x1b[".to_vec();
        for _ in 0..10_000 {
            seq.extend_from_slice(b"1;");
        }
        seq.push(b'm');
        seq.push(b'z');
        grid.feed(&seq);
        assert_eq!(grid.visible_rows()[0][0].ch, 'z');
    }

    #[test]
    fn ridiculously_long_param_digits_do_not_overflow_panic() {
        let mut grid = TerminalGrid::new(3, 10);
        let mut seq = b"\x1b[".to_vec();
        seq.extend_from_slice(&[b'9'; 100]);
        seq.push(b'A');
        seq.push(b'z');
        grid.feed(&seq);
        assert_eq!(grid.visible_rows()[0][0].ch, 'z');
    }

    /// Adversarial live fuzz, not just the specific crafted sequences
    /// above -- a malicious/buggy `claude` process's PTY output is
    /// untrusted-ish input this parser must never panic or hang on
    /// (`docs/features/tui-claude-panel.md` §4.1). A small
    /// xorshift-style PRNG (no new dependency) drives thousands of
    /// random-byte feeds, including plenty of `0x1b` bytes to actually
    /// exercise the CSI/OSC state machine rather than mostly hitting
    /// `Ground`, across a range of tiny grid sizes (edge-heavy: cursor
    /// clamping bugs show up fastest on a 1x1 grid).
    #[test]
    fn random_byte_stream_never_panics_or_hangs() {
        let mut state: u64 = 0x2545F4914F6CDD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for (rows, cols) in [(1, 1), (2, 3), (5, 5), (24, 80)] {
            let mut grid = TerminalGrid::new(rows, cols);
            for _ in 0..20_000 {
                let r = next();
                // ESC (0x1b) is heavily overrepresented vs. a uniform
                // byte distribution -- otherwise almost every fed byte
                // lands in `Ground` and the CSI/OSC parser states barely
                // get exercised at all.
                let byte = if r % 4 == 0 { 0x1b } else { (r >> 8) as u8 };
                grid.feed(&[byte]);
            }
            // Reaching here at all (no panic, no infinite loop) is the
            // actual assertion; this just proves the grid is still in a
            // usable state afterward.
            assert_eq!(grid.visible_rows().len(), rows);
        }
    }

    // -- key_event_to_bytes --------------------------------------------

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn plain_char_event_forwards_utf8_bytes() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Char('g'), KeyModifiers::NONE)),
            Some(b"g".to_vec())
        );
    }

    #[test]
    fn enter_backspace_tab_escape_map_to_their_bytes() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(vec![0x7f])
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Tab, KeyModifiers::NONE)),
            Some(vec![b'\t'])
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn arrow_keys_map_to_csi_sequences() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Down, KeyModifiers::NONE)),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Right, KeyModifiers::NONE)),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Left, KeyModifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn ctrl_c_maps_to_interrupt_byte() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
    }

    #[test]
    fn ctrl_d_and_ctrl_l_use_the_general_formula_not_a_special_case() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(vec![0x04])
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Some(vec![0x0c])
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(vec![0x01])
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            Some(vec![0x1a])
        );
    }

    #[test]
    fn ctrl_shift_letter_maps_the_same_as_ctrl_letter() {
        // Indistinguishable in a real terminal -- see doc §1.1/§3.4.
        assert_eq!(
            key_event_to_bytes(key(
                KeyCode::Char('A'),
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT)
            )),
            Some(vec![0x01])
        );
    }

    #[test]
    fn unclaimed_key_returns_none() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::F(5), KeyModifiers::NONE)),
            None
        );
    }

    // -- ClaudeTerminalTab / ClaudeTerminalPanel ------------------------

    #[test]
    fn open_tab_with_a_nonexistent_directory_creates_an_exited_tab() {
        let mut panel = ClaudeTerminalPanel::default();
        panel.open_tab(PathBuf::from("/does/not/exist/at/all"), 24, 80);
        assert_eq!(panel.tabs().len(), 1);
        assert_eq!(panel.active, Some(0));
        let tab = panel.active_tab().unwrap();
        assert!(tab.exited);
        assert!(tab.grid().plain_text().contains("is not a directory"));
    }

    #[test]
    fn write_to_an_exited_tab_is_a_harmless_noop() {
        let mut panel = ClaudeTerminalPanel::default();
        panel.open_tab(PathBuf::from("/does/not/exist/at/all"), 24, 80);
        let tab = panel.active_tab_mut().unwrap();
        assert!(tab.write(b"hello").is_ok());
    }

    #[test]
    fn close_tab_out_of_bounds_index_is_a_noop() {
        let mut panel = ClaudeTerminalPanel::default();
        panel.close_tab(0);
        assert_eq!(panel.tabs().len(), 0);
    }

    #[test]
    fn close_tab_clears_active_when_it_was_the_last_tab() {
        let mut panel = ClaudeTerminalPanel::default();
        panel.open_tab(PathBuf::from("/does/not/exist/1"), 24, 80);
        panel.close_tab(0);
        assert_eq!(panel.active, None);
        assert!(panel.tabs().is_empty());
    }

    #[test]
    fn close_tab_before_active_shifts_active_index_down() {
        let mut panel = ClaudeTerminalPanel::default();
        panel.open_tab(PathBuf::from("/does/not/exist/1"), 24, 80);
        panel.open_tab(PathBuf::from("/does/not/exist/2"), 24, 80);
        panel.active = Some(1);
        panel.close_tab(0);
        assert_eq!(panel.active, Some(0));
        assert_eq!(panel.tabs().len(), 1);
    }

    #[test]
    fn close_tab_that_was_active_and_last_selects_new_last() {
        let mut panel = ClaudeTerminalPanel::default();
        panel.open_tab(PathBuf::from("/does/not/exist/1"), 24, 80);
        panel.open_tab(PathBuf::from("/does/not/exist/2"), 24, 80);
        assert_eq!(panel.active, Some(1));
        panel.close_tab(1);
        assert_eq!(panel.active, Some(0));
    }

    #[test]
    fn title_falls_back_to_claude_when_cwd_has_no_file_name() {
        let mut panel = ClaudeTerminalPanel::default();
        panel.open_tab(PathBuf::from("/"), 24, 80);
        assert_eq!(panel.active_tab().unwrap().title, "claude");
    }

    #[test]
    fn poll_with_no_tabs_returns_false() {
        let mut panel = ClaudeTerminalPanel::default();
        assert!(!panel.poll());
    }

    // -- PtySession: real-subprocess plumbing (unix-only, spawns `cat`) --

    #[cfg(unix)]
    #[test]
    fn pty_session_write_read_round_trip_and_clean_shutdown() {
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir();
        let mut session = spawn_cat(&dir);

        session.write(b"hello\r").unwrap();

        let start = Instant::now();
        let mut collected = Vec::new();
        loop {
            for event in session.poll() {
                if let PtyEvent::Data(bytes) = event {
                    collected.extend_from_slice(&bytes);
                }
            }
            if String::from_utf8_lossy(&collected).contains("hello") {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "cat never echoed input back"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        session.resize(30, 100); // must not panic

        drop(session); // Drop kills the child
    }

    #[cfg(unix)]
    fn spawn_cat(dir: &Path) -> PtySession {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new("cat");
        cmd.cwd(dir);
        let child = pair.slave.spawn_command(cmd).unwrap();
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(PtyEvent::Exited);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(PtyEvent::Data(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(PtyEvent::Exited);
                        break;
                    }
                }
            }
        });
        PtySession {
            writer,
            rx,
            master: pair.master,
            child,
        }
    }

    #[cfg(unix)]
    #[test]
    fn pty_session_spawn_rejects_nonexistent_directory() {
        match PtySession::spawn(Path::new("/does/not/exist/at/all"), 24, 80) {
            Err(err) => assert!(err.contains("is not a directory")),
            Ok(_) => panic!("expected an error"),
        }
    }

    /// Live verification of `docs/features/tui-claude-panel.md` §4.2's
    /// resource-cleanup invariant -- not just "the test doesn't hang" (as
    /// `pty_session_write_read_round_trip_and_clean_shutdown` above
    /// already checks), but that the real OS child process is actually
    /// gone after `Drop`, checked by asking the OS about that exact PID
    /// (`kill -0`), not inferred. Spawns `sleep 100` (long-lived, so a
    /// bug here would otherwise leave an orphaned process sleeping for
    /// ~100s rather than failing fast).
    #[cfg(unix)]
    #[test]
    fn drop_actually_kills_the_child_process() {
        let dir = std::env::temp_dir();
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("100");
        cmd.cwd(&dir);
        let child = pair.slave.spawn_command(cmd).unwrap();
        let pid = child.process_id().expect("child has a pid");
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(PtyEvent::Exited);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(PtyEvent::Data(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let session = PtySession {
            writer,
            rx,
            master: pair.master,
            child,
        };

        assert!(
            is_pid_alive(pid),
            "sleep should still be running before drop"
        );
        drop(session);
        assert!(!is_pid_alive(pid), "sleep should be dead after drop");
    }

    #[cfg(unix)]
    fn is_pid_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
