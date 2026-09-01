# T23 — Scratch files (`ide-tui`)

## 1. Purpose

`docs/roadmap.md`'s T23 row: "`ide-tui` today can only open an existing
file from the tree." There is no way to jot down a quick, disposable note
or snippet without first creating a real file inside some project.

Unlike most `T`-items, this is **not** a port of an already-shipped
`ide-ui` screen: `ide-ui`'s own G4 (`docs/features/scratch-files.md`) is
still `❌` in `docs/roadmap.md` — only a much smaller "untitled tabs"
mechanism exists there (`IdeApp::new_untitled_tab`, an in-memory buffer
with no path at all, backed by `ide_core::Buffer::untitled()`). This
phase is therefore a fresh v1 design from the roadmap's own G4
description ("Scratch-файлы с выбором языка, персист в служебном
каталоге") plus real JetBrains Scratch File behaviour, the same situation
`T17`/`T24` were in.

### 1.1 Why this reuses real files instead of `Buffer::untitled()`

`ide_core::Buffer::path()` already returns `Option<&Path>`, so an
in-memory, path-less buffer is available in principle. This phase
deliberately does **not** use it, for one reason with several
consequences: `ide-tui`'s own `OpenBuffer.path` field (`app.rs`) is a
non-optional `PathBuf`, load-bearing across a large amount of already-
shipped machinery that assumes every open tab has a real path —
`open_or_focus_tab`'s dedup-by-path check, the `T25` file watcher's
dispatch-by-path, `T17`'s Recent Files/Bookmarks (both keyed by path),
`T21`'s workspace-restore, and every `LspRequest::DidOpen`/`DidChange`
call (a language server needs a real `file://` URI). Making `path`
optional to support one narrow new feature would ripple a
already-invasive change through all of that for no benefit: the roadmap's
own G4 description already asks for **persisted, on-disk** scratch files
("персист в служебном каталоге"), not throwaway in-memory ones — so the
natural, minimal-ripple implementation is a real file on disk in a
dedicated per-user directory, opened through the exact same
`open_or_focus_tab` path every other file already goes through. Every
downstream feature listed above works for a scratch file automatically,
for free, with zero code changes to any of them.

### 1.2 Scope cuts

- **Language picker UI.** Cut. `syntax_for_path`'s existing extension-
  based detection already achieves "choose a language" the moment the
  user names the file `notes.md` vs. `scratch.rs` — no separate picker
  needed on top of a mechanism this crate already has for every other
  file.
- **MRU ordering in the scratch-file list.** Cut in favour of alphabetical
  (§2.2) — real JetBrains Scratch Files lists most-recently-used first,
  but wiring a second MRU tracker (beyond `T17`'s `nav_state.recent_files`,
  which is project-scoped and scratch files are deliberately not) is
  more machinery than a v1 needs. `T17`'s own Recent Files popup already
  lists a scratch file once it's been opened once, which covers the
  "get back to one I used a moment ago" case in practice.
  Project-tree integration. **Cut, deliberately, not an oversight.** Real
  JetBrains scratch files don't appear in the Project view either (they
  live under a separate "Scratches and Consoles" root) — this phase's
  scratch directory lives outside `project_root` entirely, so
  `Project::scan_tree()` never sees it and nothing needs to filter it out.
- **Delete/rename from the picker.** Cut — `reset`-style destructive
  actions from a popup aren't established anywhere else in this crate
  yet (Recent Files/Bookmarks/TODO are all read-only lists); a user who
  wants a scratch file gone can delete it from a real shell.

## 2. Interface

New module `crates/tui/src/scratch.rs`.

### 2.1 Directory and naming

