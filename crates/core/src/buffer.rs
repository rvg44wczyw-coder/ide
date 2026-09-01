//! `Buffer` is the file-backed wrapper around `crate::text::TextBuffer`:
//! path, dirty flag and save/load, with every text operation delegated.
//! `insert`/`delete` are O(n) in the buffer length (the tail must shift on
//! every edit); `text()`/`path()`/`is_dirty()` are O(1). That tradeoff, and
//! the reason the storage is a `String` rather than a rope, live in
//! `docs/features/editor-engine.md` §4.5.
//!
//! Neither `insert` nor `delete` coalesces: one call is one undo step, the
//! same as before this crate grew a transaction model. Grouped typing is
//! `TextBuffer::type_text`, which the editor widget calls per keystroke.

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::editorconfig::Charset;
use crate::syntax::SyntaxRules;
use crate::text::{TextBuffer, Transaction};

#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("buffer has no associated path; use save_as")]
    NoPath,
    #[error("file is {size} bytes, exceeding the {limit}-byte open limit")]
    TooLarge { size: u64, limit: u64 },
}

/// v1 safety net against loading an unexpectedly huge file whole into
/// memory when the user clicks it in the tree view (see
/// `docs/security-findings/editor-shell-project-scan-2026-08-16.md`,
/// finding 2). 50 MiB comfortably covers ordinary source files and large
/// generated/log files while still bounding worst-case memory use.
const MAX_OPEN_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug)]
pub struct Buffer {
    path: Option<PathBuf>,
    text: TextBuffer,
    dirty: bool,
}

impl Buffer {
    /// A new, empty, unsaved buffer ("Untitled").
    pub fn untitled() -> Self {
        Self {
            path: None,
            text: TextBuffer::new(String::new(), None),
            dirty: false,
        }
    }

