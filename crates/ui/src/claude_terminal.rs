//! Interactive `claude` CLI sessions in PTY-backed tabs
//! (`docs/features/claude-terminal.md`). Distinct from `claude_panel.rs`'s
//! one-shot request/response wrapper, which this doesn't touch.

use eframe::egui;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

pub const TERMINAL_SCROLLBACK_LIMIT: usize = 2000;

/// `TerminalGrid::new`/`resize` clamp `rows`/`cols` to this ceiling. Real
/// terminals never get remotely close (a realistic-but-generous 150x500
/// grid is ~600KB); `claude_terminal_char_grid`'s own `u16::MAX` clamp is
/// only a cast-overflow guard, not a sane operational bound, and at that
/// ceiling `rows * cols * size_of::<Cell>()` is tens of gigabytes (hacker
/// finding 5, `docs/security-findings/rust-ui-dev-claude-terminal-
/// 2026-08-25.md` -- not practically reachable through real window/font
/// metrics, but free to close off).
const MAX_GRID_DIMENSION: usize = 4096;

/// Real xterm sequences top out well under this per CSI sequence; capping
/// here bounds `Vec<Option<u16>>` growth against an adversarial stream of
/// `;`-separated params in one unterminated sequence (doc §4.1: the parser
/// must never panic or hang on malformed input, and unbounded allocation
/// from a single escape sequence is the memory-DoS shape of that).
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
    /// Fixed xterm-standard RGB values (`claude-terminal.md` §3.3) --
    /// independent of the IDE theme. `None` for `Default`: callers map
    /// that to a theme token instead (`fg_primary`/`bg_base`).
    pub fn xterm_rgb(self) -> Option<egui::Color32> {
        use AnsiColor::*;
        Some(match self {
            Default => return None,
            Black => egui::Color32::from_rgb(0x00, 0x00, 0x00),
            Red => egui::Color32::from_rgb(0xCD, 0x00, 0x00),
            Green => egui::Color32::from_rgb(0x00, 0xCD, 0x00),
            Yellow => egui::Color32::from_rgb(0xCD, 0xCD, 0x00),
            Blue => egui::Color32::from_rgb(0x00, 0x00, 0xEE),
            Magenta => egui::Color32::from_rgb(0xCD, 0x00, 0xCD),
            Cyan => egui::Color32::from_rgb(0x00, 0xCD, 0xCD),
            White => egui::Color32::from_rgb(0xE5, 0xE5, 0xE5),
            BrightBlack => egui::Color32::from_rgb(0x7F, 0x7F, 0x7F),
            BrightRed => egui::Color32::from_rgb(0xFF, 0x00, 0x00),
            BrightGreen => egui::Color32::from_rgb(0x00, 0xFF, 0x00),
            BrightYellow => egui::Color32::from_rgb(0xFF, 0xFF, 0x00),
            BrightBlue => egui::Color32::from_rgb(0x5C, 0x5C, 0xFF),
            BrightMagenta => egui::Color32::from_rgb(0xFF, 0x00, 0xFF),
            BrightCyan => egui::Color32::from_rgb(0x00, 0xFF, 0xFF),
            BrightWhite => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
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

/// Hand-rolled, bounded ANSI/CSI subset (`claude-terminal.md` §3.2) -- not
/// a full xterm emulator. Pure, no I/O: `feed()` is the only way in.
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

    /// Bottom-anchored for rows, left-aligned for columns -- see
    /// `claude-terminal.md` §4.3 for the rationale (shrinking drops the
    /// oldest visible rows first, matching the direction scrollback
    /// already ages in, so the cursor's row always survives).
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

    pub fn scrollback_rows(&self) -> &VecDeque<Vec<Cell>> {
        &self.scrollback
    }

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

/// One `claude` child process behind a PTY (`claude-terminal.md` §2.2).
pub struct PtySession {
    writer: Box<dyn Write + Send>,
    rx: Receiver<PtyEvent>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtySession {
    /// Spawns the fixed literal `"claude"` (no shell, no args) via
    /// `portable_pty::native_pty_system()`. `cwd` is re-checked with
    /// `is_dir()` here rather than trusted from the caller (doc §4.1: the
    /// picker can only return a real directory, but it could be deleted/
    /// unmounted between the picker returning and this call). Environment
    /// is inherited unmodified -- `CommandBuilder::new` already does this
    /// by default (`get_base_env()` snapshots `std::env::vars_os()`), no
    /// extra `.env(...)` calls needed or wanted.
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
    /// Stable across the tab's lifetime, independent of its `Vec` index
    /// -- the basis for this tab's `egui::Id` (focus-tracking, §3.4).
    pub id: u64,
    pub cwd: PathBuf,
    pub title: String,
    pub exited: bool,
    grid: TerminalGrid,
    /// `None` when `PtySession::spawn` failed -- the tab still exists
    /// (`exited: true`, the spawn error fed into `grid` as text) rather
    /// than being silently dropped (doc §3.1).
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
    next_id: u64,
}

impl ClaudeTerminalPanel {
    /// Always creates and selects a tab, even if `claude` couldn't be
    /// spawned -- a failed spawn is shown inline (`exited: true`, the
    /// error text fed into the tab's grid), never silently dropped (doc
    /// §3.1). This is why `open_tab` has no `Result` return: nothing
    /// about opening a tab is ever an error the caller has to handle.
    pub fn open_tab(&mut self, cwd: PathBuf, rows: u16, cols: u16) {
        let id = self.next_id;
        self.next_id += 1;
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
            id,
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

fn ctrl_letter_byte(key: egui::Key) -> Option<u8> {
    use egui::Key::*;
    let letter = match key {
        A => b'A',
        B => b'B',
        C => b'C',
        D => b'D',
        E => b'E',
        F => b'F',
        G => b'G',
        H => b'H',
        I => b'I',
        J => b'J',
        K => b'K',
        L => b'L',
        M => b'M',
        N => b'N',
        O => b'O',
        P => b'P',
        Q => b'Q',
        R => b'R',
        S => b'S',
        T => b'T',
        U => b'U',
        V => b'V',
        W => b'W',
        X => b'X',
        Y => b'Y',
        Z => b'Z',
        _ => return None,
    };
    Some(letter - b'A' + 1)
}

/// Translates one input event into the bytes a real terminal would send
/// for it, or `None` for an event the terminal doesn't forward
/// (`claude-terminal.md` §2.4/§3.4). Pure and egui-context-free, same
/// shape as `editor/input.rs`'s `intent_for`.
pub fn key_event_to_bytes(event: &egui::Event) -> Option<Vec<u8>> {
    match event {
        egui::Event::Text(text) if !text.is_empty() => Some(text.as_bytes().to_vec()),
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            if modifiers.ctrl {
                if let Some(byte) = ctrl_letter_byte(*key) {
                    return Some(vec![byte]);
                }
            }
            match key {
                egui::Key::Enter => Some(vec![b'\r']),
                egui::Key::Backspace => Some(vec![0x7f]),
                egui::Key::Tab => Some(vec![b'\t']),
                egui::Key::Escape => Some(vec![0x1b]),
                egui::Key::ArrowUp => Some(b"\x1b[A".to_vec()),
                egui::Key::ArrowDown => Some(b"\x1b[B".to_vec()),
                egui::Key::ArrowRight => Some(b"\x1b[C".to_vec()),
                egui::Key::ArrowLeft => Some(b"\x1b[D".to_vec()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// A stable per-tab `egui::Id` for focus-tracking (doc §2.3/§3.4), keyed
/// on the tab's `id` field rather than its `Vec` index so focus doesn't
/// jump to a different tab when an earlier one closes.
pub fn terminal_tab_egui_id(tab_id: u64) -> egui::Id {
    egui::Id::new(("claude_terminal_tab", tab_id))
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
        grid.feed(b"\x1b[99A"); // up past row 0
        assert_eq!(grid.cursor(), (0, 0));
        grid.feed(b"\x1b[99C"); // forward past last col
        assert_eq!(grid.cursor(), (0, 2));
        grid.feed(b"\x1b[99B"); // down past last row
        assert_eq!(grid.cursor(), (2, 2));
        grid.feed(b"\x1b[99D"); // back past col 0
        assert_eq!(grid.cursor(), (2, 0));
    }

    #[test]
    fn ed_mode_0_erases_from_cursor_to_end_of_display() {
        // 6 cols, 5-char rows: writing the 5th char never lands on the
        // last column, so no wrap-triggered scroll muddies the fixture.
        let mut grid = TerminalGrid::new(2, 6);
        grid.feed(b"abcde\r\nfghij");
        grid.feed(b"\x1b[1;2H\x1b[0J"); // row1 col2 (1-indexed) = row0 col1
        assert_eq!(cells_text(&grid.visible_rows()[0]), "a     ");
        assert_eq!(cells_text(&grid.visible_rows()[1]), "      ");
    }

    #[test]
    fn ed_mode_1_erases_from_start_to_cursor() {
        let mut grid = TerminalGrid::new(2, 6);
        grid.feed(b"abcde\r\nfghij");
        grid.feed(b"\x1b[2;2H\x1b[1J"); // row2 col2 (1-indexed) = row1 col1
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
        assert_eq!(grid.scrollback_rows().len(), 1); // untouched by clear
    }

    #[test]
    fn el_mode_0_1_2() {
        // 6 cols so the 5-char "abcde" doesn't land on (and wrap off of)
        // the last column.
        let mut grid = TerminalGrid::new(1, 6);
        grid.feed(b"abcde\x1b[1;3H\x1b[0K"); // CUP to col 2 (0-indexed)
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
        grid.feed(b"\x1b[6nz"); // DSR, not in our table
        assert_eq!(grid.visible_rows()[0][0].ch, 'z');
        assert_eq!(grid.cursor(), (0, 1));
    }

    #[test]
    fn private_mode_params_are_recognized_and_discarded() {
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"\x1b[?1049hz"); // alt-screen enable, out of scope
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
        grid.feed(b"\x1b=z"); // keypad-application-mode, out of scope
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
        let bytes = "é".as_bytes(); // 2 bytes
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
        // Matches claude-terminal.md §5.4 exactly.
        let mut grid = TerminalGrid::new(3, 10);
        grid.feed(b"one\r\ntwo\r\n");
        grid.resize(2, 10);
        assert_eq!(grid.rows(), 2);
        assert!(grid.scrollback_rows().is_empty());
        assert_eq!(cells_text(&grid.visible_rows()[0][0..3]), "two");
        assert_eq!(cells_text(&grid.visible_rows()[1][0..3]), "   ");

        grid.resize(3, 10);
        assert_eq!(cells_text(&grid.visible_rows()[0][0..3]), "   "); // "one" is gone
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
        // hacker finding 5: an unclamped u16::MAX x u16::MAX grid would
        // try to allocate tens of gigabytes.
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
        grid.feed(&seq); // saturating arithmetic: must not panic
        assert_eq!(grid.visible_rows()[0][0].ch, 'z');
    }

    // -- key_event_to_bytes --------------------------------------------

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn plain_text_event_forwards_utf8_bytes() {
        assert_eq!(
            key_event_to_bytes(&egui::Event::Text("g".into())),
            Some(b"g".to_vec())
        );
    }

    #[test]
    fn empty_text_event_forwards_nothing() {
        assert_eq!(key_event_to_bytes(&egui::Event::Text(String::new())), None);
    }

    #[test]
    fn enter_backspace_tab_escape_map_to_their_bytes() {
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::Enter, egui::Modifiers::NONE)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::Backspace, egui::Modifiers::NONE)),
            Some(vec![0x7f])
        );
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::Tab, egui::Modifiers::NONE)),
            Some(vec![b'\t'])
        );
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::Escape, egui::Modifiers::NONE)),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn arrow_keys_map_to_csi_sequences() {
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::ArrowUp, egui::Modifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::ArrowDown, egui::Modifiers::NONE)),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::ArrowRight, egui::Modifiers::NONE)),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::ArrowLeft, egui::Modifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn ctrl_c_maps_to_interrupt_byte() {
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::C, egui::Modifiers::CTRL)),
            Some(vec![0x03])
        );
    }

    #[test]
    fn ctrl_d_and_ctrl_l_use_the_general_formula_not_a_special_case() {
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::D, egui::Modifiers::CTRL)),
            Some(vec![0x04])
        );
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::L, egui::Modifiers::CTRL)),
            Some(vec![0x0c])
        );
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::A, egui::Modifiers::CTRL)),
            Some(vec![0x01])
        );
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::Z, egui::Modifiers::CTRL)),
            Some(vec![0x1a])
        );
    }

    #[test]
    fn unmodified_letter_key_event_is_not_forwarded_text_event_handles_it() {
        // Otherwise a plain keypress would be sent twice: once via `Text`,
        // once via `Key`.
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::G, egui::Modifiers::NONE)),
            None
        );
    }

    #[test]
    fn key_release_events_are_not_forwarded() {
        let event = egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        assert_eq!(key_event_to_bytes(&event), None);
    }

    #[test]
    fn unclaimed_key_returns_none() {
        assert_eq!(
            key_event_to_bytes(&key(egui::Key::F5, egui::Modifiers::NONE)),
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

    #[test]
    fn terminal_tab_egui_id_is_stable_for_the_same_id_and_differs_across_ids() {
        assert_eq!(terminal_tab_egui_id(1), terminal_tab_egui_id(1));
        assert_ne!(terminal_tab_egui_id(1), terminal_tab_egui_id(2));
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

        drop(session); // Drop kills the child -- nothing left to assert on
                       // directly, but this must not hang or panic.
    }

    #[cfg(unix)]
    fn spawn_cat(dir: &Path) -> PtySession {
        // `cat` echoes stdin back to stdout -- a small, always-present
        // binary that proves the PTY read/write plumbing works end to
        // end without depending on the real `claude` CLI being
        // installed (mirrors `claude_panel.rs`'s existing test
        // precedent).
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
}