```rust
/// `~/.config/ide-tui/scratch/` -- global, per-user, independent of
/// whatever project is currently open (matches real JetBrains Scratch
/// Files: reachable regardless of project). Same home-dir resolution
/// `state.rs`/`keymap.rs` already use.
pub fn scratch_dir() -> Option<PathBuf>;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScratchNameError {
    #[error("name cannot be empty")]
    Empty,
    #[error("name cannot contain a path separator")]
    PathSeparator,
    #[error("name cannot be \".\" or \"..\"")]
    DotOrDotDot,
}

/// Trims whitespace, then rejects: empty, `.`/`..` exactly, and any `/`
/// or `\` (both, regardless of host OS -- a name typed on any platform
/// must not be able to escape `scratch_dir()` on any other). This is the
/// only place user-typed text becomes part of a filesystem path in this
/// phase, so it is validated even though `crates/tui/**` isn't on
/// `CLAUDE.md`'s mandatory-`hacker`-review list.
pub fn validate_scratch_name(name: &str) -> Result<String, ScratchNameError>;

/// `validate_scratch_name` then `scratch_dir()?.join(name)`. Creates
/// `scratch_dir()` (not the file itself) if missing. `None` if
/// `scratch_dir()` can't be resolved (unresolvable `$HOME`).
pub fn new_scratch_path(name: &str) -> Result<Option<PathBuf>, ScratchNameError>;

/// Every regular file directly inside `scratch_dir()`, sorted by file
/// name (§1.2 -- no MRU tracker). Empty if the directory doesn't exist
/// yet or can't be resolved/read -- same best-effort-degrades-to-empty
/// contract `search_tree`'s callers already expect from a missing root.
pub fn list_scratch_files() -> Vec<PathBuf>;
```

### 2.2 `app.rs`/`commands.rs` additions

```rust
pub(crate) struct NewScratchFileState {
    pub(crate) name: String,
}

pub(crate) struct ScratchFilesState {
    pub(crate) query: String,
    pub(crate) selected: usize,
}
```

`App` gains `new_scratch_file: Option<NewScratchFileState>` and
`scratch_files: Option<ScratchFilesState>` (presence is visibility, same
convention every other popup already follows); `close_all_overlays`
clears both.

`commands.rs` gains two ids, both **palette-only, no default binding**:
real JetBrains IDEs have no default keybinding for "New Scratch File" or
"Scratch Files" in the tracked macOS keymap table (`docs/roadmap.md`
§5.2 has no such row) — per `CLAUDE.md`'s "never invent a binding" rule,
both join `ToggleTodoPanel`/`ToggleKeymapSettings` in the no-binding
category.
- `NewScratchFile` (`Action::NewScratchFile`) — opens the name-entry
  prompt (§2.3).
- `ScratchFiles` (`Action::ToggleScratchFiles`) — opens the browse-list
  popup (§2.3), reusing `keymap_popup`'s exact query/selected/Esc/Up/Down
  shape (a plain filtered list, same as Recent Files/TODO/Keymap).

New `App` methods:
- `fn toggle_new_scratch_file(&mut self)` / `fn
  handle_new_scratch_file_key(&mut self, key: KeyEvent) -> LoopSignal` —
  `Esc` cancels; typed chars extend `name`; `Backspace` shrinks it;
  `Enter` calls `confirm_new_scratch_file`.
- `fn confirm_new_scratch_file(&mut self)` — `scratch::new_scratch_path
  (&state.name)`; on `Err`/`Ok(None)`, `notify()`s the problem and leaves
  the prompt open (§3.1); on `Ok(Some(path))`, creates an empty file at
  `path` if it doesn't already exist (`std::fs::write(&path, "")`,
  skipped if the file is already there -- opening an *existing*
  same-named scratch file must never truncate it), then calls the
  existing `self.open_or_focus_tab(path)` and closes the prompt.
- `fn toggle_scratch_files(&mut self)` / `fn scratch_files_rows(&self) ->
  Vec<PathBuf>` (substring-filtered by file name against the popup's
  query, same shape `keymap_popup_rows` already established) / `fn
  handle_scratch_files_key` / `fn confirm_scratch_file` (opens the
  selected row via `open_or_focus_tab`, closes the popup).

### 2.3 Rendering

Two new `ui.rs` functions, both gated the same way every other popup is
in `render`:
- `render_new_scratch_file_prompt` — a single-line bordered box titled
  `"New Scratch File (name with extension, Enter to create, Esc to
  cancel):"`, body is just the typed `name` (this crate's first
  single-line *text-entry* popup that isn't the Find/Replace bar's
  status-line field or a list's own search box -- no new widget kind,
  just a `List` with one `ListItem` showing the current text, the
  simplest thing that reuses existing rendering machinery).
- `render_scratch_files_popup` — a plain filtered list of file names
  (`scratch_files_rows`), same shape `render_recent_files_popup` already
  is.

## 3. Behaviour

### 3.1 Creating a new scratch file

Typing a name and pressing Enter: an invalid name (§2.1) or an
unresolvable `scratch_dir()` notifies the specific problem and leaves the
prompt open for correction, rather than silently closing it — matches
this crate's established "signal, don't silence" convention
(`file-watcher.md`/`todo-panel.md`). A name that collides with an
existing scratch file opens that file's existing content (never
truncates it) — same "open or focus, never clobber" semantics
`open_or_focus_tab` already guarantees for every other path.

### 3.2 Browsing existing scratch files

`ScratchFiles` lists every file already in `scratch_dir()` (§1.2:
alphabetical, no tree grouping — this directory is flat by construction,
nothing to group by). Selecting one opens it exactly like any other file.

### 3.3 Interaction with everything else

Once opened, a scratch-file tab is indistinguishable from any other
open tab to the rest of this crate: `T25`'s file watcher does **not**
watch `scratch_dir()` (the watcher is rooted at `project_root`, an
unrelated directory — no code change needed for this, it's a natural
consequence of §1.1's "just a real file" design), so external edits to a
scratch file made from outside the app are only picked up on next open,
not live-refreshed; `T17`'s Recent Files/Bookmarks work on it exactly
like any other file the moment it's opened.

## 4. Constraints

1. `validate_scratch_name` is the only gate between typed text and a
   filesystem path in this phase — every call site that builds a scratch
   path goes through `new_scratch_path`, never a raw `scratch_dir()
   .join(name)`.
2. `list_scratch_files` never creates `scratch_dir()` as a side effect of
   listing (only `new_scratch_path` creates it, on an actual write) and
   never panics on a missing/unreadable directory.
3. Opening an existing scratch file never truncates it (§3.1).

## 5. Examples

`NewScratchFile` → type `notes.md` → `Enter`: creates (if missing) and
opens `~/.config/ide-tui/scratch/notes.md`, syntax-highlighted as
Markdown via the existing extension-based detection. `ScratchFiles` later
lists `notes.md` alongside anything else ever created; selecting it
reopens the same file with whatever was saved.

Typing `../../etc/passwd` into the prompt is rejected
(`ScratchNameError::PathSeparator`) before any path is ever constructed.

## 6. Dependencies / integration / tests

No new dependency. Diff scope: `crates/tui/src/{app,commands,lib,ui}.rs`
(new `mod scratch;` in `lib.rs`; `app.rs`'s `close_all_overlays` gains the
two new fields), new `crates/tui/src/scratch.rs`, this doc, `docs/
roadmap.md`. Not security-sensitive per `CLAUDE.md`'s list (no
subprocess, no project-root path handling — `scratch_dir()` is a fixed,
hardcoded per-user path, not a user-chosen project root); `hacker` is
skipped, though `validate_scratch_name` still defends against the one
real user-input-into-a-path case this phase introduces (§2.1's own
reasoning).

Tests: `validate_scratch_name` covers empty/whitespace-only/`.`/`..`/
embedded `/` and `\\`/an ordinary valid name; `new_scratch_path` covers a
valid name (path ends up under `scratch_dir()`), a rejected name (`Err`
propagates, nothing created), and directory-creation-on-demand (via a
test-local override of the home directory the same way `state.rs`'s own
tests split `state_file_path_from_home` out for testability — this
module needs the equivalent split, e.g. a private `scratch_dir_from_home`
`list_scratch_files`/`new_scratch_path` can be built against in tests);
`list_scratch_files` covers an empty/missing directory, a directory with
several files (sorted), and skips a subdirectory if one exists. `app.rs`:
`NewScratchFile`'s full key routing (typing, `Backspace`, `Esc`,
`Enter`-with-a-valid-name creating+opening+closing, `Enter`-with-an-
invalid-name notifying and staying open); creating a scratch file with
the same name twice opens the same tab/content the second time without
truncating it; `ScratchFiles`'s list/filter/open/`Esc` behaviour, same
shape already covered for Recent Files' equivalent tests.

## Revision notes

- `confirm_scratch_file`'s no-rows behaviour was initially implemented as
  "unconditionally close the popup, then optionally open a file" — wrong,
  and caught by comparing against `confirm_recent_file`'s own established
  convention before ever running the test suite: with zero matching rows,
  the popup must stay open (a true no-op), not close silently. Fixed to
  the same early-return shape `confirm_recent_file` already uses (`let
  Some(path) = rows.get(state.selected).cloned() else { return; };` before
  ever touching `self.scratch_files`), and the corresponding test
  (`confirm_scratch_file_with_no_rows_is_a_noop`) asserts
  `app.scratch_files.is_some()` accordingly.
- `thiserror = "1"` was added to `crates/tui/Cargo.toml` for
  `ScratchNameError`. Not a new-dependency decision — `thiserror = "1"` is
  already an approved, already-used workspace dependency (`ide-core`/
  `ide-lsp`'s own `Cargo.toml`s), just newly enabled for this third crate.
- `scratch.rs`'s public `scratch_dir()`/`new_scratch_path()`/
  `list_scratch_files()` operate against the real, shared
  `~/.config/ide-tui/scratch/` directory (unlike `state.rs`/`keymap.rs`,
  which are read far more than written in tests). `app.rs`'s tests that
  exercise these paths use test-unique filenames, an explicit
  `cleanup_scratch_file` helper, and nonsense-substring queries to assert
  "no matches" rather than assuming the shared directory itself is empty —
  needed to stay safe under `cargo test`'s parallel execution and any
  other real use of the `ide-tui` binary on the same machine.