    /// Reads `path` into a new buffer. Buffer starts clean (not dirty).
    /// Errors with `BufferError::TooLarge` — without reading the file's
    /// contents — if it exceeds `MAX_OPEN_BYTES`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BufferError> {
        Self::open_with_limit(path, MAX_OPEN_BYTES)
    }

    fn open_with_limit(path: impl AsRef<Path>, limit: u64) -> Result<Self, BufferError> {
        let path = path.as_ref().to_path_buf();
        let size = fs::metadata(&path)?.len();
        if size > limit {
            return Err(BufferError::TooLarge { size, limit });
        }
        let text = fs::read_to_string(&path)?;
        Ok(Self {
            path: Some(path),
            text: TextBuffer::new(text, None),
            dirty: false,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn text(&self) -> &str {
        self.text.text()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The text model underneath, for callers that need lines, selections or
    /// tokens rather than a flat string.
    pub fn text_buffer(&self) -> &TextBuffer {
        &self.text
    }

    /// Does **not** mark the buffer dirty. The editor widget
    /// (`docs/features/code-editor-widget.md` §2.0) calls this every frame
    /// just to read and hit-test, so dirtying on access would light the
    /// modified indicator on every file the moment it is opened. The widget
    /// calls `mark_dirty` when an edit actually lands instead.
    pub fn text_buffer_mut(&mut self) -> &mut TextBuffer {
        &mut self.text
    }

    /// Records that the content no longer matches what is on disk -- what
    /// `text_buffer_mut` used to assume on the caller's behalf.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Installs highlighting rules on the underlying `TextBuffer`,
    /// retokenizing from scratch. Deliberately leaves the dirty flag alone:
    /// choosing a language is not an edit.
    pub fn set_syntax(&mut self, syntax: Option<&'static SyntaxRules>) {
        self.text.set_syntax(syntax);
    }

    /// Applies `transaction` as one undoable step, marking the buffer dirty.
    pub fn apply(&mut self, transaction: Transaction) {
        self.text.apply(transaction);
        self.dirty = true;
    }

    /// Inserts `text` at byte offset `offset`. `offset` is clamped to the
    /// nearest valid UTF-8 char boundary if it isn't already on one.
    /// Marks the buffer dirty and pushes an undo entry. No-op if `text` is
    /// empty.
    pub fn insert(&mut self, offset: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        self.text.apply(Transaction::insert(offset, text));
        self.dirty = true;
    }

    /// Deletes the byte range `range`. Both ends are clamped to the
    /// nearest valid UTF-8 char boundary. No-op if `range` is empty after
    /// clamping. Marks the buffer dirty and pushes an undo entry.
    pub fn delete(&mut self, range: Range<usize>) {
        let before = self.text.len();
        self.text.apply(Transaction::delete(range));
        if self.text.len() != before {
            self.dirty = true;
        }
    }

    /// Reverts the most recent edit. Returns `false` if the undo stack is
    /// empty (no-op). Marks the buffer dirty, since the content may no
    /// longer match what's on disk.
    pub fn undo(&mut self) -> bool {
        if !self.text.undo() {
            return false;
        }
        self.dirty = true;
        true
    }

    /// Re-applies the most recently undone edit. Returns `false` if the
    /// redo stack is empty (no-op). Any new edit clears the redo stack.
    pub fn redo(&mut self) -> bool {
        if !self.text.redo() {
            return false;
        }
        self.dirty = true;
        true
    }

    /// Writes `text()` to `path()`. Errors with `BufferError::NoPath` if
    /// this buffer has never been saved (use `save_as` first).
    pub fn save(&mut self) -> Result<(), BufferError> {
        self.save_with(None)
    }

    /// `save`, but writing under `charset` rather than plain UTF-8: a
    /// `Utf8Bom` prepends the BOM, `Utf8` and `None` write the text as-is.
    /// The BOM lives here rather than in the buffer's text because it is a
    /// property of the file -- a BOM inside `TextBuffer` would show as a
    /// character in the editor (`line-commands-and-editorconfig.md` §3.6).
    /// The charsets the buffer cannot represent never reach this method:
    /// `editorconfig::save_charset` returns `None` for them.
    pub fn save_with(&mut self, charset: Option<Charset>) -> Result<(), BufferError> {
        let path = self.path.clone().ok_or(BufferError::NoPath)?;
        let mut bytes = Vec::new();
        if charset == Some(Charset::Utf8Bom) {
            bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        bytes.extend_from_slice(self.text.text().as_bytes());
        fs::write(&path, bytes)?;
        self.dirty = false;
        Ok(())
    }

    /// Sets the buffer's path and writes `text()` to it.
    pub fn save_as(&mut self, path: impl AsRef<Path>) -> Result<(), BufferError> {
        let path = path.as_ref().to_path_buf();
        fs::write(&path, self.text.text())?;
        self.path = Some(path);
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untitled_starts_clean_and_empty() {
        let buf = Buffer::untitled();
        assert_eq!(buf.text(), "");
        assert!(!buf.is_dirty());
        assert_eq!(buf.path(), None);
    }

    #[test]
    fn insert_marks_dirty_and_updates_text() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "hello");
        assert_eq!(buf.text(), "hello");
        assert!(buf.is_dirty());
    }

    #[test]
    fn apply_runs_a_transaction_as_one_undo_step() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "aa bb");
        buf.apply(
            Transaction::new(vec![
                crate::text::Change::new(0..2, "X"),
                crate::text::Change::new(3..5, "Y"),
            ])
            .expect("non-overlapping"),
        );
        assert_eq!(buf.text(), "X Y");
        assert!(buf.is_dirty());
        assert!(buf.undo());
        assert_eq!(buf.text(), "aa bb");
    }

    #[test]
    fn text_buffer_exposes_lines_and_selections() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "one\ntwo");
        assert_eq!(buf.text_buffer().lines().line_count(), 2);
        assert_eq!(buf.text_buffer().line_text(1), Some("two"));

        buf.save_as(tempfile::tempdir().unwrap().path().join("f.txt"))
            .unwrap();
        assert!(!buf.is_dirty());
        buf.text_buffer_mut().type_text("!");
        assert_eq!(buf.text(), "one\ntwo!");
        assert!(
            !buf.is_dirty(),
            "reading or editing through the model does not dirty"
        );
        buf.mark_dirty();
        assert!(buf.is_dirty());
    }

    #[test]
    fn set_syntax_highlights_without_dirtying() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "let a = 1;");
        buf.save_as(tempfile::tempdir().unwrap().path().join("a.rs"))
            .unwrap();
        assert!(buf.text_buffer().tokens().is_empty());

        buf.set_syntax(Some(&crate::syntax::RUST));
        assert!(!buf.text_buffer().tokens().is_empty());
        assert!(!buf.is_dirty());

        buf.set_syntax(None);
        assert!(buf.text_buffer().tokens().is_empty());
        assert!(!buf.is_dirty());
    }

    #[test]
    fn a_typed_run_undoes_as_one_step_unlike_repeated_inserts() {
        let mut buf = Buffer::untitled();
        for ch in ["a", "b", "c"] {
            buf.text_buffer_mut().type_text(ch);
        }
        assert_eq!(buf.text(), "abc");
        assert!(buf.undo());
        assert_eq!(buf.text(), "");

        let mut buf = Buffer::untitled();
        for ch in ["a", "b", "c"] {
            buf.insert(buf.text().len(), ch);
        }
        assert!(buf.undo());
        assert_eq!(buf.text(), "ab");
    }

    #[test]
    fn insert_empty_text_is_noop() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "");
        assert_eq!(buf.text(), "");
        assert!(!buf.is_dirty());
    }

    #[test]
    fn insert_past_end_clamps_to_end() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "abc");
        buf.insert(9999, "def");
        assert_eq!(buf.text(), "abcdef");
    }

    #[test]
    fn insert_and_delete_clamp_to_utf8_char_boundary() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "a\u{1F600}b"); // 'a' + 4-byte emoji + 'b'
        let mid_of_emoji = 2; // inside the 4-byte emoji, not a boundary
        buf.insert(mid_of_emoji, "X");
        // clamps down to offset 1 (right after 'a', a valid boundary)
        assert_eq!(buf.text(), "aX\u{1F600}b");

        let mut buf2 = Buffer::untitled();
        buf2.insert(0, "a\u{1F600}b");
        buf2.delete(2..3); // both ends inside the emoji, clamp to same boundary -> no-op
        assert_eq!(buf2.text(), "a\u{1F600}b");
    }

    #[test]
    fn delete_empty_range_is_noop() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "hello");
        buf.delete(2..2);
        assert_eq!(buf.text(), "hello");
    }

    #[test]
    fn delete_reversed_range_normalizes() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "hello");
        let reversed = Range { start: 4, end: 1 }; // reversed on purpose, bypasses the `a..b` literal lint
        buf.delete(reversed);
        assert_eq!(buf.text(), "ho");
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "hello");
        buf.insert(5, " world");
        assert_eq!(buf.text(), "hello world");

        assert!(buf.undo());
        assert_eq!(buf.text(), "hello");
        assert!(buf.undo());
        assert_eq!(buf.text(), "");
        assert!(!buf.undo()); // stack empty, no-op

        assert!(buf.redo());
        assert_eq!(buf.text(), "hello");
        assert!(buf.redo());
        assert_eq!(buf.text(), "hello world");
        assert!(!buf.redo()); // stack empty, no-op
    }

    #[test]
    fn new_edit_after_undo_clears_redo_stack() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "hello");
        buf.undo();
        buf.insert(0, "bye");
        assert!(!buf.redo()); // redo stack was cleared by the new edit
        assert_eq!(buf.text(), "bye");
    }

    #[test]
    fn open_reads_existing_file_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, "on disk").unwrap();

        let buf = Buffer::open(&path).unwrap();
        assert_eq!(buf.text(), "on disk");
        assert!(!buf.is_dirty());
        assert_eq!(buf.path(), Some(path.as_path()));
    }

    #[test]
    fn open_rejects_file_over_limit_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        fs::write(&path, "0123456789").unwrap(); // 10 bytes

        let err = Buffer::open_with_limit(&path, 5).unwrap_err();
        assert!(matches!(err, BufferError::TooLarge { size: 10, limit: 5 }));
    }

    #[test]
    fn open_accepts_file_at_or_under_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.txt");
        fs::write(&path, "12345").unwrap(); // exactly 5 bytes

        let buf = Buffer::open_with_limit(&path, 5).unwrap();
        assert_eq!(buf.text(), "12345");
    }

    #[test]
    fn open_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.txt");
        assert!(matches!(Buffer::open(&path), Err(BufferError::Io(_))));
    }

    #[test]
    fn save_without_path_errors() {
        let mut buf = Buffer::untitled();
        buf.insert(0, "hi");
        assert!(matches!(buf.save(), Err(BufferError::NoPath)));
    }

    #[test]
    fn save_writes_and_clears_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, "old").unwrap();

        let mut buf = Buffer::open(&path).unwrap();
        buf.insert(0, "new ");
        assert!(buf.is_dirty());
        buf.save().unwrap();
        assert!(!buf.is_dirty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new old");
    }

    #[test]
    fn save_as_sets_path_and_clears_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        let mut buf = Buffer::untitled();
        buf.insert(0, "content");
        buf.save_as(&path).unwrap();

        assert!(!buf.is_dirty());
        assert_eq!(buf.path(), Some(path.as_path()));
        assert_eq!(fs::read_to_string(&path).unwrap(), "content");
    }
}
