//! Application state and key dispatch (`docs/features/tui-shell-and-editor.md`
//! §2.2, extended by `docs/features/tui-multi-buffer-tabs.md` §2.1 for
//! multiple open tabs, `docs/features/tui-syntax-highlighting.md` §2.1 for
//! installing syntax at tab-open time, and `docs/features/tui-find.md`
//! §2.2 for the in-buffer find bar). No terminal I/O here -- `main.rs`
//! owns the `ratatui::Terminal` and calls `handle_key` per event,
//! `ui::render` per frame.

use std::ops::Range;
use std::path::{Path, PathBuf};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ide_core::{
    all_occurrences, detect_language, editorconfig, fuzzy_score, newline_indent, next_occurrence,
    splits_a_pair, syntax_for_path, word_at, Buffer, BufferError, Change, Charset, DirEntry,
    EditorConfig, FileWatcher, IndentUnit, LanguageConfig, LineDirection, Project, ProjectError,
    ReplaceResult, Selection, Selections, SyntaxRules, TextBuffer, Transaction, WatchEvent,
};
use ide_lsp::{Diagnostic, Location, LspRequest, Position, Symbol};

use crate::cargo_panel::{CargoCommand, CargoPanel};
use crate::claude_panel::ClaudePanel;
use crate::claude_terminal::{self, ClaudeTerminalPanel};
use crate::commands::{commands, Action, Command};
use crate::debug_config::{self, DebugAdapterConfig, DebugAdapterEntry};
use crate::debug_panel::DebugPanel;
use crate::docker_panel::{DockerLifecycleAction, DockerPanel, DockerTab};
use crate::editor::{
    closer_for, cursor_line_column, is_quoted_or_commented, line_end_offset, line_start_offset,
    may_open_pair, move_cursor, offset_for_line_column, scroll_to_keep_visible, word_end_after,
    word_range_at, word_start_before, Direction,
};
use crate::files_search::FilesSearchPanel;
use crate::find::{FindField, FindState};
use crate::folding::{self, VisualLines};
use crate::git_panel::{GitPanel, WorktreeAddField};
use crate::k8s_panel::{K8sPanel, K8sPicker, K8sTab};
use crate::keymap::{self, KeymapOverlay};
use crate::lsp_bridge::LspBridge;
use crate::nav_history::{NavHistory, NavLocation};
use crate::project_state::{self, ProjectNavigationState};
use crate::scratch;
use crate::search_panel::SearchPanel;
use crate::todo_panel::TodoPanel;
use crate::tree::TreeState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Editor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopSignal {
    Continue,
    Exit,
}

/// Which extended (non-single-step) caret motion a key press requested --
/// shares one fold-aware, multi-cursor-aware dispatch method
/// (`move_caret_extended`) rather than four near-identical ones
/// (`docs/features/tui-word-and-document-navigation.md` §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtendedMotion {
    LineStart,
    LineEnd,
    WordLeft,
    WordRight,
    DocumentStart,
    DocumentEnd,
}

/// Which sub-view the Claude panel currently shows (`docs/features/
/// tui-claude-panel.md` §1.1/§3.1). `Terminal`'s index is into
/// `App.claude_terminals.tabs()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeView {
    Chat,
    Terminal(usize),
}

pub(crate) struct NewClaudeTerminalState {
    pub(crate) name: String,
}

pub(crate) struct OpenBuffer {
    pub(crate) path: PathBuf,
    pub(crate) buffer: Buffer,
    pub(crate) scroll: u16,
    /// Sticky column for consecutive vertical moves (`editor::move_cursor`'s
    /// `desired_column`) -- lives here because it must survive between
    /// separate `handle_key` calls, one per keystroke.
    pub(crate) desired_column: Option<usize>,
    /// The offset of a closer this buffer's last keystroke auto-inserted, if
    /// any -- the one-keystroke-wide type-over window
    /// (`docs/features/tui-smart-editing.md` §3.2). Taken (reset to `None`)
    /// at the top of every `handle_editor_key` call; only a `Char` that
    /// opens a delimiter writes it back.
    pub(crate) auto_closed: Option<usize>,
    /// Resolved once at tab-open time from `editorconfig::resolve`
    /// (`docs/features/tui-line-commands-and-editorconfig.md` §2.1/§3.5) --
    /// `EditorConfig::default()` for an unresolvable path. Kept separately
    /// from `indent` because `editorconfig::save_edit`/`save_charset` need
    /// the untranslated config, not the `IndentUnit` derived from it.
    pub(crate) config: EditorConfig,
    /// `config.indent_style`/`config.indent_size` mapped onto
    /// `IndentUnit::default()` -- every editing function that used to read
    /// `IndentUnit::default()` directly now reads this instead. Computed
    /// once at tab-open time (`resolve_editor_config` + `indent_unit_for`),
    /// not per keystroke.
    pub(crate) indent: IndentUnit,
    /// Whether the "saved as UTF-8" notice has already fired for this
    /// tab's `config.charset` -- reset whenever `config` is (re-)applied.
    pub(crate) charset_notice_shown: bool,
    /// `Extend`/`Shrink Selection`'s stack of prior `Selections`, newest
    /// last (`docs/features/tui-line-commands-and-editorconfig.md`
    /// §3.3/§3.4). Cleared by any edit and by any arrow move --
    /// unconditionally at the top of `handle_editor_key`, and by
    /// `run_line_op` for the dozen actions that reach the buffer through
    /// `run_action` instead.
    pub(crate) shrink_stack: Vec<Selections>,
    /// `start_line` of every currently-collapsed fold
    /// (`docs/features/tui-code-folding.md` §2.2) -- session-only view
    /// state, reset (empty) whenever a tab is (re)opened, the same
    /// category `shrink_stack`/`auto_closed` are already in.
    pub(crate) folded: std::collections::BTreeSet<usize>,
    /// Set by `poll_watcher` (`docs/features/tui-file-watcher.md` §3.2/
    /// §3.3) when this tab's path changed or was removed on disk. Cleared
    /// by `ReloadFromDisk`, `DismissExternalChange`, or the tab closing.
    pub(crate) external_change: Option<ExternalChange>,
    /// `None` = off (`docs/features/tui-blame.md` §2.3). Populated by
    /// `toggle_blame_annotations`, refreshed by `refresh_blame_if_on`
    /// after Save and after Reload-from-disk -- `ide-tui` has no Save As,
    /// so unlike `ide-ui`'s three call sites this crate needs exactly two.
    pub(crate) blame: Option<Vec<crate::blame_gutter::BlameAnnotation>>,
}

/// `docs/features/tui-file-watcher.md` §2.2, ported from `ide-ui`'s own
/// `ExternalChange` (`crates/ui/src/app.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalChange {
    /// A dirty tab's file changed on disk -- offers `ReloadFromDisk`/
    /// `DismissExternalChange`.
    Modified,
    /// The tab's file was removed on disk, regardless of dirty state --
    /// offers `DismissExternalChange` only (no reload target exists).
    Deleted,
}

pub(crate) struct PaletteState {
    pub(crate) query: String,
    pub(crate) filtered: Vec<&'static Command>,
    pub(crate) selected: usize,
}

/// Zero-or-many result picker shared by Go to Declaration and Find Usages
/// (`docs/features/tui-goto-and-usages.md` §2.2) -- exactly one result
/// jumps immediately and never creates this; this only exists while the
/// user has to choose among several, or to show an explicit "none found"
/// state.
pub(crate) struct GotoState {
    pub(crate) title: &'static str,
    pub(crate) results: Vec<Location>,
    pub(crate) selected: usize,
}

/// One entry in `App::notifications` -- an in-app notification log (not a
/// desktop/OS notification), currently populated only by Go to
/// Declaration/Find Usages outcomes (`docs/features/
/// tui-goto-and-usages.md` §2.4). Newest last; `read` starts `false` and
/// is only ever flipped in bulk, by the panel's own `r` (mark all read)
/// key (§2.4) -- no per-entry read tracking in v1.
pub(crate) struct Notification {
    pub(crate) message: String,
    pub(crate) read: bool,
}

/// Selection state for the Problems panel (`docs/features/tui-problems.md`,
/// `T9`) -- the diagnostics themselves live in `lsp.diagnostics`
/// (per-file, LSP's own storage shape); this only tracks which flattened
/// row is selected, since `flattened_diagnostics` recomputes that
/// flattening fresh on every call rather than caching it.
pub(crate) struct ProblemsState {
    pub(crate) selected: usize,
}

/// Find in Path's typed query and list selection (`docs/features/
/// tui-find-in-path.md` §2.2) -- separate from `search: SearchPanel`
/// (the background search machinery, ported from `ide-ui`) the same way
/// `ProblemsState` is separate from `lsp.diagnostics`: this only tracks
/// UI-local state, not the results themselves. Persists across the
/// overlay closing/reopening, same as `cargo`/`CargoPanel` does.
#[derive(Default)]
pub(crate) struct SearchOverlayState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    /// The exact (trimmed) query string `search.results` currently
    /// reflects, or that a still-in-flight search is answering -- `None`
    /// until the first search ever runs. Compared against `query` by
    /// `submit_or_open_search_result` to tell "Enter should search again"
    /// from "Enter should open the selected row" apart, since this crate
    /// has only one key for both actions (§2.2's disambiguation).
    ran_query: Option<String>,
}

/// Go to File's typed query and list selection (`docs/features/
/// tui-go-to-file-and-symbol.md` §2.3) -- separate from `files_search:
/// FilesSearchPanel` the same way `SearchOverlayState` is separate from
/// `search: SearchPanel`. Unlike `SearchOverlayState`'s submit-then-open
/// `Enter` model, this overlay refreshes **live**, every frame the query
/// changes -- `ran_query` here tracks what was last *asked for* purely to
/// avoid re-running an unchanged query, not to disambiguate a shared
/// submit/open key the way `SearchOverlayState::ran_query` does.
#[derive(Default)]
pub(crate) struct GoToFileState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    ran_query: Option<String>,
}

/// Go to Symbol's typed query and list selection (`docs/features/
/// tui-go-to-file-and-symbol.md` §2.3). The results themselves live in
/// `lsp.document_symbols`/`lsp.workspace_symbols`, same "this only tracks
/// UI-local state" convention `ProblemsState`/`SearchOverlayState` already
/// establish.
#[derive(Default)]
pub(crate) struct GoToSymbolState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    last_workspace_query: Option<String>,
    /// The path `request_document_symbols` was last *sent* for -- gates
    /// the empty-query branch the same way `search-everywhere.md` §3.2's
    /// `document_symbols_requested_for` does in `ide-ui`, for the
    /// identical reason: `lsp.document_symbols_path` only updates once a
    /// *response* lands, so gating on it alone would re-send a fresh
    /// request every single frame for the whole duration a slow server
    /// takes to answer.
    requested_for: Option<PathBuf>,
}

/// Show Intention Actions' list-selection state (`docs/features/
/// tui-code-actions-and-rename.md` §2.3) -- presence is visibility, same
/// convention `ProblemsState` establishes: the actions themselves live in
/// `lsp.code_actions`, this only tracks which row is selected.
pub(crate) struct CodeActionsState {
    pub(crate) selected: usize,
}

/// Recent Files' typed query and list selection (`docs/features/
/// tui-recent-files-and-bookmarks.md` §2.3). The candidate list itself is
/// `nav_state.recent_files` -- this only tracks the live filter/selection,
/// same "this only tracks UI-local state" convention `GoToFileState`
/// establishes. No `ran_query`/background search here -- filtering the
/// (bounded, in-memory) recent-files list is cheap enough to redo
/// synchronously every keystroke, unlike a whole-project scan.
#[derive(Default)]
pub(crate) struct RecentFilesState {
    pub(crate) query: String,
    pub(crate) selected: usize,
}

/// Show Bookmarks' list selection (`docs/features/
/// tui-recent-files-and-bookmarks.md` §2.3) -- the bookmarks themselves
/// live in `nav_state.bookmarks`, same convention `CodeActionsState`
/// establishes.
#[derive(Default)]
pub(crate) struct BookmarksPopupState {
    pub(crate) selected: usize,
}

/// TODO panel's list selection (`docs/features/tui-todo-panel.md` §2.2) --
/// the matches themselves live in `todo.results`, same convention
/// `CodeActionsState`/`BookmarksPopupState` establish.
#[derive(Default)]
pub(crate) struct TodoPanelState {
    pub(crate) selected: usize,
}

/// Rename's editable popup (`docs/features/tui-code-actions-and-rename.md`
/// §2.3), ported from `ide-ui`'s own `RenamePopup` -- presence is
/// visibility, no separate `show_*` bool (unlike `hover_open`'s pair with
/// `lsp.hover`: a `RenamePopup` has no reason to survive its own close).
pub(crate) struct RenamePopup {
    pub(crate) path: PathBuf,
    pub(crate) position: Position,
    pub(crate) original_name: String,
    /// The popup's editable text, mutated directly by `handle_rename_
    /// popup_key` -- same "render code reads a field the key handler
    /// writes" convention `find`'s query and `search_state.query` already
    /// use. Pre-filled with `original_name`.
    pub(crate) input: String,
}

/// Which top-level view the Git Panel overlay shows (`docs/features/
/// tui-git-staging-branches-and-log-filters.md` §2.2, T28) -- `Log` is
/// `T11`'s original Graph/Conflicts/Diff/Filter view, `Changes` is the new
/// staged/unstaged/commit-message view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum GitPanelView {
    #[default]
    Log,
    Changes,
}

/// Which list/pane has keyboard focus inside the Git Panel overlay's `Log`
/// view (`docs/features/tui-git-panel.md` §2.2/§3.2, `Filter` added by
/// `tui-git-staging-branches-and-log-filters.md` §2.2). Conflict
/// resolution is a distinct mode layered on top of this, not a fifth
/// variant -- see `handle_git_panel_key`'s doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum GitPanelFocus {
    #[default]
    Graph,
    Conflicts,
    Diff,
    Filter,
}

impl GitPanelFocus {
    /// `Tab`'s cycle order, skipping `Conflicts` when there are none to
    /// browse and skipping `Filter` while viewing file history (the
    /// filter bar has nothing to apply to in that mode, `tui-git-staging-
    /// branches-and-log-filters.md` §3.5).
    fn next(self, conflicts_empty: bool, filter_hidden: bool) -> Self {
        match self {
            GitPanelFocus::Graph => {
                if conflicts_empty {
                    GitPanelFocus::Diff
                } else {
                    GitPanelFocus::Conflicts
                }
            }
            GitPanelFocus::Conflicts => GitPanelFocus::Diff,
            GitPanelFocus::Diff => {
                if filter_hidden {
                    GitPanelFocus::Graph
                } else {
                    GitPanelFocus::Filter
                }
            }
            GitPanelFocus::Filter => GitPanelFocus::Graph,
        }
    }
}

/// Which sub-widget has focus inside the Git Panel overlay's `Changes`
/// view (`docs/features/tui-git-staging-branches-and-log-filters.md`
/// §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ChangesFocus {
    #[default]
    Staged,
    Unstaged,
    Message,
}

impl ChangesFocus {
    fn next(self) -> Self {
        match self {
            ChangesFocus::Staged => ChangesFocus::Unstaged,
            ChangesFocus::Unstaged => ChangesFocus::Message,
            ChangesFocus::Message => ChangesFocus::Staged,
        }
    }

    fn previous(self) -> Self {
        match self {
            ChangesFocus::Staged => ChangesFocus::Message,
            ChangesFocus::Unstaged => ChangesFocus::Staged,
            ChangesFocus::Message => ChangesFocus::Unstaged,
        }
    }
}

/// Which `LogFilterState` text field is being edited while `GitPanelFocus
/// ::Filter` has focus (`docs/features/
/// tui-git-staging-branches-and-log-filters.md` §2.2/§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FilterField {
    #[default]
    Branch,
    Author,
    Path,
    Since,
    Until,
    Query,
}

impl FilterField {
    fn next(self) -> Self {
        match self {
            FilterField::Branch => FilterField::Author,
            FilterField::Author => FilterField::Path,
            FilterField::Path => FilterField::Since,
            FilterField::Since => FilterField::Until,
            FilterField::Until => FilterField::Query,
            FilterField::Query => FilterField::Branch,
        }
    }

    fn previous(self) -> Self {
        match self {
            FilterField::Branch => FilterField::Query,
            FilterField::Author => FilterField::Branch,
            FilterField::Path => FilterField::Author,
            FilterField::Since => FilterField::Path,
            FilterField::Until => FilterField::Since,
            FilterField::Query => FilterField::Until,
        }
    }

    /// The `LogFilterState` field this variant addresses, as a mutable
    /// `&mut String` -- one dispatch point for both typing and Backspace
    /// rather than duplicating the match in both call sites.
    fn text_mut(self, filter: &mut crate::git_panel::LogFilterState) -> &mut String {
        match self {
            FilterField::Branch => &mut filter.branch,
            FilterField::Author => &mut filter.author,
            FilterField::Path => &mut filter.path,
            FilterField::Since => &mut filter.since,
            FilterField::Until => &mut filter.until,
            FilterField::Query => &mut filter.query,
        }
    }
}

/// The Git Panel overlay's cursor/scroll state (`docs/features/
/// tui-git-panel.md` §2.2, extended by `tui-git-staging-branches-and-
/// log-filters.md` §2.2) -- presence is visibility. `App::git`'s own
/// fields (graph, diff, conflicts, status, branches, log_filter, ...)
/// persist across the overlay opening/closing, matching `ide-ui`'s
/// toolbar-toggle persistence; only this transient UI state resets on
/// every open (a fresh `default()`).
#[derive(Default)]
pub(crate) struct GitPanelState {
    pub(crate) view: GitPanelView,
    pub(crate) focus: GitPanelFocus,
    pub(crate) changes_focus: ChangesFocus,
    pub(crate) filter_field: FilterField,
    pub(crate) graph_selected: usize,
    pub(crate) conflicts_selected: usize,
    pub(crate) diff_scroll: u16,
    pub(crate) staged_selected: usize,
    pub(crate) unstaged_selected: usize,
}

/// The Keymap popup's cursor/search/capture state (`docs/features/
/// tui-keymap.md` §2.4) -- presence is visibility, same convention every
/// other popup in this crate already follows.
pub(crate) struct KeymapPopupState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    /// `Some(id)` while the *next* raw key event this popup receives is
    /// captured as `id`'s new binding instead of navigation/search input.
    pub(crate) capturing: Option<&'static str>,
}

/// The "New Scratch File" name-entry prompt's state (`docs/features/
/// tui-scratch-files.md` §2.2) -- presence is visibility.
pub(crate) struct NewScratchFileState {
    pub(crate) name: String,
}

/// Which of `DebugAdapterConfigPopupState`'s two text fields `Tab`/
/// `Shift+Tab` currently targets (`docs/features/tui-debugger.md` §2.2) --
/// this crate's only existing text-entry popups are single-field, so this
/// is the first two-field text popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DebugConfigField {
    Command,
    Args,
}

/// The "Configure Debug Adapter" popup's state (`docs/features/
/// tui-debugger.md` §2.5) -- presence is visibility.
pub(crate) struct DebugAdapterConfigPopupState {
    pub(crate) command: String,
    pub(crate) args: String,
    pub(crate) field: DebugConfigField,
}

/// Which section of the Debug tool window (`docs/features/
/// tui-debugger.md` §2.6) currently has keyboard focus -- `Tab`/
/// `Shift+Tab` cycles, mirroring `GitPanelFocus`'s own cycle convention.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DebugPanelFocus {
    #[default]
    Threads,
    Stack,
    Output,
}

impl DebugPanelFocus {
    fn next(self) -> Self {
        match self {
            DebugPanelFocus::Threads => DebugPanelFocus::Stack,
            DebugPanelFocus::Stack => DebugPanelFocus::Output,
            DebugPanelFocus::Output => DebugPanelFocus::Threads,
        }
    }

    fn previous(self) -> Self {
        match self {
            DebugPanelFocus::Threads => DebugPanelFocus::Output,
            DebugPanelFocus::Stack => DebugPanelFocus::Threads,
            DebugPanelFocus::Output => DebugPanelFocus::Stack,
        }
    }
}

/// The Debug tool window's transient browsing cursor (`docs/features/
/// tui-debugger.md` §2.6) -- `self.debug`'s own fields (session, threads,
/// stack, output) persist across close/reopen; only this cursor resets on
/// every open, the same "transient overlay cursor" role `GitPanelState`
/// already plays relative to `App::git`.
#[derive(Debug, Default)]
pub(crate) struct DebugPanelState {
    pub(crate) focus: DebugPanelFocus,
    pub(crate) thread_selected: usize,
    pub(crate) stack_selected: usize,
    pub(crate) output_scroll: u16,
}

/// The Scratch Files browse-list popup's state -- presence is visibility,
/// same shape `RecentFilesState` already establishes.
pub(crate) struct ScratchFilesState {
    pub(crate) query: String,
    pub(crate) selected: usize,
}

pub struct App {
    pub(crate) project_root: PathBuf,
    pub(crate) tree: DirEntry,
    pub(crate) tree_state: TreeState,
    pub(crate) focus: Focus,
    pub(crate) tabs: Vec<OpenBuffer>,
    pub(crate) active_tab: Option<usize>,
    pub(crate) palette: Option<PaletteState>,
    pub(crate) find: Option<FindState>,
    pub(crate) goto: Option<GotoState>,
    /// The `(path, position)` a `Ctrl+B` press actually fired from --
    /// remembered so `handle_interface_check_response` knows where to
    /// re-query `Implementation` from if the resolved declaration turns
    /// out to need one (`docs/features/goto-declaration-interface-
    /// redirect.md` §2.3). `None` whenever no such check is pending.
    goto_declaration_origin: Option<(PathBuf, Position)>,
    /// The single `Goto` result `handle_goto_results` is holding while it
    /// waits on a `DocumentSymbol` response (for that result's own file)
    /// to decide whether to jump to it directly or redirect to
    /// `Implementation` from `goto_declaration_origin` instead. `None`
    /// whenever no check is pending.
    pending_interface_check: Option<Location>,
    /// `true` for exactly the one `poll_lsp` call meant to consume the
    /// `Goto` response the interface redirect itself sent -- lets
    /// `poll_lsp` label that response `"Implementation"` instead of the
    /// hardcoded `"Declaration"`, and (since `handle_goto_results` only
    /// takes the redirect branch for title `"Declaration"`) keeps the
    /// redirected result from being checked against `DocumentSymbol` a
    /// second time.
    expect_implementation_next: bool,
    pub(crate) notifications: Vec<Notification>,
    pub(crate) notifications_open: bool,
    pub(crate) problems: Option<ProblemsState>,
    pub(crate) cargo: CargoPanel,
    pub(crate) cargo_panel_open: bool,
    pub(crate) hover_open: bool,
    /// The `(path, position)` `sync_document_highlights` most recently
    /// fired a `DocumentHighlight` query for -- lets it tell "the caret
    /// moved to somewhere new" apart from "still sitting where it was
    /// last frame" without re-querying every single frame (`docs/features/
    /// tui-hover-and-inlay-hints.md` §2.2).
    last_highlighted_target: Option<(PathBuf, Position)>,
    pub(crate) search: SearchPanel,
    pub(crate) search_state: SearchOverlayState,
    pub(crate) search_open: bool,
    pub(crate) files_search: FilesSearchPanel,
    pub(crate) go_to_file: Option<GoToFileState>,
    pub(crate) go_to_symbol: Option<GoToSymbolState>,
    pub(crate) nav_state: ProjectNavigationState,
    /// Back/forward jump history (`docs/features/
    /// tui-back-forward-navigation.md`, T31) -- an unrelated concept from
    /// `nav_state` above (Recent Files/Bookmarks); named distinctly from
    /// it (rather than `ide-ui`'s own bare `nav`) precisely to avoid the
    /// two being confused at a glance.
    pub(crate) nav_history: NavHistory,
    pub(crate) recent_files: Option<RecentFilesState>,
    pub(crate) bookmarks_popup: Option<BookmarksPopupState>,
    pub(crate) todo: TodoPanel,
    pub(crate) todo_panel: Option<TodoPanelState>,
    /// `None` if the watcher failed to start (`docs/features/
    /// tui-file-watcher.md` §2.2) -- degrades to "no automatic refresh",
    /// never a failure to open the project.
    watcher: Option<FileWatcher>,
    pub(crate) keymap: KeymapOverlay,
    /// `Some` only ever set by a test, to redirect `persist_keymap`'s
    /// writes to a tempdir instead of the real `$HOME/.config/ide-tui/
    /// keymap.json` -- without it, a test exercising rebind-capture/
    /// reset would corrupt the machine's actual keymap file (and, since
    /// `cargo test` runs this crate's tests in one process, race every
    /// other concurrently-running test's `App::new` -> `keymap::load()`
    /// against that corruption). Always `None` in production.
    keymap_path_override: Option<std::path::PathBuf>,
    pub(crate) keymap_popup: Option<KeymapPopupState>,
    pub(crate) new_scratch_file: Option<NewScratchFileState>,
    pub(crate) scratch_files: Option<ScratchFilesState>,
    pub(crate) claude: ClaudePanel,
    pub(crate) claude_terminals: ClaudeTerminalPanel,
    pub(crate) claude_panel_open: bool,
    pub(crate) claude_view: ClaudeView,
    /// `true` while the active Claude Terminal tab has raw keyboard focus
    /// (every key forwarded to its PTY); `false` in "chrome mode" (`Tab`/
    /// `Ctrl+N`/`Ctrl+W`/`Esc` navigate the panel instead) -- `docs/
    /// features/tui-claude-panel.md` §1.1's TUI-only two-mode split.
    pub(crate) claude_terminal_focus: bool,
    pub(crate) new_claude_terminal: Option<NewClaudeTerminalState>,
    pub(crate) code_actions: Option<CodeActionsState>,
    /// The `(path, position)` `sync_code_actions` most recently fired a
    /// `CodeAction` query for -- mirrors `last_highlighted_target`
    /// (`docs/features/tui-code-actions-and-rename.md` §2.3).
    last_code_actions_target: Option<(PathBuf, Position)>,
    pub(crate) rename_popup: Option<RenamePopup>,
    /// `(edit, new_name)` awaiting Apply/Cancel -- presence is visibility,
    /// same reasoning `rename_popup` documents.
    pub(crate) pending_rename_preview: Option<(ide_lsp::WorkspaceEdit, String)>,
    pub(crate) lsp: LspBridge,
    pub(crate) git: GitPanel,
    pub(crate) git_panel: Option<GitPanelState>,
    /// `sync_git_working_tree_diff`'s guard, mirrors `last_code_actions_
    /// target` (`docs/features/tui-git-panel.md` §3.1).
    last_git_diff_target: Option<PathBuf>,
    /// The commit id whose detail the Commit Details popup is showing
    /// (`docs/features/tui-blame.md` §2.3) -- presence is visibility,
    /// same convention `git_panel` uses. `GitPanel::commit_detail` is
    /// re-fetched fresh every render frame this is `Some`, not cached.
    pub(crate) blame_popup: Option<String>,
    /// Scroll offset for a `blame_popup` body too tall to fit -- mirrors
    /// `GitPanelState.diff_scroll`'s shape exactly. Reset to `0` every
    /// time `blame_popup` transitions to a different (or freshly `Some`)
    /// commit id, so a second lookup never inherits the first one's
    /// scroll position.
    pub(crate) blame_popup_scroll: u16,
    /// The active tab's gutter marks, recomputed every frame `sync_git_
    /// gutter` runs (`docs/features/tui-git-gutter.md` §3.1) -- empty with
    /// no active tab, a dirty buffer, or no repo.
    pub(crate) git_gutter: Vec<crate::git_gutter::GutterMark>,
    /// The path `git_gutter` answers for -- lets a render call tell "these
    /// marks are for the tab on screen right now" apart from one frame of
    /// staleness at a tab switch, same role `last_git_diff_target` plays.
    git_gutter_path: Option<PathBuf>,
    /// The buffer line a sign-column click landed on, while its "Revert
    /// Hunk (r) / Show Diff (d)" popup is open. `None` when closed.
    pub(crate) git_gutter_popup_line: Option<usize>,
    /// Presence is visibility, same convention `git_panel` uses -- unlike
    /// `git`/`git_panel`'s split (an always-present service plus a
    /// separate open/focus wrapper), `DockerPanel`/`K8sPanel` carry their
    /// own state directly since neither panel does any ambient background
    /// refresh while closed (`docs/features/tui-docker-and-kubernetes.md`
    /// §2.4 -- a deliberate scope cut, not an oversight).
    pub(crate) docker_panel: Option<DockerPanel>,
    pub(crate) k8s_panel: Option<K8sPanel>,
    /// The one detected project language, retained instead of being a
    /// `let` local `App::new` discards after starting the LSP server
    /// (`docs/features/tui-debugger.md` §2.2) -- `None` for an
    /// unrecognized project, same as `detect_language`'s own return type.
    pub(crate) language: Option<LanguageConfig>,
    /// Loaded once at startup (`debug_config::load`); mutated only by
    /// `ConfigureDebugAdapter`'s popup.
    pub(crate) debug_adapters: DebugAdapterConfig,
    pub(crate) debug: DebugPanel,
    /// Debug tool window visibility -- same bare-bool convention as
    /// `cargo_panel_open`/`claude_panel_open`. `self.debug`'s own fields
    /// persist across a close/reopen; only this flag changes.
    pub(crate) debug_panel_open: bool,
    /// "Configure Debug Adapter" popup state -- presence is visibility.
    pub(crate) debug_adapter_config_popup: Option<DebugAdapterConfigPopupState>,
    pub(crate) debug_panel: DebugPanelState,
    status: Option<String>,
    editor_viewport_rows: u16,
}

impl App {
    pub fn new(root: PathBuf) -> Result<Self, ProjectError> {
        let project = Project::open(&root)?;
        let tree = project.scan_tree();
        let mut git = GitPanel::default();
        git.refresh(project.root());
        let mut startup_notifications = Vec::new();
        let watcher = match FileWatcher::new(project.root()) {
            Ok(watcher) => Some(watcher),
            Err(err) => {
                // Degrades to "no automatic refresh", never a failure to
                // open the project (`docs/features/tui-file-watcher.md`
                // §2.2/§3.6) -- queued directly into the notification list
                // under construction, since `self.notify` needs a
                // constructed `Self` to call on.
                startup_notifications.push(Notification {
                    message: format!("file watcher failed to start: {err}"),
                    read: false,
                });
                None
            }
        };
        let mut lsp = LspBridge::default();
        let debug_adapters = debug_config::load();
        // No language-settings UI in `ide-tui` yet, so only the one
        // built-in language (Rust, via `Cargo.toml` detection) is ever
        // recognized -- `custom` is always empty (`docs/features/
        // tui-goto-and-usages.md` §4).
        let language = detect_language(&tree, &[]).map(|mut lang| {
            lsp.start_with_command(project.root(), &lang.command, &lang.args);
            // Enriches the retained `LanguageConfig` with any persisted
            // debug-adapter override for this language's name, so
            // `language.debug_adapter()` "just works" exactly like
            // `ide-ui`'s persisted `custom_languages` does -- zero new
            // `ide-core` API (`docs/features/tui-debugger.md` §2.2).
            if let Some(entry) = debug_adapters.adapters.get(&lang.name) {
                lang.debug_adapter_command = Some(entry.command.clone());
                lang.debug_adapter_args = entry.args.clone();
            }
            lang
        });
        Ok(Self {
            project_root: project.root().to_path_buf(),
            tree,
            tree_state: TreeState::new(),
            focus: Focus::Tree,
            tabs: Vec::new(),
            active_tab: None,
            palette: None,
            find: None,
            goto: None,
            goto_declaration_origin: None,
            pending_interface_check: None,
            expect_implementation_next: false,
            notifications: startup_notifications,
            notifications_open: false,
            problems: None,
            cargo: CargoPanel::default(),
            cargo_panel_open: false,
            hover_open: false,
            last_highlighted_target: None,
            search: SearchPanel::default(),
            search_state: SearchOverlayState::default(),
            search_open: false,
            files_search: FilesSearchPanel::default(),
            go_to_file: None,
            go_to_symbol: None,
            nav_state: project_state::load(project.root()),
            nav_history: NavHistory::default(),
            recent_files: None,
            bookmarks_popup: None,
            todo: TodoPanel::default(),
            todo_panel: None,
            watcher,
            keymap: crate::keymap::load(),
            keymap_path_override: None,
            keymap_popup: None,
            new_scratch_file: None,
            scratch_files: None,
            claude: ClaudePanel::default(),
            claude_terminals: ClaudeTerminalPanel::default(),
            claude_panel_open: false,
            claude_view: ClaudeView::Chat,
            claude_terminal_focus: false,
            new_claude_terminal: None,
            code_actions: None,
            last_code_actions_target: None,
            rename_popup: None,
            pending_rename_preview: None,
            lsp,
            git,
            git_panel: None,
            last_git_diff_target: None,
            blame_popup: None,
            blame_popup_scroll: 0,
            git_gutter: Vec::new(),
            git_gutter_path: None,
            git_gutter_popup_line: None,
            docker_panel: None,
            k8s_panel: None,
            language,
            debug_adapters,
            debug: DebugPanel::default(),
            debug_panel_open: false,
            debug_adapter_config_popup: None,
            debug_panel: DebugPanelState::default(),
            status: None,
            // Real value arrives from `main.rs`'s next `set_editor_viewport_
            // rows` call, once the terminal size is known -- `u16::MAX`
            // until then (and in every test that never calls it) makes
            // `scroll_to_keep_visible` an unconditional no-op, matching
            // this crate's pre-scroll-follow behavior exactly rather than
            // guessing a real viewport height no test fixture needs.
            editor_viewport_rows: u16::MAX,
        })
    }

    /// Called once per frame by `main.rs`, before `handle_key` -- drains
    /// any ready Goto/Find Usages response and either jumps straight to
    /// the sole result, opens the picker for zero-or-many, or (on zero)
    /// leaves a status message (`docs/features/tui-goto-and-usages.md`
    /// §3.3).
    pub fn poll_lsp(&mut self) {
        self.lsp.poll();
        if self.lsp.goto_ready {
            let results = std::mem::take(&mut self.lsp.goto);
            let title = if std::mem::take(&mut self.expect_implementation_next) {
                "Implementation"
            } else {
                "Declaration"
            };
            self.handle_goto_results(title, results);
        }
        if self.lsp.references_ready {
            let results = std::mem::take(&mut self.lsp.references);
            self.handle_goto_results("Usages", results);
        }
        self.handle_interface_check_response();
        self.handle_workspace_edit_ready();
        self.handle_prepare_rename_ready();
        self.handle_rename_ready();
    }

    /// Called once per frame by `lib.rs`'s run loop, alongside `poll_lsp`
    /// -- drains any output the currently-running `cargo` command has
    /// produced since the last frame, regardless of whether the Cargo
    /// panel is open (`docs/features/tui-cargo-panel.md` §3).
    pub fn poll_cargo(&mut self) {
        self.cargo.poll();
    }

    /// Called once per frame (`lib.rs`'s main loop), but only while the
    /// respective panel is open -- unlike `poll_cargo`/`poll_search`/
    /// `poll_todo`'s unconditional background polling, this is a
    /// deliberate scope cut (`docs/features/tui-docker-and-kubernetes.md`
    /// §2.4/§4): closing the panel mid-request just drops the in-flight
    /// `Receiver`, which is fine here since a fresh list/logs fetch is
    /// cheap to re-request the next time the panel reopens, unlike a
    /// build the user explicitly started and may want to alt-tab away
    /// from.
    pub fn poll_docker(&mut self) {
        if let Some(panel) = self.docker_panel.as_mut() {
            panel.poll();
        }
    }

    /// Same reasoning as `poll_docker`, for the Kubernetes panel.
    pub fn poll_k8s(&mut self) {
        if let Some(panel) = self.k8s_panel.as_mut() {
            panel.poll();
        }
    }

    /// Same shape, for a Find in Path search running in the background
    /// (`docs/features/tui-find-in-path.md` §3.1).
    pub fn poll_search(&mut self) {
        self.search.poll();
        self.files_search.poll();
    }

    /// Called once per frame (`lib.rs`'s main loop), unconditionally --
    /// same "keeps streaming into state while the panel is closed" shape
    /// `poll_search`/`poll_cargo` already use (`docs/features/
    /// tui-todo-panel.md` §2.2).
    pub fn poll_todo(&mut self) {
        self.todo.poll();
    }

    /// Called once per frame (`lib.rs`'s main loop). No-op if the watcher
    /// failed to start. Drains `watcher.poll()` and dispatches every event
    /// (`docs/features/tui-file-watcher.md` §3.1).
    pub fn poll_watcher(&mut self) {
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        let events = watcher.poll();
        for event in events {
            match event {
                WatchEvent::TreeChanged => self.refresh_tree(),
                WatchEvent::FileModified(path) => self.handle_external_modification(&path),
                WatchEvent::FileRemoved(path) => self.handle_external_removal(&path),
            }
        }
    }

    /// Called once per frame (`lib.rs`'s main loop), unconditionally --
    /// never gated on `claude_panel_open` (`docs/features/
    /// tui-claude-panel.md` §4.2: an ungated PTY channel is a DoS
    /// surface, already found and fixed once in `ide-ui`'s own `hacker`
    /// pass on this same feature). Drains the chat's in-flight `claude -p`
    /// reply, if any, and every open terminal tab's PTY output.
    pub fn poll_claude(&mut self) {
        self.claude.poll();
        self.claude_terminals.poll();
    }

    /// Called once per frame (`lib.rs`'s main loop), unconditionally --
    /// same "keeps streaming into state while the panel is closed" shape
    /// `poll_claude`/`poll_cargo` already use (`docs/features/
    /// tui-debugger.md` §2.3).
    pub fn poll_debug(&mut self) {
        self.debug.poll();
    }

    /// `Debug` command (`docs/features/tui-debugger.md` §3): opens the
    /// launch popup. Silent no-op if a session is already active or the
    /// detected project language has no configured debug adapter -- the
    /// same "no active file has a configured language" no-op `ide-ui`'s
    /// own `trigger_debug` uses; this crate has no separate
    /// command-enablement registry to gate on instead.
    fn trigger_debug(&mut self) {
        if self.debug.is_active() {
            return;
        }
        if self
            .language
            .as_ref()
            .and_then(|c| c.debug_adapter())
            .is_none()
        {
            return;
        }
        if self.debug.launch_args_draft.trim().is_empty() {
            self.debug.launch_args_draft = "{}".to_string();
        }
        self.debug.error = None;
        self.close_all_overlays();
        self.debug.show_launch_popup = true;
    }

    /// The launch popup's "Launch" entry point: parses `launch_args_draft`
    /// as raw JSON, rejecting (via `debug.error`, popup stays open) rather
    /// than sending anything on invalid JSON (`docs/features/
    /// tui-debugger.md` §2.5). No-op with no configured adapter -- already
    /// checked by `trigger_debug` before the popup could open, this is
    /// just the same defensive re-check `confirm_debug_adapter_config`'s
    /// empty-command check mirrors.
    fn confirm_debug_launch(&mut self) {
        let Some((command, args)) = self
            .language
            .as_ref()
            .and_then(|c| c.debug_adapter())
            .map(|(command, args)| (command.to_string(), args.to_vec()))
        else {
            return;
        };
        let arguments =
            match serde_json::from_str::<serde_json::Value>(&self.debug.launch_args_draft) {
                Ok(value) => value,
                Err(e) => {
                    self.debug.error = Some(format!("Invalid JSON: {e}"));
                    return;
                }
            };
        self.debug.show_launch_popup = false;
        let root = self.project_root.clone();
        self.debug.start_session(&command, &args, root, arguments);
    }

    fn handle_debug_launch_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.debug.show_launch_popup = false,
            KeyCode::Backspace => {
                self.debug.launch_args_draft.pop();
            }
            KeyCode::Enter => self.confirm_debug_launch(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.debug.launch_args_draft.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// `ToggleLineBreakpoint` (`docs/features/tui-debugger.md` §2.7):
    /// toggles a breakpoint on the active editor's current caret line.
    /// There is no gutter click to also wire this to (§2.4) -- the
    /// keyboard command is `ide-tui`'s only way to toggle a breakpoint.
    fn toggle_breakpoint_at_caret(&mut self) {
        let Some(buf) = self.active_buffer() else {
            return;
        };
        let path = buf.path.clone();
        let line = cursor_line_column(
            buf.buffer.text_buffer(),
            buf.buffer.text_buffer().selections().primary().head,
        )
        .0;
        self.debug.toggle_breakpoint(path, line as u32 + 1);
    }

    /// `ToggleDebugPanel` command: opens/closes the Debug tool window.
    /// Never resets `self.debug`'s own session/breakpoints/stack -- same
    /// "closing never resets state" convention `toggle_cargo_panel`/
    /// `toggle_git_panel` already establish. The browsing cursor
    /// (`debug_panel`) does reset on every open, since it has no meaning
    /// to preserve across a close.
    fn toggle_debug_panel(&mut self) {
        let opening = !self.debug_panel_open;
        self.close_all_overlays();
        self.debug_panel_open = opening;
        if opening {
            self.debug_panel = DebugPanelState::default();
        }
    }

    fn handle_debug_panel_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.debug_panel_open = false,
            KeyCode::Tab => self.debug_panel.focus = self.debug_panel.focus.next(),
            KeyCode::BackTab => self.debug_panel.focus = self.debug_panel.focus.previous(),
            KeyCode::Char('c') => self.debug.resume(),
            KeyCode::Char('o') => self.debug.step_over(),
            KeyCode::Char('i') => self.debug.step_into(),
            KeyCode::Char('u') => self.debug.step_out(),
            KeyCode::Char('p') => self.debug.pause(),
            KeyCode::Char('x') => self.debug.stop(),
            KeyCode::Up => match self.debug_panel.focus {
                DebugPanelFocus::Threads => {
                    self.debug_panel.thread_selected =
                        self.debug_panel.thread_selected.saturating_sub(1);
                }
                DebugPanelFocus::Stack => {
                    self.debug_panel.stack_selected =
                        self.debug_panel.stack_selected.saturating_sub(1);
                }
                // `output_scroll` counts lines held back from the tail
                // (0 = following the latest output) -- `Up` reveals older
                // lines, so it *increases* the hold-back amount.
                DebugPanelFocus::Output => {
                    self.debug_panel.output_scroll =
                        self.debug_panel.output_scroll.saturating_add(1);
                }
            },
            KeyCode::Down => match self.debug_panel.focus {
                DebugPanelFocus::Threads => {
                    if self.debug_panel.thread_selected + 1 < self.debug.threads.len() {
                        self.debug_panel.thread_selected += 1;
                    }
                }
                DebugPanelFocus::Stack => {
                    if self.debug_panel.stack_selected + 1 < self.debug.stack.len() {
                        self.debug_panel.stack_selected += 1;
                    }
                }
                // `Down` moves back toward the latest output, the
                // inverse of `Up` immediately above.
                DebugPanelFocus::Output => {
                    self.debug_panel.output_scroll =
                        self.debug_panel.output_scroll.saturating_sub(1);
                }
            },
            // Only meaningful for the Output section (`docs/features/
            // tui-debugger.md` §2.6) -- gated on focus so paging doesn't
            // silently scroll a pane that isn't even highlighted while
            // Threads/Stack has focus.
            KeyCode::PageUp if self.debug_panel.focus == DebugPanelFocus::Output => {
                self.debug_panel.output_scroll = self.debug_panel.output_scroll.saturating_add(10);
            }
            KeyCode::PageDown if self.debug_panel.focus == DebugPanelFocus::Output => {
                self.debug_panel.output_scroll = self.debug_panel.output_scroll.saturating_sub(10);
            }
            KeyCode::Enter => self.confirm_debug_panel_selection(),
            _ => {}
        }
        LoopSignal::Continue
    }

    /// `Enter` inside the Debug tool window: `Threads` commits the
    /// highlighted row as the active thread (`DebugPanel::select_thread`);
    /// `Stack` navigates to the highlighted frame's source, if it has one
    /// (a frame with `source: None` is shown but `Enter` no-ops on it,
    /// `docs/features/tui-debugger.md` §2.6); `Output` has nothing to
    /// confirm.
    fn confirm_debug_panel_selection(&mut self) {
        match self.debug_panel.focus {
            DebugPanelFocus::Threads => {
                if let Some(thread) = self.debug.threads.get(self.debug_panel.thread_selected) {
                    self.debug.select_thread(thread.id);
                }
            }
            DebugPanelFocus::Stack => {
                if let Some(frame) = self
                    .debug
                    .stack
                    .get(self.debug_panel.stack_selected)
                    .cloned()
                {
                    if let Some(path) = frame.source {
                        self.open_stack_frame(path, frame.line, frame.column);
                    }
                }
            }
            DebugPanelFocus::Output => {}
        }
    }

    /// Opens `path` (a `StackFrame::source` -- already canonicalized and
    /// checked against `project_root` by `ide-dap`, `debugger.md` §3.6)
    /// and best-effort places the cursor at `line`/`column`. DAP reports
    /// both 1-based, unlike `ide_lsp::Position`'s 0-based convention
    /// `open_location` assumes -- same conversion `ide-ui`'s
    /// `open_stack_frame` already does.
    fn open_stack_frame(&mut self, path: PathBuf, line: u32, column: u32) {
        let position = Position {
            line: line.saturating_sub(1),
            character: column.saturating_sub(1),
        };
        self.open_location(Location {
            path,
            range: ide_lsp::Range {
                start: position,
                end: position,
            },
        });
    }

    /// `ConfigureDebugAdapter` command (`docs/features/tui-debugger.md`
    /// §2.5): opens the two-field popup, pre-filled from `self.language`'s
    /// current debug-adapter fields (or empty if `self.language` is
    /// `None` or has no adapter configured yet).
    fn toggle_debug_adapter_config_popup(&mut self) {
        let opening = self.debug_adapter_config_popup.is_none();
        let (command, args) = self
            .language
            .as_ref()
            .map(|lang| {
                (
                    lang.debug_adapter_command.clone().unwrap_or_default(),
                    lang.debug_adapter_args.join(" "),
                )
            })
            .unwrap_or_default();
        self.close_all_overlays();
        if opening {
            self.debug_adapter_config_popup = Some(DebugAdapterConfigPopupState {
                command,
                args,
                field: DebugConfigField::Command,
            });
        }
    }

    fn handle_debug_adapter_config_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.debug_adapter_config_popup.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.debug_adapter_config_popup = None,
            KeyCode::Tab | KeyCode::BackTab => {
                state.field = match state.field {
                    DebugConfigField::Command => DebugConfigField::Args,
                    DebugConfigField::Args => DebugConfigField::Command,
                };
            }
            KeyCode::Backspace => match state.field {
                DebugConfigField::Command => {
                    state.command.pop();
                }
                DebugConfigField::Args => {
                    state.args.pop();
                }
            },
            KeyCode::Enter => self.confirm_debug_adapter_config(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match state.field {
                    DebugConfigField::Command => state.command.push(c),
                    DebugConfigField::Args => state.args.push(c),
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Validates and saves the "Configure Debug Adapter" popup
    /// (`docs/features/tui-debugger.md` §2.5): trims `command`, rejecting
    /// (via `notify`, popup stays open) if empty; splits `args` on
    /// whitespace. On success, updates `self.language`'s debug-adapter
    /// fields in place, persists the override keyed by language name, and
    /// closes the popup.
    fn confirm_debug_adapter_config(&mut self) {
        let Some(state) = self.debug_adapter_config_popup.as_ref() else {
            return;
        };
        let command = state.command.trim().to_string();
        if command.is_empty() {
            self.notify("Debug adapter command cannot be empty.");
            return;
        }
        let Some(lang) = self.language.as_mut() else {
            self.notify("No detected project language to configure a debug adapter for.");
            return;
        };
        let args: Vec<String> = state.args.split_whitespace().map(str::to_string).collect();
        lang.debug_adapter_command = Some(command.clone());
        lang.debug_adapter_args = args.clone();
        let name = lang.name.clone();
        self.debug_adapters
            .adapters
            .insert(name, DebugAdapterEntry { command, args });
        debug_config::save(&self.debug_adapters);
        self.debug_adapter_config_popup = None;
    }

    /// Both slices `ui.rs`'s `render_editor` folds into `LineOverlays`
    /// (`docs/features/tui-debugger.md` §2.4): one whole-line byte range
    /// per breakpoint on `path`, split by the adapter's `verified` status
    /// (defaulting to `true`, i.e. solid, when no confirmation has arrived
    /// yet -- same resolved default `ide-ui`'s own
    /// `breakpoint_marks_for_active_tab` uses).
    pub(crate) fn breakpoint_line_ranges(
        &self,
        path: &Path,
        text_buffer: &TextBuffer,
    ) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
        let mut verified_ranges = Vec::new();
        let mut unverified_ranges = Vec::new();
        let Some(lines) = self.debug.breakpoints.get(path) else {
            return (verified_ranges, unverified_ranges);
        };
        let confirmed = self.debug.confirmed_breakpoints.get(path);
        let text = text_buffer.text();
        for &dap_line in lines {
            let line = (dap_line as usize).saturating_sub(1);
            let Some(range) = text_buffer.lines().line_range(line, text) else {
                continue;
            };
            let verified = confirmed
                .and_then(|c| c.iter().find(|v| v.line == dap_line))
                .map(|v| v.verified)
                .unwrap_or(true);
            if verified {
                verified_ranges.push(range);
            } else {
                unverified_ranges.push(range);
            }
        }
        (verified_ranges, unverified_ranges)
    }

    /// Called once per frame (`lib.rs`'s main loop), mirrors `set_editor_
    /// viewport_rows`. `(rows, cols)` is the terminal grid size implied by
    /// the current terminal dimensions, computed by `ui::claude_terminal_
    /// grid_size` -- the single source of truth also used when actually
    /// rendering the grid, so the two never drift apart. A no-op unless
    /// the active terminal tab's current grid size actually differs (`docs/
    /// features/tui-claude-panel.md` §3.3's "cheap integer comparison").
    pub fn sync_claude_terminal_size(&mut self, rows: u16, cols: u16) {
        if let Some(tab) = self.claude_terminals.active_tab_mut() {
            if tab.grid().rows() != rows as usize || tab.grid().cols() != cols as usize {
                tab.resize(rows, cols);
            }
        }
    }

    /// `ide-tui`'s first tree-refresh action of any kind (§1 of the doc --
    /// there was none before this phase). Silently no-ops if the project
    /// root itself has become unopenable in the meantime; the next
    /// successful event still retries.
    fn refresh_tree(&mut self) {
        if let Ok(project) = Project::open(&self.project_root) {
            self.tree = project.scan_tree();
        }
    }

    /// §3.2: a clean tab reloads silently (nothing of the user's to
    /// lose); a dirty tab gets `external_change = Some(Modified)`,
    /// surfaced via the tab strip and a notification, left for
    /// `ReloadFromDisk`/`DismissExternalChange` to resolve.
    fn handle_external_modification(&mut self, path: &Path) {
        let Some(idx) = self.tabs.iter().position(|t| t.path == path) else {
            return;
        };
        if self.tabs[idx].buffer.is_dirty() {
            self.tabs[idx].external_change = Some(ExternalChange::Modified);
            self.notify(format!(
                "{} changed on disk (unsaved edits here) -- Reload or Keep Mine from the palette.",
                path.display()
            ));
        } else {
            self.reload_tab_from_disk(idx);
            self.notify(format!("{} reloaded (changed on disk).", path.display()));
        }
    }

    /// §3.3: regardless of dirty state -- unlike a content change, there's
    /// no "nothing to lose" case for a file that's simply gone.
    fn handle_external_removal(&mut self, path: &Path) {
        if let Some(idx) = self.tabs.iter().position(|t| t.path == path) {
            self.tabs[idx].external_change = Some(ExternalChange::Deleted);
            self.notify(format!("{} was deleted on disk.", path.display()));
        }
    }

    /// Shared by the silent auto-reload (`handle_external_modification`,
    /// any tab) and the explicit `ReloadFromDisk` action (always the
    /// active tab). A read failure notifies rather than panicking.
    fn reload_tab_from_disk(&mut self, idx: usize) {
        let path = self.tabs[idx].path.clone();
        match Buffer::open(&path) {
            Ok(mut buffer) => {
                buffer.set_syntax(syntax_for_path(&path));
                self.tabs[idx].buffer = buffer;
                self.tabs[idx].desired_column = None;
                self.tabs[idx].external_change = None;
                self.refresh_blame_if_on(idx);
            }
            Err(err) => self.notify(err.to_string()),
        }
    }

    /// `ReloadFromDisk` command target (palette-only, §1.1). Discards
    /// unsaved edits and undo history, replacing the buffer with what's on
    /// disk now -- same as closing and reopening the tab would.
    fn reload_active_from_disk(&mut self) {
        if let Some(idx) = self.active_tab {
            self.reload_tab_from_disk(idx);
        }
    }

    /// `DismissExternalChange` command target (palette-only, §1.1):
    /// clears `external_change` without touching the buffer -- the next
    /// save still overwrites disk, exactly as it already would.
    fn dismiss_external_change(&mut self) {
        if let Some(idx) = self.active_tab {
            self.tabs[idx].external_change = None;
        }
    }

    /// Ported near-verbatim from `ide-ui`'s own helper of the same name
    /// (`docs/features/tui-file-watcher.md` §2.2's "Path identity"
    /// invariant): every `OpenBuffer::path` must be canonical so a
    /// `WatchEvent`'s already-canonical path matches it by plain equality.
    /// Falls back to the parent directory's canonical form joined with the
    /// file name (a path that doesn't exist yet), then to the raw path
    /// unchanged if even the parent can't be resolved -- lets the normal
    /// `Buffer::open` failure path surface instead of this silently
    /// swallowing the problem.
    fn canonicalize_best_effort(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| {
            let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
                return path.to_path_buf();
            };
            std::fs::canonicalize(parent)
                .map(|p| p.join(file_name))
                .unwrap_or_else(|_| path.to_path_buf())
        })
    }

    /// For `title == "Declaration"`'s single-result case, the jump is
    /// deferred to `handle_interface_check_response` instead of happening
    /// here directly (`docs/features/goto-declaration-interface-
    /// redirect.md` §2.3) -- `"Usages"` never takes this branch, and
    /// `title == "Implementation"` (the redirect's own re-query, labelled
    /// by `poll_lsp` via `expect_implementation_next`) falls through to the
    /// plain jump below same as `"Usages"` does.
    fn handle_goto_results(&mut self, title: &'static str, mut results: Vec<Location>) {
        match results.len() {
            0 => self.notify(format!("{title}: no results.")),
            1 if title == "Declaration" => {
                let location = results.remove(0);
                self.pending_interface_check = Some(location.clone());
                self.lsp.request_document_symbols(&location.path);
            }
            1 => {
                let location = results.remove(0);
                self.jump_to_goto_result(title, location);
            }
            n => {
                self.notify(format!("{title}: {n} results."));
                self.close_all_overlays();
                self.goto = Some(GotoState {
                    title,
                    results,
                    selected: 0,
                });
            }
        }
    }

    /// Shared "jump immediately, no popup" tail of `handle_goto_results`'s
    /// single-result case and `handle_interface_check_response`'s own
    /// resolution.
    fn jump_to_goto_result(&mut self, title: &'static str, location: Location) {
        let line = location.range.start.line + 1;
        let file = location
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| location.path.display().to_string());
        self.notify(format!("{title}: jumped to {file}:{line}"));
        self.open_location(location);
    }

    /// Called once per frame by `poll_lsp`, right after the `goto_ready`/
    /// `references_ready` dispatch. No-op unless `self.lsp.
    /// document_symbols_ready` and `pending_interface_check` is `Some` and
    /// the fresh response's path matches the pending location's -- a
    /// `DocumentSymbol` answer for some other file (e.g. a stale in-flight
    /// Go to Symbol query) is left untouched for its own intended
    /// consumer, and this check's own request stays outstanding until its
    /// own matching response arrives (`docs/features/
    /// goto-declaration-interface-redirect.md` §2.3).
    fn handle_interface_check_response(&mut self) {
        if !self.lsp.document_symbols_ready {
            return;
        }
        let Some(location) = self.pending_interface_check.clone() else {
            return;
        };
        if self.lsp.document_symbols_path.as_deref() != Some(location.path.as_path()) {
            return;
        }
        self.pending_interface_check = None;
        let redirect =
            ide_lsp::position_is_within_interface(&self.lsp.document_symbols, location.range.start);
        if redirect {
            if let Some((origin_path, origin_position)) = self.goto_declaration_origin.clone() {
                self.expect_implementation_next = true;
                self.lsp.go_to_implementation(&origin_path, origin_position);
                return;
            }
        }
        self.jump_to_goto_result("Declaration", location);
    }

    /// Opens `location.path` (a picker row or the sole Goto/Find Usages
    /// result -- never from Claude panel text or any other untrusted
    /// source, this crate's only LSP-response consumer) and best-effort
    /// places the cursor at its range's start, top-aligning the same way
    /// `jump_to_match` does. A path the language server named but that no
    /// longer opens (deleted since indexing, a permission error) surfaces
    /// as a notification rather than panicking.
    fn open_location(&mut self, location: Location) {
        if let Err(err) = self.open_or_focus_tab(location.path) {
            self.notify(err.to_string());
            return;
        }
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let Some(offset) =
            ide_lsp::position_to_byte_offset(buf.buffer.text(), location.range.start)
        else {
            return;
        };
        let (line, _) = cursor_line_column(buf.buffer.text_buffer(), offset);
        Self::scroll_to_and_reveal(buf, line);
        buf.desired_column = None;
        buf.buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(offset)));
        self.push_nav_location(offset);
    }

    /// `Ctrl+B` entry point (`docs/features/tui-goto-and-usages.md` §2.3).
    /// No-op with no active tab or no running language server (`lsp.
    /// go_to_definition`'s own gating) -- clears any previously open
    /// picker first, so a second gesture fired while one from an earlier
    /// query is still open closes it rather than leaving stale rows under
    /// a new query.
    fn trigger_go_to_declaration(&mut self) {
        self.goto = None;
        self.pending_interface_check = None;
        if !self.lsp.is_running() {
            self.notify("No language server running.");
            self.goto_declaration_origin = None;
            return;
        }
        if let Some((path, position)) = self.lsp_query_target() {
            self.goto_declaration_origin = Some((path.clone(), position));
            self.lsp.go_to_definition(&path, position);
        } else {
            self.goto_declaration_origin = None;
        }
    }

    /// `Ctrl+U` entry point. Same shape as `trigger_go_to_declaration`.
    fn trigger_find_usages(&mut self) {
        self.goto = None;
        if !self.lsp.is_running() {
            self.notify("No language server running.");
            return;
        }
        if let Some((path, position)) = self.lsp_query_target() {
            self.lsp.find_references(&path, position);
        }
    }

    /// Appends a new, unread entry to `App::notifications` -- an in-app
    /// log, not a desktop notification (`docs/features/
    /// tui-goto-and-usages.md` §2.4).
    fn notify(&mut self, message: impl Into<String>) {
        self.notifications.push(Notification {
            message: message.into(),
            read: false,
        });
    }

    pub(crate) fn unread_notification_count(&self) -> usize {
        self.notifications.iter().filter(|n| !n.read).count()
    }

    /// Closes every overlay panel (Goto picker, Notifications, Problems,
    /// Cargo, Hover, Find in Path, Code Actions, Rename popup, Rename
    /// preview) -- called before opening any one of them, so at most one is
    /// ever open at a time (`docs/features/tui-problems.md` §4, extended by
    /// `docs/features/tui-cargo-panel.md` §4, `docs/features/
    /// tui-hover-and-inlay-hints.md` §2.2, `docs/features/
    /// tui-find-in-path.md` §3.1, and `docs/features/
    /// tui-code-actions-and-rename.md` §2.3/§4). Does not touch
    /// `find`/`palette`, which sit at an outer interception tier in
    /// `handle_key` and are never open at the same time as one of these
    /// nine in the first place. Closing the Cargo panel or the Find in Path
    /// panel this way never stops a running command/search -- only the
    /// `_open` flag changes, `self.cargo`/`self.search` themselves are
    /// untouched.
    fn close_all_overlays(&mut self) {
        self.goto = None;
        self.notifications_open = false;
        self.problems = None;
        self.cargo_panel_open = false;
        self.hover_open = false;
        self.search_open = false;
        self.go_to_file = None;
        self.go_to_symbol = None;
        self.code_actions = None;
        self.rename_popup = None;
        self.pending_rename_preview = None;
        self.git_panel = None;
        self.docker_panel = None;
        self.k8s_panel = None;
        self.recent_files = None;
        self.bookmarks_popup = None;
        self.todo_panel = None;
        self.keymap_popup = None;
        self.new_scratch_file = None;
        self.scratch_files = None;
        self.claude_panel_open = false;
        self.new_claude_terminal = None;
        self.debug_panel_open = false;
        self.debug_adapter_config_popup = None;
        self.debug.show_launch_popup = false;
    }

    /// `ToggleClaudePanel` command (palette-only, no default binding --
    /// see `commands.rs`): opens/closes the Claude chat + terminal panel.
    /// Never resets `claude_view`/`claude_terminal_focus`/`claude`/
    /// `claude_terminals` -- closing hides the panel, same "closing
    /// never resets state" convention `toggle_cargo_panel`/
    /// `toggle_search_panel` already established.
    fn toggle_claude_panel(&mut self) {
        let opening = !self.claude_panel_open;
        self.close_all_overlays();
        self.claude_panel_open = opening;
    }

    /// Handles every key while `claude_panel_open` (`docs/features/
    /// tui-claude-panel.md` §3.1-§3.4). `new_claude_terminal`'s own popup
    /// is checked first (it's a nested overlay on top of this one, same
    /// shape `new_scratch_file`/`scratch_files` already are relative to
    /// each other).
    fn handle_claude_panel_key(&mut self, key: KeyEvent) -> LoopSignal {
        if self.new_claude_terminal.is_some() {
            return self.handle_new_claude_terminal_key(key);
        }
        if let ClaudeView::Terminal(idx) = self.claude_view {
            if self.claude_terminal_focus {
                return self.handle_claude_terminal_raw_key(key, idx);
            }
        }
        match key.code {
            KeyCode::Esc => self.claude_panel_open = false,
            KeyCode::Tab => self.cycle_claude_view(1),
            KeyCode::BackTab => self.cycle_claude_view(-1),
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.new_claude_terminal = Some(NewClaudeTerminalState {
                    name: String::new(),
                });
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.close_active_claude_terminal();
            }
            KeyCode::Enter if matches!(self.claude_view, ClaudeView::Terminal(_)) => {
                self.claude_terminal_focus = true;
            }
            _ => match self.claude_view {
                ClaudeView::Chat => self.handle_claude_chat_key(key),
                ClaudeView::Terminal(_) => {}
            },
        }
        LoopSignal::Continue
    }

    /// `Chat` view's text field -- same shape as `NewScratchFile`'s.
    fn handle_claude_chat_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Backspace => {
                self.claude.input.pop();
            }
            KeyCode::Enter => {
                let prompt = std::mem::take(&mut self.claude.input);
                self.claude.submit(prompt);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.claude.input.push(c);
            }
            _ => {}
        }
    }

    /// `Tab`/`Shift+Tab`: cycles `Chat, Terminal(0), Terminal(1), ..`
    /// forward/backward, wrapping. A no-op (always `Chat`) with zero
    /// terminal tabs open. Always resets to chrome mode.
    fn cycle_claude_view(&mut self, direction: i32) {
        self.claude_terminal_focus = false;
        let tab_count = self.claude_terminals.tabs().len();
        let views = tab_count + 1; // Chat, plus one entry per terminal tab
        let current = match self.claude_view {
            ClaudeView::Chat => 0,
            ClaudeView::Terminal(idx) => idx + 1,
        };
        let next = (current as i32 + direction).rem_euclid(views as i32) as usize;
        self.claude_view = if next == 0 {
            ClaudeView::Chat
        } else {
            let idx = next - 1;
            self.claude_terminals.active = Some(idx);
            ClaudeView::Terminal(idx)
        };
    }

    /// `Ctrl+W`: closes the active terminal tab. A no-op in `Chat` view or
    /// with no terminal tabs open. Resets to `Chat` if that was the last
    /// terminal tab.
    fn close_active_claude_terminal(&mut self) {
        let ClaudeView::Terminal(idx) = self.claude_view else {
            return;
        };
        self.claude_terminals.close_tab(idx);
        self.claude_terminal_focus = false;
        self.claude_view = match self.claude_terminals.active {
            Some(active) => ClaudeView::Terminal(active),
            None => ClaudeView::Chat,
        };
    }

    /// Every key while `claude_terminal_focus` is true (`docs/features/
    /// tui-claude-panel.md` §3.4): `Shift+Esc` always exits to chrome
    /// mode without forwarding anything (checked first, since it would
    /// otherwise be indistinguishable from plain `Esc` once `Shift`'s
    /// case-folding is accounted for -- see `claude_terminal::
    /// key_event_to_bytes`'s own doc on `Ctrl+Shift` collisions, the same
    /// reasoning applies here). Everything else goes through
    /// `key_event_to_bytes` and is written to the tab's PTY.
    fn handle_claude_terminal_raw_key(&mut self, key: KeyEvent, _idx: usize) -> LoopSignal {
        if key.code == KeyCode::Esc && key.modifiers.contains(KeyModifiers::SHIFT) {
            self.claude_terminal_focus = false;
            return LoopSignal::Continue;
        }
        if let Some(bytes) = claude_terminal::key_event_to_bytes(key) {
            if let Some(tab) = self.claude_terminals.active_tab_mut() {
                let _ = tab.write(&bytes);
            }
        }
        LoopSignal::Continue
    }

    /// Handles every key while `new_claude_terminal` is open -- same shape
    /// as `handle_new_scratch_file_key`.
    fn handle_new_claude_terminal_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.new_claude_terminal.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.new_claude_terminal = None,
            KeyCode::Backspace => {
                state.name.pop();
            }
            KeyCode::Enter => self.confirm_new_claude_terminal(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.name.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Blank => `project_root`; a typed relative path is joined onto it;
    /// an absolute path is used as-is. Pure and side-effect-free on
    /// purpose (`docs/features/tui-claude-panel.md` §3.1) -- kept
    /// separate from `confirm_new_claude_terminal` so it's unit-testable
    /// without ever calling `open_tab`/`PtySession::spawn`, which this
    /// crate's tests must never do against a directory that actually
    /// exists (it would try to launch the real `claude` CLI).
    fn resolve_claude_terminal_dir(&self, typed: &str) -> PathBuf {
        let typed = typed.trim();
        if typed.is_empty() {
            return self.project_root.clone();
        }
        let typed_path = Path::new(typed);
        if typed_path.is_absolute() {
            typed_path.to_path_buf()
        } else {
            self.project_root.join(typed_path)
        }
    }

    /// Never fails, matching `ClaudeTerminalPanel::open_tab`'s own
    /// contract (`docs/features/tui-claude-panel.md` §2.2/§3.1): an
    /// invalid directory (typo, deleted since being typed) still creates
    /// a tab, `exited: true`, with the error shown inline in its grid --
    /// deliberately not re-validated here, since duplicating that check
    /// at this layer would only produce a cruder, popup-stays-open error
    /// instead of the richer behaviour `open_tab` already provides.
    fn confirm_new_claude_terminal(&mut self) {
        let Some(state) = self.new_claude_terminal.as_ref() else {
            return;
        };
        let dir = self.resolve_claude_terminal_dir(&state.name);
        self.new_claude_terminal = None;
        let (rows, cols) = self
            .claude_terminals
            .active_tab()
            .map(|tab| (tab.grid().rows() as u16, tab.grid().cols() as u16))
            .unwrap_or((24, 80));
        self.claude_terminals.open_tab(dir, rows, cols);
        if let Some(active) = self.claude_terminals.active {
            self.claude_view = ClaudeView::Terminal(active);
            self.claude_terminal_focus = true;
        }
    }

    /// `ToggleNotifications` command (palette-only, no default binding --
    /// see `commands.rs`): opens/closes the notification log panel.
    fn toggle_notifications(&mut self) {
        let opening = !self.notifications_open;
        self.close_all_overlays();
        self.notifications_open = opening;
    }

    /// Handles every key while `notifications_open` -- intercepts all
    /// input the same way `handle_goto_key`/`handle_palette_key` do.
    /// `c`/`r` are plain, unmodified letters (this panel has no text
    /// query to type into, unlike find/palette, so there's no ambiguity
    /// to guard against the way `handle_find_key` must for `Ctrl`-held
    /// letters).
    fn handle_notifications_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.notifications_open = false,
            KeyCode::Char('c') => self.notifications.clear(),
            KeyCode::Char('r') => {
                for n in &mut self.notifications {
                    n.read = true;
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Every current diagnostic across every file, flattened out of
    /// `lsp.diagnostics`'s per-file `HashMap` and sorted by path then
    /// range start -- a stable, deterministic order for the Problems
    /// panel's row indices to mean the same thing across two consecutive
    /// calls, since a `HashMap`'s own iteration order isn't stable
    /// (`docs/features/tui-problems.md` §2.2).
    pub(crate) fn flattened_diagnostics(&self) -> Vec<(&PathBuf, &Diagnostic)> {
        let mut rows: Vec<(&PathBuf, &Diagnostic)> = self
            .lsp
            .diagnostics
            .iter()
            .flat_map(|(path, diags)| diags.iter().map(move |d| (path, d)))
            .collect();
        rows.sort_by(|(a_path, a), (b_path, b)| {
            a_path
                .cmp(b_path)
                .then(a.range.start.line.cmp(&b.range.start.line))
                .then(a.range.start.character.cmp(&b.range.start.character))
        });
        rows
    }

    /// The active tab's raw, `Position`-based semantic tokens, or `&[]`
    /// with no active tab or no entry yet (`docs/features/
    /// tui-semantic-highlighting.md` §2.2) -- `render_editor` converts
    /// this to absolute byte ranges once per frame via `highlight::
    /// semantic_token_marks`.
    pub(crate) fn active_semantic_tokens(&self) -> &[ide_lsp::SemanticToken] {
        self.active_buffer()
            .and_then(|buf| self.lsp.semantic_tokens.get(&buf.path))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// `ToggleProblems` command (`Ctrl+P`, see `commands.rs`).
    fn toggle_problems(&mut self) {
        let opening = self.problems.is_none();
        self.close_all_overlays();
        if opening {
            self.problems = Some(ProblemsState { selected: 0 });
        }
    }

    /// Handles every key while `problems.is_some()` -- same interception
    /// shape as `handle_goto_key`. `Enter` opens the selected diagnostic's
    /// file and jumps to its range's start via `open_location`, reusing
    /// exactly the Goto/Find Usages jump logic (a `Diagnostic` plus its
    /// owning path is the same `path` + `range` shape a `Location` is).
    fn handle_problems_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(problems) = self.problems.as_ref() else {
            return LoopSignal::Continue;
        };
        let row_count = self.flattened_diagnostics().len();
        match key.code {
            KeyCode::Esc => {
                self.problems = None;
            }
            KeyCode::Up => {
                if let Some(problems) = self.problems.as_mut() {
                    if problems.selected > 0 {
                        problems.selected -= 1;
                    }
                }
            }
            KeyCode::Down => {
                if let Some(problems) = self.problems.as_mut() {
                    if problems.selected + 1 < row_count {
                        problems.selected += 1;
                    }
                }
            }
            KeyCode::Enter => {
                let selected = problems.selected;
                let location = self
                    .flattened_diagnostics()
                    .get(selected)
                    .map(|(path, diag)| Location {
                        path: (*path).clone(),
                        range: diag.range,
                    });
                self.problems = None;
                if let Some(location) = location {
                    self.open_location(location);
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// `ToggleCargoPanel` command (palette-only, no default binding -- see
    /// `commands.rs`): opens/closes the Cargo output panel. Never touches
    /// `self.cargo` itself -- closing hides the panel, it never stops a
    /// running command (`docs/features/tui-cargo-panel.md` §3).
    fn toggle_cargo_panel(&mut self) {
        let opening = !self.cargo_panel_open;
        self.close_all_overlays();
        self.cargo_panel_open = opening;
    }

    /// Handles every key while `cargo_panel_open` -- same interception
    /// shape as `handle_notifications_key`: plain, unmodified letters,
    /// since this panel has no text query to type into. Each of the six
    /// letters starts the matching `cargo` subcommand in `project_root`;
    /// `CargoPanel::run` itself is the sole guard against overlapping runs
    /// (no-op while one is already in flight).
    fn handle_cargo_panel_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.cargo_panel_open = false,
            KeyCode::Char('b') => self.cargo.run(&self.project_root, CargoCommand::Build),
            KeyCode::Char('r') => self.cargo.run(&self.project_root, CargoCommand::Run),
            KeyCode::Char('t') => self.cargo.run(&self.project_root, CargoCommand::Test),
            KeyCode::Char('c') => self.cargo.run(&self.project_root, CargoCommand::Check),
            KeyCode::Char('l') => self.cargo.run(&self.project_root, CargoCommand::Clippy),
            KeyCode::Char('f') => self.cargo.run(&self.project_root, CargoCommand::Fmt),
            _ => {}
        }
        LoopSignal::Continue
    }

    /// `Ctrl+Shift+F` command: opens/closes the Find in Path panel. Never
    /// touches `self.search`/`self.search_state` -- closing hides the
    /// panel; it never stops a running search or clears the typed query/
    /// results, so reopening shows exactly what was last there
    /// (`docs/features/tui-find-in-path.md` §3.1).
    fn toggle_search_panel(&mut self) {
        let opening = !self.search_open;
        self.close_all_overlays();
        self.search_open = opening;
    }

    /// Handles every key while `search_open` (`docs/features/
    /// tui-find-in-path.md` §2.2). Typing/Backspace edit the query freely
    /// at any time; `Up`/`Down` move the row selection, clamped to
    /// whatever `self.search.results` currently holds; `Enter` defers to
    /// `submit_or_open_search_result`'s submit-vs-open disambiguation.
    fn handle_search_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.search_open = false,
            KeyCode::Up => {
                if self.search_state.selected > 0 {
                    self.search_state.selected -= 1;
                }
            }
            KeyCode::Down => {
                let len = self
                    .search
                    .results
                    .as_ref()
                    .map(|r| r.matches.len())
                    .unwrap_or(0);
                if self.search_state.selected + 1 < len {
                    self.search_state.selected += 1;
                }
            }
            KeyCode::Backspace => {
                self.search_state.query.pop();
            }
            KeyCode::Enter => self.submit_or_open_search_result(),
            // Any other `Ctrl`-held combo falls through to the wildcard
            // arm below (ignored), never typed into the query -- same
            // guard `handle_find_key`'s own query-typing arm uses.
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_state.query.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// `Enter` while `search_open` (`docs/features/tui-find-in-path.md`
    /// §2.2). No-op while a search is already in flight. Otherwise: an
    /// empty (trimmed) query is a no-op, matching `ide-ui`'s own
    /// `run_search` guard; a query that differs from `search_state.
    /// ran_query` (or nothing has run yet) starts a fresh search and
    /// resets the row selection; an unchanged query opens the currently
    /// selected row (if any) and closes the panel.
    fn submit_or_open_search_result(&mut self) {
        if self.search.searching {
            return;
        }
        let query = self.search_state.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        if Some(&query) != self.search_state.ran_query.as_ref() {
            self.search.run(self.tree.clone(), query.clone());
            self.search_state.ran_query = Some(query);
            self.search_state.selected = 0;
            return;
        }
        let Some(results) = &self.search.results else {
            return;
        };
        let Some(m) = results.matches.get(self.search_state.selected) else {
            return;
        };
        let path = m.path.clone();
        let byte_offset = m.byte_offset;
        self.open_search_result(path, byte_offset);
        self.search_open = false;
    }

    /// Same shape as `open_location` but starting from an already-absolute
    /// byte offset -- a `SearchMatch` carries one directly, unlike a
    /// `Location`'s LSP `Position`, so there's no `position_to_byte_offset`
    /// conversion step (`docs/features/tui-find-in-path.md` §2.2).
    /// `Selection::caret` clamps internally, so a `byte_offset` stale
    /// against a file that changed on disk since the search ran can't
    /// panic -- it just lands somewhere sane.
    fn open_search_result(&mut self, path: PathBuf, byte_offset: usize) {
        if let Err(err) = self.open_or_focus_tab(path) {
            self.notify(err.to_string());
            return;
        }
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let (line, _) = cursor_line_column(buf.buffer.text_buffer(), byte_offset);
        Self::scroll_to_and_reveal(buf, line);
        buf.desired_column = None;
        buf.buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(byte_offset)));
        self.push_nav_location(byte_offset);
    }

    /// `Ctrl+Shift+N` entry point (`docs/features/
    /// tui-go-to-file-and-symbol.md` §3.1). Resets `query`/`selected`/
    /// `ran_query` on open, matching `toggle_search_panel`'s own reset.
    fn toggle_go_to_file(&mut self) {
        let opening = self.go_to_file.is_none();
        self.close_all_overlays();
        if opening {
            self.go_to_file = Some(GoToFileState::default());
        }
    }

    /// Handles every key while `go_to_file.is_some()` (§3.1). Typing/
    /// Backspace edit the query freely; `Up`/`Down` move the row
    /// selection, clamped to the current result count; `Enter` defers to
    /// `confirm_go_to_file`.
    fn handle_go_to_file_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.go_to_file.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.go_to_file = None,
            KeyCode::Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            KeyCode::Down => {
                let len = self
                    .files_search
                    .results
                    .as_ref()
                    .map(|r| r.matches.len())
                    .unwrap_or(0);
                let state = self.go_to_file.as_mut().unwrap();
                if state.selected + 1 < len {
                    state.selected += 1;
                }
            }
            KeyCode::Backspace => {
                state.query.pop();
            }
            KeyCode::Enter => self.confirm_go_to_file(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.query.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Opens the selected match at offset 0 -- a fuzzy file match carries
    /// no target position, unlike a search/symbol match (JetBrains' own
    /// Go to File also just opens at the top). No-op if nothing is
    /// selected (e.g. an empty or still-running query).
    fn confirm_go_to_file(&mut self) {
        let Some(state) = self.go_to_file.as_ref() else {
            return;
        };
        let Some(results) = &self.files_search.results else {
            return;
        };
        let Some(m) = results.matches.get(state.selected) else {
            return;
        };
        let path = m.path.clone();
        if let Err(err) = self.open_or_focus_tab(path) {
            self.notify(err.to_string());
            return;
        }
        if let Some(buf) = self.active_buffer_mut() {
            buf.desired_column = None;
            buf.buffer
                .text_buffer_mut()
                .set_selections(Selections::single(Selection::caret(0)));
        }
        self.push_nav_location(0);
        self.go_to_file = None;
    }

    /// Called once per frame (`lib.rs`'s main loop), right alongside
    /// `sync_document_highlights` (§3.1). No-op unless `go_to_file.
    /// is_some()`. An empty (trimmed) query never runs a search --
    /// `fuzzy_match_files` already returns nothing for one, so running
    /// one would only cost a needless background thread + tree scan.
    /// Otherwise, if the trimmed query differs from `ran_query` and no
    /// search is currently in flight, starts a fresh one -- a **live**
    /// refresh, unlike Find in Path's submit-based `Enter` (§2.3's own
    /// doc comment on `GoToFileState::ran_query`).
    pub(crate) fn sync_go_to_file(&mut self) {
        let Some(state) = self.go_to_file.as_mut() else {
            return;
        };
        let query = state.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        if self.files_search.searching {
            return;
        }
        if Some(&query) == state.ran_query.as_ref() {
            return;
        }
        self.files_search.run(self.tree.clone(), query.clone());
        state.ran_query = Some(query);
        // A new result set is on the way -- reset the row selection so it
        // can never point past a shorter list than whatever `selected` was
        // left at by the previous query (same reset
        // `submit_or_open_search_result` already applies for the same
        // reason).
        state.selected = 0;
    }

    /// `Ctrl+Alt+Shift+N` entry point (`docs/features/
    /// tui-go-to-file-and-symbol.md` §3.2).
    fn toggle_go_to_symbol(&mut self) {
        let opening = self.go_to_symbol.is_none();
        self.close_all_overlays();
        if opening {
            self.go_to_symbol = Some(GoToSymbolState::default());
        }
    }

    /// Handles every key while `go_to_symbol.is_some()` (§3.2). Same shape
    /// as `handle_go_to_file_key`.
    fn handle_go_to_symbol_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.go_to_symbol.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.go_to_symbol = None,
            KeyCode::Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            KeyCode::Down => {
                let len = self.go_to_symbol_rows().len();
                let state = self.go_to_symbol.as_mut().unwrap();
                if state.selected + 1 < len {
                    state.selected += 1;
                }
            }
            KeyCode::Backspace => {
                state.query.pop();
            }
            KeyCode::Enter => self.confirm_go_to_symbol(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.query.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// The rows `go_to_symbol` currently has to show (§3.2): the active
    /// tab's own outline (`lsp.document_symbols`) for an empty query,
    /// `lsp.workspace_symbols` (already server-ranked, never re-sorted
    /// client-side) for a non-empty one.
    pub(crate) fn go_to_symbol_rows(&self) -> &[Symbol] {
        let Some(state) = &self.go_to_symbol else {
            return &[];
        };
        if state.query.trim().is_empty() {
            &self.lsp.document_symbols
        } else {
            &self.lsp.workspace_symbols
        }
    }

    /// Jumps to the selected symbol's location via `open_location` --
    /// reused as-is, the same helper Go to Declaration/Find Usages use
    /// (a symbol jump isn't meaningfully different from a `Goto` result).
    fn confirm_go_to_symbol(&mut self) {
        let Some(state) = self.go_to_symbol.as_ref() else {
            return;
        };
        let Some(symbol) = self.go_to_symbol_rows().get(state.selected).cloned() else {
            return;
        };
        self.open_location(symbol.location);
        self.go_to_symbol = None;
    }

    /// Called once per frame, right alongside `sync_go_to_file` (§3.2).
    /// No-op unless `go_to_symbol.is_some()`.
    pub(crate) fn sync_go_to_symbol(&mut self) {
        let Some(state) = &self.go_to_symbol else {
            return;
        };
        let query = state.query.trim().to_string();
        if query.is_empty() {
            let Some(path) = self.active_buffer().map(|b| b.path.clone()) else {
                return;
            };
            let state = self.go_to_symbol.as_mut().unwrap();
            if state.requested_for.as_deref() != Some(path.as_path()) {
                state.requested_for = Some(path.clone());
                state.selected = 0;
                self.lsp.request_document_symbols(&path);
            }
            return;
        }
        let state = self.go_to_symbol.as_mut().unwrap();
        if Some(&query) != state.last_workspace_query.as_ref() {
            state.last_workspace_query = Some(query.clone());
            state.selected = 0;
            self.lsp.query_workspace_symbols(&query);
        }
    }

    /// `Ctrl+E` entry point (`docs/features/
    /// tui-recent-files-and-bookmarks.md` §3.1).
    fn toggle_recent_files(&mut self) {
        let opening = self.recent_files.is_none();
        self.close_all_overlays();
        if opening {
            self.recent_files = Some(RecentFilesState::default());
        }
    }

    /// Empty query: `nav_state.recent_files` verbatim (MRU order
    /// preserved). Non-empty query: every recent path scored via
    /// `fuzzy_score`, dropped on no match, sorted by score descending. No
    /// background thread -- the list is bounded (`project_state::
    /// MAX_RECENT_FILES`), cheap enough to score synchronously every
    /// keystroke, unlike `files_search`'s whole-project scan.
    pub(crate) fn recent_files_rows(&self) -> Vec<PathBuf> {
        let query = self
            .recent_files
            .as_ref()
            .map(|s| s.query.trim())
            .unwrap_or("");
        if query.is_empty() {
            return self.nav_state.recent_files.clone();
        }
        let mut scored: Vec<(i64, &PathBuf)> = self
            .nav_state
            .recent_files
            .iter()
            .filter_map(|p| {
                // Score against the project-relative display, not the full
                // absolute path -- `files_search.rs`'s `FuzzyFileMatch.
                // relative` does the same. Scoring the full path would let
                // an unrelated temp/home-directory segment (e.g. an "a"
                // somewhere in `/private/var/.../T/`) spuriously match
                // every entry, since `fuzzy_score` is a subsequence match
                // over the whole candidate string.
                let relative = p.strip_prefix(&self.project_root).unwrap_or(p);
                fuzzy_score(query, &relative.display().to_string()).map(|m| (m.score, p))
            })
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, p)| p.clone()).collect()
    }

    /// The branches popup's currently filtered/sorted rows (fuzzy-filtered
    /// by `git.branches_popup.filter`, score-descending), as owned
    /// `(name, is_head)` pairs -- ported from `ide-ui`'s own
    /// `filtered_branch_rows` (`docs/features/
    /// tui-git-staging-branches-and-log-filters.md`, filed as a gap in the
    /// original doc, added during implementation). Shared by rendering and
    /// `handle_git_branches_key`/`handle_git_branches_filter_key` so
    /// keyboard nav and what the popup draws can never disagree about row
    /// order.
    pub(crate) fn filtered_branch_rows(&self) -> Vec<(String, bool)> {
        let filter = self.git.branches_popup.filter.trim();
        let named: Vec<(String, bool)> = self
            .git
            .branches
            .iter()
            .map(|b| (b.name.clone(), b.is_head))
            .collect();
        if filter.is_empty() {
            return named;
        }
        let mut scored: Vec<(i64, (String, bool))> = named
            .into_iter()
            .filter_map(|(name, is_head)| {
                fuzzy_score(filter, &name).map(|m| (m.score, (name, is_head)))
            })
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, row)| row).collect()
    }

    /// Keeps `branches_popup.selected` in bounds of the *filtered* row
    /// count after a `filter` edit shrinks or grows the list -- typing a
    /// character that eliminates the currently-selected row must not
    /// leave `selected` pointing past the end.
    fn clamp_branches_popup_selection(&mut self) {
        let count = self.filtered_branch_rows().len();
        self.git.branches_popup.selected = if count == 0 {
            0
        } else {
            self.git.branches_popup.selected.min(count - 1)
        };
    }

    /// Handles every key while `recent_files.is_some()` (§3.1). Same shape
    /// as `handle_go_to_file_key`, except typing/`Backspace` also reset
    /// `selected` (the filtered list can shrink on any keystroke here,
    /// same "stale selection" concern `T16`'s self-review fix caught for
    /// Go to File/Go to Symbol).
    fn handle_recent_files_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.recent_files.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.recent_files = None,
            KeyCode::Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            KeyCode::Down => {
                let len = self.recent_files_rows().len();
                let state = self.recent_files.as_mut().unwrap();
                if state.selected + 1 < len {
                    state.selected += 1;
                }
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
            }
            KeyCode::Enter => self.confirm_recent_file(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.query.push(c);
                state.selected = 0;
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Opens/refocuses the selected row without moving its caret -- unlike
    /// `confirm_go_to_file`'s forced jump to offset 0, Recent Files' whole
    /// point is "go back to where you were" (`docs/features/
    /// tui-recent-files-and-bookmarks.md` §2.3). No-op if nothing is
    /// selected.
    fn confirm_recent_file(&mut self) {
        let Some(state) = self.recent_files.as_ref() else {
            return;
        };
        let rows = self.recent_files_rows();
        let Some(path) = rows.get(state.selected).cloned() else {
            return;
        };
        if let Err(err) = self.open_or_focus_tab(path) {
            self.notify(err.to_string());
            return;
        }
        self.recent_files = None;
    }

    /// `F3` entry point (`docs/features/tui-recent-files-and-bookmarks.md`
    /// §3.2). No active tab: notifies and no-ops.
    fn toggle_bookmark_at_cursor(&mut self) {
        let Some(buf) = self.active_buffer() else {
            self.notify("No file open to bookmark.");
            return;
        };
        let path = buf.path.clone();
        let line = cursor_line_column(
            buf.buffer.text_buffer(),
            buf.buffer.text_buffer().selections().primary().head,
        )
        .0;
        let added = self.nav_state.toggle_bookmark(path, line);
        project_state::save(&self.project_root, &self.nav_state);
        self.notify(if added {
            format!("Bookmark added at line {}.", line + 1)
        } else {
            format!("Bookmark removed at line {}.", line + 1)
        });
    }

    /// `Ctrl+F3` entry point.
    fn toggle_bookmarks_popup(&mut self) {
        let opening = self.bookmarks_popup.is_none();
        self.close_all_overlays();
        if opening {
            self.bookmarks_popup = Some(BookmarksPopupState::default());
        }
    }

    fn handle_bookmarks_popup_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.bookmarks_popup.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.bookmarks_popup = None,
            KeyCode::Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            KeyCode::Down => {
                let len = self.nav_state.bookmarks.len();
                let state = self.bookmarks_popup.as_mut().unwrap();
                if state.selected + 1 < len {
                    state.selected += 1;
                }
            }
            KeyCode::Enter => self.confirm_bookmark_jump(),
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Opens the selected bookmark's file and best-effort places the caret
    /// at that line's start (`None` -- the file shrank since the bookmark
    /// was recorded -- silently closes the popup without moving the caret,
    /// the same permissive shape `open_location` already uses for its own
    /// "line vanished" case). No-op if nothing is selected.
    fn confirm_bookmark_jump(&mut self) {
        let Some(state) = self.bookmarks_popup.as_ref() else {
            return;
        };
        let Some(bookmark) = self.nav_state.bookmarks.get(state.selected).cloned() else {
            return;
        };
        if let Err(err) = self.open_or_focus_tab(bookmark.path) {
            self.notify(err.to_string());
            return;
        }
        self.bookmarks_popup = None;
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let Some(offset) = buf.buffer.text_buffer().lines().line_start(bookmark.line) else {
            return;
        };
        self.push_nav_location(offset);
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        Self::scroll_to_and_reveal(buf, bookmark.line);
        buf.desired_column = None;
        buf.buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(offset)));
    }

    /// Palette-only entry point (`docs/features/tui-todo-panel.md` §1.2/
    /// §2.2). Always re-scans on open (`todo.run` itself is the no-op-
    /// while-already-running guard) -- no live refresh in v1 (§1.1's
    /// scope cut), so this is the only trigger for a fresh scan.
    fn toggle_todo_panel(&mut self) {
        let opening = self.todo_panel.is_none();
        self.close_all_overlays();
        if opening {
            self.todo_panel = Some(TodoPanelState::default());
            self.todo.run(self.tree.clone());
        }
    }

    fn handle_todo_panel_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.todo_panel.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.todo_panel = None,
            KeyCode::Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            KeyCode::Down => {
                let len = self
                    .todo
                    .results
                    .as_ref()
                    .map(|r| r.matches.len())
                    .unwrap_or(0);
                let state = self.todo_panel.as_mut().unwrap();
                if state.selected + 1 < len {
                    state.selected += 1;
                }
            }
            KeyCode::Enter => self.confirm_todo_jump(),
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Reuses `open_search_result` verbatim -- a `TodoMatch`'s `inner:
    /// SearchMatch` carries the same `path`/`byte_offset` shape Find in
    /// Path's own jump already consumes (`docs/features/tui-todo-panel.md`
    /// §2.2). No-op if nothing is selected.
    fn confirm_todo_jump(&mut self) {
        let Some(state) = self.todo_panel.as_ref() else {
            return;
        };
        let Some(results) = &self.todo.results else {
            return;
        };
        let Some(m) = results.matches.get(state.selected) else {
            return;
        };
        let path = m.inner.path.clone();
        let byte_offset = m.inner.byte_offset;
        self.open_search_result(path, byte_offset);
        self.todo_panel = None;
    }

    fn lsp_query_target(&self) -> Option<(PathBuf, Position)> {
        let buf = self.active_buffer()?;
        let offset = buf.buffer.text_buffer().selections().primary().start();
        let position = ide_lsp::byte_offset_to_position(buf.buffer.text(), offset)?;
        Some((buf.path.clone(), position))
    }

    /// `F1` entry point (`docs/features/tui-hover-and-inlay-hints.md`
    /// §2.2), ported from `ide-ui`'s own `trigger_quick_documentation`.
    /// Opens the popup immediately -- always exactly one hover answer to
    /// show (never zero-or-many, unlike the Goto/Find Usages pickers), so
    /// there's no jump-vs-popup branch to defer opening for. Opens even
    /// with no active tab or no valid query target (shows an empty/no-
    /// answer popup rather than silently doing nothing), mirroring
    /// `ide-ui`'s own behaviour.
    fn trigger_quick_documentation(&mut self) {
        self.close_all_overlays();
        self.hover_open = true;
        if let Some((path, position)) = self.lsp_query_target() {
            self.lsp.request_hover(&path, position);
        }
    }

    /// Handles every key while `hover_open` -- only `Esc` is recognised,
    /// closing the popup; everything else is swallowed the same way
    /// `handle_notifications_key`/`handle_cargo_panel_key` swallow
    /// unmapped keys, since this panel has no navigable rows or text
    /// query (`docs/features/tui-hover-and-inlay-hints.md` §2.2).
    fn handle_hover_key(&mut self, key: KeyEvent) -> LoopSignal {
        if key.code == KeyCode::Esc {
            self.hover_open = false;
        }
        LoopSignal::Continue
    }

    /// The active tab's raw, `Position`-based inlay hints, or `&[]` with no
    /// active tab or no entry yet -- same shape as `active_semantic_tokens`
    /// (`docs/features/tui-hover-and-inlay-hints.md` §2.2).
    pub(crate) fn active_inlay_hints(&self) -> &[ide_lsp::InlayHint] {
        self.active_buffer()
            .and_then(|buf| self.lsp.inlay_hints.get(&buf.path))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Called once per frame by `lib.rs`'s run loop, immediately after
    /// `poll_lsp` (`docs/features/tui-hover-and-inlay-hints.md` §2.2).
    /// Fires a fresh `DocumentHighlight` query whenever `lsp_query_target`
    /// names a different `(path, position)` than the last one queried;
    /// clears `document_highlights` (without sending anything) when there's
    /// no valid target at all, so switching to a tab with no target doesn't
    /// leave the previous file's highlights bleeding through. Ported from
    /// `ide-ui`'s own `sync_document_highlights`.
    pub(crate) fn sync_document_highlights(&mut self) {
        match self.lsp_query_target() {
            Some(target) if Some(target.clone()) != self.last_highlighted_target => {
                self.lsp.request_document_highlight(&target.0, target.1);
                self.last_highlighted_target = Some(target);
            }
            Some(_) => {}
            None => {
                if self.last_highlighted_target.is_some() {
                    self.lsp.clear_document_highlights();
                    self.last_highlighted_target = None;
                }
            }
        }
    }

    /// Called once per frame by `lib.rs`'s run loop, alongside
    /// `sync_document_highlights` (`docs/features/
    /// tui-code-actions-and-rename.md` §2.3/§3.1). Same shape: a new target
    /// fires a fresh `CodeAction` query; an unchanged target is a no-op; no
    /// target at all clears `lsp.code_actions` (only if there was a
    /// previous target to clear -- this crate's own established guard,
    /// not `ide-ui`'s unconditional-every-frame version).
    pub(crate) fn sync_code_actions(&mut self) {
        match self.lsp_query_target() {
            Some(target) if Some(target.clone()) != self.last_code_actions_target => {
                self.lsp.request_code_actions(&target.0, target.1);
                self.last_code_actions_target = Some(target);
            }
            Some(_) => {}
            None => {
                if self.last_code_actions_target.is_some() {
                    self.lsp.clear_code_actions();
                    self.last_code_actions_target = None;
                }
            }
        }
    }

    /// Called once per frame by `lib.rs`'s run loop, unconditionally
    /// (`docs/features/tui-git-panel.md` §3.1) -- cheap to no-op: returns
    /// immediately if the Git Panel is closed or if `git.selected_commit`
    /// is `Some` (an explicit commit selection wins over the ambient
    /// working-tree diff). Otherwise compares the active tab's path
    /// against `last_git_diff_target`, the same changed-since-last-frame
    /// guard `sync_code_actions` already established, so an open panel
    /// doesn't re-run a `git2` diff every single frame while the caret
    /// sits still.
    pub(crate) fn sync_git_working_tree_diff(&mut self) {
        if self.git_panel.is_none() || self.git.selected_commit.is_some() {
            return;
        }
        let current = self.active_buffer().map(|b| b.path.clone());
        if current == self.last_git_diff_target {
            return;
        }
        match &current {
            Some(path) => self.git.show_working_tree_diff(path),
            None => self.git.diff = None,
        }
        self.last_git_diff_target = current;
    }

    /// Called once per frame by `lib.rs`'s run loop alongside
    /// `sync_git_working_tree_diff` (`docs/features/
    /// tui-git-staging-branches-and-log-filters.md` §3.1) -- cheap to
    /// no-op via `sync_status`'s own no-repo/transient-error handling, so
    /// an open Git Panel's Staged/Unstaged lists stay live against
    /// out-of-band git activity (e.g. a commit made from another
    /// terminal) the same way the diff pane already does.
    pub(crate) fn sync_git_status(&mut self) {
        if self.git_panel.is_none() {
            return;
        }
        self.git.sync_status();
    }

    /// `ToggleBlameAnnotations` command target (`docs/features/
    /// tui-blame.md` §3.1). No-op with no active tab. Toggling off drops
    /// the cache back to `None`, immediately freeing the reserved columns
    /// next frame. Toggling on calls `blame_for` + `annotations_from_blame`
    /// against the tab's on-disk path -- blame reflects last-saved
    /// content, not the live buffer, same as the working-tree diff.
    fn toggle_blame_annotations(&mut self) {
        let Some(idx) = self.active_tab else {
            return;
        };
        if self.tabs[idx].blame.is_some() {
            self.tabs[idx].blame = None;
            return;
        }
        let path = self.tabs[idx].path.clone();
        let lines = self.git.blame_for(&path);
        self.tabs[idx].blame = Some(crate::blame_gutter::annotations_from_blame(&lines));
    }

    /// Called from `trigger_save_active` (on success) and
    /// `reload_tab_from_disk` -- no-op if blame is off for that tab
    /// (`docs/features/tui-blame.md` §2.3/§3.1).
    fn refresh_blame_if_on(&mut self, idx: usize) {
        if self.tabs[idx].blame.is_none() {
            return;
        }
        let path = self.tabs[idx].path.clone();
        let lines = self.git.blame_for(&path);
        self.tabs[idx].blame = Some(crate::blame_gutter::annotations_from_blame(&lines));
    }

    /// `ShowBlameForCurrentLine` command target (`docs/features/
    /// tui-blame.md` §3.4 -- a deliberate, documented non-parity addition:
    /// `ide-ui` has no keyboard path to the blame popup at all, only a
    /// gutter-label mouse click; this is the keyboard fallback for a
    /// mouse-hostile terminal session). No-op if the active tab's blame
    /// is off, or the caret's current buffer line has no covering
    /// annotation (e.g. beyond `MAX_BLAME_LINES`).
    fn show_blame_for_current_line(&mut self) {
        let Some(buf) = self.active_buffer() else {
            return;
        };
        let Some(annotations) = &buf.blame else {
            return;
        };
        let offset = buf.buffer.text_buffer().selections().primary().head;
        let (line, _) = cursor_line_column(buf.buffer.text_buffer(), offset);
        let Some(annotation) = crate::blame_gutter::blame_annotation_at(annotations, line) else {
            return;
        };
        self.blame_popup = Some(annotation.commit_id.clone());
        self.blame_popup_scroll = 0;
    }

    /// Routes `Up`/`Down` to `blame_popup_scroll` while the Commit
    /// Details popup is open; any other key closes it (`docs/features/
    /// tui-blame.md` §2.4 -- "Esc, or any key... closes it").
    fn handle_blame_popup_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Up => {
                self.blame_popup_scroll = self.blame_popup_scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                self.blame_popup_scroll = self.blame_popup_scroll.saturating_add(1);
            }
            _ => {
                self.blame_popup = None;
                self.blame_popup_scroll = 0;
            }
        }
        LoopSignal::Continue
    }

    /// `0` when the active tab has no `blame` loaded, else
    /// `blame_gutter::BLAME_LANE_WIDTH` -- the single source of the
    /// reserved-column value, called from both `handle_mouse_click`'s
    /// blame-aware split above and `ui.rs`'s `render_editor` (native-
    /// cursor-position fix, `docs/features/tui-blame.md` §2.3) via the
    /// same method call, not two independent computations.
    pub(crate) fn blame_lane_width(&self) -> u16 {
        match self.active_buffer() {
            Some(buf) if buf.blame.is_some() => crate::blame_gutter::BLAME_LANE_WIDTH as u16,
            _ => 0,
        }
    }

    /// Called once per frame from `crates/tui/src/lib.rs`'s main loop,
    /// alongside `sync_git_working_tree_diff`/`sync_code_actions`
    /// (`docs/features/tui-git-gutter.md` §2.3/§3.1). Clears `git_gutter`/
    /// `git_gutter_path` on no active tab or a dirty buffer (§3.1 --
    /// `GitRepo::diff_file` diffs on-disk content, which no longer matches
    /// an unsaved buffer's line numbers); else, only when the active tab's
    /// path differs from `git_gutter_path` (the same changed-since-last-
    /// frame guard those siblings use), recomputes from `self.git.
    /// gutter_marks_for(path)`.
    pub(crate) fn sync_git_gutter(&mut self) {
        let Some(idx) = self.active_tab else {
            self.git_gutter.clear();
            self.git_gutter_path = None;
            return;
        };
        if self.tabs[idx].buffer.is_dirty() {
            self.git_gutter.clear();
            self.git_gutter_path = None;
            return;
        }
        let path = self.tabs[idx].path.clone();
        if self.git_gutter_path.as_ref() == Some(&path) {
            return;
        }
        self.git_gutter = self.git.gutter_marks_for(&path);
        self.git_gutter_path = Some(path);
    }

    /// `2` when `self.git.is_repo()` **and** the active tab's buffer isn't
    /// dirty -- i.e. the same condition under which `sync_git_gutter`
    /// would (re)compute marks at all -- else `0`. Deliberately **not**
    /// based on whether `git_gutter` happens to be empty: a clean,
    /// unchanged file still inside a repo must reserve the same 2 columns
    /// a modified one does, or the lane would jump from 0 to 2 the instant
    /// the file's first hunk appears -- exactly the resize-on-arrival
    /// jitter `docs/features/tui-git-gutter.md` §1.1/§4 says this lane
    /// must not have.
    pub(crate) fn git_gutter_lane_width(&self) -> u16 {
        if !self.git.is_repo() {
            return 0;
        }
        match self.active_buffer() {
            Some(buf) if !buf.buffer.is_dirty() => 2,
            _ => 0,
        }
    }

    /// `blame_lane_width() + git_gutter_lane_width()` -- the one value
    /// both the mouse-click column math (`handle_mouse_click`) and
    /// `render_editor`'s native-cursor-position fix use, so neither ever
    /// computes the combined offset independently (`docs/features/
    /// tui-git-gutter.md` §1.1/§2.3, the same "two things that could
    /// drift" concern `tui-blame.md` §2.3 already resolved for its own
    /// single lane).
    pub(crate) fn editor_lane_width(&self) -> u16 {
        self.blame_lane_width() + self.git_gutter_lane_width()
    }

    /// The active tab's path, only while `git_gutter_popup_line` names a
    /// line still backed by fresh (non-stale) marks for that same path --
    /// shared gating for `trigger_revert_hunk` and the popup's own render
    /// call, mirroring `ide-ui`'s identical `git_gutter_popup_target`
    /// helper for this same feature.
    fn git_gutter_popup_target(&self) -> Option<(PathBuf, usize)> {
        let idx = self.active_tab?;
        let path = self.tabs[idx].path.clone();
        if self.git_gutter_path.as_ref() != Some(&path) {
            return None;
        }
        Some((path, self.git_gutter_popup_line?))
    }

    /// Row is relative to the editor's text area's top-left corner, same
    /// bounds-check shape as `click_blame_lane`/`click_editor_at`
    /// (`docs/features/tui-git-gutter.md` §2.3). Maps to a buffer line,
    /// looks it up in `git_gutter` by exact line match (`GutterMark.line`,
    /// not a run -- unlike blame annotations, one mark decorates exactly
    /// one line); a hit opens the popup (`git_gutter_popup_line =
    /// Some(line)`), a miss (a reserved-but-unmarked row -- most rows,
    /// since only changed lines carry a mark) does nothing.
    fn click_git_gutter_lane(&mut self, area_row: u16) {
        let Some(buf) = self.active_buffer() else {
            return;
        };
        let text_buffer = buf.buffer.text_buffer();
        let ranges = text_buffer.fold_ranges();
        let line_count = text_buffer.lines().line_count();
        let visual = VisualLines::build(line_count, &ranges, &buf.folded);
        let clicked_row = buf.scroll as usize + area_row as usize;
        if clicked_row >= visual.row_count() {
            return;
        }
        let line = visual.buffer_line(clicked_row);
        if self.git_gutter.iter().any(|m| m.line == line) {
            self.git_gutter_popup_line = Some(line);
        }
    }

    /// Routes the git-gutter popup's two single-letter actions -- `r`
    /// (Revert Hunk), `d` (Show Diff) -- checked in `handle_key`'s
    /// popup-precedence chain alongside `blame_popup`; any other key
    /// (including `Esc`) closes it, same "any key closes a confirm-style
    /// popup" convention `handle_blame_popup_key`'s default arm uses
    /// (`docs/features/tui-git-gutter.md` §2.3).
    fn handle_git_gutter_popup_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Char('r') => self.trigger_revert_hunk(),
            KeyCode::Char('d') => self.trigger_show_diff_for_gutter(),
            _ => self.git_gutter_popup_line = None,
        }
        LoopSignal::Continue
    }

    /// No-op unless `git_gutter_popup_line` names a line still backed by
    /// fresh (non-stale) marks for the active tab's current path. Builds
    /// `revert_hunk_change` from `self.git.hunks_for(path)` against the
    /// active buffer's *current* text and, on `Some`, applies it via
    /// `Transaction::new(vec![change]).expect("a single change never
    /// overlaps")` on the active buffer (one undo step -- `Ctrl+Z` undoes
    /// it) and closes the popup either way. **Never writes to disk
    /// directly** -- the user's own next `Ctrl+S` persists it
    /// (`docs/features/tui-git-gutter.md` §2.3/§3.3).
    fn trigger_revert_hunk(&mut self) {
        let target = self.git_gutter_popup_target();
        self.git_gutter_popup_line = None;
        let Some((path, line)) = target else {
            return;
        };
        let Some(idx) = self.active_tab else {
            return;
        };
        let hunks = self.git.hunks_for(&path);
        let Some(change) = crate::git_gutter::revert_hunk_change(
            &hunks,
            line,
            self.tabs[idx].buffer.text_buffer(),
        ) else {
            return;
        };
        let transaction =
            Transaction::new(vec![change]).expect("a single change never overlaps itself");
        self.tabs[idx].buffer.apply(transaction);
    }

    /// Closes the popup, opens the Git Panel if not already open
    /// (`toggle_git_panel`-style, but never *closing* an already-open
    /// panel), and calls `self.git.show_working_tree_diff(path)` for the
    /// active tab's path -- reuses the existing diff pane wholesale rather
    /// than a hunk-scrolled sub-view, matching `editor-git-gutter.md`
    /// §3.4's own v1 simplification. Clears `git.selected_commit` first so
    /// this always shows the *ambient* working-tree diff even if a commit
    /// was previously pinned via the commit graph, and updates
    /// `last_git_diff_target` to match so `sync_git_working_tree_diff`'s
    /// own per-frame refresh keeps working afterward rather than staying
    /// gated on a stale selection. Also sets `state.view =
    /// GitPanelView::Log` and `state.focus = GitPanelFocus::Diff`
    /// (resetting `state.diff_scroll = 0`) on the panel's `GitPanelState`,
    /// the same pair `handle_git_panel_key`'s existing "Enter on commit
    /// graph" flow sets together -- `show_working_tree_diff` alone only
    /// populates `self.git.diff`, it does not touch panel navigation state
    /// (`docs/features/tui-git-gutter.md` §2.3).
    fn trigger_show_diff_for_gutter(&mut self) {
        self.git_gutter_popup_line = None;
        let Some(idx) = self.active_tab else {
            return;
        };
        let path = self.tabs[idx].path.clone();
        self.git.selected_commit = None;
        self.git.show_working_tree_diff(&path);
        self.last_git_diff_target = Some(path);
        if self.git_panel.is_none() {
            self.git_panel = Some(GitPanelState::default());
        }
        let state = self.git_panel.as_mut().expect("just ensured Some above");
        state.view = GitPanelView::Log;
        state.focus = GitPanelFocus::Diff;
        state.diff_scroll = 0;
    }

    /// `ToggleGitPanel` command (palette-only, no default binding -- see
    /// `commands.rs`): opens/closes the Git Panel overlay. `self.git`'s own
    /// fields persist across the toggle -- only the transient cursor/scroll
    /// state in `GitPanelState` resets to a fresh `default()` on open.
    fn toggle_git_panel(&mut self) {
        let opening = self.git_panel.is_none();
        self.close_all_overlays();
        if opening {
            self.git_panel = Some(GitPanelState::default());
        }
    }

    /// `ToggleDockerPanel` command (palette-only, no default binding --
    /// see `commands.rs`): opens/closes the Docker Panel overlay
    /// (`docs/features/tui-docker-and-kubernetes.md` §2.4). Opening starts
    /// a fresh `DockerPanel::default()` and immediately kicks off
    /// `refresh()` so the list is already loading by the time the panel
    /// renders, rather than empty-until-the-user-presses-refresh.
    fn toggle_docker_panel(&mut self) {
        let opening = self.docker_panel.is_none();
        self.close_all_overlays();
        if opening {
            let mut panel = DockerPanel::default();
            panel.refresh();
            self.docker_panel = Some(panel);
        }
    }

    /// `ToggleK8sPanel` command (palette-only, no default binding -- see
    /// `commands.rs`): same immediate-refresh-on-open convention as
    /// `toggle_docker_panel`.
    fn toggle_k8s_panel(&mut self) {
        let opening = self.k8s_panel.is_none();
        self.close_all_overlays();
        if opening {
            let mut panel = K8sPanel::default();
            panel.refresh();
            self.k8s_panel = Some(panel);
        }
    }

    /// Handles every key while `git_panel.is_some()` (`docs/features/
    /// tui-git-staging-branches-and-log-filters.md` §3.2, extending `T11`'s
    /// original single-mode dispatch). Checked in this order, first match
    /// wins -- getting this order wrong lets one mode's keys leak into
    /// another's, e.g. `g`/`s`/`b` reaching a commit-message field mid-edit
    /// as a view switch instead of a typed character:
    ///
    /// 1. `pending_discard` -- confirm/cancel a pending discard.
    /// 2. `branches_popup.pending_delete` -- confirm/cancel a branch delete.
    /// 3. `branches_popup.show_new_branch_input` -- typing a new branch name.
    /// 4. `branches_popup.open` -- the branches popup itself.
    /// 5. `active_conflict`/`binary_conflict` -- `T11`'s conflict resolution,
    ///    unchanged.
    /// 6. `Esc` -- resolved against the doc's precedence chain directly
    ///    (items 6/6a/7; items 1-5 of that chain are exactly the four
    ///    early returns above, which already intercepted `Esc` themselves
    ///    if applicable).
    /// 7. `g`/`s`/`b` view-switch keys -- gated off while `Message`
    ///    (`Changes` view) or `Filter` (`Log` view) focus is active, since
    ///    both are free-text fields a real value is likely to contain
    ///    these letters in (§3.2).
    /// 8. Per-view dispatch: `handle_git_log_key` for `Log`,
    ///    `handle_git_changes_key` for `Changes`.
    fn handle_git_panel_key(&mut self, key: KeyEvent) -> LoopSignal {
        if self.git_panel.is_none() {
            return LoopSignal::Continue;
        }
        if self.git.pending_discard.is_some() {
            return self.handle_git_discard_confirm_key(key);
        }
        if self.git.branches_popup.pending_delete.is_some() {
            return self.handle_git_branch_delete_confirm_key(key);
        }
        if self.git.branches_popup.show_new_branch_input {
            return self.handle_git_new_branch_key(key);
        }
        if self.git.branches_popup.typing_filter {
            return self.handle_git_branches_filter_key(key);
        }
        if self.git.branches_popup.open {
            return self.handle_git_branches_key(key);
        }
        if self.git.worktrees_popup.pending_force_remove.is_some() {
            return self.handle_git_worktree_remove_confirm_key(key);
        }
        if self.git.worktrees_popup.adding {
            return self.handle_git_worktree_add_key(key);
        }
        if self.git.worktrees_popup.open {
            return self.handle_git_worktrees_key(key);
        }
        if self.git.active_conflict.is_some() || self.git.binary_conflict.is_some() {
            match key.code {
                KeyCode::Esc => self.git.cancel_conflict(),
                KeyCode::Char('o') => self.git.accept_ours(),
                KeyCode::Char('t') => self.git.accept_theirs(),
                KeyCode::Enter => {
                    if let Err(e) = self.git.mark_resolved() {
                        self.status = Some(e);
                    }
                }
                _ => {}
            }
            return LoopSignal::Continue;
        }

        let Some(state) = self.git_panel.as_ref() else {
            return LoopSignal::Continue;
        };
        let in_log_view = state.view == GitPanelView::Log;
        let log_focus = state.focus;
        let changes_focus = state.changes_focus;

        if key.code == KeyCode::Esc {
            if in_log_view
                && log_focus == GitPanelFocus::Graph
                && self.git.log_filter.viewing_file_history.is_some()
            {
                self.git.back_to_log();
            } else if in_log_view && log_focus == GitPanelFocus::Filter {
                // Doc §3.5's own Esc rule ("leaves Filter focus, returning
                // to Graph") -- a case §3.2's Esc-precedence list omits
                // (its item 6 only names `Graph` focus), which would
                // otherwise fall through to item 7 and close the whole
                // overlay instead of just backing out of the filter bar.
                self.git_panel.as_mut().expect("checked above").focus = GitPanelFocus::Graph;
            } else {
                self.git_panel = None;
            }
            return LoopSignal::Continue;
        }

        let in_text_entry = (!in_log_view && changes_focus == ChangesFocus::Message)
            || (in_log_view && log_focus == GitPanelFocus::Filter);

        if !in_text_entry {
            match key.code {
                KeyCode::Char('g') => {
                    self.git_panel.as_mut().expect("checked above").view = GitPanelView::Log;
                    return LoopSignal::Continue;
                }
                KeyCode::Char('s') => {
                    self.git_panel.as_mut().expect("checked above").view = GitPanelView::Changes;
                    return LoopSignal::Continue;
                }
                KeyCode::Char('b') => {
                    self.git.open_branches_popup(&self.project_root);
                    return LoopSignal::Continue;
                }
                _ => {}
            }
        }

        if in_log_view {
            self.handle_git_log_key(key)
        } else {
            self.handle_git_changes_key(key)
        }
    }

    /// `Log` view dispatch (`T11`'s original Graph/Conflicts/Diff
    /// behaviour, extended by `docs/features/
    /// tui-git-staging-branches-and-log-filters.md` §3.2/§3.5 with a
    /// `Filter` focus stop and an `f` entry point). `Esc` is handled by
    /// the caller (`handle_git_panel_key`), never here.
    fn handle_git_log_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.git_panel.as_mut() else {
            return LoopSignal::Continue;
        };

        if state.focus == GitPanelFocus::Filter {
            return self.handle_git_filter_key(key);
        }

        match key.code {
            KeyCode::Tab => {
                let filter_hidden = self.git.log_filter.viewing_file_history.is_some();
                state.focus = state
                    .focus
                    .next(self.git.conflicts.is_empty(), filter_hidden);
            }
            KeyCode::Char('f') => {
                if self.git.log_filter.viewing_file_history.is_none() {
                    state.focus = GitPanelFocus::Filter;
                }
            }
            KeyCode::Up => match state.focus {
                GitPanelFocus::Graph => {
                    state.graph_selected = state.graph_selected.saturating_sub(1);
                }
                GitPanelFocus::Conflicts => {
                    state.conflicts_selected = state.conflicts_selected.saturating_sub(1);
                }
                GitPanelFocus::Diff => {
                    state.diff_scroll = state.diff_scroll.saturating_sub(1);
                }
                GitPanelFocus::Filter => unreachable!("handled above"),
            },
            KeyCode::Down => match state.focus {
                GitPanelFocus::Graph => {
                    if state.graph_selected + 1 < self.git.graph.len() {
                        state.graph_selected += 1;
                    }
                }
                GitPanelFocus::Conflicts => {
                    if state.conflicts_selected + 1 < self.git.conflicts.len() {
                        state.conflicts_selected += 1;
                    }
                }
                GitPanelFocus::Diff => {
                    state.diff_scroll = state.diff_scroll.saturating_add(1);
                }
                GitPanelFocus::Filter => unreachable!("handled above"),
            },
            KeyCode::PageUp if state.focus == GitPanelFocus::Diff => {
                state.diff_scroll = state.diff_scroll.saturating_sub(10);
            }
            KeyCode::PageDown if state.focus == GitPanelFocus::Diff => {
                state.diff_scroll = state.diff_scroll.saturating_add(10);
            }
            KeyCode::Enter => match state.focus {
                GitPanelFocus::Graph => {
                    if let Some(commit) = self.git.graph.get(state.graph_selected) {
                        let id = commit.id.clone();
                        self.git.select_commit(&id);
                        state.diff_scroll = 0;
                        state.focus = GitPanelFocus::Diff;
                    }
                }
                GitPanelFocus::Conflicts => {
                    if let Some(path) = self.git.conflicts.get(state.conflicts_selected).cloned() {
                        self.git.select_conflict(&path);
                    }
                }
                GitPanelFocus::Diff => {}
                GitPanelFocus::Filter => unreachable!("handled above"),
            },
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Log filter bar dispatch (`docs/features/
    /// tui-git-staging-branches-and-log-filters.md` §3.5), reached only
    /// while `state.focus == GitPanelFocus::Filter`. Clear Filter is bound
    /// to `Ctrl+C`, not bare `c` -- a bare `c` would corrupt any typed
    /// field value containing that letter (e.g. an author "carol"), the
    /// same text-corrupting-shortcut bug class `g`/`s`/`b` had before
    /// being gated off text-entry focus in §3.2.
    fn handle_git_filter_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.git_panel.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Tab => state.filter_field = state.filter_field.next(),
            KeyCode::BackTab => state.filter_field = state.filter_field.previous(),
            KeyCode::Enter => self.git.apply_log_filter(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.git.clear_log_filter();
            }
            KeyCode::Backspace => {
                let field = state.filter_field;
                field.text_mut(&mut self.git.log_filter).pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let field = state.filter_field;
                field.text_mut(&mut self.git.log_filter).push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// `Changes` view dispatch (`docs/features/
    /// tui-git-staging-branches-and-log-filters.md` §3.3). `Message` focus
    /// delegates entirely to `handle_git_commit_message_key`, which
    /// recognizes only Tab/BackTab/Backspace/Enter/plain-character-typing
    /// -- no single-letter command (`a` included) is reachable while it
    /// owns the key, the same fix shape as `handle_git_filter_key`'s
    /// `Ctrl+C` rebind: a text-entry field must never fall back to a
    /// command binding for a character it doesn't recognize.
    fn handle_git_changes_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.git_panel.as_mut() else {
            return LoopSignal::Continue;
        };

        if state.changes_focus == ChangesFocus::Message {
            return self.handle_git_commit_message_key(key);
        }

        match key.code {
            KeyCode::Tab => state.changes_focus = state.changes_focus.next(),
            KeyCode::BackTab => state.changes_focus = state.changes_focus.previous(),
            KeyCode::Up => match state.changes_focus {
                ChangesFocus::Staged => {
                    state.staged_selected = state.staged_selected.saturating_sub(1);
                }
                ChangesFocus::Unstaged => {
                    state.unstaged_selected = state.unstaged_selected.saturating_sub(1);
                }
                ChangesFocus::Message => unreachable!("handled above"),
            },
            KeyCode::Down => match state.changes_focus {
                ChangesFocus::Staged => {
                    if state.staged_selected + 1 < self.git.status.staged.len() {
                        state.staged_selected += 1;
                    }
                }
                ChangesFocus::Unstaged => {
                    if state.unstaged_selected + 1 < self.git.status.unstaged.len() {
                        state.unstaged_selected += 1;
                    }
                }
                ChangesFocus::Message => unreachable!("handled above"),
            },
            KeyCode::Enter => match state.changes_focus {
                ChangesFocus::Staged => {
                    if let Some(entry) = self.git.status.staged.get(state.staged_selected).cloned()
                    {
                        if let Err(e) = self.git.unstage(&entry.path) {
                            self.status = Some(e);
                        }
                    }
                }
                ChangesFocus::Unstaged => {
                    if let Some(entry) = self
                        .git
                        .status
                        .unstaged
                        .get(state.unstaged_selected)
                        .cloned()
                    {
                        if let Err(e) = self.git.stage(&entry.path) {
                            self.status = Some(e);
                        }
                    }
                }
                ChangesFocus::Message => unreachable!("handled above"),
            },
            KeyCode::Char('x') if state.changes_focus == ChangesFocus::Unstaged => {
                if let Some(entry) = self
                    .git
                    .status
                    .unstaged
                    .get(state.unstaged_selected)
                    .cloned()
                {
                    self.git.request_discard(&entry.path);
                }
            }
            // Only reachable outside `Message` focus -- see this method's
            // doc comment on why that's load-bearing, not incidental.
            KeyCode::Char('a') => self.git.amend = !self.git.amend,
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Commit-message text entry (`Changes` view, `Message` focus). Same
    /// text-entry shape `handle_debug_launch_key`/
    /// `handle_debug_adapter_config_key` (`T27`) already established:
    /// `Tab`/`BackTab` still cycle focus (leaving the field), everything
    /// else either edits `commit_message` or does nothing -- no letter is
    /// ever treated as a command here.
    fn handle_git_commit_message_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.git_panel.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Tab => state.changes_focus = state.changes_focus.next(),
            KeyCode::BackTab => state.changes_focus = state.changes_focus.previous(),
            KeyCode::Enter => {
                if let Err(e) = self.git.commit() {
                    self.status = Some(e);
                }
            }
            KeyCode::Backspace => {
                self.git.commit_message.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.git.commit_message.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Discard-confirm interception (`docs/features/
    /// tui-git-staging-branches-and-log-filters.md` §3.3) -- every key is
    /// intercepted while `git.pending_discard.is_some()`, the same
    /// modal-interception shape `T11`'s conflict resolution already uses.
    fn handle_git_discard_confirm_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if let Err(e) = self.git.confirm_discard() {
                    self.status = Some(e);
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => self.git.cancel_discard(),
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Branches popup dispatch (`docs/features/
    /// tui-git-staging-branches-and-log-filters.md` §3.4), reached only
    /// while `git.branches_popup.open` and neither a delete-confirm nor
    /// new-branch-name entry is in progress (both are their own, higher-
    /// precedence interceptions -- see `handle_git_panel_key`).
    fn handle_git_branches_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.git.close_branches_popup(),
            KeyCode::Char('/') => self.git.branches_popup.typing_filter = true,
            KeyCode::Up => {
                self.git.branches_popup.selected =
                    self.git.branches_popup.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let count = self.filtered_branch_rows().len();
                if self.git.branches_popup.selected + 1 < count {
                    self.git.branches_popup.selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some((name, _)) = self
                    .filtered_branch_rows()
                    .get(self.git.branches_popup.selected)
                    .cloned()
                {
                    let project_root = self.project_root.clone();
                    if let Err(e) = self.git.checkout_branch(&project_root, &name) {
                        self.status = Some(e);
                    }
                }
            }
            KeyCode::Char('m') => {
                if let Some((name, _)) = self
                    .filtered_branch_rows()
                    .get(self.git.branches_popup.selected)
                    .cloned()
                {
                    let project_root = self.project_root.clone();
                    match self.git.merge_branch(&project_root, &name) {
                        Ok(()) => {
                            if self.git.merging {
                                // Deliberate `ide-tui`-side deviation from
                                // `ide-ui`'s "leave the popup open" -- see
                                // `GitPanel::merge_branch`'s own doc
                                // comment for why a modal popup can't
                                // afford to strand the user here.
                                self.git.close_branches_popup();
                                if let Some(state) = self.git_panel.as_mut() {
                                    state.view = GitPanelView::Log;
                                    state.focus = GitPanelFocus::Conflicts;
                                }
                            }
                        }
                        Err(e) => self.status = Some(e),
                    }
                }
            }
            KeyCode::Char('n') => {
                self.git.branches_popup.show_new_branch_input = true;
                self.git.branches_popup.new_branch_name.clear();
            }
            KeyCode::Char('d') => {
                if let Some((name, is_head)) = self
                    .filtered_branch_rows()
                    .get(self.git.branches_popup.selected)
                    .cloned()
                {
                    if !is_head {
                        self.git.request_delete_branch(&name);
                        let project_root = self.project_root.clone();
                        if let Err(e) = self.git.confirm_delete_branch(&project_root, false) {
                            self.status = Some(e);
                        }
                    }
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Branch-list fuzzy-filter typing, entered via `/` from the branches
    /// popup's normal nav (`docs/features/
    /// tui-git-staging-branches-and-log-filters.md` §3.4, added during
    /// implementation -- see `filtered_branch_rows`'s doc comment). `Up`/
    /// `Down` still navigate the *filtered* rows while typing, and
    /// `Backspace`/`Char` edits that shrink or grow the filtered set
    /// re-clamp `selected` so it never points past the end. `Enter` checks
    /// out the currently selected filtered row, the same action normal-
    /// mode `Enter` performs. `Esc` leaves typing mode without clearing
    /// the typed filter text -- consistent with `handle_git_filter_key`'s
    /// own Esc convention (stop editing, not discard).
    fn handle_git_branches_filter_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.git.branches_popup.typing_filter = false,
            KeyCode::Backspace => {
                self.git.branches_popup.filter.pop();
                self.clamp_branches_popup_selection();
            }
            KeyCode::Up => {
                self.git.branches_popup.selected =
                    self.git.branches_popup.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let count = self.filtered_branch_rows().len();
                if self.git.branches_popup.selected + 1 < count {
                    self.git.branches_popup.selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some((name, _)) = self
                    .filtered_branch_rows()
                    .get(self.git.branches_popup.selected)
                    .cloned()
                {
                    let project_root = self.project_root.clone();
                    if let Err(e) = self.git.checkout_branch(&project_root, &name) {
                        self.status = Some(e);
                    }
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.git.branches_popup.filter.push(c);
                self.clamp_branches_popup_selection();
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Branch-delete confirm interception, reached only while `git.
    /// branches_popup.pending_delete.is_some()`. A second `d` on the same
    /// still-pending branch retries with `force: true` -- the keyboard-
    /// native rendering of `git-branches-and-blame.md` §2.2.2's inline
    /// "Force Delete" affordance (§3.4).
    fn handle_git_branch_delete_confirm_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.git.cancel_delete_branch(),
            KeyCode::Char('d') => {
                let project_root = self.project_root.clone();
                if let Err(e) = self.git.confirm_delete_branch(&project_root, true) {
                    self.status = Some(e);
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// New-branch-name text entry, reached only while `git.branches_popup.
    /// show_new_branch_input`. Always create-and-checkout (`checkout:
    /// true`) -- §3.4's documented v1 scope trim of `ide-ui`'s separate
    /// "create without checkout" affordance.
    fn handle_git_new_branch_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => {
                self.git.branches_popup.show_new_branch_input = false;
                self.git.branches_popup.new_branch_name.clear();
            }
            KeyCode::Enter => {
                let name = self.git.branches_popup.new_branch_name.clone();
                let project_root = self.project_root.clone();
                if let Err(e) = self.git.create_branch(&project_root, &name, true) {
                    self.status = Some(e);
                }
            }
            KeyCode::Backspace => {
                self.git.branches_popup.new_branch_name.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.git.branches_popup.new_branch_name.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Worktrees popup normal navigation, reached only while `git.
    /// worktrees_popup.open` and neither `adding` nor
    /// `pending_force_remove` is set (`docs/features/tui-git-worktrees.md`
    /// §2.2).
    fn handle_git_worktrees_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.git.close_worktrees_popup(),
            KeyCode::Up => {
                self.git.worktrees_popup.selected =
                    self.git.worktrees_popup.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let count = self.git.worktrees_popup.worktrees.len();
                if self.git.worktrees_popup.selected + 1 < count {
                    self.git.worktrees_popup.selected += 1;
                }
            }
            KeyCode::Char('r') => {
                if let Some(name) = self
                    .git
                    .worktrees_popup
                    .worktrees
                    .get(self.git.worktrees_popup.selected)
                    .map(|wt| wt.name.clone())
                {
                    self.git.remove_worktree(&name, false);
                }
            }
            KeyCode::Char('n') => {
                self.git.worktrees_popup.adding = true;
                self.git.worktrees_popup.add_field = WorktreeAddField::Name;
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Add-worktree form text entry, reached only while `git.
    /// worktrees_popup.adding` (`docs/features/tui-git-worktrees.md`
    /// §2.2). `Tab`/`Shift+Tab` cycle which of the three fields
    /// `Backspace`/`Char` edit.
    fn handle_git_worktree_add_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => {
                self.git.worktrees_popup.adding = false;
                self.git.worktrees_popup.new_name.clear();
                self.git.worktrees_popup.new_path.clear();
                self.git.worktrees_popup.new_branch.clear();
            }
            KeyCode::Tab => {
                self.git.worktrees_popup.add_field = self.git.worktrees_popup.add_field.next();
            }
            KeyCode::BackTab => {
                self.git.worktrees_popup.add_field = self.git.worktrees_popup.add_field.prev();
            }
            KeyCode::Enter => self.git.create_worktree(),
            KeyCode::Backspace => {
                let field = match self.git.worktrees_popup.add_field {
                    WorktreeAddField::Name => &mut self.git.worktrees_popup.new_name,
                    WorktreeAddField::Path => &mut self.git.worktrees_popup.new_path,
                    WorktreeAddField::Branch => &mut self.git.worktrees_popup.new_branch,
                };
                field.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let field = match self.git.worktrees_popup.add_field {
                    WorktreeAddField::Name => &mut self.git.worktrees_popup.new_name,
                    WorktreeAddField::Path => &mut self.git.worktrees_popup.new_path,
                    WorktreeAddField::Branch => &mut self.git.worktrees_popup.new_branch,
                };
                field.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Worktree-remove force-confirm interception, reached only while
    /// `git.worktrees_popup.pending_force_remove.is_some()`
    /// (`docs/features/tui-git-worktrees.md` §2.2). A second `r` on the
    /// same still-pending worktree retries with `force: true` -- the
    /// keyboard-native rendering of the inline "press r again to force
    /// remove" affordance (§2.4), same shape
    /// `handle_git_branch_delete_confirm_key` already uses for branches.
    fn handle_git_worktree_remove_confirm_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => self.git.worktrees_popup.pending_force_remove = None,
            KeyCode::Char('r') => {
                if let Some(name) = self.git.worktrees_popup.pending_force_remove.clone() {
                    self.git.remove_worktree(&name, true);
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// `GitBranches` command (palette-only, no default binding -- see
    /// `commands.rs`): opens the Git Panel if it wasn't already, then
    /// opens the branches popup over whichever view was active.
    fn trigger_git_branches(&mut self) {
        if self.git_panel.is_none() {
            self.toggle_git_panel();
        }
        self.git.open_branches_popup(&self.project_root);
    }

    /// `GitWorktrees` command (palette-only, no default binding -- see
    /// `commands.rs`): opens the Git Panel if it wasn't already, then
    /// opens the worktrees popup over whichever view was active.
    fn trigger_git_worktrees(&mut self) {
        if self.git_panel.is_none() {
            self.toggle_git_panel();
        }
        self.git.open_worktrees_popup(&self.project_root);
    }

    /// `ShowFileHistory` command (palette-only -- see `commands.rs`):
    /// silent no-op with no active tab or no open repository, the same
    /// shape `T27`'s `trigger_debug` already establishes for a missing
    /// precondition. `sync_git_working_tree_diff`'s existing canonicalize-
    /// then-`strip_prefix` pattern is reused to turn the active tab's
    /// absolute path into the repository-relative path `show_file_history`
    /// requires (`docs/features/tui-git-staging-branches-and-log-filters
    /// .md` §3.5's "caller responsible for stripping the project root").
    fn trigger_show_file_history(&mut self) {
        if !self.git.is_repo() {
            return;
        }
        let Some(path) = self.active_buffer().map(|b| b.path.clone()) else {
            return;
        };
        let Ok(relative) = path.strip_prefix(&self.project_root) else {
            return;
        };
        let relative = relative.to_path_buf();
        self.git.show_file_history(&relative);
        if self.git_panel.is_none() {
            self.toggle_git_panel();
        }
        if let Some(state) = self.git_panel.as_mut() {
            state.view = GitPanelView::Log;
            state.focus = GitPanelFocus::Graph;
        }
    }

    /// Handles every key while `docker_panel.is_some()` (`docs/features/
    /// tui-docker-and-kubernetes.md` §3.3). The yes/no confirm popup
    /// intercepts first -- nothing below it runs while a lifecycle action
    /// is pending confirmation, same interception-order reasoning as
    /// `handle_git_panel_key`'s conflict-resolution check.
    fn handle_docker_panel_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(panel) = self.docker_panel.as_mut() else {
            return LoopSignal::Continue;
        };

        if panel.confirm.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('n') => panel.confirm_no(),
                KeyCode::Char('y') => panel.confirm_yes(),
                _ => {}
            }
            return LoopSignal::Continue;
        }

        let list_len = match panel.tab {
            DockerTab::Containers => panel.containers.len(),
            DockerTab::Images => panel.images.len(),
        };

        match key.code {
            KeyCode::Esc => self.docker_panel = None,
            // Does not auto-refresh -- §1's scope cut names exactly two
            // refresh triggers (panel open, an explicit `r`) to bound
            // `docker` invocation frequency; a tab switch isn't a third
            // one, so an unseen tab shows stale/empty data until `r`.
            KeyCode::Tab => {
                panel.tab = match panel.tab {
                    DockerTab::Containers => DockerTab::Images,
                    DockerTab::Images => DockerTab::Containers,
                };
                panel.selected = 0;
            }
            KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
            KeyCode::Down => {
                if panel.selected + 1 < list_len {
                    panel.selected += 1;
                }
            }
            KeyCode::Char('r') => panel.refresh(),
            KeyCode::Enter if panel.tab == DockerTab::Containers => {
                if let Some(container) = panel.containers.get(panel.selected).cloned() {
                    panel.fetch_logs(&container.id);
                }
            }
            // Lifecycle mnemonics -- panel-internal micro-shortcuts, not
            // global keymap bindings, so CLAUDE.md's "never invent a
            // [global] binding" rule doesn't apply here, the same category
            // as `handle_git_panel_key`'s own 'o'/'t'. 's'tart is the
            // obvious letter; 'x' stands in for stop since 's' is taken;
            // 'b' for restart ("reboot"); 'd' for remove ("delete").
            KeyCode::Char('s') if panel.tab == DockerTab::Containers => {
                if let Some(c) = panel.containers.get(panel.selected).cloned() {
                    panel.request_lifecycle_action(DockerLifecycleAction::Start, c.id, c.names);
                }
            }
            KeyCode::Char('x') if panel.tab == DockerTab::Containers => {
                if let Some(c) = panel.containers.get(panel.selected).cloned() {
                    panel.request_lifecycle_action(DockerLifecycleAction::Stop, c.id, c.names);
                }
            }
            KeyCode::Char('b') if panel.tab == DockerTab::Containers => {
                if let Some(c) = panel.containers.get(panel.selected).cloned() {
                    panel.request_lifecycle_action(DockerLifecycleAction::Restart, c.id, c.names);
                }
            }
            KeyCode::Char('d') if panel.tab == DockerTab::Containers => {
                if let Some(c) = panel.containers.get(panel.selected).cloned() {
                    panel.request_lifecycle_action(DockerLifecycleAction::Remove, c.id, c.names);
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Handles every key while `k8s_panel.is_some()` (`docs/features/
    /// tui-docker-and-kubernetes.md` §3.4/§3.5). Checked in strict
    /// priority order -- the typed-name confirm, then the scale-replica-
    /// count prompt, then the context/namespace picker, then ordinary
    /// list navigation -- since only one of these is ever active at a
    /// time (§3.4's own state-machine description), the same reasoning
    /// `handle_git_panel_key`'s conflict-interception ordering documents.
    fn handle_k8s_panel_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(panel) = self.k8s_panel.as_mut() else {
            return LoopSignal::Continue;
        };

        if panel.confirm.is_some() {
            match key.code {
                KeyCode::Esc => panel.confirm_cancel(),
                KeyCode::Enter => panel.confirm_submit(),
                KeyCode::Backspace => panel.pop_confirm_char(),
                KeyCode::Char(c) => panel.push_confirm_char(c),
                _ => {}
            }
            return LoopSignal::Continue;
        }

        if panel.scale_input.is_some() {
            match key.code {
                KeyCode::Esc => panel.confirm_cancel(),
                KeyCode::Enter => panel.confirm_scale_input(),
                KeyCode::Backspace => panel.pop_scale_input_char(),
                KeyCode::Char(c) if c.is_ascii_digit() => panel.push_scale_input_char(c),
                _ => {}
            }
            return LoopSignal::Continue;
        }

        if let Some(picker) = panel.picker {
            // Namespace's list is one longer than `available_namespaces`:
            // index 0 is the synthetic "no namespace filter" entry (§3.5),
            // never a real namespace name.
            let len = match picker {
                K8sPicker::Context => panel.available_contexts.len(),
                K8sPicker::Namespace => panel.available_namespaces.len() + 1,
            };
            match key.code {
                KeyCode::Esc => panel.picker = None,
                KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
                KeyCode::Down => {
                    if panel.selected + 1 < len {
                        panel.selected += 1;
                    }
                }
                KeyCode::Enter => {
                    match picker {
                        K8sPicker::Context => {
                            if let Some(ctx) = panel.available_contexts.get(panel.selected).cloned()
                            {
                                panel.context = Some(ctx);
                            }
                        }
                        K8sPicker::Namespace => {
                            panel.namespace = if panel.selected == 0 {
                                None
                            } else {
                                panel.available_namespaces.get(panel.selected - 1).cloned()
                            };
                        }
                    }
                    panel.picker = None;
                    panel.selected = 0;
                }
                _ => {}
            }
            return LoopSignal::Continue;
        }

        let list_len = match panel.tab {
            K8sTab::Pods => panel.pods.len(),
            K8sTab::Deployments => panel.deployments.len(),
            K8sTab::Services => panel.services.len(),
        };

        match key.code {
            KeyCode::Esc => self.k8s_panel = None,
            // Same reasoning as `handle_docker_panel_key`'s `Tab` arm --
            // no auto-refresh, only `r` (or panel open) fetches.
            KeyCode::Tab => {
                panel.tab = match panel.tab {
                    K8sTab::Pods => K8sTab::Deployments,
                    K8sTab::Deployments => K8sTab::Services,
                    K8sTab::Services => K8sTab::Pods,
                };
                panel.selected = 0;
            }
            KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
            KeyCode::Down => {
                if panel.selected + 1 < list_len {
                    panel.selected += 1;
                }
            }
            KeyCode::Char('r') => panel.refresh(),
            KeyCode::Char('c') => {
                if panel.available_contexts.is_empty() {
                    panel.refresh_contexts();
                }
                panel.picker = Some(K8sPicker::Context);
                panel.selected = 0;
            }
            KeyCode::Char('n') => {
                if panel.available_namespaces.is_empty() {
                    panel.refresh_namespaces();
                }
                panel.picker = Some(K8sPicker::Namespace);
                panel.selected = 0;
            }
            KeyCode::Char('l') if panel.tab == K8sTab::Pods => {
                if let Some(pod) = panel.pods.get(panel.selected).cloned() {
                    panel.fetch_logs(&pod.name);
                }
            }
            KeyCode::Char('d') if panel.tab == K8sTab::Pods => {
                if let Some(pod) = panel.pods.get(panel.selected).cloned() {
                    panel.request_delete_pod(pod.name);
                }
            }
            KeyCode::Char('s') if panel.tab == K8sTab::Deployments => {
                if let Some(dep) = panel.deployments.get(panel.selected).cloned() {
                    panel.request_scale_deployment(dep.name);
                }
            }
            KeyCode::Enter => {
                let described = match panel.tab {
                    K8sTab::Pods => panel
                        .pods
                        .get(panel.selected)
                        .map(|p| ("pod", p.name.clone())),
                    K8sTab::Deployments => panel
                        .deployments
                        .get(panel.selected)
                        .map(|d| ("deployment", d.name.clone())),
                    K8sTab::Services => panel
                        .services
                        .get(panel.selected)
                        .map(|s| ("service", s.name.clone())),
                };
                if let Some((kind, name)) = described {
                    panel.fetch_describe(kind, &name);
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// `Alt+Enter`'s entry point (`docs/features/
    /// tui-code-actions-and-rename.md` §3.1). No new request -- `sync_code_
    /// actions` already keeps `lsp.code_actions` ambiently fresh, so this
    /// just opens the popup on whatever's already cached.
    fn trigger_show_intention_actions(&mut self) {
        self.close_all_overlays();
        self.code_actions = Some(CodeActionsState { selected: 0 });
    }

    fn handle_code_actions_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.code_actions.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => {
                self.code_actions = None;
            }
            KeyCode::Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            KeyCode::Down => {
                if state.selected + 1 < self.lsp.code_actions.len() {
                    state.selected += 1;
                }
            }
            KeyCode::Enter => {
                let index = state.selected;
                self.code_actions = None;
                if index < self.lsp.code_actions.len() {
                    self.lsp.apply_code_action(index);
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Shared apply primitive, ported from `ide-ui`'s own `apply_workspace_
    /// edit` with no behavioural change (`docs/features/
    /// tui-code-actions-and-rename.md` §2.3/§3.1): partitions `edit.edits`
    /// by whether an open tab already has that file, applies the disk
    /// subset first via `ide_core::apply_workspace_edit_to_disk`
    /// (all-or-nothing -- a failure here means the buffer subset never runs
    /// at all), then -- only once that has fully succeeded -- applies the
    /// buffer subset via `Buffer::apply`. `what` names the operation for its
    /// own error strings; on success, returns the number of files touched.
    /// Reused by `handle_workspace_edit_ready`, `handle_rename_ready`'s
    /// direct-apply path, and the rename preview's Apply key.
    fn apply_workspace_edit(
        &mut self,
        edit: ide_lsp::WorkspaceEdit,
        what: &str,
    ) -> Result<usize, String> {
        let mut disk_edits: Vec<ide_core::FileEdit> = Vec::new();
        let mut buffer_edits: Vec<(usize, ide_core::Transaction)> = Vec::new();

        for file_edit in &edit.edits {
            let open_tab = self.tabs.iter().position(|tab| tab.path == file_edit.path);
            let text = match open_tab {
                Some(idx) => self.tabs[idx].buffer.text().to_string(),
                None => match std::fs::read_to_string(&file_edit.path) {
                    Ok(text) => text,
                    Err(e) => {
                        return Err(format!(
                            "{what}: could not read {}: {e}",
                            file_edit.path.display()
                        ));
                    }
                },
            };
            let Some(transaction) =
                workspace_text_edits_to_transaction(&text, &file_edit.text_edits)
            else {
                return Err(format!(
                    "{what}: an edit for {} does not fit its current content",
                    file_edit.path.display()
                ));
            };
            match open_tab {
                Some(idx) => buffer_edits.push((idx, transaction)),
                None => disk_edits.push(ide_core::FileEdit {
                    path: file_edit.path.clone(),
                    transaction,
                }),
            }
        }

        if !disk_edits.is_empty() {
            let workspace_edit = ide_core::WorkspaceEdit { edits: disk_edits };
            if let Err(e) = ide_core::apply_workspace_edit_to_disk(&workspace_edit) {
                return Err(format!("{what}: {e}"));
            }
        }

        let file_count = edit.edits.len();
        for (idx, transaction) in buffer_edits {
            self.tabs[idx].buffer.apply(transaction);
        }

        Ok(file_count)
    }

    /// Called from `poll_lsp`'s own body, right after it drains `self.lsp.
    /// poll()` (`docs/features/tui-code-actions-and-rename.md` §2.3/§3.1).
    /// No-op unless `self.lsp.workspace_edit_ready`. Sets `self.status` to a
    /// one-line summary either way.
    fn handle_workspace_edit_ready(&mut self) {
        if !self.lsp.workspace_edit_ready {
            return;
        }
        let what = self
            .lsp
            .workspace_edit_label
            .clone()
            .unwrap_or_else(|| "Code action".to_string());
        let Some(edit) = self.lsp.workspace_edit.take() else {
            self.status = Some(format!("{what}: nothing to apply"));
            return;
        };

        let file_count = match self.apply_workspace_edit(edit, &what) {
            Ok(n) => n,
            Err(e) => {
                self.status = Some(e);
                return;
            }
        };

        self.status = Some(format!(
            "{what}: applied to {file_count} file{}",
            if file_count == 1 { "" } else { "s" }
        ));
    }

    /// `Shift+F6`'s entry point (`docs/features/
    /// tui-code-actions-and-rename.md` §3.2). No-op with no active tab.
    /// `self.status` set with no running language server, or no symbol
    /// under the caret. Otherwise opens the popup immediately, prefilled
    /// with the word under the caret, and fires `PrepareRename` ambiently
    /// in parallel -- not gating the popup.
    fn trigger_rename(&mut self) {
        let Some(buf) = self.active_buffer() else {
            return;
        };
        let path = buf.path.clone();
        if !self.lsp.is_running() {
            self.status = Some("Rename: no language server is running".to_string());
            return;
        }
        let offset = buf.buffer.text_buffer().selections().primary().start();
        let text = buf.buffer.text().to_string();
        let Some(local_range) = word_range_at(&text, offset) else {
            self.status = Some("Rename: no symbol under the caret".to_string());
            return;
        };
        let original_name = text[local_range.clone()].to_string();
        let Some(position) = ide_lsp::byte_offset_to_position(&text, local_range.start) else {
            self.status = Some("Rename: no symbol under the caret".to_string());
            return;
        };

        self.close_all_overlays();
        self.rename_popup = Some(RenamePopup {
            path: path.clone(),
            position,
            original_name: original_name.clone(),
            input: original_name,
        });
        self.lsp.request_prepare_rename(&path, position);
    }

    /// Handles every key while `rename_popup` is open. `Esc` cancels
    /// (closes without sending anything). `Backspace` pops the last
    /// character off `input`. A plain (non-`Ctrl`) character is typed in.
    /// `Enter` confirms.
    fn handle_rename_popup_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(popup) = self.rename_popup.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => {
                self.rename_popup = None;
            }
            KeyCode::Backspace => {
                popup.input.pop();
            }
            KeyCode::Enter => self.confirm_rename(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                popup.input.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// The popup's Enter/confirm action (`docs/features/
    /// tui-code-actions-and-rename.md` §3.2). Closes the popup immediately,
    /// regardless of outcome. Sends nothing if the typed name is empty or
    /// unchanged from the original (JetBrains itself treats confirming with
    /// an unchanged name as a silent cancel).
    fn confirm_rename(&mut self) {
        let Some(popup) = self.rename_popup.take() else {
            return;
        };
        let input = popup.input.trim();
        if input.is_empty() || input == popup.original_name {
            return;
        }
        self.lsp
            .request_rename(&popup.path, popup.position, input.to_string());
    }

    /// Called from `poll_lsp`'s own body, alongside `handle_workspace_edit_
    /// ready` (`docs/features/tui-code-actions-and-rename.md` §3.3).
    /// No-op unless `self.lsp.prepare_rename_ready`. `PrepareRenameReady`'s
    /// `renameable` is never a hard gate -- the popup always opens on
    /// trigger regardless of server support; the only effect here is
    /// closing it early on an explicit `renameable: false` from a server
    /// that does support the check, while the popup is still open for the
    /// same `(path, position)` this response answers.
    fn handle_prepare_rename_ready(&mut self) {
        if !self.lsp.prepare_rename_ready {
            return;
        }
        if self.lsp.prepare_renameable == Some(false) {
            let matches = self.rename_popup.as_ref().is_some_and(|popup| {
                Some((popup.path.clone(), popup.position)) == self.lsp.prepare_rename_target
            });
            if matches {
                self.rename_popup = None;
                self.status = Some("Rename: this element cannot be renamed".to_string());
            }
        }
    }

    /// Called from `poll_lsp`'s own body, alongside the above
    /// (`docs/features/tui-code-actions-and-rename.md` §3.3). No-op unless
    /// `self.lsp.rename_ready`. `edit: None` reports nothing to apply. A
    /// single-file edit whose one file is the *currently* active tab's path
    /// (re-read fresh here, not the popup's stale target) applies
    /// immediately. Anything else escalates to the preview instead.
    fn handle_rename_ready(&mut self) {
        if !self.lsp.rename_ready {
            return;
        }
        let Some(edit) = self.lsp.rename_edit.take() else {
            let new_name = self.lsp.rename_new_name.take().unwrap_or_default();
            self.status = Some(format!("Rename to `{new_name}`: nothing to apply"));
            return;
        };
        let new_name = self.lsp.rename_new_name.take().unwrap_or_default();
        let what = format!("Rename to `{new_name}`");

        let active_path = self.active_buffer().map(|b| b.path.clone());
        let applies_directly = match edit.edits.as_slice() {
            [only] => active_path.as_deref() == Some(only.path.as_path()),
            _ => false,
        };

        if applies_directly {
            match self.apply_workspace_edit(edit, &what) {
                Ok(file_count) => {
                    self.status = Some(format!(
                        "{what}: applied to {file_count} file{}",
                        if file_count == 1 { "" } else { "s" }
                    ));
                }
                Err(e) => self.status = Some(e),
            }
        } else {
            self.close_all_overlays();
            self.pending_rename_preview = Some((edit, new_name));
        }
    }

    /// Handles every key while `pending_rename_preview` is open. `Esc`
    /// cancels (nothing read from or written to disk/buffer -- the
    /// `rename` request already completed, cancelling only declines to
    /// apply its answer). `Enter` applies it.
    fn handle_rename_preview_key(&mut self, key: KeyEvent) -> LoopSignal {
        match key.code {
            KeyCode::Esc => {
                self.pending_rename_preview = None;
            }
            KeyCode::Enter => {
                if let Some((edit, new_name)) = self.pending_rename_preview.take() {
                    let what = format!("Rename to `{new_name}`");
                    match self.apply_workspace_edit(edit, &what) {
                        Ok(file_count) => {
                            self.status = Some(format!(
                                "{what}: applied to {file_count} file{}",
                                if file_count == 1 { "" } else { "s" }
                            ));
                        }
                        Err(e) => self.status = Some(e),
                    }
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    fn handle_goto_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(goto) = self.goto.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => {
                self.goto = None;
            }
            KeyCode::Up => {
                if goto.selected > 0 {
                    goto.selected -= 1;
                }
            }
            KeyCode::Down => {
                if goto.selected + 1 < goto.results.len() {
                    goto.selected += 1;
                }
            }
            KeyCode::Enter => {
                let location = goto.results.get(goto.selected).cloned();
                self.goto = None;
                if let Some(location) = location {
                    self.open_location(location);
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Sends `DidChange` with the active tab's current full text -- the
    /// only shape `LspRequest::DidChange` supports (no incremental sync in
    /// v1, matching `ide-ui`'s own `LspBridge`). Called after every editor
    /// mutation (typing, undo/redo, find/replace) -- a no-op past `lsp.
    /// send`'s own gating with no active tab or no running server. Also
    /// requests a fresh semantic-tokens re-tag and a fresh whole-document
    /// inlay-hints re-fetch for the same path (`docs/features/
    /// tui-semantic-highlighting.md` §2.2, `tui-hover-and-inlay-hints.md`
    /// §2.2) -- folded in here rather than duplicated at each of this
    /// function's call sites, since every real edit that needs `DidChange`
    /// also needs both.
    fn sync_lsp_did_change(&mut self) {
        let Some(buf) = self.active_buffer() else {
            return;
        };
        let path = buf.path.clone();
        let text = buf.buffer.text().to_string();
        let whole_document = whole_document_range(&text);
        self.lsp.send(LspRequest::DidChange {
            path: path.clone(),
            text,
        });
        self.lsp.request_semantic_tokens(&path);
        if let Some(range) = whole_document {
            self.lsp.request_inlay_hints(&path, range);
        }
    }

    /// Called once per frame by `main.rs`, before `handle_key`, with the
    /// editor pane's current text-row count (`ui.rs`'s
    /// `EDITOR_CHROME_ROWS` already subtracted) -- see that constant's own
    /// doc comment for why this crate keeps two independent copies of the
    /// editor's non-text-row count in sync rather than computing this from
    /// a single shared layout pass.
    pub(crate) fn set_editor_viewport_rows(&mut self, rows: u16) {
        self.editor_viewport_rows = rows;
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub(crate) fn project_root(&self) -> &std::path::Path {
        &self.project_root
    }

    /// `EditorConfig::default()` (every field `None`, so `indent_unit_for`
    /// falls back to `IndentUnit::default()`) when `path` doesn't resolve
    /// against this project's root -- `docs/features/
    /// tui-line-commands-and-editorconfig.md` §3.5 treats that the same
    /// way `ide-ui`'s own `resolve_editor_config` does: as "no config",
    /// never as an error to surface.
    fn resolve_editor_config(&self, path: &std::path::Path) -> EditorConfig {
        editorconfig::resolve(&self.project_root, path).unwrap_or_default()
    }

    pub(crate) fn active_buffer(&self) -> Option<&OpenBuffer> {
        self.active_tab.and_then(|idx| self.tabs.get(idx))
    }

    pub(crate) fn active_buffer_mut(&mut self) -> Option<&mut OpenBuffer> {
        self.active_tab.and_then(move |idx| self.tabs.get_mut(idx))
    }

    /// Records the tab that is active **at call time** (path + `offset`)
    /// as the new current entry in `nav_history` (`docs/features/
    /// tui-back-forward-navigation.md` §2.2). No-op with no active tab --
    /// every "open a new file" tab always has a path (unlike `ide-ui`,
    /// which also guards against a never-saved untitled buffer), so that
    /// case can't currently occur here, but the check costs nothing and
    /// keeps this from panicking if that ever changes.
    fn push_nav_location(&mut self, offset: usize) {
        let Some(buf) = self.active_buffer() else {
            return;
        };
        self.nav_history.push(NavLocation {
            path: buf.path.clone(),
            offset,
        });
    }

    /// The active buffer's actual current caret offset, or `0` with no
    /// active tab. Reading this live (rather than assuming a jump's own
    /// nominal target offset) is what makes `push_nav_location` accurate
    /// when `open_or_focus_tab` refocuses a tab that was already open --
    /// that branch never touches the tab's live caret, so the caret can
    /// differ from whatever offset the caller nominally jumped to.
    fn active_caret_offset(&self) -> usize {
        self.active_buffer()
            .map(|buf| buf.buffer.text_buffer().selections().primary().head)
            .unwrap_or(0)
    }

    /// `NavigateBack` command (§2.2). No-op at the oldest entry. Never
    /// calls `push_nav_location` itself -- every Back/Forward press would
    /// otherwise immediately push a new forward-erasing entry (§3).
    fn nav_back(&mut self) {
        if !self.nav_history.can_go_back() {
            return;
        }
        let Some(location) = self.nav_history.go_back() else {
            return;
        };
        self.go_to_nav_location(location);
    }

    /// `NavigateForward` command (§2.2). Same shape as `nav_back`,
    /// opposite direction.
    fn nav_forward(&mut self) {
        if !self.nav_history.can_go_forward() {
            return;
        }
        let Some(location) = self.nav_history.go_forward() else {
            return;
        };
        self.go_to_nav_location(location);
    }

    fn go_to_nav_location(&mut self, location: NavLocation) {
        if let Err(err) = self.open_or_focus_tab(location.path) {
            self.notify(err.to_string());
            return;
        }
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let (line, _) = cursor_line_column(buf.buffer.text_buffer(), location.offset);
        Self::scroll_to_and_reveal(buf, line);
        buf.desired_column = None;
        buf.buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(location.offset)));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> LoopSignal {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return LoopSignal::Continue;
        }
        if self.palette.is_some() {
            return self.handle_palette_key(key);
        }
        if self.find.is_some() {
            return self.handle_find_key(key);
        }
        if self.goto.is_some() {
            return self.handle_goto_key(key);
        }
        if self.notifications_open {
            return self.handle_notifications_key(key);
        }
        if self.problems.is_some() {
            return self.handle_problems_key(key);
        }
        if self.cargo_panel_open {
            return self.handle_cargo_panel_key(key);
        }
        if self.hover_open {
            return self.handle_hover_key(key);
        }
        if self.search_open {
            return self.handle_search_key(key);
        }
        if self.go_to_file.is_some() {
            return self.handle_go_to_file_key(key);
        }
        if self.go_to_symbol.is_some() {
            return self.handle_go_to_symbol_key(key);
        }
        if self.recent_files.is_some() {
            return self.handle_recent_files_key(key);
        }
        if self.bookmarks_popup.is_some() {
            return self.handle_bookmarks_popup_key(key);
        }
        if self.todo_panel.is_some() {
            return self.handle_todo_panel_key(key);
        }
        if self.code_actions.is_some() {
            return self.handle_code_actions_key(key);
        }
        if self.rename_popup.is_some() {
            return self.handle_rename_popup_key(key);
        }
        if self.pending_rename_preview.is_some() {
            return self.handle_rename_preview_key(key);
        }
        if self.blame_popup.is_some() {
            return self.handle_blame_popup_key(key);
        }
        if self.git_gutter_popup_line.is_some() {
            return self.handle_git_gutter_popup_key(key);
        }
        if self.git_panel.is_some() {
            return self.handle_git_panel_key(key);
        }
        if self.docker_panel.is_some() {
            return self.handle_docker_panel_key(key);
        }
        if self.k8s_panel.is_some() {
            return self.handle_k8s_panel_key(key);
        }
        if self.keymap_popup.is_some() {
            return self.handle_keymap_popup_key(key);
        }
        if self.new_scratch_file.is_some() {
            return self.handle_new_scratch_file_key(key);
        }
        if self.scratch_files.is_some() {
            return self.handle_scratch_files_key(key);
        }
        if self.claude_panel_open {
            return self.handle_claude_panel_key(key);
        }
        if self.debug.show_launch_popup {
            return self.handle_debug_launch_key(key);
        }
        if self.debug_adapter_config_popup.is_some() {
            return self.handle_debug_adapter_config_key(key);
        }
        if self.debug_panel_open {
            return self.handle_debug_panel_key(key);
        }
        if let Some(action) = self.keymap.action_for(key.modifiers, key.code) {
            return self.run_action(action);
        }
        match self.focus {
            Focus::Tree => self.handle_tree_key(key),
            Focus::Editor => self.handle_editor_key(key),
        }
        LoopSignal::Continue
    }

    /// Mirrors `handle_key`'s own popup-priority chain above (every branch
    /// before the `keymap.action_for`/`self.focus` dispatch) -- kept as a
    /// single source of truth so mouse routing (`docs/features/
    /// tui-mouse-support.md` §3.2/§3.3) never drifts from which state
    /// `handle_key` itself currently treats as "a popup is open".
    fn any_popup_open(&self) -> bool {
        self.palette.is_some()
            || self.find.is_some()
            || self.goto.is_some()
            || self.notifications_open
            || self.problems.is_some()
            || self.cargo_panel_open
            || self.hover_open
            || self.search_open
            || self.go_to_file.is_some()
            || self.go_to_symbol.is_some()
            || self.recent_files.is_some()
            || self.bookmarks_popup.is_some()
            || self.todo_panel.is_some()
            || self.code_actions.is_some()
            || self.rename_popup.is_some()
            || self.pending_rename_preview.is_some()
            || self.blame_popup.is_some()
            || self.git_gutter_popup_line.is_some()
            || self.git_panel.is_some()
            || self.docker_panel.is_some()
            || self.k8s_panel.is_some()
            || self.keymap_popup.is_some()
            || self.new_scratch_file.is_some()
            || self.scratch_files.is_some()
            || self.claude_panel_open
            || self.debug.show_launch_popup
            || self.debug_adapter_config_popup.is_some()
            || self.debug_panel_open
    }

    /// Entry point for every `Event::Mouse` (`docs/features/
    /// tui-mouse-support.md` §2.3). `hits` is the *previous* frame's
    /// `ui::HitMap` -- `main.rs`'s `run` loop reads it one frame behind
    /// what's currently on screen, the same lag its scroll-follow/resize
    /// handling already accepts.
    pub fn handle_mouse(&mut self, event: MouseEvent, hits: &crate::ui::HitMap) {
        // T26: raw PTY focus forwards every key except Shift+Esc to the
        // terminal; mouse events are dropped entirely rather than
        // forwarded (there is no PTY mouse-reporting story here) or used
        // for chrome navigation while the PTY owns input.
        if self.claude_panel_open && self.claude_terminal_focus {
            return;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.handle_mouse_click(event, hits),
            MouseEventKind::ScrollUp => self.handle_mouse_scroll(event, hits, KeyCode::Up),
            MouseEventKind::ScrollDown => self.handle_mouse_scroll(event, hits, KeyCode::Down),
            _ => {}
        }
    }

    /// Position-based, independent of `self.focus` (§3.2). A popup owns
    /// all input while open, so a click doesn't reach the base view at
    /// all in that case, matching wheel scroll's own popup-priority rule.
    fn handle_mouse_click(&mut self, event: MouseEvent, hits: &crate::ui::HitMap) {
        if self.any_popup_open() {
            return;
        }
        let point: (u16, u16) = (event.column, event.row);
        if let Some(area) = hits.tree_area {
            if area.contains(point.into()) {
                let row = (event.row - area.y) as usize;
                self.tree_state.select(&self.tree, row);
                self.handle_tree_enter();
                self.focus = Focus::Tree;
                return;
            }
        }
        for &(rect, index) in &hits.tab_strip {
            if rect.contains(point.into()) {
                self.active_tab = Some(index);
                self.focus = Focus::Editor;
                return;
            }
        }
        if let Some(area) = hits.editor_text_area {
            if area.contains(point.into()) {
                let col = event.column - area.x;
                let row = event.row - area.y;
                let blame_w = self.blame_lane_width();
                let lane = self.editor_lane_width();
                if (col as usize) < blame_w as usize {
                    self.click_blame_lane(row);
                } else if (col as usize) < lane as usize {
                    self.click_git_gutter_lane(row);
                } else {
                    self.click_editor_at(col - lane, row);
                }
                self.focus = Focus::Editor;
            }
        }
    }

    /// Row is relative to the editor's text area's top-left corner, same
    /// as `click_editor_at` (`docs/features/tui-blame.md` §2.3). Maps to
    /// a buffer line the same way, including the identical "no-op past
    /// the buffer's last visible row/line" bounds check -- a click below
    /// a short file's last line must be exactly as inert here as it
    /// already is for `click_editor_at`. A hit (a line covered by an
    /// annotation) opens the Commit Details popup and resets its scroll;
    /// a miss (reserved columns on a line with no annotation -- only
    /// reachable past `MAX_BLAME_LINES`, since `blame_for` is synchronous
    /// and has no loading state) does nothing.
    fn click_blame_lane(&mut self, area_row: u16) {
        let Some(buf) = self.active_buffer() else {
            return;
        };
        let Some(annotations) = &buf.blame else {
            return;
        };
        let text_buffer = buf.buffer.text_buffer();
        let ranges = text_buffer.fold_ranges();
        let line_count = text_buffer.lines().line_count();
        let visual = VisualLines::build(line_count, &ranges, &buf.folded);
        let clicked_row = buf.scroll as usize + area_row as usize;
        if clicked_row >= visual.row_count() {
            return;
        }
        let line = visual.buffer_line(clicked_row);
        let Some(annotation) = crate::blame_gutter::blame_annotation_at(annotations, line) else {
            return;
        };
        self.blame_popup = Some(annotation.commit_id.clone());
        self.blame_popup_scroll = 0;
    }

    /// Places the caret at the character under `(area_col, area_row)`,
    /// both relative to the editor's text area's top-left corner
    /// (`docs/features/tui-mouse-support.md` §3.2.3). No-op past the
    /// buffer's last visible row/line -- clicking blank space below a
    /// short file's content places no caret, rather than clamping into
    /// the last line (which would silently jump the caret away from
    /// where the user actually clicked).
    fn click_editor_at(&mut self, area_col: u16, area_row: u16) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let text_buffer = buf.buffer.text_buffer();
        let ranges = text_buffer.fold_ranges();
        let line_count = text_buffer.lines().line_count();
        let visual = VisualLines::build(line_count, &ranges, &buf.folded);
        let clicked_row = buf.scroll as usize + area_row as usize;
        if clicked_row >= visual.row_count() {
            return;
        }
        let line = visual.buffer_line(clicked_row);
        let line_text = text_buffer.line_text(line).unwrap_or("");
        let column = crate::highlight::char_column_for_screen_column(
            line_text,
            area_col as usize,
            buf.indent.width,
        );
        let offset = offset_for_line_column(text_buffer, line, column);
        buf.desired_column = None;
        buf.buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(offset)));
    }

    /// One wheel notch = one synthetic `direction` press (§3.3): while a
    /// popup is open it's fed through `handle_key`'s own popup-priority
    /// chain (zero new per-popup code -- reuses every popup's existing
    /// Up/Down clamp/dispatch, Git Panel's `GitPanelFocus` routing
    /// included); otherwise it's position-based against the base view.
    fn handle_mouse_scroll(
        &mut self,
        event: MouseEvent,
        hits: &crate::ui::HitMap,
        direction: KeyCode,
    ) {
        let synthetic = KeyEvent::new(direction, KeyModifiers::NONE);
        if self.any_popup_open() {
            self.handle_key(synthetic);
            return;
        }
        let point: (u16, u16) = (event.column, event.row);
        if hits.tree_area.is_some_and(|r| r.contains(point.into())) {
            self.handle_tree_key(synthetic);
            return;
        }
        if hits
            .editor_text_area
            .is_some_and(|r| r.contains(point.into()))
        {
            self.scroll_editor_view(direction);
        }
    }

    /// The one genuinely new primitive this feature adds (§3.3): the
    /// editor has no existing keyboard action that moves `buf.scroll`
    /// without also moving the caret, unlike every popup's `selected`
    /// field or the git panel's `diff_scroll` (both already have a plain
    /// Up/Down handler to reuse). Caret, selection and `desired_column`
    /// are all left untouched.
    fn scroll_editor_view(&mut self, direction: KeyCode) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let text_buffer = buf.buffer.text_buffer();
        let ranges = text_buffer.fold_ranges();
        let line_count = text_buffer.lines().line_count();
        let total_rows = VisualLines::build(line_count, &ranges, &buf.folded).row_count();
        let max_scroll = total_rows.saturating_sub(1).min(u16::MAX as usize) as u16;
        buf.scroll = match direction {
            KeyCode::Up => buf.scroll.saturating_sub(1),
            _ => buf.scroll.saturating_add(1).min(max_scroll),
        };
    }

    fn run_action(&mut self, action: Action) -> LoopSignal {
        match action {
            Action::SaveActive => self.trigger_save_active(),
            Action::Undo => {
                let viewport_rows = self.editor_viewport_rows;
                if let Some(buf) = self.active_buffer_mut() {
                    buf.buffer.undo();
                    Self::sync_editor_scroll(buf, viewport_rows);
                }
                self.sync_lsp_did_change();
            }
            Action::Redo => {
                let viewport_rows = self.editor_viewport_rows;
                if let Some(buf) = self.active_buffer_mut() {
                    buf.buffer.redo();
                    Self::sync_editor_scroll(buf, viewport_rows);
                }
                self.sync_lsp_did_change();
            }
            Action::ToggleTreeFocus => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Editor,
                    Focus::Editor => Focus::Tree,
                };
            }
            Action::NextTab => self.cycle_tab(1),
            Action::PreviousTab => self.cycle_tab(-1),
            Action::CloseTab => self.close_active_tab(),
            Action::Find => {
                if self.active_buffer().is_some() {
                    self.find = Some(FindState::new());
                    self.focus = Focus::Editor;
                }
            }
            // Only reachable while `self.find.is_none()` -- `handle_key`'s
            // `self.find.is_some()` check runs before `binding_for`, so
            // `Ctrl+R` on an already-open bar never reaches `run_action`
            // at all; that case is handled by `handle_find_key`'s own
            // local `Ctrl+R` check instead (`docs/features/
            // tui-replace.md` §2.2).
            Action::Replace => {
                if self.active_buffer().is_some() {
                    let mut find = FindState::new();
                    find.enable_replace_mode();
                    self.find = Some(find);
                    self.focus = Focus::Editor;
                }
            }
            Action::GoToDeclaration => self.trigger_go_to_declaration(),
            Action::FindUsages => self.trigger_find_usages(),
            Action::ToggleNotifications => self.toggle_notifications(),
            Action::ToggleProblems => self.toggle_problems(),
            Action::ToggleCargoPanel => self.toggle_cargo_panel(),
            Action::QuickDocumentation => self.trigger_quick_documentation(),
            Action::FindInPath => self.toggle_search_panel(),
            Action::ShowIntentionActions => self.trigger_show_intention_actions(),
            Action::Rename => self.trigger_rename(),
            Action::ToggleGitPanel => self.toggle_git_panel(),
            Action::GitBranches => self.trigger_git_branches(),
            Action::GitWorktrees => self.trigger_git_worktrees(),
            Action::ShowFileHistory => self.trigger_show_file_history(),
            Action::ToggleBlameAnnotations => self.toggle_blame_annotations(),
            Action::ShowBlameForCurrentLine => self.show_blame_for_current_line(),
            Action::ToggleDockerPanel => self.toggle_docker_panel(),
            Action::ToggleK8sPanel => self.toggle_k8s_panel(),
            Action::JumpToMatchingBracket => self.trigger_jump_to_matching_bracket(),
            Action::DuplicateLines => self.run_line_op(|tb, _unit| tb.duplicate_selection_lines()),
            Action::DeleteLines => self.run_line_op(|tb, _unit| tb.delete_selection_lines()),
            Action::JoinLines => self.run_line_op(|tb, _unit| tb.join_selection_lines()),
            Action::MoveLinesUp => {
                self.run_line_op(|tb, _unit| tb.move_selection_lines(LineDirection::Up))
            }
            Action::MoveLinesDown => {
                self.run_line_op(|tb, _unit| tb.move_selection_lines(LineDirection::Down))
            }
            Action::MoveStatementsUp => {
                self.run_line_op(|tb, _unit| tb.move_selection_statements(LineDirection::Up))
            }
            Action::MoveStatementsDown => {
                self.run_line_op(|tb, _unit| tb.move_selection_statements(LineDirection::Down))
            }
            Action::ToggleLineComment => self.run_line_op(|tb, unit| tb.toggle_line_comment(unit)),
            Action::ToggleBlockComment => self.run_line_op(|tb, _unit| tb.toggle_block_comment()),
            Action::ExtendSelection => self.trigger_extend_selection(),
            Action::ShrinkSelection => self.trigger_shrink_selection(),
            Action::ToggleCase => self.run_line_op(|tb, _unit| tb.toggle_selection_case()),
            Action::CollapseFold => self.trigger_collapse_fold(),
            Action::ExpandFold => self.trigger_expand_fold(),
            Action::CollapseAllFolds => self.trigger_collapse_all_folds(),
            Action::ExpandAllFolds => self.trigger_expand_all_folds(),
            Action::AddNextOccurrence => self.trigger_add_next_occurrence(),
            Action::UnselectOccurrence => self.trigger_unselect_occurrence(),
            Action::SelectAllOccurrences => self.trigger_select_all_occurrences(),
            Action::CollapseSelections => self.trigger_collapse_selections(),
            Action::GoToFile => self.toggle_go_to_file(),
            Action::GoToSymbol => self.toggle_go_to_symbol(),
            Action::RecentFiles => self.toggle_recent_files(),
            Action::ToggleBookmark => self.toggle_bookmark_at_cursor(),
            Action::ShowBookmarks => self.toggle_bookmarks_popup(),
            Action::ToggleTodoPanel => self.toggle_todo_panel(),
            Action::ReloadFromDisk => self.reload_active_from_disk(),
            Action::DismissExternalChange => self.dismiss_external_change(),
            Action::OpenPalette => self.open_palette(),
            Action::ToggleKeymapSettings => self.toggle_keymap_popup(),
            Action::ResetAllKeybindings => {
                self.keymap.reset_all();
                self.persist_keymap();
                self.notify("Reset all keybindings to default.");
            }
            Action::NewScratchFile => self.toggle_new_scratch_file(),
            Action::ToggleScratchFiles => self.toggle_scratch_files(),
            Action::ToggleClaudePanel => self.toggle_claude_panel(),
            Action::Debug => self.trigger_debug(),
            Action::ResumeProgram => self.debug.resume(),
            Action::StepOver => self.debug.step_over(),
            Action::StepInto => self.debug.step_into(),
            Action::StepOut => self.debug.step_out(),
            Action::ToggleLineBreakpoint => self.toggle_breakpoint_at_caret(),
            Action::StopDebugging => self.debug.stop(),
            Action::PauseProgram => self.debug.pause(),
            Action::ToggleDebugPanel => self.toggle_debug_panel(),
            Action::ConfigureDebugAdapter => self.toggle_debug_adapter_config_popup(),
            Action::NavigateBack => self.nav_back(),
            Action::NavigateForward => self.nav_forward(),
            Action::Exit => return LoopSignal::Exit,
        }
        LoopSignal::Continue
    }

    fn handle_tree_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.tree_state.move_selection(&self.tree, -1),
            KeyCode::Down => self.tree_state.move_selection(&self.tree, 1),
            KeyCode::Enter => self.handle_tree_enter(),
            _ => {}
        }
    }

    fn handle_tree_enter(&mut self) {
        let rows = self.tree_state.visible_rows(&self.tree);
        let Some(row) = self.tree_state.selected_row(&rows) else {
            return;
        };
        if row.is_dir {
            self.tree_state.toggle_expand_selected(&self.tree);
            return;
        }
        let path = row.path.clone();
        match self.open_or_focus_tab(path) {
            Ok(()) => {
                self.status = None;
                let offset = self.active_caret_offset();
                self.push_nav_location(offset);
            }
            Err(err) => self.status = Some(err.to_string()),
        }
    }

    /// Switches to `path`'s existing tab if one is already open, never
    /// re-reading it from disk (which would silently discard any unsaved
    /// edit in that tab); otherwise opens it as a new tab
    /// (`docs/features/tui-multi-buffer-tabs.md` §2.1/§4), installing
    /// syntax highlighting for it via `syntax_for_path` (`docs/features/
    /// tui-syntax-highlighting.md` §2.1) -- the same tab-open-time
    /// `syntax_for_path` + `Buffer::set_syntax` pattern
    /// `crates/ui/src/app.rs`'s `Tab::from_buffer` already uses.
    /// `set_syntax` doesn't mark the buffer dirty (its own contract), so
    /// this needs no `mark_dirty` call.
    fn open_or_focus_tab(&mut self, path: PathBuf) -> Result<(), BufferError> {
        let path = Self::canonicalize_best_effort(&path);
        if let Some(idx) = self.tabs.iter().position(|tab| tab.path == path) {
            self.active_tab = Some(idx);
            self.record_recent_file(path);
            return Ok(());
        }
        let mut buffer = Buffer::open(&path)?;
        buffer.set_syntax(syntax_for_path(&path));
        let text = buffer.text().to_string();
        let config = self.resolve_editor_config(&path);
        let indent = indent_unit_for(&config);
        self.tabs.push(OpenBuffer {
            path: path.clone(),
            buffer,
            scroll: 0,
            desired_column: None,
            auto_closed: None,
            config,
            indent,
            charset_notice_shown: false,
            shrink_stack: Vec::new(),
            folded: std::collections::BTreeSet::new(),
            external_change: None,
            blame: None,
        });
        self.active_tab = Some(self.tabs.len() - 1);
        self.record_recent_file(path.clone());
        self.lsp.request_semantic_tokens(&path);
        if let Some(range) = whole_document_range(&text) {
            self.lsp.request_inlay_hints(&path, range);
        }
        self.lsp.send(LspRequest::DidOpen { path, text });
        Ok(())
    }

    /// Records `path` as recently used and persists `nav_state`
    /// immediately (`docs/features/tui-recent-files-and-bookmarks.md`
    /// §2.3) -- every successful open *or* refocus counts, matching real
    /// JetBrains Recent Files semantics (it tracks visits, not just
    /// first-opens).
    fn record_recent_file(&mut self, path: PathBuf) {
        self.nav_state.record_recent_file(path);
        project_state::save(&self.project_root, &self.nav_state);
    }

    /// Closes the active tab, refusing (with a status message, no data
    /// loss) if its buffer is dirty. `focus` is left unchanged either way
    /// (`docs/features/tui-multi-buffer-tabs.md` §3.2).
    fn close_active_tab(&mut self) {
        let Some(idx) = self.active_tab else {
            return;
        };
        if self.tabs[idx].buffer.is_dirty() {
            self.status = Some(format!(
                "unsaved changes in {} -- save first (Ctrl+S)",
                self.tabs[idx].path.display()
            ));
            return;
        }
        let path = self.tabs[idx].path.clone();
        self.tabs.remove(idx);
        self.lsp.send(LspRequest::DidClose { path });
        self.active_tab = if self.tabs.is_empty() {
            None
        } else {
            Some(idx.min(self.tabs.len() - 1))
        };
    }

    /// `NextTab`/`PreviousTab`: wraps at either end, a no-op on zero or one
    /// tab (`docs/features/tui-multi-buffer-tabs.md` §2.1/§3.3).
    fn cycle_tab(&mut self, delta: isize) {
        let Some(active) = self.active_tab else {
            return;
        };
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as isize;
        let next = (active as isize + delta).rem_euclid(len);
        self.active_tab = Some(next as usize);
    }

    fn handle_editor_key(&mut self, key: KeyEvent) {
        let viewport_rows = self.editor_viewport_rows;
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        // Taken (not just read) here so every keystroke -- arrow moves
        // included -- clears the type-over window; only the `Char` arm
        // below may write a fresh one back (`tui-smart-editing.md` §3.2).
        let auto_closed = buf.auto_closed.take();
        // Every keystroke reaching this function is, by construction, an
        // edit or an arrow move -- `Extend`/`Shrink Selection` are global
        // `Command`s intercepted by `binding_for` before `handle_editor_key`
        // is ever called, so clearing here unconditionally is exactly
        // "any edit or arrow move" (`docs/features/
        // tui-line-commands-and-editorconfig.md` §1/§3.4), never the two
        // actions that themselves push/pop this stack.
        buf.shrink_stack.clear();
        // `Ctrl+Left`/`Ctrl+Right` are excluded here -- they're word motion
        // (`ExtendedMotion::WordLeft`/`WordRight` below), not a plain
        // character step; without this guard this match (keyed only on
        // `key.code`, blind to modifiers) would catch them first and the
        // extended-motion dispatch below would never run
        // (`docs/features/tui-word-and-document-navigation.md` §3.1).
        let direction = match key.code {
            KeyCode::Left if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Direction::Left)
            }
            KeyCode::Right if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Direction::Right)
            }
            KeyCode::Up => Some(Direction::Up),
            KeyCode::Down => Some(Direction::Down),
            _ => None,
        };
        if let Some(direction) = direction {
            // Every selection moves, not just the primary (`docs/features/
            // tui-multiple-cursors.md` §3.4) -- `Up`/`Down` share one
            // `buf.desired_column` across all of them, taken from the
            // primary's own answer specifically so the shared value
            // doesn't depend on selection iteration order.
            let selections = buf.buffer.text_buffer().selections().clone();
            let primary_index = selections.primary_index();
            let mut primary_desired_column = None;
            let carets: Vec<Selection> = selections
                .all()
                .iter()
                .enumerate()
                .map(|(i, selection)| {
                    let (offset, desired) =
                        Self::move_caret_with_folds(buf, selection.start(), direction);
                    if i == primary_index {
                        primary_desired_column = desired;
                    }
                    Selection::caret(offset)
                })
                .collect();
            buf.desired_column = primary_desired_column;
            buf.buffer
                .text_buffer_mut()
                .set_selections(Selections::new(carets, primary_index));
            Self::sync_editor_scroll(buf, viewport_rows);
            return;
        }
        // `Ctrl`-qualified arms come first: a guarded arm only matches when
        // its guard holds, so `Ctrl+Home`/`Ctrl+End` must be checked before
        // the bare `Home`/`End` arms below them, which fire regardless of
        // `Ctrl` and would otherwise shadow them
        // (`docs/features/tui-word-and-document-navigation.md` §3.1).
        let extended_motion = match key.code {
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ExtendedMotion::DocumentStart)
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ExtendedMotion::DocumentEnd)
            }
            KeyCode::Home => Some(ExtendedMotion::LineStart),
            KeyCode::End => Some(ExtendedMotion::LineEnd),
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ExtendedMotion::WordLeft)
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ExtendedMotion::WordRight)
            }
            _ => None,
        };
        if let Some(motion) = extended_motion {
            // Every selection collapses to a caret, same per-selection map
            // `Direction`-based motion already uses -- these are horizontal
            // motions, so `desired_column` is always cleared, never carried
            // (`docs/features/tui-word-and-document-navigation.md` §3.3).
            let selections = buf.buffer.text_buffer().selections().clone();
            let primary_index = selections.primary_index();
            let carets: Vec<Selection> = selections
                .all()
                .iter()
                .map(|selection| {
                    let offset = Self::move_caret_extended(buf, selection.start(), motion);
                    Selection::caret(offset)
                })
                .collect();
            buf.desired_column = None;
            buf.buffer
                .text_buffer_mut()
                .set_selections(Selections::new(carets, primary_index));
            Self::sync_editor_scroll(buf, viewport_rows);
            return;
        }
        let mut changed = false;
        match key.code {
            KeyCode::Char(c) => {
                changed = insert_char(buf, c, auto_closed);
                buf.desired_column = None;
            }
            KeyCode::Enter => {
                changed = insert_newline_with_indent(buf);
                buf.desired_column = None;
            }
            KeyCode::Tab => {
                changed = indent_or_insert_tab(buf);
                buf.desired_column = None;
            }
            KeyCode::BackTab => {
                changed = outdent_lines(buf);
                buf.desired_column = None;
            }
            KeyCode::Backspace => {
                changed = delete_backward(buf);
                buf.desired_column = None;
            }
            KeyCode::Delete => {
                // Per selection, via `apply_per_selection` (`docs/
                // features/tui-multiple-cursors.md` §3.4/Revision notes --
                // a non-empty selection now deletes its own whole range, a
                // pre-existing single-selection bug this rewrite also
                // fixes: the previous version always stepped one
                // character right of `start()` regardless of whether the
                // selection was empty, never deleting a real selection's
                // full range). An empty selection is fold-aware, not a raw
                // `move_cursor` step, otherwise deleting forward from the
                // end of a collapsed fold's `start_line` would silently
                // delete into its hidden interior (`tui-code-folding.md`
                // §3.6's correction applies here too, not just to plain
                // caret motion).
                changed = apply_per_selection(buf, |buf, selection| {
                    let range = if !selection.is_empty() {
                        selection.range()
                    } else {
                        let offset = selection.start();
                        let (end, _) = App::move_caret_with_folds(buf, offset, Direction::Right);
                        offset..end
                    };
                    (range, String::new(), 0, 0)
                });
                buf.desired_column = None;
            }
            _ => {}
        }
        Self::sync_editor_scroll(buf, viewport_rows);
        if changed {
            self.sync_lsp_did_change();
        }
    }

    /// Moves the primary caret to just past whichever half of the pair it
    /// doesn't already sit at, per `matching_bracket`'s after-the-caret-first
    /// rule (`tui-smart-editing.md` §3.4). No-op if the caret doesn't touch a
    /// bracket or the match can't be found. Palette-only, no default binding
    /// (`commands.rs`) -- there is no JetBrains macOS keymap entry for this
    /// action to reuse, and `CLAUDE.md` forbids inventing one.
    fn trigger_jump_to_matching_bracket(&mut self) {
        let viewport_rows = self.editor_viewport_rows;
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let head = buf.buffer.text_buffer().selections().primary().head;
        let Some(pair) = buf.buffer.text_buffer().matching_bracket(head) else {
            return;
        };
        let target = if head == pair.close.start || head == pair.close.end {
            pair.open.end
        } else {
            pair.close.end
        };
        buf.buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(target)));
        buf.desired_column = None;
        Self::sync_editor_scroll(buf, viewport_rows);
    }

    /// The `.editorconfig` save sequence (`docs/features/
    /// tui-line-commands-and-editorconfig.md` §3.6): applies the minimal
    /// `save_edit` transaction first (undoable, carries every selection
    /// through `Selections::map`), then writes under the resolved
    /// charset. A charset the buffer can't honor losslessly surfaces a
    /// one-time `notify` (not a transient `self.status`, per §1) the
    /// first time this tab's config names one.
    fn trigger_save_active(&mut self) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        if let Some(edit) = editorconfig::save_edit(buf.buffer.text(), &buf.config) {
            buf.buffer.apply(edit);
        }
        let charset = editorconfig::save_charset(&buf.config);
        let path = buf.path.clone();
        // `docs/features/tui-file-watcher.md` §2.2/§3.3: suppress before
        // the write, not after -- decide what's about to happen, then do
        // it, the same ordering the `.editorconfig` pre-write transaction
        // above already establishes.
        if let Some(watcher) = self.watcher.as_mut() {
            watcher.suppress(&path);
        }
        let buf = self
            .active_buffer_mut()
            .expect("the tab being saved is still the active one");
        if let Err(err) = buf.buffer.save_with(charset) {
            self.status = Some(err.to_string());
            return;
        }
        if let Some(idx) = self.active_tab {
            self.refresh_blame_if_on(idx);
        }
        let buf = self
            .active_buffer_mut()
            .expect("the tab just saved is still the active one");
        if buf.charset_notice_shown {
            return;
        }
        let Some(unsupported) = buf.config.charset else {
            return;
        };
        if !matches!(
            unsupported,
            Charset::Latin1 | Charset::Utf16Le | Charset::Utf16Be
        ) {
            return;
        }
        buf.charset_notice_shown = true;
        let message = format!(
            "{} was saved as UTF-8: {unsupported:?} from .editorconfig isn't supported",
            buf.path.display(),
        );
        self.notify(message);
    }

    /// Shared wiring for every whole-buffer command T18b adds (line ops,
    /// comments, case toggle): runs `op` against the active buffer's
    /// `TextBuffer` with its resolved `indent`, and on a real change marks
    /// dirty, clears the shrink stack (any edit invalidates "where I came
    /// from" -- `docs/features/tui-line-commands-and-editorconfig.md`
    /// §3.3/§3.4) and re-syncs scroll/LSP. A no-op (`op` returns `false`,
    /// e.g. Move Line at the buffer's edge) leaves everything untouched.
    fn run_line_op(&mut self, op: impl FnOnce(&mut TextBuffer, IndentUnit) -> bool) {
        let viewport_rows = self.editor_viewport_rows;
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let unit = buf.indent;
        let changed = op(buf.buffer.text_buffer_mut(), unit);
        if changed {
            buf.buffer.mark_dirty();
            buf.shrink_stack.clear();
        }
        buf.desired_column = None;
        Self::sync_editor_scroll(buf, viewport_rows);
        if changed {
            self.sync_lsp_did_change();
        }
    }

    /// `Alt+Up`: replaces the primary selection with `extended_selection`'s
    /// next range out, pushing the pre-extension `Selections` onto
    /// `shrink_stack` (`docs/features/tui-line-commands-and-editorconfig.md`
    /// §3.3). No-op when the selection is already the whole buffer.
    fn trigger_extend_selection(&mut self) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let current = buf.buffer.text_buffer().selections().clone();
        let Some(extended) = buf
            .buffer
            .text_buffer()
            .extended_selection(current.primary())
        else {
            return;
        };
        buf.shrink_stack.push(current);
        buf.buffer
            .text_buffer_mut()
            .set_selections(Selections::single(extended));
        buf.desired_column = None;
    }

    /// `Alt+Down`: pops `shrink_stack` and restores it verbatim; an empty
    /// stack falls back to the word under the primary caret
    /// (`ide_core::word_at`, not this crate's own `editor::word_range_at`
    /// -- see `tui-line-commands-and-editorconfig.md` §3.3), then to a
    /// bare caret.
    fn trigger_shrink_selection(&mut self) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        if let Some(previous) = buf.shrink_stack.pop() {
            buf.buffer.text_buffer_mut().set_selections(previous);
        } else {
            let text_buffer = buf.buffer.text_buffer();
            let head = text_buffer.selections().primary().head;
            let fallback = word_at(text_buffer.text(), head)
                .map(|r| Selection::new(r.start, r.end))
                .unwrap_or_else(|| Selection::caret(head));
            buf.buffer
                .text_buffer_mut()
                .set_selections(Selections::single(fallback));
        }
        buf.desired_column = None;
    }

    /// `Ctrl+-`: collapses the innermost uncollapsed fold containing the
    /// caret's line, then reveals the caret if that collapse just hid it
    /// (`tui-code-folding.md` §3.2/§3.3).
    fn trigger_collapse_fold(&mut self) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let text_buffer = buf.buffer.text_buffer();
        let ranges = text_buffer.fold_ranges();
        let (line, _) = cursor_line_column(text_buffer, text_buffer.selections().primary().head);
        folding::collapse_at_caret(&mut buf.folded, &ranges, line);
        Self::reveal_caret_if_hidden(buf);
    }

    /// `Ctrl++`: uncollapses the fold whose `start_line` is the caret's
    /// current line, if any. Thanks to the invariant that a caret on a
    /// collapsed fold is always sitting on that fold's own `start_line`
    /// (`tui-code-folding.md` §3.2), no containment search is needed the
    /// way `trigger_collapse_fold` needs one.
    fn trigger_expand_fold(&mut self) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let text_buffer = buf.buffer.text_buffer();
        let (line, _) = cursor_line_column(text_buffer, text_buffer.selections().primary().head);
        folding::expand_at_caret(&mut buf.folded, line);
    }

    /// `Ctrl+Shift+-`: collapses every fold `fold_ranges()` currently
    /// reports, then reveals the caret if that hid its own line.
    fn trigger_collapse_all_folds(&mut self) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let ranges = buf.buffer.text_buffer().fold_ranges();
        folding::collapse_all(&mut buf.folded, &ranges);
        Self::reveal_caret_if_hidden(buf);
    }

    /// `Ctrl+Shift++`: uncollapses every fold.
    fn trigger_expand_all_folds(&mut self) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        folding::expand_all(&mut buf.folded);
    }

    /// Moves every selection whose line a collapse just hid to the nearest
    /// visible line at or before it -- for a range collapsed around a
    /// caret, that is exactly the range's own `start_line`
    /// (`tui-code-folding.md` §3.3). No-op (no `set_selections` call at
    /// all) if every selection's line is still visible. Every selection is
    /// walked, not just the primary (`docs/features/
    /// tui-multiple-cursors.md` §"Revision notes" -- folding a region
    /// around one of several cursors used to discard all the others via
    /// `Selections::single`); a hidden non-empty selection collapses to a
    /// bare caret at the reveal point, same as the single-selection
    /// version already did.
    fn reveal_caret_if_hidden(buf: &mut OpenBuffer) {
        let text_buffer = buf.buffer.text_buffer();
        let ranges = text_buffer.fold_ranges();
        let line_count = text_buffer.lines().line_count();
        let visual = VisualLines::build(line_count, &ranges, &buf.folded);
        let selections = text_buffer.selections().clone();
        let primary_index = selections.primary_index();
        let mut changed = false;
        let moved: Vec<Selection> = selections
            .all()
            .iter()
            .map(|selection| {
                let (line, column) = cursor_line_column(text_buffer, selection.head);
                let visible_line = visual.buffer_line(visual.row_of(line));
                if visible_line == line {
                    *selection
                } else {
                    changed = true;
                    Selection::caret(offset_for_line_column(text_buffer, visible_line, column))
                }
            })
            .collect();
        if changed {
            buf.buffer
                .text_buffer_mut()
                .set_selections(Selections::new(moved, primary_index));
        }
    }

    /// Fold-aware caret motion (`tui-code-folding.md` §3.6), from an
    /// explicit `offset` rather than always the primary selection's own
    /// (`docs/features/tui-multiple-cursors.md` §2.2 -- every call site
    /// that needs to move more than one selection passes each selection's
    /// own position in turn). `Up`/`Down` step the *row*, not the buffer
    /// line -- `buffer_line` never returns a hidden row, so the result can
    /// never land inside a collapsed fold's interior by construction, and
    /// the row-boundary case (`Up` at row `0`, `Down` at the last row) is a
    /// no-op exactly like `move_cursor`'s own line-boundary case.
    /// `Left`/`Right` call `move_cursor` unchanged first (a raw
    /// character-boundary step, a character or word step can validly cross
    /// a line boundary the same way it always could), then redirect if the
    /// raw result landed inside a hidden interior: forward to the start of
    /// the row right after the fold, backward to the end of the fold's own
    /// `start_line` text. `buf.desired_column` is still read directly
    /// (shared across every selection in one keystroke, not per-selection
    /// -- `tui-multiple-cursors.md` §3.4's documented simplification).
    fn move_caret_with_folds(
        buf: &OpenBuffer,
        offset: usize,
        direction: Direction,
    ) -> (usize, Option<usize>) {
        let text_buffer = buf.buffer.text_buffer();
        let ranges = text_buffer.fold_ranges();
        let line_count = text_buffer.lines().line_count();
        let visual = VisualLines::build(line_count, &ranges, &buf.folded);
        match direction {
            Direction::Up | Direction::Down => {
                let (line, column_here) = cursor_line_column(text_buffer, offset);
                let column = buf.desired_column.unwrap_or(column_here);
                let row = visual.row_of(line);
                let target_row = match direction {
                    Direction::Up => row.checked_sub(1),
                    Direction::Down => (row + 1 < visual.row_count()).then_some(row + 1),
                    Direction::Left | Direction::Right => unreachable!(),
                };
                match target_row {
                    Some(target_row) => {
                        let target_line = visual.buffer_line(target_row);
                        (
                            offset_for_line_column(text_buffer, target_line, column),
                            Some(column),
                        )
                    }
                    None => (offset, buf.desired_column),
                }
            }
            Direction::Left | Direction::Right => {
                let (raw_offset, _) = move_cursor(text_buffer, offset, None, direction);
                let raw_line = cursor_line_column(text_buffer, raw_offset).0;
                let visible_line = visual.buffer_line(visual.row_of(raw_line));
                let corrected = if visible_line == raw_line {
                    raw_offset
                } else if direction == Direction::Right {
                    let target_line = visual.buffer_line(visual.row_of(raw_line) + 1);
                    offset_for_line_column(text_buffer, target_line, 0)
                } else {
                    let target_line = visual.buffer_line(visual.row_of(raw_line));
                    offset_for_line_column(text_buffer, target_line, usize::MAX)
                };
                (corrected, None)
            }
        }
    }

    /// If `offset`'s line is hidden by a collapsed fold, redirects to the
    /// nearest visible boundary in the direction of travel -- forward, the
    /// start of the row right after the fold; backward, the end of the
    /// fold's own visible `start_line` text. A no-op when `offset`'s line
    /// is already visible. Same correction `move_caret_with_folds`'s
    /// `Left`/`Right` arm applies inline to a plain character step,
    /// extracted so `move_caret_extended` can share it
    /// (`docs/features/tui-word-and-document-navigation.md` §2.2/§3.2).
    fn redirect_hidden(
        text_buffer: &TextBuffer,
        visual: &VisualLines,
        offset: usize,
        backward: bool,
    ) -> usize {
        let line = text_buffer.lines().line_at(offset);
        let hiding_row = visual.row_of(line);
        if visual.buffer_line(hiding_row) == line {
            return offset;
        }
        if backward {
            text_buffer
                .lines()
                .line_range(visual.buffer_line(hiding_row), text_buffer.text())
                .map_or(offset, |r| r.end)
        } else {
            text_buffer
                .lines()
                .line_start(visual.buffer_line(hiding_row) + 1)
                .unwrap_or(offset)
        }
    }

    /// `Home`/`End`/`Ctrl+Left`/`Ctrl+Right`/`Ctrl+Home`/`Ctrl+End`'s shared
    /// dispatch (`docs/features/tui-word-and-document-navigation.md`
    /// §2.2/§3.1): computes the raw target via `motion`'s matching pure
    /// function in `editor.rs`, then applies `redirect_hidden` exactly like
    /// `move_caret_with_folds` already does for a plain step.
    fn move_caret_extended(buf: &OpenBuffer, offset: usize, motion: ExtendedMotion) -> usize {
        let text_buffer = buf.buffer.text_buffer();
        let ranges = text_buffer.fold_ranges();
        let line_count = text_buffer.lines().line_count();
        let visual = VisualLines::build(line_count, &ranges, &buf.folded);
        let (raw, backward) = match motion {
            ExtendedMotion::LineStart => (line_start_offset(text_buffer, offset), true),
            ExtendedMotion::LineEnd => (line_end_offset(text_buffer, offset), false),
            ExtendedMotion::WordLeft => (word_start_before(text_buffer.text(), offset), true),
            ExtendedMotion::WordRight => (word_end_after(text_buffer.text(), offset), false),
            ExtendedMotion::DocumentStart => (0, true),
            // `backward: true`, not `false` like every other forward
            // motion above: `buffer.len()` sits on the buffer's true last
            // line, which *is* reachable as a hidden interior line when a
            // collapsed fold's `end_line` is that last line -- and unlike
            // every other forward motion, there is no line *after* the
            // buffer's end to redirect into, so the only sound target is
            // the nearest visible line *before* it (mirrors `ide-ui`'s own
            // `vertical_step`'s identical special-case for `Granularity::
            // Document`, `code-folding.md` §2.6 revision note 6).
            ExtendedMotion::DocumentEnd => (text_buffer.text().len(), true),
        };
        Self::redirect_hidden(text_buffer, &visual, raw, backward)
    }

    /// Applies `scroll_to_keep_visible` for `buf`'s current cursor
    /// position, in row space (`tui-code-folding.md` §3.7) -- a fresh
    /// `VisualLines` per call, same no-cache convention as every other
    /// per-frame overlay in this crate. No `reveal_line` call is needed
    /// here: every call site only ever leaves the caret on a line that
    /// was already visible before the operation ran, so `row_of` below
    /// always resolves to that line's own true row. A free function
    /// taking `buf` explicitly (rather than a `&mut self` method) so it
    /// can run after `handle_editor_key`'s `active_buffer_mut()` borrow
    /// is already held, without a second, conflicting borrow of `self`.
    fn sync_editor_scroll(buf: &mut OpenBuffer, viewport_rows: u16) {
        let text_buffer = buf.buffer.text_buffer();
        let offset = text_buffer.selections().primary().start();
        let (line, _) = cursor_line_column(text_buffer, offset);
        let ranges = text_buffer.fold_ranges();
        let line_count = text_buffer.lines().line_count();
        let visual = VisualLines::build(line_count, &ranges, &buf.folded);
        let row = visual.row_of(line);
        buf.scroll = scroll_to_keep_visible(buf.scroll, row, viewport_rows);
    }

    /// Unfolds whatever hides `line`, then top-aligns `buf.scroll` on its
    /// now-guaranteed-visible row (`tui-code-folding.md` §3.5) -- shared
    /// by `open_location`/`open_search_result`/`jump_to_match`, the three
    /// sites in this crate that jump to an externally-chosen target
    /// rather than moving the caret as a side effect of an edit or an
    /// ordinary arrow key.
    fn scroll_to_and_reveal(buf: &mut OpenBuffer, line: usize) {
        let text_buffer = buf.buffer.text_buffer();
        let ranges = text_buffer.fold_ranges();
        folding::reveal_line(&mut buf.folded, &ranges, line);
        let line_count = text_buffer.lines().line_count();
        let visual = VisualLines::build(line_count, &ranges, &buf.folded);
        buf.scroll = visual.row_of(line).min(u16::MAX as usize) as u16;
    }

    /// Reveals whatever fold hides `buf`'s new primary caret, then scrolls
    /// only as far as needed to keep it visible (`sync_editor_scroll`'s
    /// minimal adjustment, not `scroll_to_and_reveal`'s top-align) --
    /// `AddNextOccurrence` resolves a match against raw buffer text, so
    /// unlike ordinary caret motion (already fold-aware via
    /// `move_caret_with_folds`) the match can land inside a currently
    /// collapsed fold (`docs/features/tui-multiple-cursors.md` §2.2).
    fn reveal_and_sync_scroll(buf: &mut OpenBuffer, viewport_rows: u16) {
        let text_buffer = buf.buffer.text_buffer();
        let offset = text_buffer.selections().primary().head;
        let (line, _) = cursor_line_column(text_buffer, offset);
        let ranges = text_buffer.fold_ranges();
        folding::reveal_line(&mut buf.folded, &ranges, line);
        let line_count = text_buffer.lines().line_count();
        let visual = VisualLines::build(line_count, &ranges, &buf.folded);
        let row = visual.row_of(line);
        buf.scroll = scroll_to_keep_visible(buf.scroll, row, viewport_rows);
    }

    /// `Ctrl+G` (`docs/features/tui-multiple-cursors.md` §3.1). An empty
    /// primary selects the word under it and stops; a non-empty primary
    /// adds the next occurrence of its own text and becomes primary
    /// there, or no-ops if every occurrence is already selected.
    fn trigger_add_next_occurrence(&mut self) {
        let viewport_rows = self.editor_viewport_rows;
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        buf.shrink_stack.clear();
        let text_buffer = buf.buffer.text_buffer();
        let primary = text_buffer.selections().primary();
        let text = text_buffer.text();
        if primary.is_empty() {
            let Some(word) = word_at(text, primary.head) else {
                return;
            };
            let idx = text_buffer.selections().primary_index();
            let mut ranges = text_buffer.selections().all().to_vec();
            ranges[idx] = Selection::new(word.start, word.end);
            let selections = Selections::new(ranges, idx);
            buf.buffer.text_buffer_mut().set_selections(selections);
            Self::reveal_and_sync_scroll(buf, viewport_rows);
            return;
        }
        let needle = text[primary.range()].to_string();
        let Some(next) = next_occurrence(text, &needle, primary.end()) else {
            return;
        };
        let mut selections = text_buffer.selections().clone();
        if !selections.push_primary(Selection::new(next.start, next.end)) {
            return;
        }
        buf.buffer.text_buffer_mut().set_selections(selections);
        Self::reveal_and_sync_scroll(buf, viewport_rows);
    }

    /// `Ctrl+Shift+G` (`docs/features/tui-multiple-cursors.md` §3.2). No-op
    /// with one selection.
    fn trigger_unselect_occurrence(&mut self) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        buf.shrink_stack.clear();
        let mut selections = buf.buffer.text_buffer().selections().clone();
        if selections.remove_primary() {
            buf.buffer.text_buffer_mut().set_selections(selections);
        }
    }

    /// `Ctrl+Alt+Shift+J` (`docs/features/tui-multiple-cursors.md` §3.2).
    /// Resolves the needle exactly as `AddNextOccurrence` does (word under
    /// an empty primary, otherwise the primary's own text), but selects
    /// every occurrence in one press rather than staging word-select and
    /// select-all across two. The match containing the old primary stays
    /// primary, so the view does not jump -- no scroll sync here.
    fn trigger_select_all_occurrences(&mut self) {
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        buf.shrink_stack.clear();
        let text_buffer = buf.buffer.text_buffer();
        let primary = text_buffer.selections().primary();
        let text = text_buffer.text();
        let needle = if primary.is_empty() {
            match word_at(text, primary.head) {
                Some(word) => text[word].to_string(),
                None => return,
            }
        } else {
            text[primary.range()].to_string()
        };
        if needle.is_empty() {
            return;
        }
        let ranges = all_occurrences(text, &needle);
        if ranges.is_empty() {
            return;
        }
        let old_primary_start = primary.start();
        let primary_index = ranges
            .iter()
            .position(|r| r.start <= old_primary_start && old_primary_start < r.end)
            .unwrap_or(0);
        let selections: Vec<Selection> = ranges
            .iter()
            .map(|r| Selection::new(r.start, r.end))
            .collect();
        buf.buffer
            .text_buffer_mut()
            .set_selections(Selections::new(selections, primary_index));
    }

    /// `Esc` (`docs/features/tui-multiple-cursors.md` §3.3). No-op outside
    /// `Focus::Editor` or with one selection.
    fn trigger_collapse_selections(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let mut selections = buf.buffer.text_buffer().selections().clone();
        if selections.is_multiple() {
            selections.collapse_to_primary();
            buf.buffer.text_buffer_mut().set_selections(selections);
        }
    }

    /// Handles every key while `self.find.is_some()` -- intercepts all
    /// input the same way `handle_palette_key` does, so no other handler
    /// runs while the find bar owns focus (`docs/features/tui-find.md`
    /// §2.2/§4.1).
    ///
    /// The four `Ctrl`-qualified checks run first and `return` before the
    /// `match key.code` below is ever reached -- load-bearing, not
    /// stylistic: `crossterm` reports `Ctrl+G`/`Ctrl+R`/`Ctrl+Shift+R` as
    /// the same `KeyCode::Char`s a plain keystroke produces, distinguished
    /// only by `key.modifiers`. Checking modifiers via `==`/`.contains()`
    /// (never pattern-matching `KeyModifiers` itself) matches this file's
    /// existing style, e.g. `handle_key`'s own `Ctrl+Shift+A` check above.
    /// The two `Shift`-qualified checks (`Ctrl+Shift+G`/`Ctrl+Shift+R`)
    /// match a **lowercase** `'g'`/`'r'`, not `'G'`/`'R'` -- see
    /// `commands.rs`'s module doc comment for why (`crossterm`'s Kitty/
    /// CSI-u decode reports `Shift` as a separate modifier bit rather than
    /// folding it into the char's case, unlike a plain typed keystroke).
    ///
    /// `Ctrl+Shift+R` (Replace All, `docs/features/tui-replace-all.md`
    /// §2.2) is *never* also registered in `commands()`/`Action`, unlike
    /// `Ctrl+R`: closing the bar drops `FindState` entirely, so a global
    /// "fresh, bar closed" registration would always have an empty query
    /// to act on -- the same no-op-or-unreachable shape that already
    /// keeps `Ctrl+G`/`Ctrl+Shift+G` find-bar-local.
    ///
    /// `Ctrl+R` here (rather than only in `run_action`'s `Action::Replace`
    /// arm) is what makes "reveal the replace row on an already-open
    /// find-only bar" (`docs/features/tui-replace.md` §3.1) actually
    /// reachable: `handle_key`'s `self.find.is_some()` check runs before
    /// `binding_for`, so `Action::Replace` is only ever reached while
    /// `self.find` is `None` -- the already-open case has to be handled
    /// here, the same way `Ctrl+G`/`Ctrl+Shift+G` are.
    fn handle_find_key(&mut self, key: KeyEvent) -> LoopSignal {
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('g') {
            let next = self.find.as_mut().and_then(FindState::next);
            self.jump_to_match(next);
            return LoopSignal::Continue;
        }
        if key.modifiers == KeyModifiers::CONTROL.union(KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('g')
        {
            let prev = self.find.as_mut().and_then(FindState::prev);
            self.jump_to_match(prev);
            return LoopSignal::Continue;
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('r') {
            if let Some(find) = self.find.as_mut() {
                find.enable_replace_mode();
            }
            return LoopSignal::Continue;
        }
        if key.modifiers == KeyModifiers::CONTROL.union(KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('r')
        {
            self.replace_all_matches();
            return LoopSignal::Continue;
        }
        match key.code {
            KeyCode::Esc => {
                self.find = None;
            }
            KeyCode::Backspace => {
                if let Some(text) = self.active_buffer().map(|b| b.buffer.text().to_string()) {
                    if let Some(find) = self.find.as_mut() {
                        find.pop_char(&text);
                    }
                }
            }
            KeyCode::Tab => {
                if let Some(find) = self.find.as_mut() {
                    find.toggle_field();
                }
            }
            KeyCode::Enter => {
                let replace_mode = self.find.as_ref().is_some_and(FindState::replace_mode);
                let field = self.find.as_ref().map(FindState::field);
                if replace_mode && field == Some(FindField::Replacement) {
                    self.replace_current_match();
                } else {
                    let current = self.find.as_ref().and_then(FindState::current_match);
                    self.jump_to_match(current);
                    if !replace_mode {
                        self.find = None;
                    }
                }
            }
            // Any other `Ctrl`-held combo falls through to the wildcard
            // arm below (ignored), never typed into the query.
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(text) = self.active_buffer().map(|b| b.buffer.text().to_string()) {
                    if let Some(find) = self.find.as_mut() {
                        find.push_char(c, &text);
                    }
                }
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// No-op on `None`. On `Some(range)`, selects the whole match and
    /// top-aligns its line in the viewport (`docs/features/tui-find.md`
    /// §2.2/§4.3 -- unconditional, since `app.rs` has no access to
    /// `text_area`'s height to decide whether a conditional scroll would
    /// even be necessary). Clears `desired_column` -- like every other
    /// cursor-moving operation in this file that isn't a vertical arrow
    /// move (`Char`/`Enter`/`Tab`/`Backspace`/`Delete` above), a jump must
    /// not leave a stale sticky column in place for the next `Up`/`Down`
    /// to snap back to.
    fn jump_to_match(&mut self, range: Option<Range<usize>>) {
        let Some(range) = range else {
            return;
        };
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        let (line, _) = cursor_line_column(buf.buffer.text_buffer(), range.start);
        Self::scroll_to_and_reveal(buf, line);
        buf.desired_column = None;
        buf.buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(range.start, range.end)));
    }

    /// `Enter` while the replacement field is focused in replace mode
    /// (`docs/features/tui-replace.md` §2.2/§3.3): builds the replace
    /// transaction, applies it via `Buffer::apply` (marks dirty, pushes
    /// one undo step -- no separate `mark_dirty` call needed), re-syncs
    /// `FindState` against the buffer's new text, then jumps to whatever
    /// is now the current match. No-op if there's no current match to
    /// replace. Never closes the bar -- the caller (`handle_find_key`)
    /// keeps it open so a repeated `Enter` replaces the next occurrence.
    fn replace_current_match(&mut self) {
        let Some(text) = self.active_buffer().map(|b| b.buffer.text().to_string()) else {
            return;
        };
        let Some(transaction) = self.find.as_ref().and_then(|f| f.replace_current(&text)) else {
            return;
        };
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        buf.buffer.apply(transaction);
        let new_text = buf.buffer.text().to_string();
        if let Some(find) = self.find.as_mut() {
            find.resync(&new_text);
        }
        self.sync_lsp_did_change();
        let current = self.find.as_ref().and_then(FindState::current_match);
        self.jump_to_match(current);
    }

    /// `Ctrl+Shift+R` (`docs/features/tui-replace-all.md` §2.2/§3.1):
    /// replaces every match with the current replacement text as one
    /// undo step, regardless of `replace_mode`/focused field. No-op if
    /// there's nothing to replace. `enable_replace_mode()` is forced
    /// unconditionally so `status_text`'s replace-mode branch -- the only
    /// one that renders a truncation notice -- is always what's visible
    /// immediately afterward. `resync` must run before
    /// `note_replace_all_result`: `resync`'s own `refresh` resets the
    /// truncation flag as a side effect, so calling it after would
    /// immediately clobber the fresh result back to `false`.
    fn replace_all_matches(&mut self) {
        let Some(text) = self.active_buffer().map(|b| b.buffer.text().to_string()) else {
            return;
        };
        let Some(ReplaceResult {
            transaction,
            truncated,
        }) = self.find.as_ref().and_then(|f| f.replace_all(&text))
        else {
            return;
        };
        let Some(buf) = self.active_buffer_mut() else {
            return;
        };
        buf.buffer.apply(transaction);
        let new_text = buf.buffer.text().to_string();
        if let Some(find) = self.find.as_mut() {
            find.enable_replace_mode();
            find.resync(&new_text);
            find.note_replace_all_result(truncated);
        }
        self.sync_lsp_did_change();
        let current = self.find.as_ref().and_then(FindState::current_match);
        self.jump_to_match(current);
    }

    fn open_palette(&mut self) {
        self.palette = Some(PaletteState {
            query: String::new(),
            filtered: commands().iter().collect(),
            selected: 0,
        });
    }

    fn handle_palette_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(palette) = self.palette.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => {
                self.palette = None;
            }
            KeyCode::Up => {
                if palette.selected > 0 {
                    palette.selected -= 1;
                }
            }
            KeyCode::Down => {
                if palette.selected + 1 < palette.filtered.len() {
                    palette.selected += 1;
                }
            }
            KeyCode::Enter => {
                let action = palette.filtered.get(palette.selected).map(|c| c.action);
                self.palette = None;
                if let Some(action) = action {
                    return self.run_action(action);
                }
            }
            KeyCode::Backspace => {
                palette.query.pop();
                self.refilter_palette();
            }
            KeyCode::Char(c) => {
                palette.query.push(c);
                self.refilter_palette();
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    fn refilter_palette(&mut self) {
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        let query = palette.query.to_lowercase();
        palette.filtered = commands()
            .iter()
            .filter(|c| c.title.to_lowercase().contains(&query))
            .collect();
        palette.selected = 0;
    }

    /// `ToggleKeymapSettings` command (`docs/features/tui-keymap.md`
    /// §2.4/§2.5): opens/closes the Keymap popup, closing every other
    /// overlay first (same convention `toggle_todo_panel` etc. already
    /// establish).
    fn toggle_keymap_popup(&mut self) {
        let opening = self.keymap_popup.is_none();
        self.close_all_overlays();
        if opening {
            self.keymap_popup = Some(KeymapPopupState {
                query: String::new(),
                selected: 0,
                capturing: None,
            });
        }
    }

    /// Every `commands()` entry whose title, id, or effective-binding
    /// label contains the popup's query, case-insensitively -- empty
    /// query returns every command (`docs/features/tui-keymap.md` §2.5,
    /// a direct port of `ide-ui`'s own `keymap.md` §3.6 text-search
    /// approach).
    pub(crate) fn keymap_popup_rows(&self) -> Vec<&'static Command> {
        let query = self
            .keymap_popup
            .as_ref()
            .map(|s| s.query.to_lowercase())
            .unwrap_or_default();
        commands()
            .iter()
            .filter(|c| {
                if query.is_empty() {
                    return true;
                }
                if c.title.to_lowercase().contains(&query) || c.id.to_lowercase().contains(&query) {
                    return true;
                }
                self.keymap
                    .effective_binding(c.id)
                    .map(|chord| keymap::label(chord).to_lowercase().contains(&query))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn handle_keymap_popup_key(&mut self, key: KeyEvent) -> LoopSignal {
        if let Some(state) = self.keymap_popup.as_ref() {
            if let Some(id) = state.capturing {
                return self.handle_keymap_capture_key(id, key);
            }
        }
        let Some(state) = self.keymap_popup.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.keymap_popup = None,
            KeyCode::Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            KeyCode::Down => {
                let len = self.keymap_popup_rows().len();
                let state = self.keymap_popup.as_mut().unwrap();
                if state.selected + 1 < len {
                    state.selected += 1;
                }
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
            }
            KeyCode::Enter => self.start_keymap_capture(),
            KeyCode::Delete => self.reset_selected_keymap_binding(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.query.push(c);
                state.selected = 0;
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// The one place `self.keymap` is written to disk -- routes through
    /// `keymap_path_override` when a test has set one, otherwise the real
    /// `keymap::save` (`$HOME/.config/ide-tui/keymap.json`).
    fn persist_keymap(&self) {
        match &self.keymap_path_override {
            Some(path) => keymap::save_to(path, &self.keymap),
            None => keymap::save(&self.keymap),
        }
    }

    /// Enters capture mode for the currently-selected row -- no-op if
    /// nothing is selected (e.g. an empty filtered list).
    fn start_keymap_capture(&mut self) {
        let Some(state) = self.keymap_popup.as_ref() else {
            return;
        };
        let id = self.keymap_popup_rows().get(state.selected).map(|c| c.id);
        if let Some(id) = id {
            if let Some(state) = self.keymap_popup.as_mut() {
                state.capturing = Some(id);
            }
        }
    }

    /// `Delete` on a row: `self.keymap.reset(id)` + persist + notify
    /// (`docs/features/tui-keymap.md` §2.5/§3.4). No-op if nothing is
    /// selected.
    fn reset_selected_keymap_binding(&mut self) {
        let Some(state) = self.keymap_popup.as_ref() else {
            return;
        };
        let id = self.keymap_popup_rows().get(state.selected).map(|c| c.id);
        if let Some(id) = id {
            self.keymap.reset(id);
            self.persist_keymap();
            self.notify(format!("Reset \"{id}\" to its default binding."));
        }
    }

    /// The next raw key event while `keymap_popup.capturing` is
    /// `Some(id)` -- `Esc` cancels without assigning anything (bare `Esc`
    /// stays "close/cancel" everywhere in this crate, per `docs/features/
    /// tui-keymap.md` §2.5); any other key becomes `id`'s new binding
    /// immediately, no confirm step (§1.1/§3.3 of that doc).
    fn handle_keymap_capture_key(&mut self, id: &'static str, key: KeyEvent) -> LoopSignal {
        if key.code == KeyCode::Esc {
            if let Some(state) = self.keymap_popup.as_mut() {
                state.capturing = None;
            }
            return LoopSignal::Continue;
        }
        let chord = (key.modifiers, key.code);
        let conflicts = self.keymap.conflicts(id, chord);
        self.keymap.set_override(id, Some(chord));
        self.persist_keymap();
        if let Some(state) = self.keymap_popup.as_mut() {
            state.capturing = None;
        }
        if conflicts.is_empty() {
            self.notify(format!(
                "\"{id}\" is now bound to {}.",
                keymap::label(chord)
            ));
        } else {
            self.notify(format!(
                "\"{id}\" is now bound to {} (shared with {}).",
                keymap::label(chord),
                conflicts.join(", ")
            ));
        }
        LoopSignal::Continue
    }

    /// `NewScratchFile` command (`docs/features/tui-scratch-files.md`
    /// §2.2): opens the name-entry prompt.
    fn toggle_new_scratch_file(&mut self) {
        let opening = self.new_scratch_file.is_none();
        self.close_all_overlays();
        if opening {
            self.new_scratch_file = Some(NewScratchFileState {
                name: String::new(),
            });
        }
    }

    fn handle_new_scratch_file_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.new_scratch_file.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.new_scratch_file = None,
            KeyCode::Backspace => {
                state.name.pop();
            }
            KeyCode::Enter => self.confirm_new_scratch_file(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.name.push(c);
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    /// Validates and resolves the typed name (`scratch::new_scratch_path`),
    /// creates an empty file only if one doesn't already exist at that
    /// path (never truncates an existing scratch file, `docs/features/
    /// tui-scratch-files.md` §3.1), then opens it through the ordinary
    /// `open_or_focus_tab` path. Leaves the prompt open with a
    /// notification on any failure, rather than closing it silently.
    fn confirm_new_scratch_file(&mut self) {
        let Some(state) = self.new_scratch_file.as_ref() else {
            return;
        };
        match scratch::new_scratch_path(&state.name) {
            Ok(Some(path)) => {
                if !path.exists() {
                    if let Err(err) = std::fs::write(&path, "") {
                        self.notify(format!("could not create scratch file: {err}"));
                        return;
                    }
                }
                self.new_scratch_file = None;
                if let Err(err) = self.open_or_focus_tab(path) {
                    self.notify(err.to_string());
                    return;
                }
                let offset = self.active_caret_offset();
                self.push_nav_location(offset);
            }
            Ok(None) => self.notify("could not resolve a scratch files directory (no $HOME)."),
            Err(err) => self.notify(err.to_string()),
        }
    }

    /// `ScratchFiles` command: opens the browse-list popup.
    fn toggle_scratch_files(&mut self) {
        let opening = self.scratch_files.is_none();
        self.close_all_overlays();
        if opening {
            self.scratch_files = Some(ScratchFilesState {
                query: String::new(),
                selected: 0,
            });
        }
    }

    /// Every scratch file whose file name contains the popup's query,
    /// case-insensitively -- empty query returns every scratch file,
    /// same shape `keymap_popup_rows`/`recent_files_rows` already
    /// establish.
    pub(crate) fn scratch_files_rows(&self) -> Vec<PathBuf> {
        let query = self
            .scratch_files
            .as_ref()
            .map(|s| s.query.to_lowercase())
            .unwrap_or_default();
        scratch::list_scratch_files()
            .into_iter()
            .filter(|p| {
                query.is_empty()
                    || p.file_name()
                        .map(|n| n.to_string_lossy().to_lowercase().contains(&query))
                        .unwrap_or(false)
            })
            .collect()
    }

    fn handle_scratch_files_key(&mut self, key: KeyEvent) -> LoopSignal {
        let Some(state) = self.scratch_files.as_mut() else {
            return LoopSignal::Continue;
        };
        match key.code {
            KeyCode::Esc => self.scratch_files = None,
            KeyCode::Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            KeyCode::Down => {
                let len = self.scratch_files_rows().len();
                let state = self.scratch_files.as_mut().unwrap();
                if state.selected + 1 < len {
                    state.selected += 1;
                }
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
            }
            KeyCode::Enter => self.confirm_scratch_file(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.query.push(c);
                state.selected = 0;
            }
            _ => {}
        }
        LoopSignal::Continue
    }

    fn confirm_scratch_file(&mut self) {
        let Some(state) = self.scratch_files.as_ref() else {
            return;
        };
        let rows = self.scratch_files_rows();
        let Some(path) = rows.get(state.selected).cloned() else {
            return;
        };
        if let Err(err) = self.open_or_focus_tab(path) {
            self.notify(err.to_string());
            return;
        }
        let offset = self.active_caret_offset();
        self.push_nav_location(offset);
        self.scratch_files = None;
    }
}

/// Maps `config.indent_style`/`config.indent_size` onto
/// `IndentUnit::default()` -- only the fields the config actually set
/// override the default, mirroring `ide-ui`'s own
/// `Tab::apply_editor_config` (`docs/features/
/// tui-line-commands-and-editorconfig.md` §3.5).
fn indent_unit_for(config: &EditorConfig) -> IndentUnit {
    let mut unit = IndentUnit::default();
    if let Some(style) = config.indent_style {
        unit.style = style;
    }
    if let Some(width) = config.indent_size {
        unit.width = width;
    }
    unit
}

/// Typing one printable character: type-over a closer the previous
/// keystroke auto-inserted, opening a delimiter (auto-close or surround),
/// or -- when neither applies -- plain insertion, the only path that still
/// coalesces consecutive keystrokes into one undo step
/// (`docs/features/tui-smart-editing.md` §3.2). `auto_closed` is the
/// window `handle_editor_key` already took out of `buf` before dispatching
/// here; only this function may write a fresh one back.
/// Builds one `Transaction` from `per_selection`'s answer for every
/// selection in `buf`'s current `Selections` (in existing order, which
/// `Selections`' own invariant guarantees is sorted by `start()`), applies
/// it once, and re-derives every selection's post-edit position directly
/// from the sorted, non-overlapping change list this function already
/// built (`docs/features/tui-multiple-cursors.md` §2.2/§3.4).
///
/// `per_selection(text_buffer, selection)` returns `(range_to_replace,
/// replacement_text, anchor_offset_into_replacement,
/// head_offset_into_replacement)`. A bare-caret result sets the two
/// offsets equal. A true no-op for one selection returns its own empty
/// range at its own head, an empty replacement, and `(0, 0)` -- so the
/// entries list always has exactly one entry per original selection and
/// `primary_index` never needs adjusting for a skipped entry.
///
/// Returns `false` (no `Transaction` built or applied, no `set_selections`
/// call at all) when every entry is a true identity, or when any two
/// selections' own derived ranges overlap (`Transaction::new` rejects
/// that) -- a deliberate all-or-nothing fallback (§3.5): leave the buffer
/// and every selection completely untouched rather than partially apply
/// the batch.
fn apply_per_selection(
    buf: &mut OpenBuffer,
    mut per_selection: impl FnMut(&OpenBuffer, Selection) -> (Range<usize>, String, usize, usize),
) -> bool {
    let selections = buf.buffer.text_buffer().selections().clone();
    let entries: Vec<(Range<usize>, String, usize, usize)> = selections
        .all()
        .iter()
        .map(|selection| per_selection(buf, *selection))
        .collect();
    if entries
        .iter()
        .all(|(range, insert, _, _)| range.is_empty() && insert.is_empty())
    {
        return false;
    }
    let changes: Vec<Change> = entries
        .iter()
        .map(|(range, insert, _, _)| Change::new(range.clone(), insert.clone()))
        .collect();
    let Ok(transaction) = Transaction::new(changes) else {
        return false;
    };
    buf.buffer.apply(transaction);
    let mut shift: isize = 0;
    let mut carets = Vec::with_capacity(entries.len());
    for (range, insert, anchor_rel, head_rel) in &entries {
        let new_start = range.start as isize + shift;
        let anchor = (new_start + *anchor_rel as isize) as usize;
        let head = (new_start + *head_rel as isize) as usize;
        carets.push(Selection::new(anchor, head));
        shift += insert.len() as isize - (range.end - range.start) as isize;
    }
    let primary_index = selections
        .primary_index()
        .min(carets.len().saturating_sub(1));
    buf.buffer
        .text_buffer_mut()
        .set_selections(Selections::new(carets, primary_index));
    true
}

fn insert_char(buf: &mut OpenBuffer, c: char, auto_closed: Option<usize>) -> bool {
    let Some(rules) = buf.buffer.text_buffer().syntax() else {
        buf.buffer.text_buffer_mut().type_text(&c.to_string());
        buf.buffer.mark_dirty();
        return true;
    };

    // Unconditional on `c` being an opener: type-over fires for a *closer*
    // the previous keystroke auto-inserted, and a closing character never
    // has a `closer_for` mapping of its own.
    if types_over(buf.buffer.text_buffer(), c, auto_closed) {
        move_past(buf, c);
        return false;
    }
    if let Some(close) = closer_for(rules, c) {
        buf.auto_closed = open_delimiter(buf, c, close, rules);
        return true;
    }
    buf.buffer.text_buffer_mut().type_text(&c.to_string());
    buf.buffer.mark_dirty();
    true
}

/// `tui-smart-editing.md` §3.2's type-over: only when the caret sits
/// exactly where the last keystroke auto-closed a pair, and the character
/// about to be typed matches what already follows it.
fn types_over(buffer: &TextBuffer, c: char, auto_closed: Option<usize>) -> bool {
    let selection = buffer.selections().primary();
    auto_closed.is_some_and(|offset| {
        selection.is_empty()
            && selection.head == offset
            && buffer.text()[selection.head..].starts_with(c)
    })
}

/// Moves the primary's caret past the closer it's sitting on, without
/// touching the buffer -- a type-over is a cursor move, not an edit
/// (`changed = false` at the call site). Every other selection is left
/// exactly where it is (`docs/features/tui-multiple-cursors.md` §3.4) --
/// `types_over` only ever fires against the primary (`auto_closed` is a
/// single, primary-only slot, §1.2's documented scope cut), so only the
/// primary has anywhere to move past.
fn move_past(buf: &mut OpenBuffer, c: char) {
    let mut selections = buf.buffer.text_buffer().selections().clone();
    let idx = selections.primary_index();
    let mut ranges = selections.all().to_vec();
    let head = ranges[idx].head;
    ranges[idx] = Selection::caret(head + c.len_utf8());
    selections = Selections::new(ranges, idx);
    buf.buffer.text_buffer_mut().set_selections(selections);
}

/// Typing an opening bracket or quote: every non-empty selection is
/// wrapped (covering its original text, not the new delimiters) via
/// `TextBuffer::surround_selections`, already multi-selection-safe at the
/// core level (`docs/features/tui-multiple-cursors.md` §1.2 -- this
/// replaces a from-scratch reimplementation `T18a` left inline instead of
/// calling the core method that already existed for it). If every
/// selection is empty, each becomes its own auto-closed pair or bare
/// `open` depending on `may_open_pair`/`is_quoted_or_commented`, evaluated
/// against its own position (`tui-smart-editing.md` §3.2/§3.3) via
/// `apply_per_selection`. Returns the *primary*'s auto-closed caret
/// offset, if any -- `OpenBuffer::auto_closed` stays a single slot, a
/// deliberate scope cut (§3.4 of the multi-cursor doc): typing over a
/// multi-cursor auto-close types over only the primary's pair.
fn open_delimiter(
    buf: &mut OpenBuffer,
    open: char,
    close: char,
    rules: &SyntaxRules,
) -> Option<usize> {
    if buf
        .buffer
        .text_buffer_mut()
        .surround_selections(open, close)
    {
        buf.buffer.mark_dirty();
        return None;
    }
    let mut primary_admits = false;
    apply_per_selection(buf, |buf, selection| {
        let text_buffer = buf.buffer.text_buffer();
        let head = selection.head;
        let quote = rules.string_quotes.contains(&open);
        let admits = may_open_pair(text_buffer.text(), head, rules)
            && !(quote && is_quoted_or_commented(text_buffer, head));
        if selection == text_buffer.selections().primary() {
            primary_admits = admits;
        }
        let inserted = if admits {
            format!("{open}{close}")
        } else {
            open.to_string()
        };
        (head..head, inserted, open.len_utf8(), open.len_utf8())
    });
    primary_admits.then(|| buf.buffer.text_buffer().selections().primary().head)
}

/// `Enter`: a newline carrying the new line's indentation, with the `{|}`
/// case carrying a second line for the closer (`tui-smart-editing.md`
/// §3.1), computed independently per selection via `apply_per_selection`
/// (`docs/features/tui-multiple-cursors.md` §3.4).
fn insert_newline_with_indent(buf: &mut OpenBuffer) -> bool {
    let unit = buf.indent;
    apply_per_selection(buf, |buf, selection| {
        let text_buffer = buf.buffer.text_buffer();
        let at = selection.start();
        let rules = text_buffer.syntax();
        let first = newline_indent(text_buffer.text(), text_buffer.lines(), at, rules, unit);
        let full = if selection.is_empty() && splits_a_pair(text_buffer.text(), at, rules) {
            // `None` rather than `rules`: the closer's line wants the
            // *current* line's indent verbatim, exactly what this call
            // does with nothing to reason about.
            let closer_line =
                newline_indent(text_buffer.text(), text_buffer.lines(), at, None, unit);
            format!("{first}{closer_line}")
        } else {
            first.clone()
        };
        let caret = first.len();
        (selection.range(), full, caret, caret)
    })
}

/// `Backspace`: the ordinary one-character (or whole-selection) delete,
/// except an empty selection sitting between a matching bracket pair
/// deletes both halves (`tui-smart-editing.md` §3.2), computed
/// independently per selection via `apply_per_selection` (`docs/features/
/// tui-multiple-cursors.md` §3.4).
fn delete_backward(buf: &mut OpenBuffer) -> bool {
    apply_per_selection(buf, |buf, selection| {
        let text_buffer = buf.buffer.text_buffer();
        let range = if !selection.is_empty() {
            selection.range()
        } else {
            let head = selection.head;
            let text = text_buffer.text();
            let pair = text_buffer.syntax().and_then(|rules| {
                let before = text[..head].chars().next_back()?;
                let after = text[head..].chars().next()?;
                rules
                    .brackets
                    .contains(&(before, after))
                    .then(|| head - before.len_utf8()..head + after.len_utf8())
            });
            pair.unwrap_or_else(|| {
                // Fold-aware for the same reason `KeyCode::Delete`'s
                // forward case is (`tui-code-folding.md` §3.6) --
                // otherwise Backspace at the start of the row right after
                // a collapsed fold would silently delete into its hidden
                // interior. `buf` (not `text_buffer`) is fine here:
                // `move_caret_with_folds` only reads `buf.buffer`/
                // `buf.folded`/`buf.desired_column`, none of which
                // `apply_per_selection` has mutated yet at this point.
                let (start, _) = App::move_caret_with_folds(buf, head, Direction::Left);
                start.min(head)..start.max(head)
            })
        };
        (range, String::new(), 0, 0)
    })
}

/// `Tab`: one indent unit at each empty selection's own head, otherwise
/// `indent_selection_lines` over every touched line -- any non-empty
/// selection already qualifies, since a single point can never itself
/// span a line boundary (`tui-smart-editing.md` §3.4).
/// `indent_selection_lines` is already multi-selection-safe at the core
/// level; the empty-selection branch goes through `apply_per_selection`
/// (`docs/features/tui-multiple-cursors.md` §3.4).
fn indent_or_insert_tab(buf: &mut OpenBuffer) -> bool {
    let selections = buf.buffer.text_buffer().selections().clone();
    let unit = buf.indent;
    if selections.all().iter().all(|s| s.is_empty()) {
        let one = unit.one().into_owned();
        apply_per_selection(buf, |_, selection| {
            (
                selection.head..selection.head,
                one.clone(),
                one.len(),
                one.len(),
            )
        })
    } else {
        let changed = buf.buffer.text_buffer_mut().indent_selection_lines(unit);
        if changed {
            buf.buffer.mark_dirty();
        }
        changed
    }
}

/// `Shift+Tab` (`crossterm::event::KeyCode::BackTab`, never `Tab` plus a
/// shift modifier -- see this crate's `handle_editor_key` for why that
/// matters): always `outdent_selection_lines`, regardless of selection
/// state.
fn outdent_lines(buf: &mut OpenBuffer) -> bool {
    let unit = buf.indent;
    let changed = buf.buffer.text_buffer_mut().outdent_selection_lines(unit);
    if changed {
        buf.buffer.mark_dirty();
    }
    changed
}

/// `Range { start: (0, 0), end: <end of `text`> }` -- the whole-document
/// range `request_inlay_hints` always queries with (v1 doesn't scope to
/// the visible viewport, `docs/features/tui-hover-and-inlay-hints.md`
/// §2.2), ported from `ide-ui`'s own `sync_inlay_hints`. `None` only if
/// `text`'s length itself isn't a valid UTF-16 position (shouldn't happen
/// for any buffer this crate can load, but `byte_offset_to_position` is
/// fallible so this stays fallible too rather than unwrapping).
fn whole_document_range(text: &str) -> Option<ide_lsp::Range> {
    let end = ide_lsp::byte_offset_to_position(text, text.len())?;
    Some(ide_lsp::Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end,
    })
}

/// Converts a `WorkspaceEdit`'s `TextEdit`s (LSP-`Position`-addressed)
/// against `text`'s *current* content into one `ide_core::Transaction`
/// (byte-offset-addressed) -- `None` if any edit's range doesn't convert.
/// Uses `ide_lsp::position_to_byte_offset` directly, the same way every
/// other position-to-offset conversion already merged into this crate
/// does, rather than `ide-ui`'s own locally-improved `LineIndex`-based
/// variant (`docs/features/tui-code-actions-and-rename.md` §1 explains why
/// this is a deliberate, documented consistency choice, not an oversight).
fn workspace_text_edits_to_transaction(
    text: &str,
    text_edits: &[ide_lsp::TextEdit],
) -> Option<ide_core::Transaction> {
    let mut changes = Vec::with_capacity(text_edits.len());
    for edit in text_edits {
        let start = ide_lsp::position_to_byte_offset(text, edit.range.start)?;
        let end = ide_lsp::position_to_byte_offset(text, edit.range.end)?;
        changes.push(ide_core::Change::new(start..end, edit.new_text.clone()));
    }
    ide_core::Transaction::new(changes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::binding_for;
    use crate::ui;
    use crossterm::event::KeyEventState;
    use ratatui::layout::Rect;
    use std::fs;

    fn key(modifiers: KeyModifiers, code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn plain_key(code: KeyCode) -> KeyEvent {
        key(KeyModifiers::NONE, code)
    }

    fn ctrl(c: char) -> KeyEvent {
        key(KeyModifiers::CONTROL, KeyCode::Char(c))
    }

    /// A one-file Rust project, opened and focused in the editor -- every
    /// T18a test needs `syntax_for_path` to resolve to `RUST`'s
    /// `SyntaxRules` (`sample_project`'s `.txt` files never do), since
    /// auto-close/auto-indent/matching-bracket are all no-ops without them.
    fn open_rust_tab(text: &str) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.rs"), text).unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Enter)); // the only row: f.rs
        app.handle_key(ctrl('t')); // focus editor
        (dir, app)
    }

    /// Like `open_rust_tab`, but with a `.editorconfig` file at the
    /// project root -- for T18b's resolve-on-open and save-time tests.
    fn open_rust_tab_with_editorconfig(text: &str, editorconfig: &str) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".editorconfig"), editorconfig).unwrap();
        fs::write(dir.path().join("f.rs"), text).unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        // The tree now has two rows ("f.rs" first -- files sort before
        // dotfiles alphabetically is not guaranteed, so move by title via
        // repeated Down/Enter is unsafe; instead open by path directly,
        // the same private entry point `open_or_focus_tab` already is).
        app.open_or_focus_tab(dir.path().join("f.rs")).unwrap();
        app.focus = Focus::Editor;
        (dir, app)
    }

    /// A fresh, empty repo (no commits yet) -- for git-gutter tests that
    /// need to commit under their own filename. `run_git`/`git_commit`
    /// (T11's own helpers, above) do the actual `git` subprocess work.
    fn git_repo_without_commits() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        dir
    }

    /// Opens `name` (already committed inside `dir`, a real repo) as the
    /// active tab -- `App::new` calls `git.refresh` on the project root
    /// itself, so no separate `git.refresh` call is needed here.
    fn open_committed_tab(dir: &Path, name: &str) -> App {
        let mut app = App::new(dir.to_path_buf()).unwrap();
        app.open_or_focus_tab(dir.join(name)).unwrap();
        app.focus = Focus::Editor;
        app
    }

    fn set_caret(app: &mut App, offset: usize) {
        app.active_buffer_mut()
            .unwrap()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(offset)));
    }

    fn set_selection(app: &mut App, range: Range<usize>) {
        app.active_buffer_mut()
            .unwrap()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(range.start, range.end)));
    }

    fn active_text(app: &App) -> String {
        app.active_buffer().unwrap().buffer.text().to_string()
    }

    fn caret(app: &App) -> usize {
        app.active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .head
    }

    fn sample_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello\nworld").unwrap();
        fs::write(dir.path().join("b.txt"), "second file").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/c.txt"), "nested").unwrap();
        dir
    }

    #[test]
    fn new_opens_the_project_and_scans_the_tree() {
        let dir = sample_project();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(app.focus, Focus::Tree);
        assert!(app.tabs.is_empty());
        assert!(app.active_tab.is_none());
        assert_eq!(
            app.project_root().to_path_buf(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn new_on_a_missing_directory_errors() {
        let result = App::new(PathBuf::from("/does/not/exist/anywhere"));
        assert!(result.is_err());
    }

    #[test]
    fn enter_on_a_file_row_opens_it_into_a_new_tab() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        // Rows are sorted dirs-before-files (per Project::scan_tree): "sub",
        // then "a.txt", then "b.txt" -- move down once to land on "a.txt".
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.active_tab, Some(0));
        let buf = app.active_buffer().expect("a.txt should now be open");
        // `Project::open` canonicalizes the root, so every `DirEntry.path`
        // is canonical too -- compare against the canonical form, not the
        // raw `TempDir` path (which on macOS differs by a `/private`
        // prefix from its own canonicalization).
        assert_eq!(buf.path, dir.path().canonicalize().unwrap().join("a.txt"));
    }

    #[test]
    fn opening_a_second_file_while_the_first_is_dirty_opens_a_new_tab_instead_of_blocking() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t')); // focus editor
        app.handle_key(plain_key(KeyCode::Char('x'))); // dirty tab 0
        app.handle_key(ctrl('t')); // back to tree

        app.handle_key(plain_key(KeyCode::Down)); // b.txt
        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab, Some(1));
        assert!(app.status().is_none());
        // tab 0 (a.txt) is untouched -- still dirty, edit intact.
        assert!(app.tabs[0].buffer.is_dirty());
        assert!(app.tabs[0].buffer.text().starts_with('x'));
    }

    #[test]
    fn reopening_an_already_open_dirty_file_switches_to_it_without_reloading_from_disk() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Char('x'))); // dirty it, in-memory text now "xhello\nworld"
        let dirtied_text = app.active_buffer().unwrap().buffer.text().to_string();
        app.handle_key(ctrl('t')); // back to tree, still selected on a.txt

        app.handle_key(plain_key(KeyCode::Enter)); // re-open a.txt

        assert_eq!(app.tabs.len(), 1, "must not have pushed a second tab");
        assert_eq!(app.active_tab, Some(0));
        assert_eq!(app.active_buffer().unwrap().buffer.text(), dirtied_text);
        assert!(app.active_buffer().unwrap().buffer.is_dirty());
    }

    #[test]
    fn save_clears_dirty_and_persists_to_disk() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Char('!')));
        assert!(app.active_buffer().unwrap().buffer.is_dirty());

        app.handle_key(ctrl('s'));
        assert!(!app.active_buffer().unwrap().buffer.is_dirty());
        let saved = fs::read_to_string(dir.path().join("a.txt")).unwrap();
        assert!(saved.starts_with('!'));
    }

    #[test]
    fn undo_reverts_the_last_edit() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        let before = app.active_buffer().unwrap().buffer.text().to_string();
        app.handle_key(plain_key(KeyCode::Char('!')));
        assert_ne!(app.active_buffer().unwrap().buffer.text(), before);

        app.handle_key(ctrl('z'));
        assert_eq!(app.active_buffer().unwrap().buffer.text(), before);
    }

    #[test]
    fn redo_reapplies_an_undone_edit() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Char('!')));
        let after_type = app.active_buffer().unwrap().buffer.text().to_string();

        app.handle_key(ctrl('z'));
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('z'),
        ));
        assert_eq!(app.active_buffer().unwrap().buffer.text(), after_type);
    }

    // -- word/line/document caret motion
    // (`tui-word-and-document-navigation.md`) --

    #[test]
    fn home_and_end_move_to_line_boundaries() {
        let (_dir, mut app) = open_rust_tab("hello\nworld");
        set_caret(&mut app, 8); // mid "world"
        app.handle_key(plain_key(KeyCode::Home));
        assert_eq!(caret(&app), 6); // start of "world"
        app.handle_key(plain_key(KeyCode::End));
        assert_eq!(caret(&app), 11); // end of "world"
    }

    #[test]
    fn ctrl_left_and_right_move_by_word() {
        let (_dir, mut app) = open_rust_tab("foo bar baz");
        set_caret(&mut app, 0);
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Right));
        assert_eq!(caret(&app), 3); // right after "foo"
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Right));
        assert_eq!(caret(&app), 7); // right after "bar"
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Left));
        assert_eq!(caret(&app), 4); // start of "bar"
    }

    #[test]
    fn ctrl_home_and_end_move_to_document_boundaries() {
        let (_dir, mut app) = open_rust_tab("foo\nbar\nbaz");
        set_caret(&mut app, 5);
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Home));
        assert_eq!(caret(&app), 0);
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::End));
        assert_eq!(caret(&app), 11);
    }

    #[test]
    fn extended_motions_clear_desired_column() {
        let (_dir, mut app) = open_rust_tab("hello\nworld");
        // Establish a sticky column via a vertical move first.
        app.handle_key(plain_key(KeyCode::Right));
        app.handle_key(plain_key(KeyCode::Down));
        assert!(app.active_buffer().unwrap().desired_column.is_some());
        app.handle_key(plain_key(KeyCode::Home));
        assert!(app.active_buffer().unwrap().desired_column.is_none());
    }

    #[test]
    fn extended_motions_move_every_selection_not_just_the_primary() {
        let (_dir, mut app) = open_rust_tab("foo bar\nfoo bar");
        {
            let buf = app.active_buffer_mut().unwrap();
            buf.buffer.text_buffer_mut().set_selections(Selections::new(
                vec![Selection::caret(1), Selection::caret(9)],
                1,
            ));
        }
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Right));
        let offsets: Vec<usize> = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .all()
            .iter()
            .map(|s| s.head)
            .collect();
        assert_eq!(offsets, vec![3, 11]);
    }

    #[test]
    fn end_redirects_out_of_a_collapsed_folds_hidden_interior() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        let foo_start = line_of(&app, "fn foo");
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());
        app.handle_key(ctrl('-')); // collapses "fn foo() { ... }", caret lands on foo_start
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        assert_eq!(cursor_line_column(text_buffer, caret(&app)).0, foo_start);

        app.handle_key(plain_key(KeyCode::End));
        // "End" on the fold's own visible start_line lands at that line's
        // own end -- it's still a visible line, so no redirect fires; this
        // proves the fold-aware path at least does not panic or escape
        // into the hidden interior.
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        let (line, _) = cursor_line_column(text_buffer, caret(&app));
        assert_eq!(line, foo_start);
    }

    #[test]
    fn ctrl_end_redirects_out_of_a_trailing_collapsed_fold() {
        // No trailing newline: the fold's own `end_line` really is the
        // buffer's true last line, so `buffer.text().len()` (`Ctrl+End`'s
        // raw target) lands inside its hidden interior with no line after
        // it to redirect into -- must land on the nearest visible line
        // *before* it (the fold's own `start_line`) instead.
        const TRAILING_FOLD_FIXTURE: &str =
            "fn foo() {\n    let x = 1;\n}\nfn bar() {\n    let z = 3;\n}";
        let (_dir, mut app) = open_rust_tab(TRAILING_FOLD_FIXTURE);
        let bar_start = line_of(&app, "fn bar");
        set_caret(&mut app, TRAILING_FOLD_FIXTURE.find("let z").unwrap());
        app.handle_key(ctrl('-'));
        assert!(app.active_buffer().unwrap().folded.contains(&bar_start));

        set_caret(&mut app, 0);
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::End));
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        let (line, _) = cursor_line_column(text_buffer, caret(&app));
        assert_eq!(line, bar_start);
    }

    #[test]
    fn tree_up_moves_selection_back() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Up));
        let rows = app.tree_state.visible_rows(&app.tree);
        assert_eq!(app.tree_state.selected_row(&rows), rows.first());
    }

    #[test]
    fn enter_on_a_directory_row_expands_it_without_opening_a_tab() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        // "sub" (a directory) sorts first -- selection starts there already.
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.tabs.is_empty());
        let rows = app.tree_state.visible_rows(&app.tree);
        assert!(rows.iter().any(|r| r.depth == 1), "sub should be expanded");
    }

    #[test]
    fn opening_a_file_that_fails_to_read_sets_a_status_message() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // select a.txt
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.tabs.is_empty());
        assert!(app.status().is_some());
    }

    #[test]
    fn arrow_keys_move_the_editor_cursor_through_handle_key() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));

        let offset_of = |app: &App| {
            app.active_buffer()
                .unwrap()
                .buffer
                .text_buffer()
                .selections()
                .primary()
                .start()
        };
        assert_eq!(offset_of(&app), 0);
        app.handle_key(plain_key(KeyCode::Right));
        assert_eq!(offset_of(&app), 1);
        app.handle_key(plain_key(KeyCode::Left));
        assert_eq!(offset_of(&app), 0);
        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(offset_of(&app), 6); // start of "world" on line 1
        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(offset_of(&app), 0);
    }

    #[test]
    fn backspace_and_delete_remove_one_character_through_handle_key() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));

        app.handle_key(plain_key(KeyCode::Delete));
        assert_eq!(app.active_buffer().unwrap().buffer.text(), "ello\nworld");

        app.handle_key(plain_key(KeyCode::Right));
        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.active_buffer().unwrap().buffer.text(), "llo\nworld");
    }

    #[test]
    fn backspace_at_offset_zero_and_delete_at_end_of_buffer_are_no_ops() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));

        app.handle_key(plain_key(KeyCode::Backspace));
        let text = app.active_buffer().unwrap().buffer.text().to_string();
        assert_eq!(text, "hello\nworld");
        assert!(!app.active_buffer().unwrap().buffer.is_dirty());

        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Down)); // no-op: last line already
        for _ in 0.."world".len() {
            app.handle_key(plain_key(KeyCode::Right));
        }
        app.handle_key(plain_key(KeyCode::Delete));
        assert_eq!(app.active_buffer().unwrap().buffer.text(), "hello\nworld");
        assert!(!app.active_buffer().unwrap().buffer.is_dirty());
    }

    #[test]
    fn editor_keys_with_no_buffer_open_are_no_ops() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(ctrl('t')); // focus editor, nothing open
        app.handle_key(plain_key(KeyCode::Right));
        app.handle_key(plain_key(KeyCode::Char('x')));
        assert!(app.tabs.is_empty());
    }

    fn app_with_many_lines(line_count: usize) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let text = (0..line_count)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.path().join("a.txt"), text).unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        (dir, app)
    }

    #[test]
    fn arrow_down_past_the_viewport_scrolls_by_the_minimum_needed() {
        let (_dir, mut app) = app_with_many_lines(50);
        app.set_editor_viewport_rows(10);
        assert_eq!(app.active_buffer().unwrap().scroll, 0);

        for _ in 0..15 {
            app.handle_key(plain_key(KeyCode::Down));
        }

        // Cursor is now on line 15; a 10-row viewport starting at 0 only
        // shows lines 0..10, so it must have scrolled down to exactly 6
        // (the minimum that keeps line 15 as the viewport's last row).
        assert_eq!(app.active_buffer().unwrap().scroll, 6);
    }

    #[test]
    fn arrow_up_back_above_the_viewport_scrolls_up_to_top_align() {
        let (_dir, mut app) = app_with_many_lines(50);
        app.set_editor_viewport_rows(10);
        for _ in 0..20 {
            app.handle_key(plain_key(KeyCode::Down));
        }
        let scrolled = app.active_buffer().unwrap().scroll;
        assert!(scrolled > 0, "must have scrolled down first");

        for _ in 0..20 {
            app.handle_key(plain_key(KeyCode::Up));
        }

        assert_eq!(app.active_buffer().unwrap().scroll, 0);
    }

    #[test]
    fn typing_enough_newlines_scrolls_the_viewport_down() {
        let (_dir, mut app) = app_with_many_lines(5);
        app.set_editor_viewport_rows(3);
        for _ in 0..8 {
            app.handle_key(plain_key(KeyCode::Enter));
        }
        assert!(
            app.active_buffer().unwrap().scroll > 0,
            "typing past the viewport's bottom must scroll it"
        );
    }

    #[test]
    fn an_unknown_viewport_height_never_scrolls() {
        // Default (no `set_editor_viewport_rows` call, as every other test
        // in this module relies on): the placeholder `u16::MAX` viewport
        // makes the clamp an unconditional no-op, so existing tests that
        // never call it keep their pre-scroll-follow behavior exactly.
        let (_dir, mut app) = app_with_many_lines(500);
        for _ in 0..400 {
            app.handle_key(plain_key(KeyCode::Down));
        }
        assert_eq!(app.active_buffer().unwrap().scroll, 0);
    }

    #[test]
    fn undo_after_scrolling_away_scrolls_back_to_the_edit_site() {
        let (_dir, mut app) = app_with_many_lines(50);
        app.set_editor_viewport_rows(10);
        for _ in 0..20 {
            app.handle_key(plain_key(KeyCode::Down));
        }
        app.handle_key(plain_key(KeyCode::Char('!')));
        let scroll_after_edit = app.active_buffer().unwrap().scroll;
        assert!(scroll_after_edit > 0);

        for _ in 0..20 {
            app.handle_key(plain_key(KeyCode::Up));
        }
        assert_eq!(app.active_buffer().unwrap().scroll, 0);

        app.handle_key(ctrl('z'));

        assert_eq!(
            app.active_buffer().unwrap().scroll,
            scroll_after_edit,
            "undo restores the cursor to the edit site, which must scroll back into view"
        );
    }

    #[test]
    fn ctrl_shift_close_bracket_cycles_to_the_next_tab_and_wraps() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Down)); // b.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab, Some(1));

        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char(']'),
        ));
        assert_eq!(app.active_tab, Some(0), "should wrap back to tab 0");

        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('['),
        ));
        assert_eq!(app.active_tab, Some(1), "previous-tab should wrap too");
    }

    #[test]
    fn cycle_tab_on_zero_or_one_tabs_is_a_no_op() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char(']'),
        ));
        assert_eq!(app.active_tab, None);

        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char(']'),
        ));
        assert_eq!(app.active_tab, Some(0));
    }

    #[test]
    fn ctrl_w_closes_a_clean_active_tab_and_slides_the_active_index() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt -> tab 0
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Down)); // b.txt -> tab 1
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab, Some(1));

        app.handle_key(ctrl('w'));
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, Some(0));
        assert_eq!(
            app.active_buffer().unwrap().path.file_name().unwrap(),
            "a.txt"
        );
    }

    #[test]
    fn ctrl_w_on_a_dirty_tab_is_blocked_with_a_status_message() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Char('x')));

        app.handle_key(ctrl('w'));

        assert_eq!(app.tabs.len(), 1, "the dirty tab must not be closed");
        assert!(app.status().unwrap().contains("unsaved changes"));
    }

    #[test]
    fn ctrl_w_with_no_active_tab_is_a_no_op() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(ctrl('w'));
        assert!(app.tabs.is_empty());
        assert!(app.active_tab.is_none());
    }

    #[test]
    fn closing_the_last_tab_leaves_focus_unchanged() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        assert_eq!(app.focus, Focus::Editor);

        app.handle_key(ctrl('w'));
        assert!(app.tabs.is_empty());
        assert!(app.active_tab.is_none());
        assert_eq!(app.focus, Focus::Editor, "T2 never auto-switches focus");
    }

    #[test]
    fn each_tab_keeps_its_own_cursor_and_desired_column() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt -> tab 0
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Right)); // tab 0 cursor at offset 1
        app.handle_key(ctrl('t'));

        app.handle_key(plain_key(KeyCode::Down)); // b.txt -> tab 1
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        let tab1_offset = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .start();
        assert_eq!(tab1_offset, 0, "a fresh tab starts at offset 0");

        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('['),
        ));
        let tab0_offset = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .start();
        assert_eq!(tab0_offset, 1, "tab 0's cursor position must be preserved");
    }

    #[test]
    fn palette_up_down_move_the_selection() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('a'),
        ));
        assert_eq!(app.palette.as_ref().unwrap().selected, 0);
        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.palette.as_ref().unwrap().selected, 1);
        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(app.palette.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn palette_backspace_shrinks_the_query_and_refilters() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('a'),
        ));
        for c in "save".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        assert_eq!(app.palette.as_ref().unwrap().filtered.len(), 1);
        app.handle_key(plain_key(KeyCode::Backspace));
        app.handle_key(plain_key(KeyCode::Backspace));
        app.handle_key(plain_key(KeyCode::Backspace));
        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.palette.as_ref().unwrap().query, "");
        assert_eq!(
            app.palette.as_ref().unwrap().filtered.len(),
            commands().len()
        );
    }

    #[test]
    fn ctrl_t_toggles_focus_between_tree_and_editor() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(app.focus, Focus::Tree);
        app.handle_key(ctrl('t'));
        assert_eq!(app.focus, Focus::Editor);
        app.handle_key(ctrl('t'));
        assert_eq!(app.focus, Focus::Tree);
    }

    #[test]
    fn ctrl_shift_a_opens_the_palette_and_esc_closes_it() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.palette.is_none());
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('a'),
        ));
        assert!(app.palette.is_some());
        app.handle_key(plain_key(KeyCode::Esc));
        assert!(app.palette.is_none());
    }

    #[test]
    fn palette_enter_runs_the_selected_command() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('a'),
        ));
        // Type "exit" to filter down to the Exit command.
        for c in "exit".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        let signal = app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(signal, LoopSignal::Exit);
        assert!(app.palette.is_none());
    }

    #[test]
    fn palette_filters_by_substring_case_insensitively() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('a'),
        ));
        for c in "SAVE".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        let palette = app.palette.as_ref().unwrap();
        assert_eq!(palette.filtered.len(), 1);
        assert_eq!(palette.filtered[0].id, "SaveAll");
    }

    #[test]
    fn release_events_are_ignored() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        let mut release = plain_key(KeyCode::Down);
        release.kind = KeyEventKind::Release;
        let signal = app.handle_key(release);
        assert_eq!(signal, LoopSignal::Continue);
        // The tree selection must not have moved.
        let rows = app.tree_state.visible_rows(&app.tree);
        assert_eq!(app.tree_state.selected_row(&rows), rows.first());
    }

    fn shift_char(c: char) -> KeyEvent {
        key(KeyModifiers::SHIFT, KeyCode::Char(c))
    }

    /// "foo bar foo baz foo" written to `a.txt`, opened as the active tab,
    /// with focus already on the editor -- the fixture every find test
    /// below builds on.
    fn app_with_find_fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "foo bar foo baz foo").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Enter)); // open a.txt
        app.handle_key(ctrl('t')); // ensure focus is Editor
        (dir, app)
    }

    #[test]
    fn ctrl_f_with_no_active_tab_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(ctrl('f'));
        assert!(app.find.is_none());
    }

    #[test]
    fn ctrl_f_opens_find_with_an_empty_query_and_focuses_the_editor() {
        let (_dir, mut app) = app_with_find_fixture();
        app.focus = Focus::Tree;
        app.handle_key(ctrl('f'));
        assert!(app.find.is_some());
        assert_eq!(app.find.as_ref().unwrap().query(), "");
        assert_eq!(app.focus, Focus::Editor);
    }

    #[test]
    fn typing_into_find_updates_the_query_and_matches_but_not_the_cursor() {
        let (_dir, mut app) = app_with_find_fixture();
        let offset_before = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .start();

        app.handle_key(ctrl('f'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        assert_eq!(app.find.as_ref().unwrap().query(), "foo");
        assert_eq!(app.find.as_ref().unwrap().current_match(), Some(0..3));

        // Typing alone never moves the real selection.
        let offset_after = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .start();
        assert_eq!(offset_before, offset_after);
    }

    #[test]
    fn ctrl_g_while_find_is_open_advances_to_the_next_match_not_a_literal_g() {
        // Regression test for the exact bug the doc's review round 1
        // caught at the design level: a modifier-blind `Char(c)` arm
        // checked before the `Ctrl+G` arm would insert a literal 'g' into
        // the query instead of navigating.
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        assert_eq!(app.find.as_ref().unwrap().current_match(), Some(0..3));

        app.handle_key(ctrl('g'));

        assert_eq!(
            app.find.as_ref().unwrap().query(),
            "foo",
            "Ctrl+G must not type a literal 'g' into the query"
        );
        assert_eq!(app.find.as_ref().unwrap().current_match(), Some(8..11));
        assert!(app.find.is_some(), "the bar stays open after Ctrl+G");
    }

    #[test]
    fn ctrl_shift_g_while_find_is_open_moves_to_the_previous_match() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        assert_eq!(app.find.as_ref().unwrap().current_match(), Some(0..3));

        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('g'),
        ));

        assert_eq!(
            app.find.as_ref().unwrap().query(),
            "foo",
            "Ctrl+Shift+G must not type a literal 'G' into the query"
        );
        assert_eq!(app.find.as_ref().unwrap().current_match(), Some(16..19));
    }

    #[test]
    fn backspace_in_find_shrinks_the_query() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.find.as_ref().unwrap().query(), "fo");
    }

    #[test]
    fn shift_held_characters_are_typed_into_the_query_not_swallowed() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        app.handle_key(shift_char('F'));
        assert_eq!(app.find.as_ref().unwrap().query(), "F");
    }

    #[test]
    fn escape_closes_find_without_moving_the_cursor() {
        let (_dir, mut app) = app_with_find_fixture();
        let offset_before = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .start();

        app.handle_key(ctrl('f'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.find.is_none());
        let offset_after = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .start();
        assert_eq!(offset_before, offset_after);
    }

    #[test]
    fn enter_jumps_to_the_current_match_then_closes_find() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(ctrl('g')); // advance to the 2nd "foo" (8..11)

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.find.is_none(), "Enter always closes the bar");
        let buf = app.active_buffer().unwrap();
        let selection = buf.buffer.text_buffer().selections().primary();
        assert_eq!(selection.start(), 8);
        assert_eq!(selection.end(), 11);
    }

    #[test]
    fn jump_to_match_top_aligns_the_matched_line_in_the_viewport() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "l0\nl1\nl2\nfoo\nl4").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        assert_eq!(app.active_buffer().unwrap().scroll, 0);

        app.handle_key(ctrl('f'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(
            app.active_buffer().unwrap().scroll,
            3,
            "\"foo\" is on line 3 (0-indexed)"
        );
    }

    #[test]
    fn jump_to_match_clears_a_stale_sticky_desired_column() {
        // Regression test for the finding raised in `rev`'s round-1 code
        // review of T4: `jump_to_match` originally left a pre-existing
        // sticky `desired_column` in place, so the very next `Down` after
        // a find jump would snap to wherever that stale column was
        // instead of the match's own column.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.txt"),
            "0123456789\nfoo\nmatch\nlonglonglongline\n",
        )
        .unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));

        // Build a sticky `desired_column` of 9: 9 `Right`s along line 0
        // (10 chars), then `Down` onto the shorter "foo" (3 chars), which
        // clamps the visible column to 3 but remembers 9 as the column to
        // snap back to on a longer line.
        for _ in 0..9 {
            app.handle_key(plain_key(KeyCode::Right));
        }
        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.active_buffer().unwrap().desired_column, Some(9));

        // Jump (via find) to "match", which starts at column 0 of its own
        // line -- this must clear the stale `desired_column`.
        app.handle_key(ctrl('f'));
        for c in "match".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.active_buffer().unwrap().desired_column, None);

        // The next `Down` must land on column 0 of the following line (the
        // match's own column) -- not column 9, which is where the stale
        // sticky column from before the jump would have landed instead.
        app.handle_key(plain_key(KeyCode::Down));
        let offset = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .start();
        let (_, column) =
            cursor_line_column(app.active_buffer().unwrap().buffer.text_buffer(), offset);
        assert_eq!(
            column, 0,
            "must use the match's own column, not a stale sticky one"
        );
    }

    #[test]
    fn ctrl_w_cannot_close_the_tab_while_find_owns_key_interception() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        assert_eq!(app.tabs.len(), 1);

        app.handle_key(ctrl('w'));

        assert!(app.find.is_some(), "find must still own the input");
        assert_eq!(app.tabs.len(), 1, "the tab must not have been closed");
    }

    #[test]
    fn no_matches_leaves_current_match_none_and_ctrl_g_is_a_no_op() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        for c in "zzz".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        assert_eq!(app.find.as_ref().unwrap().current_match(), None);

        app.handle_key(ctrl('g'));
        assert!(app.find.is_some());
        assert_eq!(app.find.as_ref().unwrap().current_match(), None);
    }

    #[test]
    fn ctrl_r_with_no_active_tab_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(ctrl('r'));
        assert!(app.find.is_none());
    }

    #[test]
    fn ctrl_r_opens_fresh_in_replace_mode_focused_on_query() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        let find = app.find.as_ref().expect("find must be open");
        assert!(find.replace_mode());
        assert_eq!(find.field(), FindField::Query);
        assert_eq!(find.query(), "");
        assert_eq!(app.focus, Focus::Editor);
    }

    #[test]
    fn ctrl_r_on_an_existing_find_only_bar_reveals_replace_without_resetting_query() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        let matches_before = app.find.as_ref().unwrap().current_match();

        app.handle_key(ctrl('r'));

        let find = app.find.as_ref().unwrap();
        assert!(find.replace_mode());
        assert_eq!(find.query(), "foo");
        assert_eq!(find.current_match(), matches_before);
    }

    #[test]
    fn ctrl_r_on_an_already_replace_mode_bar_is_idempotent() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        app.handle_key(ctrl('r'));
        assert!(app.find.as_ref().unwrap().replace_mode());
    }

    #[test]
    fn tab_toggles_the_focused_field_in_replace_mode() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        assert_eq!(app.find.as_ref().unwrap().field(), FindField::Query);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.find.as_ref().unwrap().field(), FindField::Replacement);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.find.as_ref().unwrap().field(), FindField::Query);
    }

    #[test]
    fn tab_is_a_no_op_in_find_only_mode() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.find.as_ref().unwrap().field(), FindField::Query);
        assert!(app.find.is_some(), "Tab must not close the bar");
    }

    #[test]
    fn enter_in_the_query_field_jumps_but_does_not_close_in_replace_mode() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(
            app.find.is_some(),
            "replace-mode Enter in the query field must keep the bar open"
        );
        let buf = app.active_buffer().unwrap();
        assert_eq!(
            buf.buffer.text_buffer().selections().primary().range(),
            0..3
        );
    }

    #[test]
    fn enter_in_the_query_field_still_closes_in_find_only_mode() {
        // Regression for `tui-replace.md` §4 invariant 1/4: the `T4`
        // find-only path (`replace_mode == false`) must be byte-for-byte
        // unchanged -- Enter still always closes.
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.find.is_none());
    }

    #[test]
    fn enter_in_the_replacement_field_replaces_the_current_match_and_keeps_the_bar_open() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Tab));
        for c in "X".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.find.is_some(), "replace must keep the bar open");
        let text = app.active_buffer().unwrap().buffer.text().to_string();
        assert_eq!(text, "X bar foo baz foo");
        // The buffer is dirty and the edit is a single undoable step.
        assert!(app.active_buffer().unwrap().buffer.is_dirty());
        // Two "foo"s remain -- resync must have recomputed matches.
        assert_eq!(app.find.as_ref().unwrap().current_match(), Some(6..9));
    }

    #[test]
    fn replace_is_undoable_in_one_step() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Char('X')));
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(
            app.active_buffer().unwrap().buffer.text(),
            "X bar foo baz foo"
        );

        // Ctrl+Z is swallowed while `find` owns key interception -- same
        // invariant as `ctrl_w_cannot_close_the_tab_while_find_owns_key_interception`
        // above -- so close the bar first, matching real usage.
        app.handle_key(plain_key(KeyCode::Esc));
        app.handle_key(ctrl('z')); // Undo

        assert_eq!(
            app.active_buffer().unwrap().buffer.text(),
            "foo bar foo baz foo"
        );
    }

    #[test]
    fn enter_in_the_replacement_field_with_no_current_match_is_a_no_op() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        for c in "zzz".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Char('X')));

        let text_before = app.active_buffer().unwrap().buffer.text().to_string();
        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.find.is_some());
        assert_eq!(app.active_buffer().unwrap().buffer.text(), text_before);
        assert!(!app.active_buffer().unwrap().buffer.is_dirty());
    }

    #[test]
    fn typing_in_the_replacement_field_edits_replacement_not_query() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Tab));
        for c in "baz".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        assert_eq!(app.find.as_ref().unwrap().query(), "foo");
        assert_eq!(
            app.find.as_ref().unwrap().status_text(),
            "  Find: foo  \u{25b8} Replace: baz  (1 of 3)"
        );
    }

    #[test]
    fn ctrl_g_still_navigates_matches_while_focused_on_the_replacement_field() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.find.as_ref().unwrap().current_match(), Some(0..3));

        app.handle_key(ctrl('g'));

        assert_eq!(app.find.as_ref().unwrap().current_match(), Some(8..11));
        assert_eq!(app.find.as_ref().unwrap().field(), FindField::Replacement);
    }

    fn ctrl_shift(c: char) -> KeyEvent {
        key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char(c),
        )
    }

    #[test]
    fn ctrl_shift_r_with_no_active_find_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(ctrl_shift('r'));
        assert!(app.find.is_none());
    }

    #[test]
    fn ctrl_shift_r_replaces_every_match_as_one_undo_step_and_keeps_the_bar_open() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Char('X')));

        app.handle_key(ctrl_shift('r'));

        assert!(app.find.is_some(), "the bar must stay open");
        assert_eq!(app.active_buffer().unwrap().buffer.text(), "X bar X baz X");
        assert!(app.active_buffer().unwrap().buffer.is_dirty());
    }

    #[test]
    fn ctrl_shift_r_is_undoable_in_one_step() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Char('X')));
        app.handle_key(ctrl_shift('r'));
        assert_eq!(app.active_buffer().unwrap().buffer.text(), "X bar X baz X");

        app.handle_key(plain_key(KeyCode::Esc)); // close so Ctrl+Z reaches Undo
        app.handle_key(ctrl('z'));

        assert_eq!(
            app.active_buffer().unwrap().buffer.text(),
            "foo bar foo baz foo"
        );
    }

    #[test]
    fn ctrl_shift_r_from_find_only_mode_replaces_with_an_empty_string_and_forces_replace_mode_visible(
    ) {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('f')); // find-only, replace_mode still false
        for c in "foo".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        assert!(!app.find.as_ref().unwrap().replace_mode());

        app.handle_key(ctrl_shift('r'));

        assert_eq!(app.active_buffer().unwrap().buffer.text(), " bar  baz ");
        assert!(
            app.find.as_ref().unwrap().replace_mode(),
            "Ctrl+Shift+R must force the replace row visible"
        );
    }

    #[test]
    fn ctrl_shift_r_with_no_matches_is_a_noop() {
        let (_dir, mut app) = app_with_find_fixture();
        app.handle_key(ctrl('r'));
        for c in "zzz".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        let text_before = app.active_buffer().unwrap().buffer.text().to_string();

        app.handle_key(ctrl_shift('r'));

        assert_eq!(app.active_buffer().unwrap().buffer.text(), text_before);
        assert!(!app.active_buffer().unwrap().buffer.is_dirty());
    }

    #[test]
    fn ctrl_shift_r_surfaces_a_truncation_notice_in_status_text() {
        let dir = tempfile::tempdir().unwrap();
        let big = "a".repeat(ide_core::MAX_SEARCH_MATCHES + 10);
        std::fs::write(dir.path().join("a.txt"), &big).unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Enter)); // open a.txt
        app.handle_key(ctrl('t')); // focus Editor
        app.handle_key(ctrl('r'));
        app.handle_key(plain_key(KeyCode::Char('a')));

        app.handle_key(ctrl_shift('r'));

        assert!(app
            .find
            .as_ref()
            .unwrap()
            .status_text()
            .contains(&format!("capped at {}", ide_core::MAX_SEARCH_MATCHES)));
    }

    fn location(path: PathBuf, line: u32, character: u32) -> Location {
        Location {
            path,
            range: ide_lsp::Range {
                start: Position { line, character },
                end: Position { line, character },
            },
        }
    }

    // `sample_project()`'s fixture has no `Cargo.toml`, so `detect_language`
    // never matches and `App::new` never starts a language server -- every
    // test below that opens a plain `sample_project()`/`app_with_many_lines`
    // app is exercising the "no language server running" path by
    // construction, not by coincidence.

    #[test]
    fn ctrl_b_with_no_language_server_running_notifies_instead_of_querying() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t')); // focus Editor

        app.handle_key(ctrl('b'));

        assert!(app.goto.is_none());
        assert_eq!(app.notifications.len(), 1);
        assert!(app.notifications[0]
            .message
            .contains("No language server running"));
        assert!(!app.notifications[0].read);
    }

    #[test]
    fn ctrl_u_with_no_language_server_running_notifies_instead_of_querying() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.handle_key(ctrl('u'));

        assert_eq!(app.notifications.len(), 1);
        assert!(app.notifications[0]
            .message
            .contains("No language server running"));
    }

    #[test]
    fn go_to_declaration_and_find_usages_are_registered_as_named_commands() {
        // `commands.rs`'s own binding table already proves `Ctrl+B`/`Ctrl+U`
        // resolve to these actions; this proves the palette (which filters
        // by `Command::title`, not `Action`) can actually find them too.
        assert!(commands().iter().any(|c| c.id == "GoToDeclaration"));
        assert!(commands().iter().any(|c| c.id == "FindUsages"));
    }

    #[test]
    fn handle_goto_key_enter_opens_the_selected_result_and_closes_the_picker() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let b = dir.path().canonicalize().unwrap().join("b.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto = Some(GotoState {
            title: "Declaration",
            results: vec![location(a, 0, 0), location(b.clone(), 0, 0)],
            selected: 1,
        });

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.goto.is_none());
        assert_eq!(app.active_buffer().unwrap().path, b);
    }

    #[test]
    fn handle_goto_key_up_and_down_move_the_selection_and_clamp_at_both_ends() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto = Some(GotoState {
            title: "Usages",
            results: vec![location(a.clone(), 0, 0), location(a, 1, 0)],
            selected: 0,
        });

        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(app.goto.as_ref().unwrap().selected, 0, "must clamp at 0");

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.goto.as_ref().unwrap().selected, 1);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(
            app.goto.as_ref().unwrap().selected,
            1,
            "must clamp at the last index"
        );
    }

    #[test]
    fn handle_goto_key_esc_closes_the_picker_without_opening_anything() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto = Some(GotoState {
            title: "Declaration",
            results: vec![location(a, 0, 0)],
            selected: 0,
        });

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.goto.is_none());
        assert!(app.active_tab.is_none());
    }

    #[test]
    fn open_location_places_the_cursor_at_the_locations_position() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt"); // "hello\nworld"
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.open_location(location(a, 1, 2)); // line 1 ("world"), column 2

        let buf = app.active_buffer().unwrap();
        let offset = buf.buffer.text_buffer().selections().primary().start();
        assert_eq!(offset, "hello\nwo".len());
    }

    #[test]
    fn open_location_on_a_missing_file_notifies_instead_of_panicking() {
        let dir = sample_project();
        let missing = dir.path().canonicalize().unwrap().join("nope.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.open_location(location(missing, 0, 0));

        assert!(app.active_tab.is_none());
        assert_eq!(app.notifications.len(), 1);
    }

    // -- goto-declaration-interface-redirect --

    fn interface_symbol(name: &str, path: PathBuf) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: ide_lsp::SymbolKind::Interface,
            container_name: None,
            location: Location {
                path,
                range: ide_lsp::Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 1,
                        character: 0,
                    },
                },
            },
        }
    }

    #[test]
    fn trigger_go_to_declaration_records_the_origin_for_the_redirect_check() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(a.clone()).unwrap();
        app.lsp
            .start_with_command(&app.project_root.clone(), "cat", &[]);

        app.run_action(Action::GoToDeclaration);

        let (path, _) = app.goto_declaration_origin.clone().unwrap();
        assert_eq!(path, a);
    }

    #[test]
    fn trigger_go_to_declaration_with_no_language_server_clears_the_origin() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto_declaration_origin = Some((PathBuf::from("/stale.rs"), location_position()));

        app.run_action(Action::GoToDeclaration);

        assert!(app.goto_declaration_origin.is_none());
    }

    fn location_position() -> Position {
        Position {
            line: 0,
            character: 0,
        }
    }

    #[test]
    fn handle_goto_results_for_declaration_with_one_result_defers_to_the_interface_check() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.handle_goto_results("Declaration", vec![location(a.clone(), 0, 3)]);

        assert!(app.active_tab.is_none());
        assert!(app.goto.is_none());
        assert_eq!(app.pending_interface_check.clone().unwrap().path, a);
    }

    #[test]
    fn handle_goto_results_for_usages_with_one_result_jumps_immediately_unaffected() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.handle_goto_results("Usages", vec![location(a, 0, 3)]);

        assert!(app.active_tab.is_some());
        assert!(app.pending_interface_check.is_none());
    }

    #[test]
    fn handle_interface_check_response_is_a_noop_when_not_ready() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.document_symbols_ready = false;
        app.pending_interface_check = Some(location(a, 0, 0));

        app.handle_interface_check_response();

        assert!(app.pending_interface_check.is_some());
        assert!(app.active_tab.is_none());
    }

    #[test]
    fn handle_interface_check_response_ignores_a_response_for_a_different_file() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let b = dir.path().canonicalize().unwrap().join("b.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.document_symbols_ready = true;
        app.lsp.document_symbols_path = Some(b);
        app.pending_interface_check = Some(location(a, 0, 0));

        app.handle_interface_check_response();

        // Left outstanding -- this response belongs to some other query.
        assert!(app.pending_interface_check.is_some());
        assert!(app.active_tab.is_none());
    }

    #[test]
    fn handle_interface_check_response_jumps_directly_on_a_plain_symbol() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto_declaration_origin = Some((a.clone(), location_position()));
        app.pending_interface_check = Some(location(a.clone(), 0, 3));
        app.lsp.document_symbols_ready = true;
        app.lsp.document_symbols_path = Some(a.clone());
        app.lsp.document_symbols = vec![symbol("helper", a.clone())];

        app.handle_interface_check_response();

        assert!(app.pending_interface_check.is_none());
        assert_eq!(app.active_buffer().unwrap().path, a);
        assert!(app.notifications[0].message.contains("Declaration"));
    }

    #[test]
    fn handle_interface_check_response_redirects_to_implementation_on_an_interface() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp
            .start_with_command(&app.project_root.clone(), "cat", &[]);
        app.goto_declaration_origin = Some((a.clone(), location_position()));
        app.pending_interface_check = Some(location(a.clone(), 0, 3));
        app.lsp.document_symbols_ready = true;
        app.lsp.document_symbols_path = Some(a.clone());
        app.lsp.document_symbols = vec![interface_symbol("Logger", a.clone())];

        app.handle_interface_check_response();

        assert!(app.pending_interface_check.is_none());
        // Redirected -- no jump from this call; the follow-up `Goto`
        // response (not exercised here) is what would jump.
        assert!(app.active_tab.is_none());
        assert!(app.expect_implementation_next);
    }

    #[test]
    fn handle_interface_check_response_on_an_interface_with_no_recorded_origin_falls_back() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto_declaration_origin = None;
        app.pending_interface_check = Some(location(a.clone(), 0, 3));
        app.lsp.document_symbols_ready = true;
        app.lsp.document_symbols_path = Some(a.clone());
        app.lsp.document_symbols = vec![interface_symbol("Logger", a.clone())];

        app.handle_interface_check_response();

        assert!(app.pending_interface_check.is_none());
        assert_eq!(app.active_buffer().unwrap().path, a);
        assert!(!app.expect_implementation_next);
    }

    #[test]
    fn poll_lsp_labels_the_redirected_query_response_as_implementation() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.expect_implementation_next = true;
        app.lsp.goto_ready = true;
        app.lsp.goto = vec![location(a, 0, 3)];

        // `self.lsp.poll()` unconditionally resets `goto_ready` at its own
        // top -- with no real client running it would immediately wipe out
        // the manually-primed state above, so this test exercises
        // `poll_lsp`'s own title-selection logic directly rather than
        // going through the full `poll_lsp` call.
        if app.lsp.goto_ready {
            let results = std::mem::take(&mut app.lsp.goto);
            let title = if std::mem::take(&mut app.expect_implementation_next) {
                "Implementation"
            } else {
                "Declaration"
            };
            app.handle_goto_results(title, results);
        }

        assert!(!app.expect_implementation_next);
        assert!(app.notifications[0].message.contains("Implementation"));
    }

    #[test]
    fn run_action_toggle_notifications_opens_and_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(!app.notifications_open);

        app.run_action(Action::ToggleNotifications);
        assert!(app.notifications_open);

        app.run_action(Action::ToggleNotifications);
        assert!(!app.notifications_open);
    }

    #[test]
    fn notifications_is_reachable_from_the_palette_by_title() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('a'),
        ));
        for c in "Notif".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.palette.is_none());
        assert!(app.notifications_open);
    }

    #[test]
    fn opening_notifications_closes_an_open_goto_picker() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto = Some(GotoState {
            title: "Declaration",
            results: vec![location(a.clone(), 0, 0), location(a, 1, 0)],
            selected: 0,
        });

        app.run_action(Action::ToggleNotifications);

        assert!(app.goto.is_none());
        assert!(app.notifications_open);
    }

    #[test]
    fn handle_notifications_key_esc_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.notifications_open = true;

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(!app.notifications_open);
    }

    #[test]
    fn handle_notifications_key_c_clears_every_notification() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.notify("one");
        app.notify("two");
        app.notifications_open = true;

        app.handle_key(plain_key(KeyCode::Char('c')));

        assert!(app.notifications.is_empty());
        assert!(
            app.notifications_open,
            "clearing must not also close the panel"
        );
    }

    #[test]
    fn handle_notifications_key_r_marks_every_notification_read_without_removing_any() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.notify("one");
        app.notify("two");
        app.notifications_open = true;

        app.handle_key(plain_key(KeyCode::Char('r')));

        assert_eq!(app.notifications.len(), 2);
        assert!(app.notifications.iter().all(|n| n.read));
        assert_eq!(app.unread_notification_count(), 0);
    }

    #[test]
    fn unread_notification_count_only_counts_unread_entries() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(app.unread_notification_count(), 0);

        app.notify("one");
        app.notify("two");
        assert_eq!(app.unread_notification_count(), 2);

        for n in &mut app.notifications {
            n.read = true;
        }
        assert_eq!(app.unread_notification_count(), 0);

        app.notify("three");
        assert_eq!(app.unread_notification_count(), 1);
    }

    #[test]
    fn while_notifications_are_open_other_keys_are_swallowed_not_forwarded() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.notifications_open = true;

        // A plain, unhandled character must not fall through to tree/editor
        // dispatch (this panel has no rows to select, so `Down` moving a
        // tree selection would be a leak of focus, not a real feature).
        app.handle_key(plain_key(KeyCode::Down));

        assert!(app.notifications_open);
        assert!(app.tabs.is_empty());
    }

    fn diagnostic(line: u32, message: &str) -> Diagnostic {
        Diagnostic {
            range: ide_lsp::Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
            severity: ide_lsp::DiagnosticSeverity::Error,
            message: message.to_string(),
        }
    }

    #[test]
    fn flattened_diagnostics_is_sorted_by_path_then_range_start() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let b = dir.path().canonicalize().unwrap().join("b.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp
            .diagnostics
            .insert(b.clone(), vec![diagnostic(0, "b0")]);
        app.lsp
            .diagnostics
            .insert(a.clone(), vec![diagnostic(5, "a5"), diagnostic(1, "a1")]);

        let rows = app.flattened_diagnostics();

        assert_eq!(
            rows.iter()
                .map(|(p, d)| ((*p).clone(), d.message.clone()))
                .collect::<Vec<_>>(),
            vec![
                (a.clone(), "a1".to_string()),
                (a, "a5".to_string()),
                (b, "b0".to_string()),
            ]
        );
    }

    #[test]
    fn active_semantic_tokens_with_no_active_tab_is_empty() {
        let dir = sample_project();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.active_semantic_tokens().is_empty());
    }

    #[test]
    fn active_semantic_tokens_with_no_entry_for_the_active_tab_is_empty() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.active_semantic_tokens().is_empty());
    }

    #[test]
    fn active_semantic_tokens_returns_the_active_tabs_entry() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        let tokens = vec![ide_lsp::SemanticToken {
            position: Position {
                line: 0,
                character: 0,
            },
            length: 5,
            kind: ide_lsp::SemanticTokenKind::Variable,
        }];
        app.lsp.semantic_tokens.insert(a, tokens.clone());
        assert_eq!(app.active_semantic_tokens(), tokens.as_slice());
    }

    #[test]
    fn run_action_toggle_problems_opens_and_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.problems.is_none());

        app.run_action(Action::ToggleProblems);
        assert!(app.problems.is_some());

        app.run_action(Action::ToggleProblems);
        assert!(app.problems.is_none());
    }

    #[test]
    fn ctrl_p_toggles_the_problems_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.handle_key(ctrl('p'));

        assert!(app.problems.is_some());
    }

    #[test]
    fn opening_problems_closes_an_open_goto_picker_and_notifications_panel() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto = Some(GotoState {
            title: "Declaration",
            results: vec![location(a, 0, 0)],
            selected: 0,
        });
        app.notifications_open = true;

        app.run_action(Action::ToggleProblems);

        assert!(app.goto.is_none());
        assert!(!app.notifications_open);
        assert!(app.problems.is_some());
    }

    #[test]
    fn handle_problems_key_esc_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.problems = Some(ProblemsState { selected: 0 });

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.problems.is_none());
    }

    #[test]
    fn handle_problems_key_up_and_down_clamp_at_both_ends() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp
            .diagnostics
            .insert(a, vec![diagnostic(0, "first"), diagnostic(1, "second")]);
        app.problems = Some(ProblemsState { selected: 0 });

        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(
            app.problems.as_ref().unwrap().selected,
            0,
            "must clamp at 0"
        );

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.problems.as_ref().unwrap().selected, 1);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(
            app.problems.as_ref().unwrap().selected,
            1,
            "must clamp at the last row"
        );
    }

    #[test]
    fn handle_problems_key_enter_opens_the_selected_diagnostics_location() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt"); // "hello\nworld"
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp
            .diagnostics
            .insert(a.clone(), vec![diagnostic(1, "world is wrong")]);
        app.problems = Some(ProblemsState { selected: 0 });

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.problems.is_none());
        assert_eq!(app.active_buffer().unwrap().path, a);
        let offset = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .start();
        assert_eq!(offset, "hello\n".len());
    }

    #[test]
    fn while_problems_are_open_other_keys_are_swallowed_not_forwarded() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.problems = Some(ProblemsState { selected: 0 });

        app.handle_key(plain_key(KeyCode::Char('x')));

        assert!(app.problems.is_some());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn run_action_toggle_cargo_panel_opens_and_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(!app.cargo_panel_open);

        app.run_action(Action::ToggleCargoPanel);
        assert!(app.cargo_panel_open);

        app.run_action(Action::ToggleCargoPanel);
        assert!(!app.cargo_panel_open);
    }

    #[test]
    fn opening_cargo_panel_closes_goto_notifications_and_problems() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto = Some(GotoState {
            title: "Declaration",
            results: vec![location(a, 0, 0)],
            selected: 0,
        });
        app.notifications_open = true;
        app.problems = Some(ProblemsState { selected: 0 });

        app.run_action(Action::ToggleCargoPanel);

        assert!(app.goto.is_none());
        assert!(!app.notifications_open);
        assert!(app.problems.is_none());
        assert!(app.cargo_panel_open);
    }

    #[test]
    fn opening_problems_closes_an_open_cargo_panel() {
        // The reverse direction of the mutual-exclusion rule: every
        // overlay's `open` path routes through `close_all_overlays`, so
        // this must hold symmetrically, not just from the Cargo panel's
        // own toggle.
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.cargo_panel_open = true;

        app.run_action(Action::ToggleProblems);

        assert!(!app.cargo_panel_open);
        assert!(app.problems.is_some());
    }

    #[test]
    fn handle_cargo_panel_key_esc_closes_the_panel_without_stopping_a_running_command() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.cargo_panel_open = true;
        // Set directly rather than via a real `run` call -- this test is
        // only about `Esc` leaving `self.cargo` alone, not about spawning
        // a real subprocess.
        app.cargo.running = Some(CargoCommand::Test);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(!app.cargo_panel_open);
        assert_eq!(app.cargo.running, Some(CargoCommand::Test));
    }

    #[test]
    fn handle_cargo_panel_key_letters_start_the_matching_subcommand() {
        let cases = [
            ('b', CargoCommand::Build),
            ('r', CargoCommand::Run),
            ('t', CargoCommand::Test),
            ('c', CargoCommand::Check),
            ('l', CargoCommand::Clippy),
            ('f', CargoCommand::Fmt),
        ];
        for (letter, expected) in cases {
            let dir = sample_project();
            let mut app = App::new(dir.path().to_path_buf()).unwrap();
            app.cargo_panel_open = true;

            app.handle_key(plain_key(KeyCode::Char(letter)));

            assert_eq!(
                app.cargo.running,
                Some(expected),
                "letter {letter:?} should start {expected:?}"
            );
        }
    }

    #[test]
    fn while_cargo_panel_is_open_ctrl_w_does_not_close_the_active_tab() {
        // Same invariant `tui-find.md`/`tui-problems.md` already prove for
        // their own overlays: an overlay-local key set fully intercepts
        // input, so a global binding like `Ctrl+W` (`CloseTab`) never
        // reaches `run_action` while the Cargo panel is open.
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.tabs.len(), 1);
        app.cargo_panel_open = true;

        app.handle_key(ctrl('w'));

        assert!(app.cargo_panel_open);
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn while_cargo_panel_is_open_other_keys_are_swallowed_not_forwarded() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.cargo_panel_open = true;

        app.handle_key(plain_key(KeyCode::Char('x')));

        assert!(app.cargo_panel_open);
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn f1_opens_the_hover_popup_even_with_no_active_tab() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(!app.hover_open);

        app.handle_key(plain_key(KeyCode::F(1)));

        assert!(app.hover_open);
    }

    #[test]
    fn trigger_quick_documentation_with_a_valid_target_does_not_panic() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));

        app.handle_key(plain_key(KeyCode::F(1)));

        assert!(app.hover_open);
    }

    #[test]
    fn esc_closes_the_hover_popup() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.hover_open = true;

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(!app.hover_open);
    }

    #[test]
    fn while_hover_is_open_other_keys_are_swallowed_not_forwarded() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.hover_open = true;

        app.handle_key(plain_key(KeyCode::Char('x')));

        assert!(app.hover_open);
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn while_hover_is_open_ctrl_w_does_not_close_the_active_tab() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.tabs.len(), 1);
        app.hover_open = true;

        app.handle_key(ctrl('w'));

        assert!(app.hover_open);
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn opening_hover_closes_goto_notifications_problems_and_cargo_panel() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto = Some(GotoState {
            title: "Declaration",
            results: vec![location(a, 0, 0)],
            selected: 0,
        });
        app.notifications_open = true;
        app.problems = Some(ProblemsState { selected: 0 });
        app.cargo_panel_open = true;

        app.run_action(Action::QuickDocumentation);

        assert!(app.goto.is_none());
        assert!(!app.notifications_open);
        assert!(app.problems.is_none());
        assert!(!app.cargo_panel_open);
        assert!(app.hover_open);
    }

    #[test]
    fn opening_the_cargo_panel_closes_an_open_hover_popup() {
        // The reverse direction of the mutual-exclusion rule -- every
        // overlay's `open` path routes through `close_all_overlays`, so
        // this must hold symmetrically, not just from `F1`'s own toggle.
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.hover_open = true;

        app.run_action(Action::ToggleCargoPanel);

        assert!(!app.hover_open);
        assert!(app.cargo_panel_open);
    }

    #[test]
    fn active_inlay_hints_with_no_active_tab_is_empty() {
        let dir = sample_project();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.active_inlay_hints().is_empty());
    }

    #[test]
    fn active_inlay_hints_with_no_entry_for_the_active_tab_is_empty() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.active_inlay_hints().is_empty());
    }

    #[test]
    fn active_inlay_hints_returns_the_active_tabs_entry() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        let hints = vec![ide_lsp::InlayHint {
            position: Position {
                line: 0,
                character: 0,
            },
            label: ": i32".to_string(),
            padding_left: true,
            padding_right: false,
        }];
        app.lsp.inlay_hints.insert(a, hints.clone());
        assert_eq!(app.active_inlay_hints(), hints.as_slice());
    }

    #[test]
    fn sync_document_highlights_with_no_target_is_a_noop_when_nothing_was_recorded() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.last_highlighted_target.is_none());

        app.sync_document_highlights();

        assert!(app.last_highlighted_target.is_none());
        assert!(app.lsp.document_highlights.is_empty());
    }

    #[test]
    fn sync_document_highlights_with_a_new_target_records_it() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        assert!(app.last_highlighted_target.is_none());

        app.sync_document_highlights();

        assert!(app.last_highlighted_target.is_some());
    }

    #[test]
    fn sync_document_highlights_with_the_same_target_twice_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));

        app.sync_document_highlights();
        let first = app.last_highlighted_target.clone();
        app.sync_document_highlights();

        assert_eq!(app.last_highlighted_target, first);
    }

    #[test]
    fn sync_document_highlights_clears_when_the_target_disappears() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        app.sync_document_highlights();
        assert!(app.last_highlighted_target.is_some());
        app.lsp.document_highlights.push(ide_lsp::Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        });

        // No active tab any more -- `lsp_query_target` now returns `None`.
        app.close_active_tab();
        app.sync_document_highlights();

        assert!(app.last_highlighted_target.is_none());
        assert!(app.lsp.document_highlights.is_empty());
    }

    #[test]
    fn open_or_focus_tab_requests_inlay_hints_for_the_whole_document() {
        // No running language server -- `request_inlay_hints` is a no-op
        // past `lsp.is_running()`'s own gating, so this only proves the
        // call site doesn't panic and doesn't accidentally skip the
        // whole-document range computation for a real file's text.
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn sync_lsp_did_change_requests_inlay_hints_for_the_whole_document() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Char('!')));
        // No running language server, so `request_inlay_hints` no-ops --
        // this proves `sync_lsp_did_change` (called via the edit above)
        // doesn't panic computing `whole_document_range` for real text.
        assert!(app.active_buffer().unwrap().buffer.is_dirty());
    }

    #[test]
    fn whole_document_range_spans_the_full_text() {
        let text = "let x = 1;\nlet y = 2;\n";
        let range = whole_document_range(text).unwrap();
        assert_eq!(
            range.start,
            Position {
                line: 0,
                character: 0
            }
        );
        assert_eq!(
            range.end,
            ide_lsp::byte_offset_to_position(text, text.len()).unwrap()
        );
    }

    fn wait_until<F: FnMut() -> bool>(mut condition: F) {
        let start = std::time::Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "condition did not become true in time"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn ctrl_shift_f_opens_the_search_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(!app.search_open);

        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('f'),
        ));

        assert!(app.search_open);
    }

    #[test]
    fn run_action_find_in_path_toggles_the_panel_open_and_closed() {
        // Toggling twice through `run_action` directly, the same way
        // `run_action_toggle_cargo_panel_opens_and_closes_the_panel` does
        // -- once the panel is open, its own key interception (`docs/
        // features/tui-find-in-path.md` §2.2) swallows every `Ctrl`-held
        // combo including its own opening chord, so a *second* real
        // `Ctrl+Shift+F` keypress while open can't reach `run_action` at
        // all (same interception order every other overlay already has);
        // `Esc` is the only way to close it from the keyboard.
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(!app.search_open);

        app.run_action(Action::FindInPath);
        assert!(app.search_open);

        app.run_action(Action::FindInPath);
        assert!(!app.search_open);
    }

    #[test]
    fn opening_search_closes_goto_notifications_problems_cargo_and_hover() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto = Some(GotoState {
            title: "Declaration",
            results: vec![location(a, 0, 0)],
            selected: 0,
        });
        app.notifications_open = true;
        app.problems = Some(ProblemsState { selected: 0 });
        app.cargo_panel_open = true;
        app.hover_open = true;

        app.run_action(Action::FindInPath);

        assert!(app.goto.is_none());
        assert!(!app.notifications_open);
        assert!(app.problems.is_none());
        assert!(!app.cargo_panel_open);
        assert!(!app.hover_open);
        assert!(app.search_open);
    }

    #[test]
    fn opening_the_cargo_panel_closes_an_open_search_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;

        app.run_action(Action::ToggleCargoPanel);

        assert!(!app.search_open);
        assert!(app.cargo_panel_open);
    }

    #[test]
    fn while_search_is_open_other_keys_are_swallowed_not_forwarded_to_the_tree() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;

        // `Down` while `search_open` moves the (empty) result selection,
        // not the tree's own selection -- confirm the tree's selection
        // index (not exposed directly, so proven via `Enter` not opening
        // any tab) stays untouched.
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.search_open);
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn while_search_is_open_ctrl_w_does_not_close_the_active_tab() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.tabs.len(), 1);
        app.search_open = true;

        app.handle_key(ctrl('w'));

        assert!(app.search_open);
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn esc_closes_the_search_panel_without_discarding_state() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;
        app.search_state.query = "hello".to_string();

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(!app.search_open);
        assert_eq!(app.search_state.query, "hello");
    }

    #[test]
    fn typing_while_search_is_open_edits_the_query() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;

        app.handle_key(plain_key(KeyCode::Char('h')));
        app.handle_key(plain_key(KeyCode::Char('i')));
        assert_eq!(app.search_state.query, "hi");

        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.search_state.query, "h");
    }

    #[test]
    fn ctrl_held_letters_are_not_typed_into_the_search_query() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;

        app.handle_key(ctrl('x'));

        assert_eq!(app.search_state.query, "");
    }

    #[test]
    fn enter_with_an_empty_query_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(!app.search.searching);
        assert!(app.search_state.ran_query.is_none());
    }

    #[test]
    fn enter_runs_a_search_and_a_second_unchanged_enter_opens_the_selected_result() {
        let dir = sample_project(); // a.txt: "hello\nworld"
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;
        for c in "hello".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.search.searching || app.search.results.is_some());
        wait_until(|| {
            app.poll_search();
            !app.search.searching
        });
        let results = app.search.results.as_ref().unwrap();
        assert_eq!(results.matches.len(), 1);
        assert_eq!(app.search_state.ran_query, Some("hello".to_string()));

        // Same, unchanged query -- this Enter should open the match, not
        // search again.
        app.handle_key(plain_key(KeyCode::Enter));

        assert!(!app.search_open);
        assert_eq!(app.tabs.len(), 1);
        let buf = app.active_buffer().unwrap();
        assert_eq!(buf.buffer.text_buffer().selections().primary().start(), 0);
    }

    #[test]
    fn editing_the_query_after_results_arrive_makes_the_next_enter_search_again() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;
        for c in "hello".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Enter));
        wait_until(|| {
            app.poll_search();
            !app.search.searching
        });
        assert_eq!(app.search_state.ran_query, Some("hello".to_string()));

        // Edit the query -- it now differs from `ran_query`.
        app.handle_key(plain_key(KeyCode::Char('!')));
        app.handle_key(plain_key(KeyCode::Enter));

        // A fresh search was started (not an "open"), so the panel is
        // still open and a new run is either in flight or already
        // reflected in `ran_query`.
        assert!(app.search_open);
        assert_eq!(app.search_state.ran_query, Some("hello!".to_string()));
    }

    #[test]
    fn enter_while_a_search_is_in_flight_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search.run(app.tree.clone(), "hello".to_string());
        assert!(app.search.searching);
        app.search_open = true;
        app.search_state.query = "hello".to_string();
        app.search_state.ran_query = Some("different".to_string());

        app.submit_or_open_search_result();

        // Neither a new run was started (generation-gated inside
        // `SearchPanel::run` itself) nor did `ran_query` change, proving
        // the no-op happened before either branch below it ran.
        assert_eq!(app.search_state.ran_query, Some("different".to_string()));
    }

    #[test]
    fn up_and_down_move_the_selection_clamped_to_the_result_count() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;
        app.search.results = Some(ide_core::SearchResults {
            matches: vec![
                ide_core::SearchMatch {
                    path: PathBuf::from("/a.txt"),
                    line: 0,
                    column: 0,
                    byte_offset: 0,
                    line_text: "a".to_string(),
                },
                ide_core::SearchMatch {
                    path: PathBuf::from("/b.txt"),
                    line: 0,
                    column: 0,
                    byte_offset: 0,
                    line_text: "b".to_string(),
                },
            ],
            truncated: false,
        });

        app.handle_key(plain_key(KeyCode::Up)); // clamped at 0
        assert_eq!(app.search_state.selected, 0);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.search_state.selected, 1);

        app.handle_key(plain_key(KeyCode::Down)); // clamped at len - 1
        assert_eq!(app.search_state.selected, 1);

        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(app.search_state.selected, 0);
    }

    #[test]
    fn open_search_result_places_the_caret_at_the_byte_offset() {
        let dir = sample_project(); // a.txt: "hello\nworld"
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.open_search_result(a, 6);

        let buf = app.active_buffer().unwrap();
        assert_eq!(buf.buffer.text_buffer().selections().primary().start(), 6);
    }

    // -- T13: Code Actions + Rename
    // (docs/features/tui-code-actions-and-rename.md) --

    fn workspace_edit(edits: Vec<ide_lsp::FileEdit>) -> ide_lsp::WorkspaceEdit {
        ide_lsp::WorkspaceEdit { edits }
    }

    fn text_edit(start: (u32, u32), end: (u32, u32), new_text: &str) -> ide_lsp::TextEdit {
        ide_lsp::TextEdit {
            range: ide_lsp::Range {
                start: Position {
                    line: start.0,
                    character: start.1,
                },
                end: Position {
                    line: end.0,
                    character: end.1,
                },
            },
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn sync_code_actions_with_no_target_is_a_noop_when_nothing_was_recorded() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.sync_code_actions();
        assert!(app.last_code_actions_target.is_none());
    }

    #[test]
    fn sync_code_actions_with_a_new_target_records_it() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));

        app.sync_code_actions();

        assert!(app.last_code_actions_target.is_some());
    }

    #[test]
    fn sync_code_actions_with_the_same_target_twice_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));

        app.sync_code_actions();
        let first = app.last_code_actions_target.clone();
        app.sync_code_actions();
        assert_eq!(app.last_code_actions_target, first);
    }

    #[test]
    fn sync_code_actions_clears_when_the_target_disappears() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.sync_code_actions();
        assert!(app.last_code_actions_target.is_some());

        app.active_tab = None;
        app.sync_code_actions();

        assert!(app.last_code_actions_target.is_none());
    }

    #[test]
    fn alt_enter_opens_the_code_actions_popup_without_sending_a_request() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.code_actions.is_none());

        app.handle_key(key(KeyModifiers::ALT, KeyCode::Enter));

        assert!(app.code_actions.is_some());
    }

    #[test]
    fn esc_closes_the_code_actions_popup() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.code_actions = Some(CodeActionsState { selected: 0 });

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.code_actions.is_none());
    }

    #[test]
    fn while_code_actions_is_open_ctrl_w_does_not_close_the_active_tab() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.code_actions = Some(CodeActionsState { selected: 0 });

        app.handle_key(ctrl('w'));

        assert!(app.code_actions.is_some());
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn up_and_down_move_the_code_actions_selection_clamped_to_the_action_count() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.code_actions.push(ide_lsp::CodeAction {
            index: 0,
            title: "First".to_string(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
        });
        app.lsp.code_actions.push(ide_lsp::CodeAction {
            index: 1,
            title: "Second".to_string(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
        });
        app.code_actions = Some(CodeActionsState { selected: 0 });

        app.handle_key(plain_key(KeyCode::Up)); // clamped at 0
        assert_eq!(app.code_actions.as_ref().unwrap().selected, 0);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.code_actions.as_ref().unwrap().selected, 1);

        app.handle_key(plain_key(KeyCode::Down)); // clamped at len - 1
        assert_eq!(app.code_actions.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn enter_on_an_empty_code_actions_list_closes_it_without_sending_anything() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.code_actions = Some(CodeActionsState { selected: 0 });

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.code_actions.is_none());
    }

    #[test]
    fn opening_code_actions_closes_goto_notifications_problems_cargo_hover_and_search() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.goto = Some(GotoState {
            title: "Declaration",
            results: vec![location(a, 0, 0)],
            selected: 0,
        });
        app.notifications_open = true;
        app.problems = Some(ProblemsState { selected: 0 });
        app.cargo_panel_open = true;
        app.hover_open = true;
        app.search_open = true;

        app.run_action(Action::ShowIntentionActions);

        assert!(app.goto.is_none());
        assert!(!app.notifications_open);
        assert!(app.problems.is_none());
        assert!(!app.cargo_panel_open);
        assert!(!app.hover_open);
        assert!(!app.search_open);
        assert!(app.code_actions.is_some());
    }

    #[test]
    fn opening_the_cargo_panel_closes_an_open_code_actions_popup() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.code_actions = Some(CodeActionsState { selected: 0 });

        app.run_action(Action::ToggleCargoPanel);

        assert!(app.code_actions.is_none());
        assert!(app.cargo_panel_open);
    }

    #[test]
    fn handle_workspace_edit_ready_is_a_noop_when_not_ready() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.workspace_edit_ready = false;
        app.lsp.workspace_edit = Some(workspace_edit(vec![]));

        app.handle_workspace_edit_ready();

        assert!(app.status().is_none());
        assert!(app.lsp.workspace_edit.is_some());
    }

    #[test]
    fn handle_workspace_edit_ready_with_no_edit_reports_nothing_to_apply() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit = None;
        app.lsp.workspace_edit_label = Some("Import `Foo`".to_string());

        app.handle_workspace_edit_ready();

        assert_eq!(app.status(), Some("Import `Foo`: nothing to apply"));
    }

    #[test]
    fn handle_workspace_edit_ready_applies_to_disk_for_a_file_with_no_open_tab() {
        let dir = sample_project();
        let b = dir.path().canonicalize().unwrap().join("b.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Code action".to_string());
        app.lsp.workspace_edit = Some(workspace_edit(vec![ide_lsp::FileEdit {
            path: b.clone(),
            text_edits: vec![text_edit((0, 0), (0, 6), "REPLACED")],
        }]));

        app.handle_workspace_edit_ready();

        assert_eq!(app.status(), Some("Code action: applied to 1 file"));
        assert_eq!(fs::read_to_string(&b).unwrap(), "REPLACED file");
    }

    #[test]
    fn handle_workspace_edit_ready_applies_to_an_open_tabs_buffer() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt"); // "hello\nworld"
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Code action".to_string());
        app.lsp.workspace_edit = Some(workspace_edit(vec![ide_lsp::FileEdit {
            path: a,
            text_edits: vec![text_edit((0, 0), (0, 5), "goodbye")],
        }]));

        app.handle_workspace_edit_ready();

        assert_eq!(app.status(), Some("Code action: applied to 1 file"));
        assert_eq!(app.tabs[0].buffer.text(), "goodbye\nworld");
        assert!(app.tabs[0].buffer.is_dirty());
    }

    #[test]
    fn handle_workspace_edit_ready_with_an_unreadable_file_reports_the_io_error() {
        let dir = sample_project();
        let missing = dir.path().join("does-not-exist.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Code action".to_string());
        app.lsp.workspace_edit = Some(workspace_edit(vec![ide_lsp::FileEdit {
            path: missing,
            text_edits: vec![text_edit((0, 0), (0, 0), "x")],
        }]));

        app.handle_workspace_edit_ready();

        assert!(app
            .status()
            .unwrap()
            .contains("Code action: could not read"));
    }

    #[test]
    fn handle_workspace_edit_ready_with_an_out_of_range_edit_reports_it_and_writes_nothing() {
        let dir = sample_project();
        let b = dir.path().canonicalize().unwrap().join("b.txt"); // "second file"
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Code action".to_string());
        app.lsp.workspace_edit = Some(workspace_edit(vec![ide_lsp::FileEdit {
            path: b.clone(),
            text_edits: vec![text_edit((99, 0), (99, 1), "x")],
        }]));

        app.handle_workspace_edit_ready();

        assert!(app
            .status()
            .unwrap()
            .contains("does not fit its current content"));
        assert_eq!(fs::read_to_string(&b).unwrap(), "second file");
    }

    #[test]
    fn trigger_rename_with_no_active_tab_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(key(KeyModifiers::SHIFT, KeyCode::F(6)));
        assert!(app.rename_popup.is_none());
    }

    #[test]
    fn trigger_rename_with_no_running_language_server_sets_a_status_message() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));

        app.handle_key(key(KeyModifiers::SHIFT, KeyCode::F(6)));

        assert!(app.rename_popup.is_none());
        assert_eq!(app.status(), Some("Rename: no language server is running"));
    }

    /// `cat` is a real, always-spawnable process that blocks reading stdin
    /// and never replies -- the same technique `lsp_bridge.rs`'s own tests
    /// use to prove `is_running()`-gated logic runs past that gate without
    /// needing a real LSP responder.
    #[test]
    fn trigger_rename_with_caret_off_a_symbol_sets_a_status_message() {
        // A dedicated fixture (not `sample_project`) whose sole file's
        // content is two adjacent spaces -- the caret's default offset (0)
        // sits between two non-identifier characters, so `word_range_at`
        // returns `None` with no cursor movement needed.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("punct.txt"), "  ").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.start_with_command(dir.path(), "cat", &[]);
        assert!(app.lsp.is_running());
        app.handle_key(plain_key(KeyCode::Enter)); // the only row: punct.txt
        app.handle_key(ctrl('t'));

        app.handle_key(key(KeyModifiers::SHIFT, KeyCode::F(6)));

        assert!(app.rename_popup.is_none());
        assert_eq!(app.status(), Some("Rename: no symbol under the caret"));
    }

    #[test]
    fn trigger_rename_opens_a_prefilled_popup_and_requests_prepare_rename() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.start_with_command(dir.path(), "cat", &[]);
        app.handle_key(plain_key(KeyCode::Down)); // a.txt: "hello\nworld"
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));

        app.handle_key(key(KeyModifiers::SHIFT, KeyCode::F(6)));

        let popup = app.rename_popup.as_ref().expect("popup should be open");
        assert_eq!(popup.original_name, "hello");
        assert_eq!(popup.input, "hello");
        assert!(app.lsp.prepare_rename_target.is_some());
    }

    #[test]
    fn esc_cancels_the_rename_popup() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/f.rs"),
            position: Position {
                line: 0,
                character: 0,
            },
            original_name: "x".to_string(),
            input: "x".to_string(),
        });

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.rename_popup.is_none());
    }

    #[test]
    fn typing_while_the_rename_popup_is_open_edits_the_input() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/f.rs"),
            position: Position {
                line: 0,
                character: 0,
            },
            original_name: "x".to_string(),
            input: "x".to_string(),
        });

        app.handle_key(plain_key(KeyCode::Char('y')));
        assert_eq!(app.rename_popup.as_ref().unwrap().input, "xy");

        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.rename_popup.as_ref().unwrap().input, "x");
    }

    #[test]
    fn ctrl_held_letters_are_not_typed_into_the_rename_input() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/f.rs"),
            position: Position {
                line: 0,
                character: 0,
            },
            original_name: "x".to_string(),
            input: "x".to_string(),
        });

        app.handle_key(ctrl('w')); // would otherwise close the tab

        assert!(app.rename_popup.is_some());
        assert_eq!(app.rename_popup.as_ref().unwrap().input, "x");
    }

    #[test]
    fn confirm_rename_with_no_popup_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.confirm_rename();
        assert!(app.rename_popup.is_none());
    }

    #[test]
    fn confirm_rename_with_unchanged_or_empty_input_sends_nothing_and_closes_silently() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.start_with_command(dir.path(), "cat", &[]);
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/f.rs"),
            position: Position {
                line: 0,
                character: 0,
            },
            original_name: "x".to_string(),
            input: "x".to_string(),
        });
        app.confirm_rename();
        assert!(app.rename_popup.is_none());
        assert!(app.lsp.rename_new_name.is_none());

        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/f.rs"),
            position: Position {
                line: 0,
                character: 0,
            },
            original_name: "x".to_string(),
            input: "   ".to_string(),
        });
        app.confirm_rename();
        assert!(app.rename_popup.is_none());
        assert!(app.lsp.rename_new_name.is_none());
    }

    #[test]
    fn confirm_rename_with_a_changed_name_closes_the_popup_and_sends_a_rename_request() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.start_with_command(dir.path(), "cat", &[]);
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/f.rs"),
            position: Position {
                line: 0,
                character: 0,
            },
            original_name: "x".to_string(),
            input: " count ".to_string(),
        });

        app.confirm_rename();

        assert!(app.rename_popup.is_none());
    }

    #[test]
    fn handle_prepare_rename_ready_is_a_noop_when_not_ready() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/f.rs"),
            position: Position {
                line: 0,
                character: 0,
            },
            original_name: "x".to_string(),
            input: "x".to_string(),
        });
        app.lsp.prepare_rename_ready = false;
        app.lsp.prepare_renameable = Some(false);

        app.handle_prepare_rename_ready();

        assert!(app.rename_popup.is_some());
    }

    #[test]
    fn handle_prepare_rename_ready_with_true_leaves_the_popup_open() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        let target = (
            PathBuf::from("/f.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        app.rename_popup = Some(RenamePopup {
            path: target.0.clone(),
            position: target.1,
            original_name: "x".to_string(),
            input: "x".to_string(),
        });
        app.lsp.prepare_rename_target = Some(target);
        app.lsp.prepare_renameable = Some(true);
        app.lsp.prepare_rename_ready = true;

        app.handle_prepare_rename_ready();

        assert!(app.rename_popup.is_some());
    }

    #[test]
    fn handle_prepare_rename_ready_with_false_and_a_matching_target_closes_the_popup() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        let target = (
            PathBuf::from("/f.rs"),
            Position {
                line: 0,
                character: 0,
            },
        );
        app.rename_popup = Some(RenamePopup {
            path: target.0.clone(),
            position: target.1,
            original_name: "x".to_string(),
            input: "x".to_string(),
        });
        app.lsp.prepare_rename_target = Some(target);
        app.lsp.prepare_renameable = Some(false);
        app.lsp.prepare_rename_ready = true;

        app.handle_prepare_rename_ready();

        assert!(app.rename_popup.is_none());
        assert_eq!(app.status(), Some("Rename: this element cannot be renamed"));
    }

    #[test]
    fn handle_prepare_rename_ready_with_false_but_a_stale_target_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/current.rs"),
            position: Position {
                line: 0,
                character: 0,
            },
            original_name: "x".to_string(),
            input: "x".to_string(),
        });
        app.lsp.prepare_rename_target = Some((
            PathBuf::from("/stale.rs"),
            Position {
                line: 0,
                character: 0,
            },
        ));
        app.lsp.prepare_renameable = Some(false);
        app.lsp.prepare_rename_ready = true;

        app.handle_prepare_rename_ready();

        assert!(app.rename_popup.is_some());
    }

    #[test]
    fn handle_prepare_rename_ready_with_false_and_no_popup_open_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.rename_popup = None;
        app.lsp.prepare_renameable = Some(false);
        app.lsp.prepare_rename_ready = true;

        app.handle_prepare_rename_ready();

        assert!(app.rename_popup.is_none());
        assert!(app.status().is_none());
    }

    #[test]
    fn handle_rename_ready_is_a_noop_when_not_ready() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.rename_ready = false;
        app.lsp.rename_edit = Some(workspace_edit(vec![]));
        app.handle_rename_ready();
        assert!(app.status().is_none());
        assert!(app.lsp.rename_edit.is_some());
    }

    #[test]
    fn handle_rename_ready_with_no_edit_reports_nothing_to_apply() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.rename_ready = true;
        app.lsp.rename_edit = None;
        app.lsp.rename_new_name = Some("count".to_string());

        app.handle_rename_ready();

        assert_eq!(app.status(), Some("Rename to `count`: nothing to apply"));
    }

    #[test]
    fn handle_rename_ready_with_a_single_file_edit_matching_the_active_tab_applies_immediately() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt"); // "hello\nworld"
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.lsp.rename_ready = true;
        app.lsp.rename_new_name = Some("count".to_string());
        app.lsp.rename_edit = Some(workspace_edit(vec![ide_lsp::FileEdit {
            path: a,
            text_edits: vec![text_edit((0, 0), (0, 5), "count")],
        }]));

        app.handle_rename_ready();

        assert_eq!(app.status(), Some("Rename to `count`: applied to 1 file"));
        assert_eq!(app.tabs[0].buffer.text(), "count\nworld");
        assert!(app.pending_rename_preview.is_none());
    }

    #[test]
    fn handle_rename_ready_with_multiple_files_escalates_to_preview() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let b = dir.path().canonicalize().unwrap().join("b.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));
        app.lsp.rename_ready = true;
        app.lsp.rename_new_name = Some("process_batch".to_string());
        app.lsp.rename_edit = Some(workspace_edit(vec![
            ide_lsp::FileEdit {
                path: a,
                text_edits: vec![text_edit((0, 0), (0, 5), "process_batch")],
            },
            ide_lsp::FileEdit {
                path: b,
                text_edits: vec![text_edit((0, 0), (0, 6), "process_batch")],
            },
        ]));

        app.handle_rename_ready();

        assert!(app.status().is_none());
        let (edit, new_name) = app
            .pending_rename_preview
            .as_ref()
            .expect("should escalate to preview");
        assert_eq!(edit.edits.len(), 2);
        assert_eq!(new_name, "process_batch");
    }

    #[test]
    fn handle_rename_ready_with_a_single_file_not_matching_the_active_tab_escalates_to_preview() {
        let dir = sample_project();
        let b = dir.path().canonicalize().unwrap().join("b.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // opens a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.lsp.rename_ready = true;
        app.lsp.rename_new_name = Some("count".to_string());
        app.lsp.rename_edit = Some(workspace_edit(vec![ide_lsp::FileEdit {
            path: b,
            text_edits: vec![text_edit((0, 0), (0, 6), "count")],
        }]));

        app.handle_rename_ready();

        assert!(app.pending_rename_preview.is_some());
    }

    #[test]
    fn rename_ready_escalating_to_preview_closes_overlays_opened_during_the_async_wait() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let b = dir.path().canonicalize().unwrap().join("b.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.cargo_panel_open = true;
        app.lsp.rename_ready = true;
        app.lsp.rename_new_name = Some("count".to_string());
        app.lsp.rename_edit = Some(workspace_edit(vec![
            ide_lsp::FileEdit {
                path: a,
                text_edits: vec![text_edit((0, 0), (0, 5), "count")],
            },
            ide_lsp::FileEdit {
                path: b,
                text_edits: vec![text_edit((0, 0), (0, 6), "count")],
            },
        ]));

        app.handle_rename_ready();

        assert!(!app.cargo_panel_open);
        assert!(app.pending_rename_preview.is_some());
    }

    #[test]
    fn esc_cancels_the_rename_preview_without_writing_anything() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.pending_rename_preview = Some((
            workspace_edit(vec![ide_lsp::FileEdit {
                path: a.clone(),
                text_edits: vec![text_edit((0, 0), (0, 5), "count")],
            }]),
            "count".to_string(),
        ));

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.pending_rename_preview.is_none());
        assert_eq!(fs::read_to_string(&a).unwrap(), "hello\nworld");
    }

    #[test]
    fn enter_applies_the_rename_preview() {
        let dir = sample_project();
        let b = dir.path().canonicalize().unwrap().join("b.txt"); // "second file"
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.pending_rename_preview = Some((
            workspace_edit(vec![ide_lsp::FileEdit {
                path: b.clone(),
                text_edits: vec![text_edit((0, 0), (0, 6), "REPLACED")],
            }]),
            "REPLACED".to_string(),
        ));

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.pending_rename_preview.is_none());
        assert_eq!(fs::read_to_string(&b).unwrap(), "REPLACED file");
        assert_eq!(
            app.status(),
            Some("Rename to `REPLACED`: applied to 1 file")
        );
    }

    #[test]
    fn while_rename_preview_is_open_other_keys_are_swallowed() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.pending_rename_preview = Some((
            workspace_edit(vec![ide_lsp::FileEdit {
                path: a,
                text_edits: vec![text_edit((0, 0), (0, 5), "count")],
            }]),
            "count".to_string(),
        ));

        app.handle_key(plain_key(KeyCode::Char('x')));

        assert!(app.pending_rename_preview.is_some());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn workspace_text_edits_to_transaction_converts_a_single_edit() {
        let text = "hello world";
        let edits = vec![text_edit((0, 0), (0, 5), "goodbye")];
        let transaction = workspace_text_edits_to_transaction(text, &edits).unwrap();
        assert_eq!(transaction.changes().len(), 1);
    }

    #[test]
    fn workspace_text_edits_to_transaction_with_an_unconvertible_position_is_none() {
        let text = "short";
        let edits = vec![text_edit((99, 0), (99, 1), "x")];
        assert!(workspace_text_edits_to_transaction(text, &edits).is_none());
    }

    // -- T11: Git Panel (docs/features/tui-git-panel.md) --

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git_commit(dir: &std::path::Path, name: &str, content: &str, message: &str) {
        fs::write(dir.join(name), content).unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", message]);
    }

    /// `-b main` and a repo-local identity make these repos independent of
    /// the runner's ambient git config: `init.defaultBranch` isn't set on
    /// every CI image (tests that `checkout main` later would otherwise
    /// fail with a "pathspec 'main' did not match" error), and `GitRepo`'s
    /// production commit path goes through git2, which reads `user.name`/
    /// `user.email` from git config -- never from the `GIT_AUTHOR_NAME`
    /// env vars `run_git` sets for its own `git` CLI subprocess -- so a
    /// runner with no global identity configured fails signature creation
    /// (mirrors the fix already applied to `ide-core`'s own git tests).
    fn init_git_repo(dir: &std::path::Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.name", "Test"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
    }

    fn sample_git_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        git_commit(dir.path(), "a.txt", "hello\nworld", "first");
        dir
    }

    #[test]
    fn new_refreshes_git_state_once_at_startup() {
        let dir = sample_git_project();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.git.is_repo());
        assert_eq!(app.git.graph.len(), 1);
    }

    #[test]
    fn new_on_a_non_repo_leaves_git_state_empty() {
        let dir = sample_project();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(!app.git.is_repo());
        assert!(app.git.graph.is_empty());
    }

    #[test]
    fn toggle_git_panel_opens_and_closes() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.git_panel.is_none());

        app.toggle_git_panel();
        assert!(app.git_panel.is_some());

        app.toggle_git_panel();
        assert!(app.git_panel.is_none());
    }

    #[test]
    fn toggle_git_panel_closes_other_overlays() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.problems = Some(ProblemsState { selected: 0 });

        app.toggle_git_panel();

        assert!(app.git_panel.is_some());
        assert!(app.problems.is_none());
    }

    #[test]
    fn close_all_overlays_closes_the_git_panel() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        assert!(app.git_panel.is_some());

        app.close_all_overlays();
        assert!(app.git_panel.is_none());
    }

    #[test]
    fn handle_git_panel_key_esc_closes_the_panel() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.git_panel.is_none());
    }

    #[test]
    fn handle_git_panel_key_tab_cycles_focus_skipping_empty_conflicts() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.git.conflicts.is_empty());
        app.toggle_git_panel();
        assert_eq!(app.git_panel.as_ref().unwrap().focus, GitPanelFocus::Graph);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.git_panel.as_ref().unwrap().focus, GitPanelFocus::Diff);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.git_panel.as_ref().unwrap().focus, GitPanelFocus::Filter);

        // A further `Tab` while `Filter` has focus is captured by
        // `handle_git_filter_key`'s own internal `FilterField` cycle
        // (§3.5) rather than continuing `GitPanelFocus`'s cycle -- `Esc`
        // is the only way back to `Graph` from here (§3.2/§3.5).
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.git_panel.as_ref().unwrap().focus, GitPanelFocus::Filter);
        assert_eq!(
            app.git_panel.as_ref().unwrap().filter_field,
            FilterField::Author
        );

        app.handle_key(plain_key(KeyCode::Esc));
        assert_eq!(app.git_panel.as_ref().unwrap().focus, GitPanelFocus::Graph);
    }

    #[test]
    fn handle_git_panel_key_up_down_clamp_the_graph_selection() {
        let dir = sample_git_project();
        git_commit(dir.path(), "a.txt", "hello\nworld2", "second");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(app.git.graph.len(), 2);
        app.toggle_git_panel();

        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(app.git_panel.as_ref().unwrap().graph_selected, 0);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.git_panel.as_ref().unwrap().graph_selected, 1);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.git_panel.as_ref().unwrap().graph_selected, 1);
    }

    #[test]
    fn handle_git_panel_key_enter_on_graph_loads_diff_and_focuses_diff() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.git.selected_commit.is_some());
        assert!(app.git.diff.is_some());
        assert_eq!(app.git_panel.as_ref().unwrap().focus, GitPanelFocus::Diff);
    }

    #[test]
    fn handle_git_panel_key_diff_focus_scrolls() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Enter)); // -> Diff focus

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.git_panel.as_ref().unwrap().diff_scroll, 1);

        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(app.git_panel.as_ref().unwrap().diff_scroll, 0);

        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(app.git_panel.as_ref().unwrap().diff_scroll, 0);

        app.handle_key(plain_key(KeyCode::PageDown));
        assert_eq!(app.git_panel.as_ref().unwrap().diff_scroll, 10);

        app.handle_key(plain_key(KeyCode::PageUp));
        assert_eq!(app.git_panel.as_ref().unwrap().diff_scroll, 0);
    }

    fn setup_conflict(dir: &std::path::Path) {
        git_commit(dir, "f.txt", "base\n", "base");
        run_git(dir, &["checkout", "-qb", "theirs"]);
        git_commit(dir, "f.txt", "theirs\n", "theirs change");
        run_git(dir, &["checkout", "-q", "-"]);
        git_commit(dir, "f.txt", "ours\n", "ours change");
        let status = std::process::Command::new("git")
            .args(["merge", "-q", "theirs"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(!status.success(), "expected a merge conflict");
    }

    #[test]
    fn handle_git_panel_key_enter_on_conflicts_enters_resolving_mode() {
        let dir = sample_git_project();
        setup_conflict(dir.path());
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(app.git.conflicts.len(), 1);
        app.toggle_git_panel();

        app.handle_key(plain_key(KeyCode::Tab)); // Graph -> Conflicts
        assert_eq!(
            app.git_panel.as_ref().unwrap().focus,
            GitPanelFocus::Conflicts
        );
        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.git.active_conflict.is_some());
    }

    #[test]
    fn handle_git_panel_key_resolving_accept_ours_and_theirs() {
        let dir = sample_git_project();
        setup_conflict(dir.path());
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.git.active_conflict.is_some());

        app.handle_key(plain_key(KeyCode::Char('t')));
        assert_eq!(app.git.active_conflict.as_ref().unwrap().result, "theirs\n");

        app.handle_key(plain_key(KeyCode::Char('o')));
        assert_eq!(app.git.active_conflict.as_ref().unwrap().result, "ours\n");
    }

    #[test]
    fn handle_git_panel_key_resolving_esc_cancels_without_closing_the_panel() {
        let dir = sample_git_project();
        setup_conflict(dir.path());
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.git.active_conflict.is_some());

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.git.active_conflict.is_none());
        assert!(app.git_panel.is_some());
    }

    #[test]
    fn handle_git_panel_key_resolving_enter_marks_resolved() {
        let dir = sample_git_project();
        setup_conflict(dir.path());
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(plain_key(KeyCode::Char('t')));

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.git.active_conflict.is_none());
        assert!(app.git.conflicts.is_empty());
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "theirs\n"
        );
    }

    // ---- T28: view switching ----

    #[test]
    fn g_s_b_switch_views_and_open_the_branches_popup() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        assert_eq!(app.git_panel.as_ref().unwrap().view, GitPanelView::Log);

        app.handle_key(plain_key(KeyCode::Char('s')));
        assert_eq!(app.git_panel.as_ref().unwrap().view, GitPanelView::Changes);

        app.handle_key(plain_key(KeyCode::Char('g')));
        assert_eq!(app.git_panel.as_ref().unwrap().view, GitPanelView::Log);

        app.handle_key(plain_key(KeyCode::Char('b')));
        assert!(app.git.branches_popup.open);
    }

    #[test]
    fn g_s_b_do_not_leak_into_a_commit_message_being_typed() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('s')));
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(
            app.git_panel.as_ref().unwrap().changes_focus,
            ChangesFocus::Message
        );

        for c in "Fix bug in gitignore".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        assert_eq!(app.git.commit_message, "Fix bug in gitignore");
        assert_eq!(app.git_panel.as_ref().unwrap().view, GitPanelView::Changes);
        assert!(!app.git.branches_popup.open);
    }

    #[test]
    fn g_s_b_do_not_leak_into_a_log_filter_field_being_typed() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('f')));
        assert_eq!(app.git_panel.as_ref().unwrap().focus, GitPanelFocus::Filter);

        for c in "gsb-author".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        assert_eq!(app.git.log_filter.branch, "gsb-author");
        assert_eq!(app.git_panel.as_ref().unwrap().view, GitPanelView::Log);
        assert!(!app.git.branches_popup.open);
    }

    #[test]
    fn esc_from_filter_focus_returns_to_graph_without_closing_the_panel() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('f')));
        app.handle_key(plain_key(KeyCode::Char('x')));

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.git_panel.is_some());
        assert_eq!(app.git_panel.as_ref().unwrap().focus, GitPanelFocus::Graph);
        assert_eq!(
            app.git.log_filter.branch, "x",
            "Esc must not discard typed text"
        );
    }

    // ---- T28: Changes view ----

    #[test]
    fn changes_view_tab_cycles_staged_unstaged_message() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('s')));
        assert_eq!(
            app.git_panel.as_ref().unwrap().changes_focus,
            ChangesFocus::Staged
        );

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(
            app.git_panel.as_ref().unwrap().changes_focus,
            ChangesFocus::Unstaged
        );

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(
            app.git_panel.as_ref().unwrap().changes_focus,
            ChangesFocus::Message
        );

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(
            app.git_panel.as_ref().unwrap().changes_focus,
            ChangesFocus::Staged
        );
    }

    #[test]
    fn changes_view_enter_stages_and_unstages() {
        let dir = sample_git_project();
        fs::write(dir.path().join("a.txt"), "hello\nworld2").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.git.sync_status();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('s')));
        assert_eq!(app.git.status.unstaged.len(), 1);

        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.git.status.unstaged.is_empty());
        assert_eq!(app.git.status.staged.len(), 1);

        app.handle_key(plain_key(KeyCode::BackTab));
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.git.status.unstaged.len(), 1);
        assert!(app.git.status.staged.is_empty());
    }

    #[test]
    fn changes_view_x_requests_discard_and_y_confirms_it() {
        let dir = sample_git_project();
        fs::write(dir.path().join("a.txt"), "hello\nworld2").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.git.sync_status();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('s')));
        app.handle_key(plain_key(KeyCode::Tab));

        app.handle_key(plain_key(KeyCode::Char('x')));
        assert!(app.git.pending_discard.is_some());

        app.handle_key(plain_key(KeyCode::Char('y')));
        assert!(app.git.pending_discard.is_none());
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "hello\nworld"
        );
    }

    #[test]
    fn changes_view_discard_confirm_n_cancels_without_touching_the_file() {
        let dir = sample_git_project();
        fs::write(dir.path().join("a.txt"), "hello\nworld2").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.git.sync_status();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('s')));
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Char('x')));

        app.handle_key(plain_key(KeyCode::Char('n')));

        assert!(app.git.pending_discard.is_none());
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "hello\nworld2"
        );
    }

    #[test]
    fn changes_view_a_toggles_amend_but_not_while_typing_a_message() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('s')));
        assert!(!app.git.amend);

        app.handle_key(plain_key(KeyCode::Char('a')));
        assert!(app.git.amend);
        app.handle_key(plain_key(KeyCode::Char('a')));
        assert!(!app.git.amend);

        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(
            app.git_panel.as_ref().unwrap().changes_focus,
            ChangesFocus::Message
        );
        app.handle_key(plain_key(KeyCode::Char('a')));
        assert_eq!(app.git.commit_message, "a");
        assert!(!app.git.amend, "'a' while typing must not toggle amend");
    }

    #[test]
    fn changes_view_enter_on_message_commits() {
        let dir = sample_git_project();
        fs::write(dir.path().join("a.txt"), "hello\nworld2").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.git.sync_status();
        app.git.stage(std::path::Path::new("a.txt")).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('s')));
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Tab));

        for c in "second".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.git.commit_message.is_empty());
        assert_eq!(app.git.graph.len(), 2);
    }

    // ---- T28: branches popup ----

    #[test]
    fn branches_popup_enter_checks_out_the_selected_branch() {
        let dir = sample_git_project();
        run_git(dir.path(), &["branch", "feature"]);
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('b')));
        let idx = app
            .filtered_branch_rows()
            .iter()
            .position(|(name, _)| name == "feature")
            .unwrap();
        for _ in 0..idx {
            app.handle_key(plain_key(KeyCode::Down));
        }

        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(app.git.current_branch.as_deref(), Some("feature"));
        assert!(!app.git.branches_popup.open);
    }

    #[test]
    fn branches_popup_n_then_typing_then_enter_creates_and_checks_out() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('b')));

        app.handle_key(plain_key(KeyCode::Char('n')));
        assert!(app.git.branches_popup.show_new_branch_input);
        for c in "feature".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(app.git.current_branch.as_deref(), Some("feature"));
        assert!(!app.git.branches_popup.open);
    }

    #[test]
    fn branches_popup_new_branch_esc_cancels_typing_without_creating() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('b')));
        app.handle_key(plain_key(KeyCode::Char('n')));
        for c in "abandoned".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(!app.git.branches_popup.show_new_branch_input);
        assert!(
            app.git.branches_popup.open,
            "Esc only cancels typing, not the popup"
        );
        assert!(!app.git.branches.iter().any(|b| b.name == "abandoned"));
    }

    #[test]
    fn branches_popup_d_then_d_force_deletes_an_unmerged_branch() {
        let dir = sample_git_project();
        run_git(dir.path(), &["checkout", "-qb", "feature"]);
        git_commit(dir.path(), "a.txt", "hello\nfeature", "feature change");
        run_git(dir.path(), &["checkout", "-q", "main"]);
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('b')));
        let idx = app
            .filtered_branch_rows()
            .iter()
            .position(|(name, _)| name == "feature")
            .unwrap();
        for _ in 0..idx {
            app.handle_key(plain_key(KeyCode::Down));
        }

        app.handle_key(plain_key(KeyCode::Char('d')));
        assert_eq!(
            app.git.branches_popup.pending_delete.as_deref(),
            Some("feature")
        );

        app.handle_key(plain_key(KeyCode::Char('d')));
        assert!(app.git.branches_popup.pending_delete.is_none());
        assert!(!app.git.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn branches_popup_delete_confirm_esc_cancels() {
        // Must be an *unmerged* branch -- a fully-merged one deletes on
        // the first `d` (the "safe attempt" succeeds immediately), never
        // reaching a pending-confirm state for `Esc` to cancel.
        let dir = sample_git_project();
        run_git(dir.path(), &["checkout", "-qb", "feature"]);
        git_commit(dir.path(), "a.txt", "hello\nfeature", "feature change");
        run_git(dir.path(), &["checkout", "-q", "main"]);
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('b')));
        let idx = app
            .filtered_branch_rows()
            .iter()
            .position(|(name, _)| name == "feature")
            .unwrap();
        for _ in 0..idx {
            app.handle_key(plain_key(KeyCode::Down));
        }
        app.handle_key(plain_key(KeyCode::Char('d')));
        assert!(app.git.branches_popup.pending_delete.is_some());

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.git.branches_popup.pending_delete.is_none());
        assert!(app.git.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn branches_popup_slash_filters_by_typed_text_and_letters_do_not_trigger_commands() {
        let dir = sample_git_project();
        run_git(dir.path(), &["branch", "develop"]);
        run_git(dir.path(), &["branch", "release"]);
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('b')));
        assert_eq!(app.filtered_branch_rows().len(), 3);

        app.handle_key(plain_key(KeyCode::Char('/')));
        for c in "dev".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        // "dev" contains 'd', which is the delete command outside typing
        // mode -- proving it reached `filter` instead is exactly what
        // this test is for.
        assert_eq!(app.git.branches_popup.filter, "dev");
        let rows = app.filtered_branch_rows();
        assert!(rows.iter().any(|(name, _)| name == "develop"));
        assert!(!rows.iter().any(|(name, _)| name == "release"));
        assert!(app.git.branches.iter().any(|b| b.name == "develop"));
    }

    #[test]
    fn branches_popup_filter_esc_stops_typing_without_clearing_the_text() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('b')));
        app.handle_key(plain_key(KeyCode::Char('/')));
        app.handle_key(plain_key(KeyCode::Char('m')));

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(!app.git.branches_popup.typing_filter);
        assert_eq!(app.git.branches_popup.filter, "m");
        assert!(app.git.branches_popup.open);
    }

    // ---- T28: log filter bar ----

    #[test]
    fn filter_bar_tab_cycles_fields_and_enter_applies() {
        let dir = sample_git_project();
        git_commit(dir.path(), "a.txt", "hello\nworld2", "second");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('f')));
        assert_eq!(
            app.git_panel.as_ref().unwrap().filter_field,
            FilterField::Branch
        );

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(
            app.git_panel.as_ref().unwrap().filter_field,
            FilterField::Author
        );
        // "Test" -- `run_git`'s fixed `GIT_AUTHOR_NAME` -- must actually
        // match, since `author` and `query` are ANDed together.
        for c in "Test".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(
            app.git_panel.as_ref().unwrap().filter_field,
            FilterField::Query
        );
        for c in "second".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.git.log_filter.error.is_none());
        assert_eq!(app.git.graph.len(), 1);
    }

    #[test]
    fn filter_bar_ctrl_c_clears_but_bare_c_types_into_the_field() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Char('f')));

        for c in "carol".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        assert_eq!(app.git.log_filter.branch, "carol");

        app.handle_key(ctrl('c'));
        assert!(app.git.log_filter.branch.is_empty());
    }

    // ---- T28: ShowFileHistory / GitBranches commands ----

    #[test]
    fn trigger_show_file_history_is_a_noop_with_no_active_tab() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.trigger_show_file_history();
        assert!(app.git_panel.is_none());
    }

    #[test]
    fn trigger_show_file_history_opens_the_panel_on_the_files_history() {
        let dir = sample_git_project();
        git_commit(dir.path(), "a.txt", "hello\nworld2", "second");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(dir.path().join("a.txt")).unwrap();

        app.trigger_show_file_history();

        assert!(app.git_panel.is_some());
        assert_eq!(app.git_panel.as_ref().unwrap().view, GitPanelView::Log);
        assert_eq!(
            app.git.log_filter.viewing_file_history,
            Some(std::path::PathBuf::from("a.txt"))
        );
        assert_eq!(app.git.graph.len(), 2);
    }

    #[test]
    fn trigger_git_branches_opens_the_panel_and_the_popup() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.trigger_git_branches();

        assert!(app.git_panel.is_some());
        assert!(app.git.branches_popup.open);
    }

    #[test]
    fn sync_git_status_is_a_noop_with_the_panel_closed() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        fs::write(dir.path().join("a.txt"), "hello\nworld2").unwrap();

        app.sync_git_status();

        assert!(app.git.status.unstaged.is_empty());
    }

    #[test]
    fn sync_git_status_refreshes_the_open_panels_lists() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        fs::write(dir.path().join("a.txt"), "hello\nworld2").unwrap();

        app.sync_git_status();

        assert_eq!(app.git.status.unstaged.len(), 1);
    }

    #[test]
    fn sync_git_working_tree_diff_is_a_noop_with_the_panel_closed() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.sync_git_working_tree_diff();
        assert!(app.git.diff.is_none());
        assert!(app.last_git_diff_target.is_none());
    }

    #[test]
    fn sync_git_working_tree_diff_loads_the_active_tabs_diff() {
        let dir = sample_git_project();
        fs::write(dir.path().join("a.txt"), "hello\nworld2").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        let path = dir.path().join("a.txt");
        app.tabs.push(OpenBuffer {
            path: path.clone(),
            buffer: Buffer::open(&path).unwrap(),
            scroll: 0,
            desired_column: None,
            auto_closed: None,
            config: EditorConfig::default(),
            indent: IndentUnit::default(),
            charset_notice_shown: false,
            shrink_stack: Vec::new(),
            folded: std::collections::BTreeSet::new(),
            external_change: None,
            blame: None,
        });
        app.active_tab = Some(0);

        app.sync_git_working_tree_diff();

        assert!(app.git.diff.is_some());
        assert_eq!(app.last_git_diff_target, Some(path));
    }

    #[test]
    fn sync_git_working_tree_diff_clears_when_no_active_tab() {
        let dir = sample_git_project();
        fs::write(dir.path().join("a.txt"), "hello\nworld2").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        let path = dir.path().join("a.txt");
        app.tabs.push(OpenBuffer {
            path: path.clone(),
            buffer: Buffer::open(&path).unwrap(),
            scroll: 0,
            desired_column: None,
            auto_closed: None,
            config: EditorConfig::default(),
            indent: IndentUnit::default(),
            charset_notice_shown: false,
            shrink_stack: Vec::new(),
            folded: std::collections::BTreeSet::new(),
            external_change: None,
            blame: None,
        });
        app.active_tab = Some(0);
        app.sync_git_working_tree_diff();
        assert!(app.git.diff.is_some());

        app.active_tab = None;
        app.sync_git_working_tree_diff();

        assert!(app.git.diff.is_none());
    }

    #[test]
    fn sync_git_working_tree_diff_is_a_noop_once_a_commit_is_selected() {
        let dir = sample_git_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.toggle_git_panel();
        app.handle_key(plain_key(KeyCode::Enter)); // selects the commit, loads its diff
        let commit_diff = app.git.diff.clone();

        let path = dir.path().join("a.txt");
        app.tabs.push(OpenBuffer {
            path: path.clone(),
            buffer: Buffer::open(&path).unwrap(),
            scroll: 0,
            desired_column: None,
            auto_closed: None,
            config: EditorConfig::default(),
            indent: IndentUnit::default(),
            charset_notice_shown: false,
            shrink_stack: Vec::new(),
            folded: std::collections::BTreeSet::new(),
            external_change: None,
            blame: None,
        });
        app.active_tab = Some(0);
        app.sync_git_working_tree_diff();

        assert_eq!(
            app.git.diff.as_ref().map(|d| d.len()),
            commit_diff.as_ref().map(|d| d.len())
        );
        assert!(app.last_git_diff_target.is_none());
    }

    // -- T18a: smart editing (`docs/features/tui-smart-editing.md`) --

    #[test]
    fn enter_indents_one_level_after_an_opening_brace() {
        let (_dir, mut app) = open_rust_tab("fn main() {\n");
        let caret_at = "fn main() {".len();
        set_caret(&mut app, caret_at);

        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(active_text(&app), "fn main() {\n    \n");
        assert_eq!(caret(&app), "fn main() {\n    ".len());
    }

    #[test]
    fn enter_expands_a_split_pair_into_three_lines() {
        let (_dir, mut app) = open_rust_tab("fn main() {}");
        let caret_at = "fn main() {".len();
        set_caret(&mut app, caret_at);

        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(active_text(&app), "fn main() {\n    \n}");
        assert_eq!(caret(&app), "fn main() {\n    ".len());
    }

    #[test]
    fn enter_without_syntax_rules_falls_back_to_a_plain_newline() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t'));
        set_caret(&mut app, 0);

        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(active_text(&app), "\nhello\nworld");
    }

    #[test]
    fn auto_close_inserts_the_closer_and_leaves_the_caret_between() {
        let (_dir, mut app) = open_rust_tab("");

        app.handle_key(plain_key(KeyCode::Char('(')));

        assert_eq!(active_text(&app), "()");
        assert_eq!(caret(&app), 1);
    }

    #[test]
    fn auto_close_is_skipped_before_an_identifier() {
        let (_dir, mut app) = open_rust_tab("foo");
        set_caret(&mut app, 0);

        app.handle_key(plain_key(KeyCode::Char('(')));

        assert_eq!(active_text(&app), "(foo", "no closer inserted before 'f'");
        assert_eq!(caret(&app), 1);
    }

    #[test]
    fn auto_close_admits_a_pair_before_whitespace_and_before_a_closer() {
        let (_dir, mut app) = open_rust_tab(" )");
        set_caret(&mut app, 0);
        app.handle_key(plain_key(KeyCode::Char('(')));
        assert_eq!(active_text(&app), "() )");

        set_caret(&mut app, "()".len());
        app.handle_key(plain_key(KeyCode::Char('[')));
        assert_eq!(active_text(&app), "()[] )");
    }

    #[test]
    fn auto_close_quote_is_skipped_inside_an_existing_string() {
        let (_dir, mut app) = open_rust_tab(r#"let s = "hi";"#);
        let inside_the_string = r#"let s = "hi"#.len();
        set_caret(&mut app, inside_the_string);

        app.handle_key(plain_key(KeyCode::Char('\'')));

        assert_eq!(
            active_text(&app),
            r#"let s = "hi'";"#,
            "a bare quote is typed, not an auto-closed pair, inside a string"
        );
    }

    #[test]
    fn type_over_skips_a_closer_the_previous_keystroke_auto_inserted() {
        let (_dir, mut app) = open_rust_tab("");

        app.handle_key(plain_key(KeyCode::Char('(')));
        assert_eq!(active_text(&app), "()");
        app.handle_key(plain_key(KeyCode::Char(')')));

        assert_eq!(
            active_text(&app),
            "()",
            "type-over must move past, not insert a second ')'"
        );
        assert_eq!(caret(&app), 2);
    }

    #[test]
    fn type_over_does_not_fire_once_another_keystroke_clears_the_window() {
        let (_dir, mut app) = open_rust_tab("");

        app.handle_key(plain_key(KeyCode::Char('(')));
        assert_eq!(active_text(&app), "()");
        app.handle_key(plain_key(KeyCode::Left));
        app.handle_key(plain_key(KeyCode::Right));
        app.handle_key(plain_key(KeyCode::Char(')')));

        assert_eq!(
            active_text(&app),
            "())",
            "the type-over window is exactly one keystroke wide"
        );
    }

    #[test]
    fn backspace_deletes_both_halves_of_an_adjacent_pair() {
        let (_dir, mut app) = open_rust_tab("()");
        set_caret(&mut app, 1);

        app.handle_key(plain_key(KeyCode::Backspace));

        assert_eq!(active_text(&app), "");
    }

    #[test]
    fn backspace_without_a_pair_deletes_one_character() {
        let (_dir, mut app) = open_rust_tab("ab");
        set_caret(&mut app, 2);

        app.handle_key(plain_key(KeyCode::Backspace));

        assert_eq!(active_text(&app), "a");
    }

    #[test]
    fn backspace_with_a_non_empty_selection_deletes_the_whole_selection() {
        let (_dir, mut app) = open_rust_tab("foobar");
        set_selection(&mut app, 3..6);

        app.handle_key(plain_key(KeyCode::Backspace));

        assert_eq!(active_text(&app), "foo");
        assert_eq!(caret(&app), 3);
    }

    #[test]
    fn surround_selection_wraps_it_and_keeps_it_selected() {
        let (_dir, mut app) = open_rust_tab("foo");
        set_selection(&mut app, 0..3);

        app.handle_key(plain_key(KeyCode::Char('(')));

        assert_eq!(active_text(&app), "(foo)");
        let selection = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary();
        assert_eq!(selection.range(), 1..4, "selection still covers \"foo\"");
    }

    #[test]
    fn jump_to_matching_bracket_moves_past_the_match() {
        let (_dir, mut app) = open_rust_tab("(foo)");
        set_caret(&mut app, 0);

        app.run_action(Action::JumpToMatchingBracket);

        assert_eq!(caret(&app), 5);
    }

    #[test]
    fn jump_to_matching_bracket_with_no_match_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("foo");
        set_caret(&mut app, 1);

        app.run_action(Action::JumpToMatchingBracket);

        assert_eq!(caret(&app), 1);
    }

    #[test]
    fn tab_with_no_selection_inserts_one_indent_unit() {
        let (_dir, mut app) = open_rust_tab("");

        app.handle_key(plain_key(KeyCode::Tab));

        assert_eq!(active_text(&app), "    ");
        assert_eq!(caret(&app), 4);
    }

    #[test]
    fn tab_with_a_selection_indents_every_touched_line() {
        let (_dir, mut app) = open_rust_tab("a\nb\n");
        set_selection(&mut app, 0..3);

        app.handle_key(plain_key(KeyCode::Tab));

        assert_eq!(active_text(&app), "    a\n    b\n");
    }

    #[test]
    fn backtab_outdents_even_without_a_selection() {
        let (_dir, mut app) = open_rust_tab("    a");
        set_caret(&mut app, 4);

        app.handle_key(plain_key(KeyCode::BackTab));

        assert_eq!(active_text(&app), "a");
    }

    // -- T18b: line commands + EditorConfig
    // (`docs/features/tui-line-commands-and-editorconfig.md`) --

    #[test]
    fn duplicate_lines_copies_the_line_below_and_moves_the_caret_onto_it() {
        let (_dir, mut app) = open_rust_tab("foo\nbar\n");
        set_caret(&mut app, 1); // inside "foo"

        app.run_action(Action::DuplicateLines);

        assert_eq!(active_text(&app), "foo\nfoo\nbar\n");
        assert_eq!(caret(&app), 5); // inside the copy, same column
    }

    #[test]
    fn delete_lines_removes_the_line_and_its_trailing_newline() {
        let (_dir, mut app) = open_rust_tab("foo\nbar\n");
        set_caret(&mut app, 1);

        app.run_action(Action::DeleteLines);

        assert_eq!(active_text(&app), "bar\n");
    }

    #[test]
    fn join_lines_collapses_the_newline_into_one_space() {
        let (_dir, mut app) = open_rust_tab("foo\nbar\n");
        set_caret(&mut app, 1);

        app.run_action(Action::JoinLines);

        assert_eq!(active_text(&app), "foo bar\n");
    }

    #[test]
    fn move_lines_up_swaps_with_the_line_above() {
        let (_dir, mut app) = open_rust_tab("foo\nbar\n");
        set_caret(&mut app, 5); // inside "bar"

        app.run_action(Action::MoveLinesUp);

        assert_eq!(active_text(&app), "bar\nfoo\n");
    }

    #[test]
    fn move_lines_down_swaps_with_the_line_below() {
        let (_dir, mut app) = open_rust_tab("foo\nbar\n");
        set_caret(&mut app, 1);

        app.run_action(Action::MoveLinesDown);

        assert_eq!(active_text(&app), "bar\nfoo\n");
    }

    #[test]
    fn move_lines_up_at_the_buffers_first_line_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("foo\nbar\n");
        set_caret(&mut app, 1);

        app.run_action(Action::MoveLinesUp);

        assert_eq!(active_text(&app), "foo\nbar\n");
    }

    #[test]
    fn move_statements_down_jumps_the_whole_balanced_block() {
        let (_dir, mut app) = open_rust_tab("fn a() {\n    one();\n}\nfn b() {}\n");
        set_caret(&mut app, 0); // on "fn a() {"

        app.run_action(Action::MoveStatementsDown);

        assert_eq!(active_text(&app), "fn b() {}\nfn a() {\n    one();\n}\n");
    }

    #[test]
    fn toggle_line_comment_comments_then_uncomments() {
        let (_dir, mut app) = open_rust_tab("let a = 1;\n");
        set_caret(&mut app, 0);

        app.run_action(Action::ToggleLineComment);
        assert_eq!(active_text(&app), "// let a = 1;\n");

        app.run_action(Action::ToggleLineComment);
        assert_eq!(active_text(&app), "let a = 1;\n");
    }

    #[test]
    fn toggle_block_comment_wraps_then_unwraps() {
        let (_dir, mut app) = open_rust_tab("foo");
        set_selection(&mut app, 0..3);

        app.run_action(Action::ToggleBlockComment);
        assert_eq!(active_text(&app), "/*foo*/");

        set_selection(&mut app, 0.."/*foo*/".len());
        app.run_action(Action::ToggleBlockComment);
        assert_eq!(active_text(&app), "foo");
    }

    #[test]
    fn toggle_case_upper_cases_a_lowercase_selection() {
        let (_dir, mut app) = open_rust_tab("foo");
        set_selection(&mut app, 0..3);

        app.run_action(Action::ToggleCase);

        assert_eq!(active_text(&app), "FOO");
    }

    #[test]
    fn toggle_case_on_an_empty_selection_acts_on_the_word_under_the_caret() {
        let (_dir, mut app) = open_rust_tab("foo bar");
        set_caret(&mut app, 1); // inside "foo"

        app.run_action(Action::ToggleCase);

        assert_eq!(active_text(&app), "FOO bar");
    }

    #[test]
    fn extend_then_shrink_selection_walks_back_down_the_same_path() {
        let (_dir, mut app) = open_rust_tab("f(x + 1)");
        set_caret(&mut app, 2); // inside "x"

        app.run_action(Action::ExtendSelection); // "x"
        let after_first = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .range();
        assert_eq!(&active_text(&app)[after_first.clone()], "x");

        app.run_action(Action::ExtendSelection); // "x + 1"
        let after_second = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .range();
        assert_eq!(&active_text(&app)[after_second], "x + 1");

        app.run_action(Action::ShrinkSelection); // back to "x"
        let restored = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary()
            .range();
        assert_eq!(restored, after_first);
    }

    #[test]
    fn shrink_selection_with_an_empty_stack_falls_back_to_the_word_under_the_caret() {
        let (_dir, mut app) = open_rust_tab("foo bar");
        set_caret(&mut app, 1); // inside "foo", nothing extended yet

        app.run_action(Action::ShrinkSelection);

        let selection = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .primary();
        assert_eq!(selection.range(), 0..3);
    }

    #[test]
    fn an_edit_clears_the_shrink_stack() {
        let (_dir, mut app) = open_rust_tab("f(x + 1)");
        set_caret(&mut app, 2);
        app.run_action(Action::ExtendSelection);
        assert!(!app.active_buffer().unwrap().shrink_stack.is_empty());

        app.handle_key(plain_key(KeyCode::Char('!')));

        assert!(app.active_buffer().unwrap().shrink_stack.is_empty());
    }

    #[test]
    fn an_arrow_move_clears_the_shrink_stack() {
        let (_dir, mut app) = open_rust_tab("f(x + 1)");
        set_caret(&mut app, 2);
        app.run_action(Action::ExtendSelection);
        assert!(!app.active_buffer().unwrap().shrink_stack.is_empty());

        app.handle_key(plain_key(KeyCode::Left));

        assert!(app.active_buffer().unwrap().shrink_stack.is_empty());
    }

    #[test]
    fn opening_a_file_resolves_its_editorconfig_indent_size() {
        let (_dir, app) = open_rust_tab_with_editorconfig(
            "fn main() {}",
            "root = true\n\n[*.rs]\nindent_size = 2\n",
        );

        assert_eq!(app.active_buffer().unwrap().indent.width, 2);
    }

    #[test]
    fn tab_uses_the_resolved_editorconfig_indent_size() {
        let (_dir, mut app) =
            open_rust_tab_with_editorconfig("", "root = true\n\n[*.rs]\nindent_size = 2\n");

        app.handle_key(plain_key(KeyCode::Tab));

        assert_eq!(active_text(&app), "  ");
    }

    #[test]
    fn a_file_outside_any_editorconfig_section_gets_the_default_indent() {
        let (_dir, app) = open_rust_tab_with_editorconfig(
            "fn main() {}",
            "root = true\n\n[*.py]\nindent_size = 2\n",
        );

        assert_eq!(app.active_buffer().unwrap().indent, IndentUnit::default());
    }

    #[test]
    fn save_active_trims_trailing_whitespace_per_editorconfig() {
        let (dir, mut app) = open_rust_tab_with_editorconfig(
            "let a = 1;   ",
            "root = true\n\n[*.rs]\ntrim_trailing_whitespace = true\n",
        );

        app.run_action(Action::SaveActive);

        let on_disk = fs::read_to_string(dir.path().join("f.rs")).unwrap();
        assert_eq!(on_disk, "let a = 1;");
        assert!(!app.active_buffer().unwrap().buffer.is_dirty());
    }

    #[test]
    fn save_active_without_an_editorconfig_behaves_like_a_plain_save() {
        let (_dir, mut app) = open_rust_tab("let a = 1;   ");

        app.run_action(Action::SaveActive);

        // No `.editorconfig` anywhere above a bare tempdir project root --
        // `EditorConfig::default()`, so `save_edit` is `None` and the text
        // is written verbatim, exactly as the pre-T18b bare `save()` did.
        assert_eq!(active_text(&app), "let a = 1;   ");
        assert!(!app.active_buffer().unwrap().buffer.is_dirty());
    }

    #[test]
    fn save_active_notifies_once_for_an_unsupported_charset() {
        let (_dir, mut app) = open_rust_tab_with_editorconfig(
            "let a = 1;",
            "root = true\n\n[*.rs]\ncharset = latin1\n",
        );

        app.run_action(Action::SaveActive);
        assert_eq!(app.notifications.len(), 1);
        assert!(app.notifications[0].message.contains("UTF-8"));

        app.handle_key(plain_key(KeyCode::Char('!')));
        app.run_action(Action::SaveActive);
        assert_eq!(
            app.notifications.len(),
            1,
            "the notice must not repeat on a later save of the same tab"
        );
    }

    #[test]
    fn duplicate_lines_has_a_ctrl_d_binding() {
        let action = binding_for(ctrl('d'));
        assert_eq!(action, Some(Action::DuplicateLines));
    }

    #[test]
    fn extend_and_shrink_selection_have_literal_alt_arrow_bindings() {
        assert_eq!(
            binding_for(key(KeyModifiers::ALT, KeyCode::Up)),
            Some(Action::ExtendSelection)
        );
        assert_eq!(
            binding_for(key(KeyModifiers::ALT, KeyCode::Down)),
            Some(Action::ShrinkSelection)
        );
    }

    // -- T19: code folding (`tui-code-folding.md`) --

    const FOLD_FIXTURE: &str =
        "fn foo() {\n    let x = 1;\n    let y = 2;\n}\nfn bar() {\n    let z = 3;\n}\n";

    fn line_of(app: &App, needle: &str) -> usize {
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        let offset = text_buffer.text().find(needle).unwrap();
        cursor_line_column(text_buffer, offset).0
    }

    #[test]
    fn collapse_fold_folds_the_innermost_range_and_ctrl_plus_reverses_it() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        let foo_start = line_of(&app, "fn foo");
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());

        app.handle_key(ctrl('-'));
        assert!(app.active_buffer().unwrap().folded.contains(&foo_start));

        app.handle_key(ctrl('+'));
        assert!(app.active_buffer().unwrap().folded.is_empty());
    }

    #[test]
    fn collapse_fold_with_no_containing_range_is_a_noop() {
        // A standalone statement after both functions, inside neither
        // range -- `FOLD_FIXTURE` itself has no such line (its two
        // functions cover every line back-to-back).
        const NO_FOLD_HERE: &str = "fn foo() {\n    let x = 1;\n}\nlet end = 1;\n";
        let (_dir, mut app) = open_rust_tab(NO_FOLD_HERE);
        set_caret(&mut app, NO_FOLD_HERE.find("let end").unwrap());
        app.handle_key(ctrl('-'));
        assert!(app.active_buffer().unwrap().folded.is_empty());
    }

    #[test]
    fn expand_fold_with_the_caret_not_on_a_collapsed_start_line_is_a_noop() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        let foo_start = line_of(&app, "fn foo");
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());
        app.handle_key(ctrl('-'));
        set_caret(&mut app, FOLD_FIXTURE.find("let y").unwrap()); // now hidden, but try anyway
        app.handle_key(ctrl('+'));
        assert!(app.active_buffer().unwrap().folded.contains(&foo_start));
    }

    #[test]
    fn collapsing_around_the_caret_moves_it_onto_the_start_line() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        let foo_start = line_of(&app, "fn foo");
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());

        app.handle_key(ctrl('-'));

        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        assert_eq!(cursor_line_column(text_buffer, caret(&app)).0, foo_start);
    }

    #[test]
    fn collapse_all_folds_folds_every_top_level_range_and_expand_all_clears_them() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        let foo_start = line_of(&app, "fn foo");
        let bar_start = line_of(&app, "fn bar");
        set_caret(&mut app, 0);

        app.handle_key(ctrl_shift('-'));
        let folded = app.active_buffer().unwrap().folded.clone();
        assert!(folded.contains(&foo_start));
        assert!(folded.contains(&bar_start));

        app.handle_key(ctrl_shift('+'));
        assert!(app.active_buffer().unwrap().folded.is_empty());
    }

    #[test]
    fn down_from_a_collapsed_start_line_skips_straight_past_its_interior() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        let foo_start = line_of(&app, "fn foo");
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());
        app.handle_key(ctrl('-')); // caret lands on foo_start (line 0)
        assert_eq!(
            cursor_line_column(
                app.active_buffer().unwrap().buffer.text_buffer(),
                caret(&app)
            )
            .0,
            foo_start
        );

        app.handle_key(plain_key(KeyCode::Down));

        let bar_start = line_of(&app, "fn bar");
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        assert_eq!(cursor_line_column(text_buffer, caret(&app)).0, bar_start);
    }

    #[test]
    fn up_into_a_collapsed_range_lands_on_its_start_line() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());
        app.handle_key(ctrl('-'));
        let bar_start = line_of(&app, "fn bar");
        set_caret(&mut app, FOLD_FIXTURE.find("fn bar").unwrap());
        assert_eq!(
            cursor_line_column(
                app.active_buffer().unwrap().buffer.text_buffer(),
                caret(&app)
            )
            .0,
            bar_start
        );

        app.handle_key(plain_key(KeyCode::Up));

        let foo_start = line_of(&app, "fn foo");
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        assert_eq!(cursor_line_column(text_buffer, caret(&app)).0, foo_start);
    }

    #[test]
    fn right_at_the_end_of_a_collapsed_start_line_skips_to_the_row_after_the_fold() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());
        app.handle_key(ctrl('-'));
        // Caret sits at foo_start's own line after the collapse; move it
        // to the end of that line's visible text before pressing `Right`.
        let end_of_start_line = FOLD_FIXTURE.find("fn foo() {").unwrap() + "fn foo() {".len();
        set_caret(&mut app, end_of_start_line);

        app.handle_key(plain_key(KeyCode::Right));

        let bar_start = line_of(&app, "fn bar");
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        let (line, column) = cursor_line_column(text_buffer, caret(&app));
        assert_eq!(line, bar_start);
        assert_eq!(column, 0);
    }

    #[test]
    fn left_from_the_row_after_a_fold_skips_back_to_the_end_of_the_start_line() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());
        app.handle_key(ctrl('-'));
        let bar_start_offset = FOLD_FIXTURE.find("fn bar").unwrap();
        set_caret(&mut app, bar_start_offset);

        app.handle_key(plain_key(KeyCode::Left));

        let foo_start = line_of(&app, "fn foo");
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        let (line, column) = cursor_line_column(text_buffer, caret(&app));
        assert_eq!(line, foo_start);
        assert_eq!(column, "fn foo() {".len());
    }

    #[test]
    fn open_location_reveals_a_fold_hiding_its_target() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        let foo_start = line_of(&app, "fn foo");
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());
        app.handle_key(ctrl('-'));
        assert!(app.active_buffer().unwrap().folded.contains(&foo_start));

        let path = app.active_buffer().unwrap().path.clone();
        let target = ide_lsp::position_to_byte_offset(
            FOLD_FIXTURE,
            ide_lsp::Position {
                line: line_of(&app, "let y") as u32,
                character: 4,
            },
        )
        .unwrap();
        app.open_location(Location {
            path,
            range: ide_lsp::Range {
                start: ide_lsp::Position {
                    line: line_of(&app, "let y") as u32,
                    character: 4,
                },
                end: ide_lsp::Position {
                    line: line_of(&app, "let y") as u32,
                    character: 4,
                },
            },
        });

        assert!(!app.active_buffer().unwrap().folded.contains(&foo_start));
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();
        assert_eq!(
            cursor_line_column(text_buffer, target).0,
            line_of(&app, "let y")
        );
    }

    #[test]
    fn delete_at_the_end_of_a_collapsed_start_line_does_not_eat_the_hidden_newline() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());
        app.handle_key(ctrl('-')); // collapses foo's body; caret lands on foo_start
        let end_of_start_line = FOLD_FIXTURE.find("fn foo() {").unwrap() + "fn foo() {".len();
        set_caret(&mut app, end_of_start_line);

        app.handle_key(plain_key(KeyCode::Delete));

        // Deletes the whole hidden interior as one unit (mirroring
        // `Right`'s own redirect target, `tui-code-folding.md` §3.6) --
        // never a single silently-eaten hidden character.
        assert_eq!(
            active_text(&app),
            "fn foo() {fn bar() {\n    let z = 3;\n}\n"
        );
    }

    #[test]
    fn backspace_at_the_start_of_the_row_after_a_fold_does_not_eat_the_hidden_newline() {
        let (_dir, mut app) = open_rust_tab(FOLD_FIXTURE);
        set_caret(&mut app, FOLD_FIXTURE.find("let x").unwrap());
        app.handle_key(ctrl('-'));
        let bar_start_offset = FOLD_FIXTURE.find("fn bar").unwrap();
        set_caret(&mut app, bar_start_offset);

        app.handle_key(plain_key(KeyCode::Backspace));

        assert_eq!(
            active_text(&app),
            "fn foo() {fn bar() {\n    let z = 3;\n}\n"
        );
    }

    #[test]
    fn collapse_fold_has_a_ctrl_minus_binding_and_expand_all_has_ctrl_shift_plus() {
        assert_eq!(binding_for(ctrl('-')), Some(Action::CollapseFold));
        assert_eq!(binding_for(ctrl('+')), Some(Action::ExpandFold));
        assert_eq!(binding_for(ctrl_shift('-')), Some(Action::CollapseAllFolds));
        assert_eq!(binding_for(ctrl_shift('+')), Some(Action::ExpandAllFolds));
    }

    // -- T20: multiple cursors (`tui-multiple-cursors.md`) --

    fn set_selections_multi(app: &mut App, ranges: &[Range<usize>], primary: usize) {
        let selections = ranges
            .iter()
            .map(|r| Selection::new(r.start, r.end))
            .collect();
        app.active_buffer_mut()
            .unwrap()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::new(selections, primary));
    }

    fn all_ranges(app: &App) -> Vec<Range<usize>> {
        app.active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .selections()
            .all()
            .iter()
            .map(|s| s.range())
            .collect()
    }

    #[test]
    fn add_next_occurrence_on_an_empty_caret_selects_the_word_and_stops() {
        let (_dir, mut app) = open_rust_tab("let count = 1;\nlet count2 = count;");
        set_caret(&mut app, 4); // inside "count"
        app.handle_key(ctrl('g'));
        assert_eq!(all_ranges(&app), vec![4..9]);
    }

    #[test]
    fn add_next_occurrence_then_a_second_press_adds_the_next_match() {
        let (_dir, mut app) = open_rust_tab("count + count + count");
        set_caret(&mut app, 0);
        app.handle_key(ctrl('g')); // selects "count" at 0..5
        app.handle_key(ctrl('g')); // adds the next "count" at 8..13
        let mut ranges = all_ranges(&app);
        ranges.sort_by_key(|r| r.start);
        assert_eq!(ranges, vec![0..5, 8..13]);
    }

    #[test]
    fn add_next_occurrence_wraps_and_stops_once_everything_is_selected() {
        let (_dir, mut app) = open_rust_tab("count + count");
        set_selections_multi(&mut app, &[0..5, 8..13], 1);
        app.handle_key(ctrl('g')); // wraps back to 0..5, already selected -> no-op
        let mut ranges = all_ranges(&app);
        ranges.sort_by_key(|r| r.start);
        assert_eq!(ranges, vec![0..5, 8..13]);
    }

    #[test]
    fn add_next_occurrence_on_whitespace_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("   \nlet x = 1;");
        set_caret(&mut app, 1);
        app.handle_key(ctrl('g'));
        assert_eq!(all_ranges(&app), vec![1..1]);
    }

    #[test]
    fn unselect_occurrence_removes_the_most_recently_added_selection() {
        let (_dir, mut app) = open_rust_tab("count + count");
        set_selections_multi(&mut app, &[0..5, 8..13], 1);
        app.handle_key(ctrl_shift('g'));
        assert_eq!(all_ranges(&app), vec![0..5]);
    }

    #[test]
    fn unselect_occurrence_with_one_selection_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("count");
        set_selection(&mut app, 0..5);
        app.handle_key(ctrl_shift('g'));
        assert_eq!(all_ranges(&app), vec![0..5]);
    }

    fn ctrl_alt_shift(c: char) -> KeyEvent {
        key(
            KeyModifiers::CONTROL
                .union(KeyModifiers::ALT)
                .union(KeyModifiers::SHIFT),
            KeyCode::Char(c),
        )
    }

    #[test]
    fn select_all_occurrences_selects_every_match_in_one_press() {
        let (_dir, mut app) = open_rust_tab("count + count + count");
        set_caret(&mut app, 4);
        app.handle_key(ctrl_alt_shift('j'));
        let mut ranges = all_ranges(&app);
        ranges.sort_by_key(|r| r.start);
        assert_eq!(ranges, vec![0..5, 8..13, 16..21]);
    }

    #[test]
    fn select_all_occurrences_keeps_the_match_containing_the_old_primary_as_primary() {
        let (_dir, mut app) = open_rust_tab("count + count + count");
        set_caret(&mut app, 12); // inside the second "count" (8..13)
        app.handle_key(ctrl_alt_shift('j'));
        assert_eq!(
            app.active_buffer()
                .unwrap()
                .buffer
                .text_buffer()
                .selections()
                .primary()
                .range(),
            8..13
        );
    }

    #[test]
    fn select_all_occurrences_on_whitespace_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("   \nlet x = 1;");
        set_caret(&mut app, 1);
        app.handle_key(ctrl_alt_shift('j'));
        assert_eq!(all_ranges(&app), vec![1..1]);
    }

    #[test]
    fn esc_collapses_multiple_selections_to_the_primary() {
        let (_dir, mut app) = open_rust_tab("count + count");
        set_selections_multi(&mut app, &[0..5, 8..13], 1);
        app.handle_key(plain_key(KeyCode::Esc));
        assert_eq!(all_ranges(&app), vec![8..13]);
    }

    #[test]
    fn esc_with_one_selection_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("count");
        set_selection(&mut app, 0..5);
        app.handle_key(plain_key(KeyCode::Esc));
        assert_eq!(all_ranges(&app), vec![0..5]);
    }

    #[test]
    fn esc_outside_editor_focus_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("count + count");
        set_selections_multi(&mut app, &[0..5, 8..13], 1);
        app.focus = Focus::Tree;
        app.handle_key(plain_key(KeyCode::Esc));
        assert_eq!(all_ranges(&app), vec![0..5, 8..13]);
    }

    #[test]
    fn typing_replaces_every_selection_in_one_undo_step() {
        let (_dir, mut app) = open_rust_tab("count + count");
        set_selections_multi(&mut app, &[0..5, 8..13], 0);
        app.handle_key(plain_key(KeyCode::Char('n')));
        assert_eq!(active_text(&app), "n + n");
        app.handle_key(ctrl('z'));
        assert_eq!(active_text(&app), "count + count");
    }

    #[test]
    fn arrow_right_moves_every_caret_independently() {
        let (_dir, mut app) = open_rust_tab("aa bb");
        set_selections_multi(&mut app, &[0..0, 3..3], 0);
        app.handle_key(plain_key(KeyCode::Right));
        assert_eq!(all_ranges(&app), vec![1..1, 4..4]);
    }

    #[test]
    fn arrow_up_moves_every_caret_using_one_shared_desired_column() {
        let (_dir, mut app) = open_rust_tab("aaaa\nbb\ncccc");
        // Caret 0 at the end of line 1 ("bb", offset 7, column 2); caret 1
        // (primary) at the end of line 2 ("cccc", offset 12, column 4).
        set_selections_multi(&mut app, &[7..7, 12..12], 1);

        app.handle_key(plain_key(KeyCode::Up));
        // First vertical move: `desired_column` starts `None`, so each
        // selection uses its *own* current column -- caret 0 -> line 0 at
        // its own column 2 (offset 2); caret 1 (primary) -> line 1 at its
        // own column 4, clamped to "bb"'s length 2 (offset 7). The shared
        // column recorded for next time is the *primary*'s own column, 4.
        assert_eq!(all_ranges(&app), vec![2..2, 7..7]);

        app.handle_key(plain_key(KeyCode::Up));
        // Second vertical move: both carets now share column 4. Caret 0
        // is already on row 0 -- `Up` there is a no-op (stays at offset
        // 2). Caret 1 moves to line 0 at the shared column 4 (offset 4,
        // "aaaa"'s own length, unclamped).
        assert_eq!(all_ranges(&app), vec![2..2, 4..4]);
    }

    #[test]
    fn enter_inserts_an_independently_indented_newline_per_selection() {
        let (_dir, mut app) = open_rust_tab("    aa\n    bb");
        let first_caret = "    aa".len();
        let second_caret = first_caret + 1 + "    bb".len();
        set_selections_multi(
            &mut app,
            &[first_caret..first_caret, second_caret..second_caret],
            0,
        );
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(active_text(&app), "    aa\n    \n    bb\n    ");
    }

    #[test]
    fn backspace_deletes_one_character_at_every_caret() {
        let (_dir, mut app) = open_rust_tab("aXa\nbXb");
        set_selections_multi(&mut app, &[2..2, 6..6], 0);
        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(active_text(&app), "aa\nbb");
    }

    #[test]
    fn delete_forward_removes_one_character_at_every_caret() {
        let (_dir, mut app) = open_rust_tab("aXa\nbXb");
        set_selections_multi(&mut app, &[1..1, 5..5], 0);
        app.handle_key(plain_key(KeyCode::Delete));
        assert_eq!(active_text(&app), "aa\nbb");
    }

    #[test]
    fn delete_forward_on_a_non_empty_selection_deletes_its_whole_range() {
        let (_dir, mut app) = open_rust_tab("hello world");
        set_selection(&mut app, 0..5);
        app.handle_key(plain_key(KeyCode::Delete));
        assert_eq!(active_text(&app), " world");
    }

    #[test]
    fn tab_inserts_an_indent_unit_at_every_empty_caret() {
        let (_dir, mut app) = open_rust_tab("a\nb");
        set_selections_multi(&mut app, &[0..0, 2..2], 0);
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(active_text(&app), "    a\n    b");
    }

    #[test]
    fn open_delimiter_wraps_every_non_empty_selection_independently() {
        let (_dir, mut app) = open_rust_tab("aa bb");
        set_selections_multi(&mut app, &[0..2, 3..5], 0);
        app.handle_key(plain_key(KeyCode::Char('(')));
        assert_eq!(active_text(&app), "(aa) (bb)");
    }

    #[test]
    fn open_delimiter_auto_closes_at_every_empty_caret() {
        // Both carets sit right before a space, so `may_open_pair` admits
        // at both independently (`editor.rs::may_open_pair`).
        let (_dir, mut app) = open_rust_tab("a b c");
        set_selections_multi(&mut app, &[1..1, 3..3], 0);
        app.handle_key(plain_key(KeyCode::Char('(')));
        assert_eq!(active_text(&app), "a() b() c");
    }

    #[test]
    fn apply_per_selection_leaves_everything_untouched_on_an_overlapping_derived_range() {
        // Two bare carets one character apart: Backspace's own-position
        // derived ranges (`head-1..head`, since neither sits between a
        // recognised bracket pair here) would be `0..1` and `1..2` --
        // adjacent, not overlapping, so this actually succeeds; the
        // all-or-nothing guarantee is instead exercised directly against
        // `apply_per_selection` with a contrived closure that reports
        // overlapping ranges for two selections.
        let (_dir, mut app) = open_rust_tab("hello");
        set_selections_multi(&mut app, &[1..1, 2..2], 0);
        let before = active_text(&app);
        let buf = app.active_buffer_mut().unwrap();
        let changed = apply_per_selection(buf, |_, selection| {
            (
                selection.head.saturating_sub(1)..selection.head + 1,
                String::new(),
                0,
                0,
            )
        });
        assert!(!changed);
        assert_eq!(active_text(&app), before);
        assert_eq!(all_ranges(&app), vec![1..1, 2..2]);
    }

    // -- T16: Go to File / Go to Symbol (`tui-go-to-file-and-symbol.md`) --

    fn symbol(name: &str, path: PathBuf) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: ide_lsp::SymbolKind::Function,
            container_name: None,
            location: location(path, 0, 0),
        }
    }

    #[test]
    fn toggle_go_to_file_opens_with_a_reset_state_and_closes() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.go_to_file.is_none());

        app.run_action(Action::GoToFile);
        assert!(app.go_to_file.is_some());
        app.go_to_file.as_mut().unwrap().query = "x".to_string();
        app.go_to_file.as_mut().unwrap().selected = 3;

        app.run_action(Action::GoToFile);
        assert!(app.go_to_file.is_none());

        app.run_action(Action::GoToFile);
        let state = app.go_to_file.as_ref().unwrap();
        assert_eq!(state.query, "");
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn ctrl_shift_n_opens_go_to_file() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('n'),
        ));
        assert!(app.go_to_file.is_some());
    }

    #[test]
    fn opening_go_to_file_closes_other_overlays() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.search_open = true;

        app.run_action(Action::GoToFile);

        assert!(!app.search_open);
        assert!(app.go_to_file.is_some());
    }

    #[test]
    fn go_to_file_typing_and_backspace_edit_the_query() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_file = Some(GoToFileState::default());

        app.handle_key(plain_key(KeyCode::Char('a')));
        app.handle_key(plain_key(KeyCode::Char('b')));
        assert_eq!(app.go_to_file.as_ref().unwrap().query, "ab");

        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.go_to_file.as_ref().unwrap().query, "a");
    }

    #[test]
    fn go_to_file_esc_closes_without_opening_anything() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_file = Some(GoToFileState::default());

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.go_to_file.is_none());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn sync_go_to_file_is_a_noop_with_an_empty_query() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_file = Some(GoToFileState::default());

        app.sync_go_to_file();

        assert!(!app.files_search.searching);
    }

    #[test]
    fn sync_go_to_file_runs_once_per_distinct_query_then_confirm_opens_the_top_match() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_file = Some(GoToFileState::default());
        app.go_to_file.as_mut().unwrap().query = "a.txt".to_string();

        app.sync_go_to_file();
        assert!(app.files_search.searching);

        // A second call with the same query while still running must not
        // start a second background search (`§3.1`'s cadence rule).
        app.sync_go_to_file();

        wait_until(|| {
            app.poll_search();
            !app.files_search.searching
        });

        let matches = &app.files_search.results.as_ref().unwrap().matches;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].relative, "a.txt");

        app.confirm_go_to_file();
        assert!(app.go_to_file.is_none());
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(
            app.active_buffer()
                .unwrap()
                .buffer
                .text_buffer()
                .selections()
                .primary()
                .head,
            0
        );
    }

    #[test]
    fn sync_go_to_file_resets_the_selection_when_a_new_query_starts() {
        // A `selected` index left pointing past a shorter, freshly-started
        // query's eventual result set must not silently stick -- confirm
        // opening a stale-out-of-range row is a safe no-op, matching
        // `submit_or_open_search_result`'s own reset-on-new-search
        // precedent.
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_file = Some(GoToFileState {
            query: "a.txt".to_string(),
            selected: 3,
            ran_query: None,
        });

        app.sync_go_to_file();

        assert_eq!(app.go_to_file.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn confirm_go_to_file_with_no_results_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_file = Some(GoToFileState::default());

        app.confirm_go_to_file();

        assert!(app.go_to_file.is_some());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn toggle_go_to_symbol_opens_with_a_reset_state_and_closes() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.go_to_symbol.is_none());

        app.run_action(Action::GoToSymbol);
        assert!(app.go_to_symbol.is_some());
        app.go_to_symbol.as_mut().unwrap().query = "x".to_string();

        app.run_action(Action::GoToSymbol);
        assert!(app.go_to_symbol.is_none());

        app.run_action(Action::GoToSymbol);
        assert_eq!(app.go_to_symbol.as_ref().unwrap().query, "");
    }

    #[test]
    fn ctrl_alt_shift_n_opens_go_to_symbol() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(key(
            KeyModifiers::CONTROL
                .union(KeyModifiers::ALT)
                .union(KeyModifiers::SHIFT),
            KeyCode::Char('n'),
        ));
        assert!(app.go_to_symbol.is_some());
    }

    #[test]
    fn go_to_symbol_rows_shows_document_symbols_for_an_empty_query() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.document_symbols = vec![symbol("Outline", a)];
        app.lsp.workspace_symbols = vec![symbol("SearchHit", PathBuf::from("/other"))];
        app.go_to_symbol = Some(GoToSymbolState::default());

        let rows = app.go_to_symbol_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Outline");
    }

    #[test]
    fn go_to_symbol_rows_shows_workspace_symbols_for_a_non_empty_query() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.document_symbols = vec![symbol("Outline", a)];
        app.lsp.workspace_symbols = vec![symbol("SearchHit", PathBuf::from("/other"))];
        app.go_to_symbol = Some(GoToSymbolState {
            query: "hit".to_string(),
            ..Default::default()
        });

        let rows = app.go_to_symbol_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "SearchHit");
    }

    #[test]
    fn sync_go_to_symbol_requests_the_outline_once_per_distinct_path() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.tabs.len(), 1);
        app.go_to_symbol = Some(GoToSymbolState::default());

        app.sync_go_to_symbol();
        let path = app.active_buffer().unwrap().path.clone();
        assert_eq!(app.go_to_symbol.as_ref().unwrap().requested_for, Some(path));

        // A second call against the same path is a no-op -- proven by the
        // guard itself not panicking/changing state a second time (no
        // running LSP client in this test project, so there's nothing
        // else observable to assert on the send side).
        app.sync_go_to_symbol();
    }

    #[test]
    fn sync_go_to_symbol_queries_the_workspace_once_per_distinct_query() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_symbol = Some(GoToSymbolState {
            query: "needle".to_string(),
            ..Default::default()
        });

        app.sync_go_to_symbol();
        assert_eq!(
            app.go_to_symbol.as_ref().unwrap().last_workspace_query,
            Some("needle".to_string())
        );
    }

    #[test]
    fn sync_go_to_symbol_resets_the_selection_when_a_new_query_starts() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_symbol = Some(GoToSymbolState {
            query: "needle".to_string(),
            selected: 5,
            ..Default::default()
        });

        app.sync_go_to_symbol();

        assert_eq!(app.go_to_symbol.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn confirm_go_to_symbol_jumps_to_the_selected_symbols_location() {
        let dir = sample_project();
        let a = dir.path().canonicalize().unwrap().join("a.txt");
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.lsp.document_symbols = vec![symbol("Outline", a.clone())];
        app.go_to_symbol = Some(GoToSymbolState::default());

        app.confirm_go_to_symbol();

        assert!(app.go_to_symbol.is_none());
        assert_eq!(app.active_buffer().unwrap().path, a);
    }

    #[test]
    fn confirm_go_to_symbol_with_no_rows_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_symbol = Some(GoToSymbolState::default());

        app.confirm_go_to_symbol();

        assert!(app.go_to_symbol.is_some());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn go_to_file_and_go_to_symbol_are_mutually_exclusive() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::GoToFile);
        assert!(app.go_to_file.is_some());
        assert!(app.go_to_symbol.is_none());

        app.run_action(Action::GoToSymbol);
        assert!(app.go_to_symbol.is_some());
        assert!(app.go_to_file.is_none());
    }

    // -- T17: Recent Files / Bookmarks (`tui-recent-files-and-bookmarks.md`) --

    #[test]
    fn ctrl_e_opens_recent_files() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.recent_files.is_none());

        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('e')));
        assert!(app.recent_files.is_some());
    }

    #[test]
    fn toggle_recent_files_opens_with_a_reset_state_and_closes() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::RecentFiles);
        assert!(app.recent_files.is_some());
        app.recent_files.as_mut().unwrap().query = "x".to_string();
        app.recent_files.as_mut().unwrap().selected = 3;

        app.run_action(Action::RecentFiles);
        assert!(app.recent_files.is_none());

        app.run_action(Action::RecentFiles);
        let state = app.recent_files.as_ref().unwrap();
        assert_eq!(state.query, "");
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn opening_a_file_records_it_as_the_most_recent() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(app.nav_state.recent_files, vec![root.join("a.txt")]);
    }

    #[test]
    fn refocusing_an_already_open_tab_moves_it_to_the_front_of_recent_files() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(plain_key(KeyCode::Down)); // b.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(
            app.nav_state.recent_files,
            vec![root.join("b.txt"), root.join("a.txt")]
        );

        // Refocus a.txt without going through the tree -- directly via
        // `open_or_focus_tab`'s already-open branch.
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        assert_eq!(
            app.nav_state.recent_files,
            vec![root.join("a.txt"), root.join("b.txt")]
        );
    }

    #[test]
    fn recent_files_rows_with_an_empty_query_preserves_mru_order() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.open_or_focus_tab(root.join("b.txt")).unwrap();

        app.recent_files = Some(RecentFilesState::default());
        assert_eq!(
            app.recent_files_rows(),
            vec![root.join("b.txt"), root.join("a.txt")]
        );
    }

    #[test]
    fn recent_files_rows_with_a_query_filters_by_fuzzy_score() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.open_or_focus_tab(root.join("b.txt")).unwrap();

        app.recent_files = Some(RecentFilesState {
            query: "a.txt".to_string(),
            selected: 0,
        });
        assert_eq!(app.recent_files_rows(), vec![root.join("a.txt")]);
    }

    #[test]
    fn recent_files_typing_resets_the_selection() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.recent_files = Some(RecentFilesState {
            query: String::new(),
            selected: 2,
        });

        app.handle_key(plain_key(KeyCode::Char('x')));
        assert_eq!(app.recent_files.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn recent_files_esc_closes_without_opening_anything() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::RecentFiles);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.recent_files.is_none());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn confirm_recent_file_opens_the_selected_row() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(dir.path().join("a.txt")).unwrap();
        app.open_or_focus_tab(dir.path().join("b.txt")).unwrap();
        app.active_tab = None;

        app.run_action(Action::RecentFiles);
        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.recent_files.is_none());
        assert_eq!(app.active_buffer().unwrap().path, root.join("b.txt"));
    }

    #[test]
    fn confirm_recent_file_with_no_rows_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::RecentFiles);

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.recent_files.is_some());
    }

    #[test]
    fn toggle_bookmark_with_no_active_tab_notifies_and_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::ToggleBookmark);

        assert!(app.nav_state.bookmarks.is_empty());
        assert!(!app.notifications.is_empty());
    }

    #[test]
    fn f3_toggles_a_bookmark_at_the_current_line() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));

        app.handle_key(plain_key(KeyCode::F(3)));
        assert_eq!(app.nav_state.bookmarks.len(), 1);
        assert_eq!(app.nav_state.bookmarks[0].line, 0);

        app.handle_key(plain_key(KeyCode::F(3)));
        assert!(app.nav_state.bookmarks.is_empty());
    }

    #[test]
    fn toggle_bookmark_persists_across_a_fresh_app_instance() {
        let dir = sample_project();
        {
            let mut app = App::new(dir.path().to_path_buf()).unwrap();
            app.handle_key(plain_key(KeyCode::Down)); // a.txt
            app.handle_key(plain_key(KeyCode::Enter));
            app.run_action(Action::ToggleBookmark);
        }

        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(app.nav_state.bookmarks.len(), 1);
    }

    #[test]
    fn ctrl_f3_opens_show_bookmarks() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.bookmarks_popup.is_none());

        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::F(3)));
        assert!(app.bookmarks_popup.is_some());
    }

    #[test]
    fn opening_bookmarks_popup_closes_recent_files() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::RecentFiles);
        assert!(app.recent_files.is_some());

        app.run_action(Action::ShowBookmarks);
        assert!(app.bookmarks_popup.is_some());
        assert!(app.recent_files.is_none());
    }

    #[test]
    fn bookmarks_popup_esc_closes_without_jumping() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ShowBookmarks);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.bookmarks_popup.is_none());
    }

    #[test]
    fn confirm_bookmark_jump_opens_the_file_and_places_the_caret_on_the_bookmarked_line() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        // a.txt is "hello\nworld" -- place the caret on line 1 ("world",
        // byte offset 6) directly, since `Down` here would move the tree
        // selection, not the caret (`open_or_focus_tab` doesn't change
        // `focus` away from `Tree`).
        app.active_buffer_mut()
            .unwrap()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(6)));
        app.run_action(Action::ToggleBookmark);
        app.active_tab = None;

        app.run_action(Action::ShowBookmarks);
        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.bookmarks_popup.is_none());
        let buf = app.active_buffer().unwrap();
        assert_eq!(buf.path, dir.path().canonicalize().unwrap().join("a.txt"));
        let offset = buf.buffer.text_buffer().selections().primary().head;
        assert_eq!(cursor_line_column(buf.buffer.text_buffer(), offset).0, 1);
    }

    #[test]
    fn confirm_bookmark_jump_with_no_bookmarks_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ShowBookmarks);

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.bookmarks_popup.is_some());
    }

    #[test]
    fn recent_files_and_bookmarks_popup_are_mutually_exclusive() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::RecentFiles);
        assert!(app.recent_files.is_some());
        assert!(app.bookmarks_popup.is_none());

        app.run_action(Action::ShowBookmarks);
        assert!(app.bookmarks_popup.is_some());
        assert!(app.recent_files.is_none());
    }

    // -- T24: TODO panel (`tui-todo-panel.md`) --

    #[test]
    fn toggle_todo_panel_opens_and_triggers_a_scan_then_closes() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.todo_panel.is_none());

        app.run_action(Action::ToggleTodoPanel);
        assert!(app.todo_panel.is_some());
        assert!(app.todo.searching);

        app.run_action(Action::ToggleTodoPanel);
        assert!(app.todo_panel.is_none());
    }

    #[test]
    fn opening_todo_panel_closes_other_overlays() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::RecentFiles);
        assert!(app.recent_files.is_some());

        app.run_action(Action::ToggleTodoPanel);
        assert!(app.todo_panel.is_some());
        assert!(app.recent_files.is_none());
    }

    #[test]
    fn todo_panel_esc_closes_without_jumping() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleTodoPanel);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.todo_panel.is_none());
    }

    #[test]
    fn todo_panel_finds_and_jumps_to_a_todo_comment() {
        let dir = sample_project();
        std::fs::write(dir.path().join("marked.rs"), "// TODO: fix this\nfn f() {}").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::ToggleTodoPanel);
        wait_until(|| {
            app.poll_todo();
            !app.todo.searching
        });

        let results = app.todo.results.as_ref().unwrap();
        assert_eq!(results.matches.len(), 1);
        assert_eq!(results.matches[0].pattern, "TODO");

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.todo_panel.is_none());
        let buf = app.active_buffer().unwrap();
        assert_eq!(
            buf.path,
            dir.path().canonicalize().unwrap().join("marked.rs")
        );
        let offset = buf.buffer.text_buffer().selections().primary().head;
        assert_eq!(cursor_line_column(buf.buffer.text_buffer(), offset).0, 0);
    }

    #[test]
    fn todo_panel_up_down_clamp_to_the_result_count() {
        let dir = sample_project();
        std::fs::write(dir.path().join("a.rs"), "// TODO: a").unwrap();
        std::fs::write(dir.path().join("b.rs"), "// TODO: b").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::ToggleTodoPanel);
        wait_until(|| {
            app.poll_todo();
            !app.todo.searching
        });
        assert_eq!(app.todo.results.as_ref().unwrap().matches.len(), 2);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.todo_panel.as_ref().unwrap().selected, 1);
        // Already at the last row -- stays clamped.
        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.todo_panel.as_ref().unwrap().selected, 1);

        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(app.todo_panel.as_ref().unwrap().selected, 0);
        // Already at the first row -- stays clamped.
        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(app.todo_panel.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn confirm_todo_jump_with_no_results_yet_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleTodoPanel);

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.todo_panel.is_some());
    }

    /// Real notify backends deliver events asynchronously; poll
    /// repeatedly up to a generous deadline rather than betting on one
    /// exact sleep duration (mirrors `ide_core::file_watcher`'s own
    /// `poll_until` test helper).
    fn poll_watcher_until(app: &mut App, pred: impl Fn(&App) -> bool) {
        let start = std::time::Instant::now();
        loop {
            app.poll_watcher();
            if pred(app) || start.elapsed() >= std::time::Duration::from_secs(5) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn new_starts_a_file_watcher_for_the_project() {
        let dir = sample_project();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.watcher.is_some());
    }

    #[test]
    fn poll_watcher_refreshes_the_tree_on_an_external_file_creation() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        let before = app.tree.children.len();

        fs::write(root.join("new_external.txt"), "hi").unwrap();

        poll_watcher_until(&mut app, |app| app.tree.children.len() > before);
        assert!(app.tree.children.len() > before);
    }

    #[test]
    fn poll_watcher_silently_reloads_a_clean_tab_whose_file_changed_externally() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        assert!(!app.tabs[0].buffer.is_dirty());

        fs::write(root.join("a.txt"), "changed externally").unwrap();

        poll_watcher_until(&mut app, |app| {
            app.tabs[0].buffer.text() == "changed externally"
        });
        assert_eq!(app.tabs[0].buffer.text(), "changed externally");
        assert!(app.tabs[0].external_change.is_none());
        assert!(app
            .notifications
            .iter()
            .any(|n| n.message.contains("reloaded")));
    }

    #[test]
    fn poll_watcher_marks_a_dirty_tab_as_modified_without_reloading() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.handle_key(ctrl('t')); // focus editor
        app.handle_key(plain_key(KeyCode::Char('x'))); // dirty the tab
        assert!(app.tabs[0].buffer.is_dirty());

        fs::write(root.join("a.txt"), "changed externally").unwrap();

        poll_watcher_until(&mut app, |app| app.tabs[0].external_change.is_some());
        assert_eq!(app.tabs[0].external_change, Some(ExternalChange::Modified));
        // Never silently overwrites the user's unsaved edits.
        assert_ne!(app.tabs[0].buffer.text(), "changed externally");
    }

    #[test]
    fn reload_from_disk_reloads_the_active_tab_and_clears_external_change() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.tabs[0].external_change = Some(ExternalChange::Modified);
        fs::write(root.join("a.txt"), "reloaded content").unwrap();

        app.run_action(Action::ReloadFromDisk);

        assert_eq!(app.tabs[0].buffer.text(), "reloaded content");
        assert!(app.tabs[0].external_change.is_none());
    }

    #[test]
    fn reload_from_disk_with_no_active_tab_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ReloadFromDisk);
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn dismiss_external_change_clears_it_without_reloading() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.tabs[0].external_change = Some(ExternalChange::Modified);
        fs::write(root.join("a.txt"), "changed externally").unwrap();

        app.run_action(Action::DismissExternalChange);

        assert!(app.tabs[0].external_change.is_none());
        assert_ne!(app.tabs[0].buffer.text(), "changed externally");
    }

    #[test]
    fn poll_watcher_marks_a_removed_file_as_deleted_regardless_of_dirty_state() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Char('x'))); // dirty the tab

        fs::remove_file(root.join("a.txt")).unwrap();

        poll_watcher_until(&mut app, |app| app.tabs[0].external_change.is_some());
        assert_eq!(app.tabs[0].external_change, Some(ExternalChange::Deleted));
    }

    #[test]
    fn saving_the_active_tab_does_not_trigger_a_spurious_external_change() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.handle_key(ctrl('t'));
        app.handle_key(plain_key(KeyCode::Char('x')));

        app.run_action(Action::SaveActive);

        // Give the watcher a real window to (wrongly) fire in, then
        // confirm it never marked the tab as externally changed --
        // `suppress` must cover this app's own write.
        std::thread::sleep(ide_core::DEBOUNCE_WINDOW * 2);
        app.poll_watcher();
        assert!(app.tabs[0].external_change.is_none());
    }

    #[test]
    fn open_or_focus_tab_canonicalizes_a_non_canonical_path_and_refocuses() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        assert_eq!(app.tabs.len(), 1);

        // A non-canonical path (raw `TempDir` path, differing by a
        // `/private` prefix on macOS) targeting the same file must
        // refocus the existing tab instead of opening a duplicate.
        app.open_or_focus_tab(dir.path().join("a.txt")).unwrap();

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, Some(0));
    }

    #[test]
    fn toggle_keymap_popup_opens_with_a_reset_state_and_closes() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::ToggleKeymapSettings);
        assert!(app.keymap_popup.is_some());
        app.keymap_popup.as_mut().unwrap().query = "x".to_string();
        app.keymap_popup.as_mut().unwrap().selected = 3;

        app.run_action(Action::ToggleKeymapSettings);
        assert!(app.keymap_popup.is_none());

        app.run_action(Action::ToggleKeymapSettings);
        let state = app.keymap_popup.as_ref().unwrap();
        assert_eq!(state.query, "");
        assert_eq!(state.selected, 0);
        assert!(state.capturing.is_none());
    }

    #[test]
    fn opening_keymap_popup_closes_other_overlays() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::RecentFiles);
        assert!(app.recent_files.is_some());

        app.run_action(Action::ToggleKeymapSettings);
        assert!(app.keymap_popup.is_some());
        assert!(app.recent_files.is_none());
    }

    #[test]
    fn keymap_popup_esc_closes_without_capturing_anything() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleKeymapSettings);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.keymap_popup.is_none());
    }

    #[test]
    fn keymap_popup_typing_filters_rows_and_resets_selection() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleKeymapSettings);
        let all = app.keymap_popup_rows().len();

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.keymap_popup.as_ref().unwrap().selected, 1);

        app.handle_key(plain_key(KeyCode::Char('s')));
        app.handle_key(plain_key(KeyCode::Char('a')));
        app.handle_key(plain_key(KeyCode::Char('v')));
        assert_eq!(app.keymap_popup.as_ref().unwrap().query, "sav");
        assert_eq!(app.keymap_popup.as_ref().unwrap().selected, 0);
        let filtered = app.keymap_popup_rows();
        assert!(filtered.len() < all);
        assert!(filtered.iter().all(|c| c.id == "SaveAll"));

        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.keymap_popup.as_ref().unwrap().query, "sa");
    }

    #[test]
    fn enter_on_a_keymap_popup_row_enters_capture_mode() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleKeymapSettings);
        let id = app.keymap_popup_rows()[0].id;

        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(app.keymap_popup.as_ref().unwrap().capturing, Some(id));
    }

    #[test]
    fn esc_during_capture_cancels_without_assigning_a_binding() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleKeymapSettings);
        let id = app.keymap_popup_rows()[0].id;
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.keymap_popup.as_ref().unwrap().capturing, Some(id));

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.keymap_popup.as_ref().unwrap().capturing.is_none());
        assert!(app.keymap_popup.is_some());
        assert!(!app.keymap.is_customized(id));
    }

    #[test]
    fn capturing_a_new_chord_rebinds_and_a_later_key_press_dispatches_the_new_action() {
        let dir = sample_project();
        let keymap_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.keymap_path_override = Some(keymap_dir.path().join("keymap.json"));
        app.run_action(Action::ToggleKeymapSettings);
        // "Save" is the first registered command (`commands.rs`'s own
        // table order).
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(
            app.keymap_popup.as_ref().unwrap().capturing,
            Some("SaveAll")
        );

        app.handle_key(ctrl('x'));

        assert!(app.keymap_popup.as_ref().unwrap().capturing.is_none());
        assert!(app.keymap.is_customized("SaveAll"));
        assert_eq!(
            app.keymap.effective_binding("SaveAll"),
            Some((KeyModifiers::CONTROL, KeyCode::Char('x')))
        );
        assert!(app
            .notifications
            .iter()
            .any(|n| n.message.contains("SaveAll")));

        // The *old* default chord no longer dispatches anything for this
        // command; the popup must be closed first since it still owns
        // key dispatch while open.
        app.run_action(Action::ToggleKeymapSettings);
        assert!(app.keymap_popup.is_none());
        assert_eq!(
            app.keymap
                .action_for(KeyModifiers::CONTROL, KeyCode::Char('s')),
            None
        );
        assert_eq!(
            app.keymap
                .action_for(KeyModifiers::CONTROL, KeyCode::Char('x')),
            Some(Action::SaveActive)
        );
    }

    #[test]
    fn capture_surfaces_a_conflict_notification_but_still_applies_the_binding() {
        let dir = sample_project();
        let keymap_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.keymap_path_override = Some(keymap_dir.path().join("keymap.json"));
        app.run_action(Action::ToggleKeymapSettings);
        app.handle_key(plain_key(KeyCode::Enter)); // capture "SaveAll"

        // `Undo`'s own default is `Ctrl+Z` -- rebind `SaveAll` onto it.
        app.handle_key(ctrl('z'));

        assert_eq!(
            app.keymap.effective_binding("SaveAll"),
            Some((KeyModifiers::CONTROL, KeyCode::Char('z')))
        );
        assert!(app.notifications.last().unwrap().message.contains("Undo"));
    }

    #[test]
    fn delete_resets_a_customized_row_to_its_default() {
        let dir = sample_project();
        let keymap_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.keymap_path_override = Some(keymap_dir.path().join("keymap.json"));
        app.keymap
            .set_override("SaveAll", Some((KeyModifiers::CONTROL, KeyCode::Char('x'))));
        app.persist_keymap();
        app.run_action(Action::ToggleKeymapSettings);

        app.handle_key(plain_key(KeyCode::Delete));

        assert!(!app.keymap.is_customized("SaveAll"));
        assert_eq!(
            app.keymap.effective_binding("SaveAll"),
            Some((KeyModifiers::CONTROL, KeyCode::Char('s')))
        );
    }

    #[test]
    fn reset_all_keybindings_action_clears_every_override() {
        let dir = sample_project();
        let keymap_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.keymap_path_override = Some(keymap_dir.path().join("keymap.json"));
        app.keymap
            .set_override("SaveAll", Some((KeyModifiers::CONTROL, KeyCode::Char('x'))));
        app.keymap.set_override("Undo", None);

        app.run_action(Action::ResetAllKeybindings);

        assert!(!app.keymap.is_customized("SaveAll"));
        assert!(!app.keymap.is_customized("Undo"));
        assert!(app
            .notifications
            .iter()
            .any(|n| n.message.contains("Reset")));
    }

    #[test]
    fn open_palette_action_still_opens_the_palette_after_the_ctrl_shift_a_fold_in() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::OpenPalette);
        assert!(app.palette.is_some());
    }

    // -- T23: scratch files -------------------------------------------
    //
    // `scratch::new_scratch_path`/`list_scratch_files` resolve against the
    // real per-user `~/.config/ide-tui/scratch/` directory (same "operates
    // on the real environment" shape `keymap::load`/`state::load` already
    // have, exercised by every `App::new` call in this whole test module).
    // Tests that actually create a file use a name unique to the test
    // function (safe under `cargo test`'s parallel execution against this
    // shared directory) and remove it again when done; tests that assert
    // "zero rows" filter on a nonsense query rather than assuming the
    // directory itself is empty, since a concurrently-running test (or a
    // real scratch file from actual prior use of this binary) could
    // otherwise make that assumption flaky.

    fn cleanup_scratch_file(name: &str) {
        if let Some(path) = scratch::new_scratch_path(name).ok().flatten() {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn toggle_new_scratch_file_opens_with_a_reset_state_and_closes() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::NewScratchFile);
        assert!(app.new_scratch_file.is_some());
        app.new_scratch_file.as_mut().unwrap().name = "x".to_string();

        app.run_action(Action::NewScratchFile);
        assert!(app.new_scratch_file.is_none());

        app.run_action(Action::NewScratchFile);
        assert_eq!(app.new_scratch_file.as_ref().unwrap().name, "");
    }

    #[test]
    fn opening_new_scratch_file_prompt_closes_other_overlays() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::RecentFiles);
        assert!(app.recent_files.is_some());

        app.run_action(Action::NewScratchFile);
        assert!(app.new_scratch_file.is_some());
        assert!(app.recent_files.is_none());
    }

    #[test]
    fn new_scratch_file_esc_closes_without_creating_anything() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::NewScratchFile);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.new_scratch_file.is_none());
    }

    #[test]
    fn new_scratch_file_typing_and_backspace_edit_the_name() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::NewScratchFile);

        app.handle_key(plain_key(KeyCode::Char('a')));
        app.handle_key(plain_key(KeyCode::Char('b')));
        assert_eq!(app.new_scratch_file.as_ref().unwrap().name, "ab");

        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.new_scratch_file.as_ref().unwrap().name, "a");
    }

    #[test]
    fn confirm_new_scratch_file_with_an_invalid_name_notifies_and_leaves_the_prompt_open() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::NewScratchFile);
        app.new_scratch_file.as_mut().unwrap().name = "../escape".to_string();

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.new_scratch_file.is_some());
        assert!(!app.notifications.is_empty());
    }

    #[test]
    fn confirm_new_scratch_file_creates_and_opens_a_new_file_then_closes_the_prompt() {
        let name = "_ide_tui_test_confirm_new_scratch_file.txt";
        cleanup_scratch_file(name);

        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::NewScratchFile);
        app.new_scratch_file.as_mut().unwrap().name = name.to_string();

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.new_scratch_file.is_none());
        let buf = app
            .active_buffer()
            .expect("the scratch file should now be open");
        assert!(buf.path.ends_with(name));
        assert_eq!(buf.buffer.text(), "");

        cleanup_scratch_file(name);
    }

    #[test]
    fn creating_a_scratch_file_with_the_same_name_twice_does_not_truncate_it() {
        let name = "_ide_tui_test_reopen_scratch_file.txt";
        cleanup_scratch_file(name);

        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::NewScratchFile);
        app.new_scratch_file.as_mut().unwrap().name = name.to_string();
        app.handle_key(plain_key(KeyCode::Enter));
        app.handle_key(ctrl('t')); // focus editor
        app.handle_key(plain_key(KeyCode::Char('x'))); // "x"
        app.run_action(Action::SaveActive);
        app.close_active_tab();

        app.run_action(Action::NewScratchFile);
        app.new_scratch_file.as_mut().unwrap().name = name.to_string();
        app.handle_key(plain_key(KeyCode::Enter));

        let buf = app.active_buffer().expect("the scratch file should reopen");
        assert_eq!(buf.buffer.text(), "x");

        cleanup_scratch_file(name);
    }

    #[test]
    fn toggle_scratch_files_opens_and_closes() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::ToggleScratchFiles);
        assert!(app.scratch_files.is_some());

        app.run_action(Action::ToggleScratchFiles);
        assert!(app.scratch_files.is_none());
    }

    #[test]
    fn opening_scratch_files_popup_closes_other_overlays() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::RecentFiles);
        assert!(app.recent_files.is_some());

        app.run_action(Action::ToggleScratchFiles);
        assert!(app.scratch_files.is_some());
        assert!(app.recent_files.is_none());
    }

    #[test]
    fn scratch_files_esc_closes_without_opening_anything() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleScratchFiles);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.scratch_files.is_none());
    }

    #[test]
    fn scratch_files_rows_with_a_nonsense_query_is_empty() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleScratchFiles);
        app.scratch_files.as_mut().unwrap().query =
            "_ide_tui_test_query_matching_nothing_at_all_".to_string();

        assert!(app.scratch_files_rows().is_empty());
    }

    #[test]
    fn confirm_scratch_file_opens_the_selected_row() {
        let name = "_ide_tui_test_confirm_scratch_file.txt";
        cleanup_scratch_file(name);
        let path = scratch::new_scratch_path(name).unwrap().unwrap();
        std::fs::write(&path, "hello").unwrap();

        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleScratchFiles);
        app.scratch_files.as_mut().unwrap().query = name.to_string();

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.scratch_files.is_none());
        let buf = app
            .active_buffer()
            .expect("the scratch file should now be open");
        assert!(buf.path.ends_with(name));
        assert_eq!(buf.buffer.text(), "hello");

        cleanup_scratch_file(name);
    }

    #[test]
    fn confirm_scratch_file_with_no_rows_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleScratchFiles);
        app.scratch_files.as_mut().unwrap().query =
            "_ide_tui_test_query_matching_nothing_at_all_".to_string();

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.scratch_files.is_some());
        assert!(app.tabs.is_empty());
    }

    // -- T26: Claude panel + Claude terminal ----------------------------
    //
    // None of these tests ever open a Claude Terminal tab pointed at a
    // directory that actually exists: `PtySession::spawn` would try to
    // launch the real `claude` CLI over a real PTY, categorically
    // different from `cargo_panel.rs`'s tests spawning real `cargo`
    // subcommands (fast, free, side-effect-bounded). Every terminal-tab
    // test below uses a path that does not exist, exercising the exact
    // same "exited: true, error fed into the grid" path
    // `claude_terminal.rs`'s own test suite already relies on for the
    // same reason. `handle_claude_chat_key`'s `Enter`/submit path
    // similarly always swaps in `ClaudePanel::with_runner(fake)` first,
    // never the real `run_claude_cli`.

    fn claude_ok(prompt: &str) -> Result<String, String> {
        Ok(format!("ack: {prompt}"))
    }

    #[test]
    fn toggle_claude_panel_opens_and_closes() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(!app.claude_panel_open);

        app.run_action(Action::ToggleClaudePanel);
        assert!(app.claude_panel_open);

        app.run_action(Action::ToggleClaudePanel);
        assert!(!app.claude_panel_open);
    }

    #[test]
    fn opening_claude_panel_closes_other_overlays() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleCargoPanel);
        assert!(app.cargo_panel_open);

        app.run_action(Action::ToggleClaudePanel);

        assert!(app.claude_panel_open);
        assert!(!app.cargo_panel_open);
    }

    #[test]
    fn claude_panel_esc_in_chat_view_closes_it() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleClaudePanel);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(!app.claude_panel_open);
    }

    #[test]
    fn claude_chat_typing_and_backspace_edit_the_input() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleClaudePanel);

        app.handle_key(plain_key(KeyCode::Char('h')));
        app.handle_key(plain_key(KeyCode::Char('i')));
        assert_eq!(app.claude.input, "hi");

        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.claude.input, "h");
    }

    #[test]
    fn claude_chat_enter_submits_and_clears_input() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.claude = ClaudePanel::with_runner(claude_ok);
        app.run_action(Action::ToggleClaudePanel);
        app.handle_key(plain_key(KeyCode::Char('h')));
        app.handle_key(plain_key(KeyCode::Char('i')));

        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(app.claude.input, "");
        assert!(app.claude.is_in_flight());
        assert_eq!(
            app.claude.history,
            vec![crate::claude_panel::ClaudeMessage::User("hi".to_string())]
        );
    }

    #[test]
    fn claude_chat_ctrl_letter_does_not_type_into_the_input() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleClaudePanel);

        app.handle_key(ctrl('x'));

        assert_eq!(app.claude.input, "");
    }

    #[test]
    fn resolve_claude_terminal_dir_blank_defaults_to_project_root() {
        let dir = sample_project();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            app.resolve_claude_terminal_dir("   "),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_claude_terminal_dir_relative_joins_onto_project_root() {
        let dir = sample_project();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            app.resolve_claude_terminal_dir("subdir"),
            dir.path().canonicalize().unwrap().join("subdir")
        );
    }

    #[test]
    fn resolve_claude_terminal_dir_absolute_is_used_as_is() {
        let dir = sample_project();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            app.resolve_claude_terminal_dir("/does/not/exist/absolute"),
            PathBuf::from("/does/not/exist/absolute")
        );
    }

    #[test]
    fn ctrl_n_opens_new_claude_terminal_prompt() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleClaudePanel);

        app.handle_key(ctrl('n'));

        assert!(app.new_claude_terminal.is_some());
        assert!(app.claude_panel_open); // nested, not a replacement
    }

    #[test]
    fn new_claude_terminal_prompt_esc_cancels_without_creating_anything() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleClaudePanel);
        app.handle_key(ctrl('n'));

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.new_claude_terminal.is_none());
        assert!(app.claude_panel_open);
        assert!(app.claude_terminals.tabs().is_empty());
    }

    #[test]
    fn new_claude_terminal_prompt_typing_and_backspace_edit_the_name() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleClaudePanel);
        app.handle_key(ctrl('n'));

        app.handle_key(plain_key(KeyCode::Char('x')));
        app.handle_key(plain_key(KeyCode::Char('y')));
        assert_eq!(app.new_claude_terminal.as_ref().unwrap().name, "xy");

        app.handle_key(plain_key(KeyCode::Backspace));
        assert_eq!(app.new_claude_terminal.as_ref().unwrap().name, "x");
    }

    #[test]
    fn confirm_new_claude_terminal_with_a_nonexistent_directory_creates_an_exited_tab_and_switches_view(
    ) {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleClaudePanel);
        app.handle_key(ctrl('n'));
        for c in "definitely-does-not-exist-xyz".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.new_claude_terminal.is_none());
        assert_eq!(app.claude_terminals.tabs().len(), 1);
        assert!(app.claude_terminals.tabs()[0].exited);
        assert_eq!(app.claude_view, ClaudeView::Terminal(0));
        assert!(app.claude_terminal_focus);
    }

    #[test]
    fn cycle_claude_view_with_no_terminal_tabs_stays_on_chat() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleClaudePanel);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.claude_view, ClaudeView::Chat);

        app.handle_key(plain_key(KeyCode::BackTab));
        assert_eq!(app.claude_view, ClaudeView::Chat);
    }

    fn open_two_dead_terminal_tabs(app: &mut App) {
        app.claude_terminals
            .open_tab(PathBuf::from("/does/not/exist/1"), 24, 80);
        app.claude_terminals
            .open_tab(PathBuf::from("/does/not/exist/2"), 24, 80);
        app.claude_view = ClaudeView::Chat;
        app.claude_terminals.active = Some(0);
    }

    #[test]
    fn cycle_claude_view_cycles_through_terminal_tabs_and_wraps() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        open_two_dead_terminal_tabs(&mut app);
        app.run_action(Action::ToggleClaudePanel);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.claude_view, ClaudeView::Terminal(0));
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.claude_view, ClaudeView::Terminal(1));
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.claude_view, ClaudeView::Chat);

        app.handle_key(plain_key(KeyCode::BackTab));
        assert_eq!(app.claude_view, ClaudeView::Terminal(1));
    }

    #[test]
    fn cycle_claude_view_resets_terminal_focus() {
        // `Tab` while raw-focused is forwarded to the PTY, not treated as
        // a view-cycle (`raw_focus_swallows_tab_and_plain_esc_without_
        // any_chrome_effect` below covers that) -- so `cycle_claude_view`
        // itself is only ever reachable in chrome mode, where focus is
        // already `false`. Exercised directly here as a defensive-reset
        // invariant on the method itself, independent of whether any
        // reachable key sequence can currently trigger it with focus
        // already `true`.
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        open_two_dead_terminal_tabs(&mut app);
        app.claude_view = ClaudeView::Terminal(0);
        app.claude_terminal_focus = true;

        app.cycle_claude_view(1);

        assert!(!app.claude_terminal_focus);
    }

    #[test]
    fn ctrl_w_in_chat_view_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        open_two_dead_terminal_tabs(&mut app);
        app.run_action(Action::ToggleClaudePanel);

        app.handle_key(ctrl('w'));

        assert_eq!(app.claude_terminals.tabs().len(), 2);
        assert_eq!(app.claude_view, ClaudeView::Chat);
    }

    #[test]
    fn ctrl_w_closes_the_active_terminal_tab() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        open_two_dead_terminal_tabs(&mut app);
        app.run_action(Action::ToggleClaudePanel);
        app.handle_key(plain_key(KeyCode::Tab)); // -> Terminal(0)

        app.handle_key(ctrl('w'));

        assert_eq!(app.claude_terminals.tabs().len(), 1);
        assert_eq!(app.claude_view, ClaudeView::Terminal(0));
    }

    #[test]
    fn ctrl_w_closing_the_last_terminal_tab_falls_back_to_chat() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.claude_terminals
            .open_tab(PathBuf::from("/does/not/exist/1"), 24, 80);
        app.claude_view = ClaudeView::Terminal(0);
        app.run_action(Action::ToggleClaudePanel);
        app.claude_view = ClaudeView::Terminal(0); // toggle re-opens, doesn't reset view

        app.handle_key(ctrl('w'));

        assert!(app.claude_terminals.tabs().is_empty());
        assert_eq!(app.claude_view, ClaudeView::Chat);
    }

    #[test]
    fn enter_in_terminal_chrome_mode_enters_raw_focus() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        open_two_dead_terminal_tabs(&mut app);
        app.run_action(Action::ToggleClaudePanel);
        app.handle_key(plain_key(KeyCode::Tab));
        assert!(!app.claude_terminal_focus);

        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.claude_terminal_focus);
    }

    #[test]
    fn raw_focus_swallows_tab_and_plain_esc_without_any_chrome_effect() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        open_two_dead_terminal_tabs(&mut app);
        app.run_action(Action::ToggleClaudePanel);
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.claude_terminal_focus);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.claude_view, ClaudeView::Terminal(0)); // did not cycle
        assert!(app.claude_terminal_focus); // still raw-focused

        app.handle_key(plain_key(KeyCode::Esc));
        assert!(app.claude_panel_open); // plain Esc forwarded, not intercepted
        assert!(app.claude_terminal_focus);
    }

    #[test]
    fn shift_esc_exits_raw_focus_back_to_chrome_mode() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        open_two_dead_terminal_tabs(&mut app);
        app.run_action(Action::ToggleClaudePanel);
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Enter));
        assert!(app.claude_terminal_focus);

        app.handle_key(key(KeyModifiers::SHIFT, KeyCode::Esc));

        assert!(!app.claude_terminal_focus);
        assert_eq!(app.claude_view, ClaudeView::Terminal(0)); // same tab, chrome mode
        assert!(app.claude_panel_open);

        // Chrome mode again: plain Esc now closes the whole panel.
        app.handle_key(plain_key(KeyCode::Esc));
        assert!(!app.claude_panel_open);
    }

    // -- Docker & Kubernetes panels (docs/features/
    // tui-docker-and-kubernetes.md) --

    fn sample_container(id: &str, name: &str) -> crate::docker_panel::DockerContainer {
        crate::docker_panel::DockerContainer {
            id: id.to_string(),
            names: name.to_string(),
            image: "nginx".to_string(),
            status: "Up 2 hours".to_string(),
        }
    }

    #[test]
    fn run_action_toggle_docker_panel_opens_and_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.docker_panel.is_none());

        app.run_action(Action::ToggleDockerPanel);
        assert!(app.docker_panel.is_some());

        app.run_action(Action::ToggleDockerPanel);
        assert!(app.docker_panel.is_none());
    }

    #[test]
    fn toggle_docker_panel_closes_other_overlays() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.cargo_panel_open = true;

        app.run_action(Action::ToggleDockerPanel);

        assert!(!app.cargo_panel_open);
        assert!(app.docker_panel.is_some());
    }

    #[test]
    fn close_all_overlays_closes_the_docker_and_k8s_panels() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleDockerPanel);
        assert!(app.docker_panel.is_some());

        app.close_all_overlays();

        assert!(app.docker_panel.is_none());

        app.run_action(Action::ToggleK8sPanel);
        assert!(app.k8s_panel.is_some());

        app.close_all_overlays();

        assert!(app.k8s_panel.is_none());
    }

    #[test]
    fn handle_docker_panel_key_esc_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleDockerPanel);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.docker_panel.is_none());
    }

    #[test]
    fn handle_docker_panel_key_tab_switches_between_containers_and_images() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleDockerPanel);
        let panel = app.docker_panel.as_mut().unwrap();
        panel.selected = 3;

        app.handle_key(plain_key(KeyCode::Tab));
        let panel = app.docker_panel.as_ref().unwrap();
        assert_eq!(panel.tab, DockerTab::Images);
        assert_eq!(panel.selected, 0);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(
            app.docker_panel.as_ref().unwrap().tab,
            DockerTab::Containers
        );
    }

    #[test]
    fn handle_docker_panel_key_up_down_clamp_the_selection() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleDockerPanel);
        app.docker_panel.as_mut().unwrap().containers =
            vec![sample_container("a", "web"), sample_container("b", "db")];

        app.handle_key(plain_key(KeyCode::Up)); // clamps at 0
        assert_eq!(app.docker_panel.as_ref().unwrap().selected, 0);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.docker_panel.as_ref().unwrap().selected, 1);

        app.handle_key(plain_key(KeyCode::Down)); // clamps at len - 1
        assert_eq!(app.docker_panel.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn handle_docker_panel_key_lifecycle_letters_open_the_matching_confirm() {
        let dir = sample_project();
        for (letter, expected) in [
            ('s', DockerLifecycleAction::Start),
            ('x', DockerLifecycleAction::Stop),
            ('b', DockerLifecycleAction::Restart),
            ('d', DockerLifecycleAction::Remove),
        ] {
            let mut app = App::new(dir.path().to_path_buf()).unwrap();
            app.run_action(Action::ToggleDockerPanel);
            app.docker_panel.as_mut().unwrap().containers = vec![sample_container("abc123", "web")];

            app.handle_key(plain_key(KeyCode::Char(letter)));

            let confirm = app
                .docker_panel
                .as_ref()
                .unwrap()
                .confirm
                .as_ref()
                .unwrap_or_else(|| panic!("'{letter}' should open a confirm popup"));
            assert_eq!(confirm.action, expected);
            assert_eq!(confirm.container_id, "abc123");
            assert_eq!(confirm.container_name, "web");
        }
    }

    #[test]
    fn handle_docker_panel_key_confirm_n_and_esc_discard_without_running_anything() {
        let dir = sample_project();
        for cancel_key in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut app = App::new(dir.path().to_path_buf()).unwrap();
            app.run_action(Action::ToggleDockerPanel);
            let panel = app.docker_panel.as_mut().unwrap();
            panel.containers = vec![sample_container("abc123", "web")];
            panel.confirm = Some(crate::docker_panel::DockerConfirm {
                action: DockerLifecycleAction::Stop,
                container_id: "abc123".to_string(),
                container_name: "web".to_string(),
            });

            app.handle_key(plain_key(cancel_key));

            // The panel itself must still be open -- only the confirm
            // popup is dismissed, `Esc` here does not also close the panel
            // (confirm-mode interception, same as `handle_git_panel_key`'s
            // conflict-resolution `Esc`).
            assert!(app.docker_panel.is_some());
            assert!(app.docker_panel.as_ref().unwrap().confirm.is_none());
        }
    }

    #[test]
    fn handle_docker_panel_key_confirm_y_clears_the_popup_and_starts_the_action() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        // Direct assignment, not `ToggleDockerPanel` -- opening via the
        // toggle also kicks off its own `refresh()`, which would still be
        // in flight here and make `confirm_yes` defer instead of running
        // (`DockerPanel::confirm_yes`'s own in-flight guard).
        app.docker_panel = Some(DockerPanel::default());
        app.docker_panel.as_mut().unwrap().confirm = Some(crate::docker_panel::DockerConfirm {
            action: DockerLifecycleAction::Stop,
            container_id: "abc123".to_string(),
            container_name: "web".to_string(),
        });

        app.handle_key(plain_key(KeyCode::Char('y')));

        assert!(app.docker_panel.as_ref().unwrap().confirm.is_none());
    }

    fn sample_pod(name: &str) -> crate::k8s_panel::K8sPod {
        crate::k8s_panel::K8sPod {
            name: name.to_string(),
            phase: "Running".to_string(),
            restarts: 0,
            ready: "1/1".to_string(),
        }
    }

    fn sample_deployment(name: &str) -> crate::k8s_panel::K8sDeployment {
        crate::k8s_panel::K8sDeployment {
            name: name.to_string(),
            ready: "3/3".to_string(),
            replicas: 3,
        }
    }

    #[test]
    fn run_action_toggle_k8s_panel_opens_and_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.k8s_panel.is_none());

        app.run_action(Action::ToggleK8sPanel);
        assert!(app.k8s_panel.is_some());

        app.run_action(Action::ToggleK8sPanel);
        assert!(app.k8s_panel.is_none());
    }

    #[test]
    fn toggle_k8s_panel_closes_other_overlays() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleDockerPanel);
        assert!(app.docker_panel.is_some());

        app.run_action(Action::ToggleK8sPanel);

        assert!(app.docker_panel.is_none());
        assert!(app.k8s_panel.is_some());
    }

    #[test]
    fn handle_k8s_panel_key_esc_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.k8s_panel.is_none());
    }

    #[test]
    fn handle_k8s_panel_key_tab_cycles_pods_deployments_services() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.k8s_panel.as_ref().unwrap().tab, K8sTab::Deployments);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.k8s_panel.as_ref().unwrap().tab, K8sTab::Services);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.k8s_panel.as_ref().unwrap().tab, K8sTab::Pods);
    }

    #[test]
    fn handle_k8s_panel_key_up_down_clamp_the_selection() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);
        app.k8s_panel.as_mut().unwrap().pods = vec![sample_pod("a"), sample_pod("b")];

        app.handle_key(plain_key(KeyCode::Up)); // clamps at 0
        assert_eq!(app.k8s_panel.as_ref().unwrap().selected, 0);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.k8s_panel.as_ref().unwrap().selected, 1);

        app.handle_key(plain_key(KeyCode::Down)); // clamps at len - 1
        assert_eq!(app.k8s_panel.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn handle_k8s_panel_key_d_on_pods_opens_the_delete_confirm() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);
        app.k8s_panel.as_mut().unwrap().pods = vec![sample_pod("worker-7f9c")];

        app.handle_key(plain_key(KeyCode::Char('d')));

        let confirm = app.k8s_panel.as_ref().unwrap().confirm.as_ref().unwrap();
        assert_eq!(confirm.target_name, "worker-7f9c");
        assert_eq!(confirm.typed, "");
    }

    #[test]
    fn handle_k8s_panel_key_typed_confirm_requires_an_exact_match() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.k8s_panel = Some(K8sPanel::default());
        app.k8s_panel.as_mut().unwrap().pods = vec![sample_pod("worker-7f9c")];
        app.handle_key(plain_key(KeyCode::Char('d')));

        for c in "worker".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        app.handle_key(plain_key(KeyCode::Enter));
        // Partial match: popup stays open, nothing ran, and the wrong
        // input the user typed so far is still visible (§3.4) rather than
        // silently cleared.
        let confirm = app.k8s_panel.as_ref().unwrap().confirm.as_ref().unwrap();
        assert_eq!(confirm.typed, "worker");

        for c in "-7f9c".chars() {
            app.handle_key(plain_key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.k8s_panel
                .as_ref()
                .unwrap()
                .confirm
                .as_ref()
                .unwrap()
                .typed,
            "worker-7f9c"
        );
    }

    #[test]
    fn handle_k8s_panel_key_confirm_esc_cancels_without_closing_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);
        app.k8s_panel.as_mut().unwrap().pods = vec![sample_pod("worker-7f9c")];
        app.handle_key(plain_key(KeyCode::Char('d')));

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.k8s_panel.is_some());
        assert!(app.k8s_panel.as_ref().unwrap().confirm.is_none());
    }

    #[test]
    fn handle_k8s_panel_key_scale_flow_opens_numeric_prompt_then_typed_confirm() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);
        app.handle_key(plain_key(KeyCode::Tab)); // Deployments
        app.k8s_panel.as_mut().unwrap().deployments = vec![sample_deployment("api-server")];

        app.handle_key(plain_key(KeyCode::Char('s')));
        assert_eq!(
            app.k8s_panel.as_ref().unwrap().scale_input.as_deref(),
            Some("")
        );

        // Non-numeric input is a no-op, per §3.4 -- the prompt stays open.
        app.handle_key(plain_key(KeyCode::Char('x')));
        assert!(app.k8s_panel.as_ref().unwrap().scale_input.is_some());
    }

    #[test]
    fn handle_k8s_panel_key_scale_flow_numeric_input_opens_the_typed_name_confirm() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);
        app.handle_key(plain_key(KeyCode::Tab)); // Deployments
        app.k8s_panel.as_mut().unwrap().deployments = vec![sample_deployment("api-server")];
        app.handle_key(plain_key(KeyCode::Char('s')));

        app.handle_key(plain_key(KeyCode::Char('5')));
        app.handle_key(plain_key(KeyCode::Enter));

        assert!(app.k8s_panel.as_ref().unwrap().scale_input.is_none());
        let confirm = app.k8s_panel.as_ref().unwrap().confirm.as_ref().unwrap();
        assert_eq!(confirm.target_name, "api-server");
        assert_eq!(
            confirm.action,
            crate::k8s_panel::K8sDestructive::ScaleDeployment {
                name: "api-server".to_string(),
                replicas: 5,
            }
        );
    }

    #[test]
    fn handle_k8s_panel_key_c_opens_the_context_picker() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);
        app.k8s_panel.as_mut().unwrap().available_contexts =
            vec!["prod".to_string(), "staging".to_string()];

        app.handle_key(plain_key(KeyCode::Char('c')));

        assert_eq!(
            app.k8s_panel.as_ref().unwrap().picker,
            Some(K8sPicker::Context)
        );

        app.handle_key(plain_key(KeyCode::Down));
        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(
            app.k8s_panel.as_ref().unwrap().context.as_deref(),
            Some("staging")
        );
        assert!(app.k8s_panel.as_ref().unwrap().picker.is_none());
    }

    #[test]
    fn handle_k8s_panel_key_namespace_picker_first_entry_clears_the_filter() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);
        let panel = app.k8s_panel.as_mut().unwrap();
        panel.available_namespaces = vec!["default".to_string()];
        panel.namespace = Some("default".to_string());

        app.handle_key(plain_key(KeyCode::Char('n')));
        assert_eq!(
            app.k8s_panel.as_ref().unwrap().picker,
            Some(K8sPicker::Namespace)
        );

        // Index 0 is the synthetic "no namespace filter" entry (§3.5).
        app.handle_key(plain_key(KeyCode::Enter));

        assert_eq!(app.k8s_panel.as_ref().unwrap().namespace, None);
        assert!(app.k8s_panel.as_ref().unwrap().picker.is_none());
    }

    #[test]
    fn handle_k8s_panel_key_picker_esc_cancels_without_changing_selection() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleK8sPanel);
        app.k8s_panel.as_mut().unwrap().available_contexts = vec!["prod".to_string()];
        app.handle_key(plain_key(KeyCode::Char('c')));

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.k8s_panel.as_ref().unwrap().picker.is_none());
        assert!(app.k8s_panel.as_ref().unwrap().context.is_none());
    }

    /// Regression test for a real bug report: a tab-indented file (Go's
    /// convention) rendered with every tab-indented line's leading
    /// whitespace collapsed and its content shifted left, since ratatui's
    /// `Buffer::set_stringn` silently drops literal tabs as control
    /// characters (`highlight.rs::expand_tabs`'s own doc comment has the
    /// full mechanism). Renders a real frame via `TestBackend` -- unlike
    /// `highlight.rs`'s own unit tests of `styled_line` in isolation, this
    /// exercises the full `ui::render` path (buffer indent resolution,
    /// scroll, tab strip, cursor) the way a user would actually hit it.
    #[test]
    fn tab_indented_lines_keep_their_indentation_on_screen() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("f.go"),
            "package p\n\ntype T struct {\n\tname string\n}\n",
        )
        .unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(dir.path().join("f.go")).unwrap();
        app.focus = Focus::Editor;

        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| crate::ui::render(f, &app, &mut crate::ui::HitMap::default()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        let row_text = |y: u16| -> String {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect()
        };
        // Row 6 is "\tname string" (row 0: top border, 1: tab strip, 2:
        // "package p", 3: blank, 4: "type T struct {", 5: the tab line) --
        // asserted indirectly below via content rather than a hardcoded
        // row index, so this doesn't break if the layout above shifts.
        let indented_row = (0..buf.area.height)
            .map(row_text)
            .find(|line| line.contains("name") && line.contains("string"))
            .expect("the struct field line is visible in this small file");
        let name_col = indented_row.find("name").unwrap();
        assert!(
            name_col > 0,
            "tab before `name` must still reserve display columns, not collapse to 0: {indented_row:?}"
        );
    }

    /// Regression test for the sibling bug the tab-collapse fix (above)
    /// exposed: a wide CJK character is 2 screen columns, not 1, so
    /// re-deriving the caret's screen column from a raw `char` count (as
    /// `cursor_line_column` alone gives) drifted the block cursor left of
    /// where it belonged on any line containing one -- e.g. landing mid-
    /// string on a `"中文字符串"` literal instead of just past its closing
    /// quote. Renders a real frame via `TestBackend`, same as
    /// `tab_indented_lines_keep_their_indentation_on_screen`, since this is
    /// specifically about `ui::render`'s cursor-placement code, not
    /// `styled_line` in isolation (`highlight.rs` has its own unit tests
    /// for the tab/width math itself).
    #[test]
    fn cursor_lands_after_a_wide_cjk_character_not_mid_glyph() {
        let dir = tempfile::tempdir().unwrap();
        // No trailing newline -- end-of-buffer must land right after the
        // closing quote, not on a following empty line.
        fs::write(dir.path().join("f.go"), "package p\n\nx := \"中文字符串\"").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(dir.path().join("f.go")).unwrap();
        app.focus = Focus::Editor;
        // End of the buffer is right after the closing quote.
        let end = app
            .active_buffer()
            .unwrap()
            .buffer
            .text_buffer()
            .text()
            .len();
        set_caret(&mut app, end);

        let backend = ratatui::backend::TestBackend::new(80, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| crate::ui::render(f, &app, &mut crate::ui::HitMap::default()))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buf = terminal.backend().buffer().clone();
        let row_chars: Vec<String> = (0..buf.area.width)
            .map(|x| buf[(x, cursor.y)].symbol().to_string())
            .collect();
        let closing_quote_col = row_chars.iter().rposition(|c| c == "\"").unwrap();

        assert_eq!(
            cursor.x as usize,
            closing_quote_col + 1,
            "cursor should land immediately after the closing quote, not drift left from undercounting wide CJK columns: {row_chars:?}"
        );
    }

    /// Not a width/alignment assertion (this crate renders logical/storage
    /// byte order, not a bidi-reordered visual order -- a known, accepted
    /// limitation, not a bug) -- just confirms a line of Arabic text
    /// doesn't panic or corrupt neighboring rows on its way through
    /// `styled_line`'s boundary-walk and tab-expansion logic.
    #[test]
    fn arabic_text_renders_without_panicking_or_corrupting_the_frame() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("f.go"),
            "package p\n\n// تعليق بالعربية من اليمين لليسار\nfunc f() {}\n",
        )
        .unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(dir.path().join("f.go")).unwrap();
        app.focus = Focus::Editor;

        let backend = ratatui::backend::TestBackend::new(80, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| crate::ui::render(f, &app, &mut crate::ui::HitMap::default()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let rendered: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(
            rendered.contains("تعليق"),
            "the Arabic comment must still appear on screen"
        );
    }

    // ---- Mouse support (docs/features/tui-mouse-support.md) ----

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn two_file_project() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "A").unwrap();
        fs::write(dir.path().join("b.txt"), "B").unwrap();
        let app = App::new(dir.path().to_path_buf()).unwrap();
        (dir, app)
    }

    #[test]
    fn any_popup_open_reflects_open_state() {
        let (_dir, mut app) = two_file_project();
        assert!(!app.any_popup_open());
        app.open_palette();
        assert!(app.any_popup_open());
    }

    #[test]
    fn handle_mouse_click_on_a_tree_row_selects_and_opens_it() {
        let (_dir, mut app) = two_file_project();
        let rows = app.tree_state.visible_rows(&app.tree);
        assert!(rows.len() >= 2);
        let target_name = rows[1].path.file_name().unwrap().to_owned();

        let hits = ui::HitMap {
            tree_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            }),
            editor_text_area: None,
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 1),
            &hits,
        );

        assert_eq!(app.focus, Focus::Tree);
        let active_path = app.tabs[app.active_tab.unwrap()].path.clone();
        assert_eq!(active_path.file_name().unwrap(), target_name);
    }

    #[test]
    fn handle_mouse_click_on_a_tab_strip_entry_switches_active_tab() {
        let (dir, mut app) = two_file_project();
        app.open_or_focus_tab(dir.path().join("a.txt")).unwrap();
        app.open_or_focus_tab(dir.path().join("b.txt")).unwrap();
        assert_eq!(app.active_tab, Some(1));

        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: None,
            tab_strip: vec![
                (
                    Rect {
                        x: 0,
                        y: 0,
                        width: 5,
                        height: 1,
                    },
                    0,
                ),
                (
                    Rect {
                        x: 5,
                        y: 0,
                        width: 5,
                        height: 1,
                    },
                    1,
                ),
            ],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 0),
            &hits,
        );
        assert_eq!(app.active_tab, Some(0));
        assert_eq!(app.focus, Focus::Editor);
    }

    #[test]
    fn handle_mouse_click_on_the_editor_places_the_caret() {
        let (_dir, mut app) = open_rust_tab("abc\ndef\n");
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 1),
            &hits,
        );
        assert_eq!(app.focus, Focus::Editor);
        let (line, column) = cursor_line_column(
            app.active_buffer().unwrap().buffer.text_buffer(),
            caret(&app),
        );
        assert_eq!((line, column), (1, 2));
    }

    #[test]
    fn click_past_the_end_of_a_line_clamps_to_line_end() {
        let (_dir, mut app) = open_rust_tab("ab\n");
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            }),
            tab_strip: vec![],
        };
        // Column 15 is well within the 20-wide hit-test area but past
        // "ab"'s own 2 characters -- must clamp to line end, not no-op.
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 15, 0),
            &hits,
        );
        let (line, column) = cursor_line_column(
            app.active_buffer().unwrap().buffer.text_buffer(),
            caret(&app),
        );
        assert_eq!((line, column), (0, 2));
    }

    #[test]
    fn click_below_the_last_line_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("only one line");
        let before = caret(&app);
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 10,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 5),
            &hits,
        );
        assert_eq!(caret(&app), before);
    }

    #[test]
    fn click_on_the_editor_pane_with_no_file_open_is_a_noop() {
        let (_dir, mut app) = two_file_project();
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 10,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 2),
            &hits,
        );
        assert_eq!(app.focus, Focus::Editor, "the click still hit the pane");
        assert!(app.active_buffer().is_none());
    }

    #[test]
    fn wheel_scroll_over_the_editor_pane_with_no_file_open_is_a_noop() {
        let (_dir, mut app) = two_file_project();
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 10,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(mouse_event(MouseEventKind::ScrollDown, 2, 2), &hits);
        assert!(app.active_buffer().is_none());
    }

    #[test]
    fn handle_mouse_click_is_ignored_while_a_popup_is_open() {
        let (_dir, mut app) = open_rust_tab("abc\n");
        app.open_palette();
        let hits = ui::HitMap {
            tree_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            }),
            editor_text_area: None,
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1),
            &hits,
        );
        assert!(app.palette.is_some(), "a click must not close the palette");
        assert_eq!(
            app.focus,
            Focus::Editor,
            "a click must not act on the tree behind an open popup"
        );
    }

    #[test]
    fn handle_mouse_click_outside_every_known_rect_is_a_noop() {
        let (_dir, mut app) = two_file_project();
        let before_focus = app.focus;
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5),
            &ui::HitMap::default(),
        );
        assert_eq!(app.focus, before_focus);
    }

    // ---- Blame (docs/features/tui-blame.md) ----

    fn blame_annotation(
        line: usize,
        run_len: usize,
        commit_id: &str,
    ) -> crate::blame_gutter::BlameAnnotation {
        crate::blame_gutter::BlameAnnotation {
            line,
            run_len,
            commit_id: commit_id.to_string(),
            short_id: commit_id[..commit_id.len().min(7)].to_string(),
            author: "Test Author".to_string(),
            timestamp: 0,
            summary: "a commit".to_string(),
        }
    }

    #[test]
    fn toggle_blame_annotations_with_no_active_tab_is_a_noop() {
        let (_dir, mut app) = two_file_project();
        app.active_tab = None;
        app.toggle_blame_annotations();
        assert!(app.tabs.iter().all(|t| t.blame.is_none()));
    }

    #[test]
    fn toggle_blame_annotations_turns_on_then_off() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        assert!(app.active_buffer().unwrap().blame.is_none());

        app.toggle_blame_annotations();
        assert!(
            app.active_buffer().unwrap().blame.is_some(),
            "toggling on with no repo behind the tab still sets Some(empty), not None"
        );

        app.toggle_blame_annotations();
        assert!(app.active_buffer().unwrap().blame.is_none());
    }

    #[test]
    fn blame_lane_width_is_zero_until_blame_is_toggled_on() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        assert_eq!(app.blame_lane_width(), 0);
        app.toggle_blame_annotations();
        assert_eq!(
            app.blame_lane_width(),
            crate::blame_gutter::BLAME_LANE_WIDTH as u16
        );
    }

    #[test]
    fn refresh_blame_if_on_is_a_noop_when_blame_is_off() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        let idx = app.active_tab.unwrap();
        app.refresh_blame_if_on(idx);
        assert!(app.tabs[idx].blame.is_none());
    }

    #[test]
    fn refresh_blame_if_on_reloads_when_blame_is_on() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        let idx = app.active_tab.unwrap();
        app.tabs[idx].blame = Some(vec![blame_annotation(0, 1, "aaaaaaa")]);
        app.refresh_blame_if_on(idx);
        // No repo behind this tab, so a fresh `blame_for` call comes back
        // empty -- confirms it actually re-ran rather than leaving the
        // stale annotation in place.
        assert_eq!(app.tabs[idx].blame.as_ref().unwrap().len(), 0);
    }

    #[test]
    fn show_blame_for_current_line_with_no_active_tab_is_a_noop() {
        let (_dir, mut app) = two_file_project();
        app.active_tab = None;
        app.show_blame_for_current_line();
        assert!(app.blame_popup.is_none());
    }

    #[test]
    fn show_blame_for_current_line_with_blame_off_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        app.show_blame_for_current_line();
        assert!(app.blame_popup.is_none());
    }

    #[test]
    fn show_blame_for_current_line_with_no_covering_annotation_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\nfn other() {}\n");
        let idx = app.active_tab.unwrap();
        app.tabs[idx].blame = Some(vec![blame_annotation(0, 1, "aaaaaaa")]);
        app.handle_key(plain_key(KeyCode::Down)); // caret now on line 1, uncovered
        app.show_blame_for_current_line();
        assert!(app.blame_popup.is_none());
    }

    #[test]
    fn show_blame_for_current_line_opens_the_popup_on_a_hit() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        let idx = app.active_tab.unwrap();
        app.tabs[idx].blame = Some(vec![blame_annotation(0, 1, "aaaaaaa")]);
        app.blame_popup_scroll = 5;
        app.show_blame_for_current_line();
        assert_eq!(app.blame_popup.as_deref(), Some("aaaaaaa"));
        assert_eq!(app.blame_popup_scroll, 0);
    }

    #[test]
    fn handle_blame_popup_key_up_and_down_adjust_scroll_without_closing() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        app.blame_popup = Some("aaaaaaa".to_string());
        app.blame_popup_scroll = 2;

        app.handle_blame_popup_key(plain_key(KeyCode::Down));
        assert_eq!(app.blame_popup_scroll, 3);
        assert!(app.blame_popup.is_some());

        app.handle_blame_popup_key(plain_key(KeyCode::Up));
        app.handle_blame_popup_key(plain_key(KeyCode::Up));
        app.handle_blame_popup_key(plain_key(KeyCode::Up));
        assert_eq!(
            app.blame_popup_scroll, 0,
            "must saturate at 0, never underflow"
        );
    }

    #[test]
    fn handle_blame_popup_key_any_other_key_closes_it() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        app.blame_popup = Some("aaaaaaa".to_string());
        app.blame_popup_scroll = 4;

        app.handle_blame_popup_key(plain_key(KeyCode::Esc));

        assert!(app.blame_popup.is_none());
        assert_eq!(app.blame_popup_scroll, 0);
    }

    #[test]
    fn handle_key_routes_to_the_blame_popup_before_anything_else() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        app.blame_popup = Some("aaaaaaa".to_string());
        app.handle_key(plain_key(KeyCode::Char('x')));
        assert!(
            app.blame_popup.is_none(),
            "any non-arrow key must close the popup rather than falling through to editor input"
        );
    }

    #[test]
    fn any_popup_open_includes_the_blame_popup() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        assert!(!app.any_popup_open());
        app.blame_popup = Some("aaaaaaa".to_string());
        assert!(app.any_popup_open());
    }

    #[test]
    fn click_blame_lane_on_a_covered_line_opens_the_popup() {
        let (_dir, mut app) = open_rust_tab("abc\ndef\n");
        let idx = app.active_tab.unwrap();
        app.tabs[idx].blame = Some(vec![
            blame_annotation(0, 1, "aaaaaaa"),
            blame_annotation(1, 1, "bbbbbbb"),
        ]);
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 5,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1),
            &hits,
        );
        assert_eq!(app.blame_popup.as_deref(), Some("bbbbbbb"));
    }

    #[test]
    fn click_blame_lane_past_the_buffer_end_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("abc\n");
        let idx = app.active_tab.unwrap();
        app.tabs[idx].blame = Some(vec![blame_annotation(0, 1, "aaaaaaa")]);
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 5,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 4),
            &hits,
        );
        assert!(app.blame_popup.is_none());
    }

    #[test]
    fn click_past_the_blame_lane_still_places_the_caret() {
        let (_dir, mut app) = open_rust_tab("abc\ndef\n");
        let idx = app.active_tab.unwrap();
        app.tabs[idx].blame = Some(vec![
            blame_annotation(0, 1, "aaaaaaa"),
            blame_annotation(1, 1, "bbbbbbb"),
        ]);
        let lane = app.blame_lane_width();
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 60,
                height: 5,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), lane + 2, 1),
            &hits,
        );
        assert!(
            app.blame_popup.is_none(),
            "clicking past the lane must fall through to caret placement, not the popup"
        );
        let (line, column) = cursor_line_column(
            app.active_buffer().unwrap().buffer.text_buffer(),
            caret(&app),
        );
        assert_eq!((line, column), (1, 2));
    }

    #[test]
    fn sync_git_gutter_with_no_active_tab_clears_marks() {
        let (_dir, mut app) = open_rust_tab("fn main() {}\n");
        app.active_tab = None;
        app.git_gutter = vec![crate::git_gutter::GutterMark {
            line: 0,
            kind: crate::git_gutter::GutterMarkKind::Added,
        }];
        app.git_gutter_path = Some(PathBuf::from("/stale.rs"));

        app.sync_git_gutter();

        assert!(app.git_gutter.is_empty());
        assert!(app.git_gutter_path.is_none());
    }

    #[test]
    fn sync_git_gutter_clears_marks_while_the_buffer_is_dirty() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.active_buffer_mut().unwrap().buffer.insert(0, "x");

        app.sync_git_gutter();

        assert!(app.git_gutter.is_empty());
        assert!(app.git_gutter_path.is_none());
    }

    #[test]
    fn sync_git_gutter_reflects_a_saved_working_tree_change() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");

        app.sync_git_gutter();

        assert_eq!(app.git_gutter.len(), 1);
        assert_eq!(app.git_gutter[0].line, 1);
        assert_eq!(
            app.git_gutter[0].kind,
            crate::git_gutter::GutterMarkKind::Modified
        );
        assert_eq!(
            app.git_gutter_path,
            Some(dir.path().join("f.txt").canonicalize().unwrap())
        );
    }

    #[test]
    fn sync_git_gutter_does_not_recompute_when_the_path_is_unchanged() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.sync_git_gutter();
        // Mutate the cached marks directly -- a second `sync_git_gutter`
        // with the same active-tab path must leave this alone rather than
        // recomputing (same guard `sync_git_working_tree_diff` already
        // uses via `last_git_diff_target`).
        app.git_gutter.clear();

        app.sync_git_gutter();

        assert!(app.git_gutter.is_empty());
    }

    #[test]
    fn git_gutter_lane_width_is_zero_with_no_repo() {
        let (_dir, app) = open_rust_tab("fn main() {}\n");
        assert_eq!(app.git_gutter_lane_width(), 0);
    }

    #[test]
    fn git_gutter_lane_width_is_two_inside_a_clean_repo() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        let app = open_committed_tab(dir.path(), "f.txt");
        assert_eq!(app.git_gutter_lane_width(), 2);
    }

    #[test]
    fn git_gutter_lane_width_is_zero_while_the_buffer_is_dirty() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.active_buffer_mut().unwrap().buffer.insert(0, "x");
        assert_eq!(app.git_gutter_lane_width(), 0);
    }

    #[test]
    fn editor_lane_width_sums_blame_and_gutter_lanes() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        let mut app = open_committed_tab(dir.path(), "f.txt");
        assert_eq!(app.editor_lane_width(), app.git_gutter_lane_width());
        app.toggle_blame_annotations();
        assert_eq!(
            app.editor_lane_width(),
            app.blame_lane_width() + app.git_gutter_lane_width()
        );
        assert!(app.editor_lane_width() > app.git_gutter_lane_width());
    }

    #[test]
    fn click_git_gutter_lane_on_a_marked_line_opens_the_popup() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.sync_git_gutter();

        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 5,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 1),
            &hits,
        );
        assert_eq!(app.git_gutter_popup_line, Some(1));
    }

    #[test]
    fn click_git_gutter_lane_on_an_unmarked_line_is_a_noop() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.sync_git_gutter();

        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 5,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &hits,
        );
        assert!(app.git_gutter_popup_line.is_none());
    }

    #[test]
    fn click_past_the_git_gutter_lane_still_places_the_caret() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.sync_git_gutter();
        let lane = app.editor_lane_width();

        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 60,
                height: 5,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), lane, 1),
            &hits,
        );
        assert!(app.git_gutter_popup_line.is_none());
        let (line, column) = cursor_line_column(
            app.active_buffer().unwrap().buffer.text_buffer(),
            caret(&app),
        );
        assert_eq!((line, column), (1, 0));
    }

    #[test]
    fn handle_git_gutter_popup_key_r_reverts_the_hunk() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.sync_git_gutter();
        app.git_gutter_popup_line = Some(1);

        app.handle_git_gutter_popup_key(plain_key(KeyCode::Char('r')));

        assert_eq!(active_text(&app), "a\nb\nc\n");
        assert!(app.git_gutter_popup_line.is_none());
    }

    #[test]
    fn handle_git_gutter_popup_key_d_shows_the_diff() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.sync_git_gutter();
        app.git_gutter_popup_line = Some(1);

        app.handle_git_gutter_popup_key(plain_key(KeyCode::Char('d')));

        assert!(app.git_gutter_popup_line.is_none());
        assert!(app.git.diff.is_some());
        let state = app.git_panel.as_ref().unwrap();
        assert_eq!(state.view, GitPanelView::Log);
        assert_eq!(state.focus, GitPanelFocus::Diff);
    }

    #[test]
    fn handle_git_gutter_popup_key_any_other_key_closes_it() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.sync_git_gutter();
        app.git_gutter_popup_line = Some(1);

        app.handle_git_gutter_popup_key(plain_key(KeyCode::Esc));

        assert!(app.git_gutter_popup_line.is_none());
        assert_eq!(
            active_text(&app),
            "a\nB\nc\n",
            "Esc must not revert anything"
        );
    }

    #[test]
    fn handle_key_routes_to_the_git_gutter_popup_before_anything_else() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.git_gutter_popup_line = Some(0);
        app.handle_key(plain_key(KeyCode::Char('z')));
        assert!(
            app.git_gutter_popup_line.is_none(),
            "any non-r/d key must close the popup rather than falling through to editor input"
        );
    }

    #[test]
    fn any_popup_open_includes_the_git_gutter_popup() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        let mut app = open_committed_tab(dir.path(), "f.txt");
        assert!(!app.any_popup_open());
        app.git_gutter_popup_line = Some(0);
        assert!(app.any_popup_open());
    }

    #[test]
    fn trigger_revert_hunk_with_no_popup_open_is_a_noop() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.sync_git_gutter();

        app.trigger_revert_hunk();

        assert_eq!(active_text(&app), "a\nB\nc\n");
    }

    #[test]
    fn trigger_revert_hunk_with_stale_marks_is_a_noop() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.sync_git_gutter();
        app.git_gutter_popup_line = Some(1);
        // Marks are now stale for the active tab's path (a fresh edit
        // invalidates them, same as `sync_git_gutter`'s own dirty-buffer
        // rule) -- `trigger_revert_hunk` must refuse to act on them.
        app.git_gutter_path = Some(PathBuf::from("/different.rs"));

        app.trigger_revert_hunk();

        assert_eq!(active_text(&app), "a\nB\nc\n");
        assert!(app.git_gutter_popup_line.is_none());
    }

    #[test]
    fn trigger_show_diff_for_gutter_opens_the_panel_on_the_diff_pane() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();
        let mut app = open_committed_tab(dir.path(), "f.txt");
        // Simulate a panel that was left open on a different view/focus,
        // and a previously-pinned commit selection, from an earlier
        // session -- both must be overridden, not merely left alone.
        app.git_panel = Some(GitPanelState {
            view: GitPanelView::Changes,
            focus: GitPanelFocus::Graph,
            ..GitPanelState::default()
        });
        app.git.selected_commit = Some("deadbeef".to_string());
        app.git_gutter_popup_line = Some(1);

        app.trigger_show_diff_for_gutter();

        assert!(app.git_gutter_popup_line.is_none());
        assert!(app.git.selected_commit.is_none());
        assert!(app.git.diff.is_some());
        let state = app.git_panel.as_ref().unwrap();
        assert_eq!(state.view, GitPanelView::Log);
        assert_eq!(state.focus, GitPanelFocus::Diff);
        assert_eq!(state.diff_scroll, 0);
    }

    #[test]
    fn trigger_show_diff_for_gutter_with_no_active_tab_is_a_noop() {
        let dir = git_repo_without_commits();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        let mut app = open_committed_tab(dir.path(), "f.txt");
        app.active_tab = None;

        app.trigger_show_diff_for_gutter();

        assert!(app.git_panel.is_none());
    }

    #[test]
    fn handle_mouse_is_ignored_while_claude_terminal_raw_focus_is_active() {
        let (_dir, mut app) = two_file_project();
        app.claude_panel_open = true;
        app.claude_terminal_focus = true;
        let rows_before = app.tree_state.visible_rows(&app.tree);
        let selected_before = app
            .tree_state
            .selected_row(&rows_before)
            .unwrap()
            .path
            .clone();

        let hits = ui::HitMap {
            tree_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            }),
            editor_text_area: None,
            tab_strip: vec![],
        };
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 1),
            &hits,
        );
        app.handle_mouse(mouse_event(MouseEventKind::ScrollDown, 2, 1), &hits);

        let rows_after = app.tree_state.visible_rows(&app.tree);
        assert_eq!(
            app.tree_state.selected_row(&rows_after).unwrap().path,
            selected_before,
            "mouse input must be dropped entirely, not routed anywhere, during raw PTY focus"
        );
    }

    #[test]
    fn handle_mouse_ignores_unhandled_event_kinds() {
        let (_dir, mut app) = two_file_project();
        let before = app.focus;
        app.handle_mouse(
            mouse_event(MouseEventKind::Moved, 1, 1),
            &ui::HitMap::default(),
        );
        assert_eq!(app.focus, before);
    }

    #[test]
    fn wheel_scroll_over_the_tree_moves_the_selection() {
        let (_dir, mut app) = two_file_project();
        let rows = app.tree_state.visible_rows(&app.tree);
        assert!(rows.len() >= 2);
        let second_row_path = rows[1].path.clone();

        let hits = ui::HitMap {
            tree_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            }),
            editor_text_area: None,
            tab_strip: vec![],
        };
        app.handle_mouse(mouse_event(MouseEventKind::ScrollDown, 2, 2), &hits);

        let rows_after = app.tree_state.visible_rows(&app.tree);
        assert_eq!(
            app.tree_state.selected_row(&rows_after).unwrap().path,
            second_row_path
        );
    }

    #[test]
    fn wheel_scroll_over_the_editor_scrolls_without_moving_the_caret() {
        let (_dir, mut app) = open_rust_tab(&"line\n".repeat(50));
        let before_caret = caret(&app);
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(mouse_event(MouseEventKind::ScrollDown, 2, 2), &hits);
        assert_eq!(app.active_buffer().unwrap().scroll, 1);
        assert_eq!(caret(&app), before_caret);

        app.handle_mouse(mouse_event(MouseEventKind::ScrollUp, 2, 2), &hits);
        assert_eq!(app.active_buffer().unwrap().scroll, 0);
    }

    #[test]
    fn wheel_scroll_up_over_the_editor_at_the_top_is_a_noop() {
        let (_dir, mut app) = open_rust_tab("a\nb\nc\n");
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 5,
            }),
            tab_strip: vec![],
        };
        app.handle_mouse(mouse_event(MouseEventKind::ScrollUp, 1, 1), &hits);
        assert_eq!(app.active_buffer().unwrap().scroll, 0);
    }

    #[test]
    fn wheel_scroll_down_over_the_editor_clamps_at_the_last_line() {
        let (_dir, mut app) = open_rust_tab("a\nb\nc\n");
        let hits = ui::HitMap {
            tree_area: None,
            editor_text_area: Some(Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 5,
            }),
            tab_strip: vec![],
        };
        for _ in 0..10 {
            app.handle_mouse(mouse_event(MouseEventKind::ScrollDown, 1, 1), &hits);
        }
        let max_scroll = app.active_buffer().unwrap().scroll;
        app.handle_mouse(mouse_event(MouseEventKind::ScrollDown, 1, 1), &hits);
        assert_eq!(app.active_buffer().unwrap().scroll, max_scroll);
    }

    #[test]
    fn wheel_scroll_while_the_palette_is_open_moves_its_selection() {
        let (_dir, mut app) = two_file_project();
        app.open_palette();
        let before = app.palette.as_ref().unwrap().selected;
        app.handle_mouse(
            mouse_event(MouseEventKind::ScrollDown, 5, 5),
            &ui::HitMap::default(),
        );
        assert_eq!(app.palette.as_ref().unwrap().selected, before + 1);
    }

    // -- tui-debugger (T27) --

    /// A one-file Rust project *with* a `Cargo.toml`, so `detect_language`
    /// actually matches and `self.language` is `Some(rust())` -- unlike
    /// `open_rust_tab`/`sample_project`, which deliberately have no
    /// `Cargo.toml` so `App::new` never starts a language server. Clears
    /// any debug-adapter override `debug_config::load()` picked up from
    /// the real `$HOME` (the same real-filesystem risk `keymap::save`'s
    /// own tests already accept) so every test below starts from a known
    /// "no adapter configured" state regardless of the machine it runs on.
    fn rust_project_with_debug_adapter(command: Option<&str>) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(dir.path().join("f.rs"), "fn main() {}\n").unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_adapters = DebugAdapterConfig::default();
        let lang = app
            .language
            .as_mut()
            .expect("Cargo.toml should be detected as Rust");
        lang.debug_adapter_command = command.map(str::to_string);
        lang.debug_adapter_args = Vec::new();
        (dir, app)
    }

    #[test]
    fn trigger_debug_with_no_language_detected_is_a_no_op() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        assert!(app.language.is_none());

        app.trigger_debug();

        assert!(!app.debug.show_launch_popup);
    }

    #[test]
    fn trigger_debug_with_no_debug_adapter_configured_is_a_no_op() {
        let (_dir, mut app) = rust_project_with_debug_adapter(None);

        app.trigger_debug();

        assert!(!app.debug.show_launch_popup);
    }

    #[test]
    fn trigger_debug_opens_the_launch_popup_when_enabled() {
        let (_dir, mut app) = rust_project_with_debug_adapter(Some("codelldb"));

        app.trigger_debug();

        assert!(app.debug.show_launch_popup);
        assert_eq!(app.debug.launch_args_draft, "{}");
    }

    #[test]
    fn trigger_debug_is_a_no_op_while_a_session_is_already_active() {
        let (dir, mut app) = rust_project_with_debug_adapter(Some("cat"));
        app.debug
            .start_session("cat", &[], dir.path(), serde_json::json!({}));
        assert!(app.debug.is_active());

        app.trigger_debug();

        assert!(!app.debug.show_launch_popup);
    }

    #[test]
    fn confirm_debug_launch_with_invalid_json_sets_error_and_keeps_popup_open() {
        let (_dir, mut app) = rust_project_with_debug_adapter(Some("codelldb"));
        app.trigger_debug();
        app.debug.launch_args_draft = "{ not json".to_string();

        app.confirm_debug_launch();

        assert!(app.debug.show_launch_popup);
        assert!(app.debug.error.is_some());
        assert!(!app.debug.is_active());
    }

    #[test]
    fn confirm_debug_launch_starts_a_session_on_valid_json() {
        let (_dir, mut app) = rust_project_with_debug_adapter(Some("cat"));
        app.trigger_debug();

        app.confirm_debug_launch();

        assert!(!app.debug.show_launch_popup);
        assert!(app.debug.is_active());
        app.debug.stop();
    }

    #[test]
    fn handle_debug_launch_key_esc_closes_without_launching() {
        let (_dir, mut app) = rust_project_with_debug_adapter(Some("codelldb"));
        app.trigger_debug();

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(!app.debug.show_launch_popup);
        assert!(!app.debug.is_active());
    }

    #[test]
    fn toggle_breakpoint_at_caret_toggles_on_the_current_line() {
        let (_dir, mut app) = open_rust_tab("fn main() {\n    let x = 1;\n}\n");
        set_caret(&mut app, "fn main() {\n    let ".len());
        let path = app.active_buffer().unwrap().path.clone();

        app.toggle_breakpoint_at_caret();
        assert_eq!(app.debug.breakpoints.get(&path), Some(&vec![2]));

        app.toggle_breakpoint_at_caret();
        assert!(!app.debug.breakpoints.contains_key(&path));
    }

    #[test]
    fn breakpoint_line_ranges_splits_verified_and_unverified() {
        let (_dir, mut app) = open_rust_tab("fn main() {\n    let x = 1;\n}\n");
        let path = app.active_buffer().unwrap().path.clone();
        app.debug.toggle_breakpoint(path.clone(), 1);
        app.debug.toggle_breakpoint(path.clone(), 2);
        app.debug.confirmed_breakpoints.insert(
            path.clone(),
            vec![ide_dap::VerifiedBreakpoint {
                line: 2,
                verified: false,
                message: None,
            }],
        );
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();

        let (verified, unverified) = app.breakpoint_line_ranges(&path, text_buffer);

        assert_eq!(verified.len(), 1);
        assert_eq!(unverified.len(), 1);
    }

    #[test]
    fn breakpoint_line_ranges_is_empty_with_no_breakpoints() {
        let (_dir, app) = open_rust_tab("fn main() {}\n");
        let path = app.active_buffer().unwrap().path.clone();
        let text_buffer = app.active_buffer().unwrap().buffer.text_buffer();

        let (verified, unverified) = app.breakpoint_line_ranges(&path, text_buffer);

        assert!(verified.is_empty());
        assert!(unverified.is_empty());
    }

    #[test]
    fn toggle_debug_panel_opens_and_closes_and_resets_the_cursor() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_panel.thread_selected = 3;

        app.run_action(Action::ToggleDebugPanel);
        assert!(app.debug_panel_open);
        assert_eq!(app.debug_panel.thread_selected, 0);

        app.debug_panel.thread_selected = 5;
        app.run_action(Action::ToggleDebugPanel);
        assert!(!app.debug_panel_open);
    }

    #[test]
    fn close_all_overlays_closes_every_debug_related_overlay() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug.show_launch_popup = true;
        app.debug_adapter_config_popup = Some(DebugAdapterConfigPopupState {
            command: String::new(),
            args: String::new(),
            field: DebugConfigField::Command,
        });
        app.debug_panel_open = true;

        app.toggle_new_scratch_file(); // any overlay-opener triggers close_all_overlays

        assert!(!app.debug.show_launch_popup);
        assert!(app.debug_adapter_config_popup.is_none());
        assert!(!app.debug_panel_open);
    }

    #[test]
    fn handle_debug_panel_key_navigates_threads_and_selects_on_enter() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_panel_open = true;
        app.debug.threads = vec![
            ide_dap::ThreadInfo {
                id: 1,
                name: "main".to_string(),
            },
            ide_dap::ThreadInfo {
                id: 2,
                name: "worker".to_string(),
            },
        ];

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.debug_panel.thread_selected, 1);
        app.handle_key(plain_key(KeyCode::Down)); // clamps at the end
        assert_eq!(app.debug_panel.thread_selected, 1);

        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.debug.selected_thread, Some(2));
    }

    #[test]
    fn handle_debug_panel_key_tab_cycles_focus() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_panel_open = true;
        assert_eq!(app.debug_panel.focus, DebugPanelFocus::Threads);

        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.debug_panel.focus, DebugPanelFocus::Stack);
        app.handle_key(plain_key(KeyCode::Tab));
        assert_eq!(app.debug_panel.focus, DebugPanelFocus::Output);
        app.handle_key(plain_key(KeyCode::BackTab));
        assert_eq!(app.debug_panel.focus, DebugPanelFocus::Stack);
    }

    #[test]
    fn handle_debug_panel_key_output_scroll_up_reveals_older_lines_down_returns_to_the_tail() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_panel_open = true;
        app.debug_panel.focus = DebugPanelFocus::Output;

        app.handle_key(plain_key(KeyCode::Up));
        assert_eq!(app.debug_panel.output_scroll, 1);
        app.handle_key(plain_key(KeyCode::PageUp));
        assert_eq!(app.debug_panel.output_scroll, 11);

        app.handle_key(plain_key(KeyCode::Down));
        assert_eq!(app.debug_panel.output_scroll, 10);
        app.handle_key(plain_key(KeyCode::PageDown));
        assert_eq!(app.debug_panel.output_scroll, 0);
    }

    #[test]
    fn handle_debug_panel_key_page_up_down_are_no_ops_outside_output_focus() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_panel_open = true;
        assert_eq!(app.debug_panel.focus, DebugPanelFocus::Threads);

        app.handle_key(plain_key(KeyCode::PageUp));
        app.handle_key(plain_key(KeyCode::PageDown));

        assert_eq!(app.debug_panel.output_scroll, 0);
    }

    #[test]
    fn handle_debug_panel_key_single_letter_shortcuts_control_the_session() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_panel_open = true;
        app.debug
            .start_session("cat", &[], dir.path(), serde_json::json!({}));
        assert!(app.debug.is_active());

        app.handle_key(plain_key(KeyCode::Char('x'))); // stop

        assert!(!app.debug.is_active());
    }

    #[test]
    fn handle_debug_panel_key_esc_closes_the_panel() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_panel_open = true;

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(!app.debug_panel_open);
    }

    #[test]
    fn toggle_debug_adapter_config_popup_pre_fills_from_the_current_language() {
        let (_dir, mut app) = rust_project_with_debug_adapter(Some("codelldb"));
        app.language.as_mut().unwrap().debug_adapter_args =
            vec!["--port".to_string(), "1234".to_string()];

        app.toggle_debug_adapter_config_popup();

        let state = app.debug_adapter_config_popup.as_ref().unwrap();
        assert_eq!(state.command, "codelldb");
        assert_eq!(state.args, "--port 1234");
        assert_eq!(state.field, DebugConfigField::Command);
    }

    #[test]
    fn toggle_debug_adapter_config_popup_is_empty_with_no_language() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.toggle_debug_adapter_config_popup();

        let state = app.debug_adapter_config_popup.as_ref().unwrap();
        assert_eq!(state.command, "");
        assert_eq!(state.args, "");
    }

    #[test]
    fn confirm_debug_adapter_config_with_empty_command_notifies_and_keeps_popup_open() {
        let (_dir, mut app) = rust_project_with_debug_adapter(None);
        app.toggle_debug_adapter_config_popup();

        app.confirm_debug_adapter_config();

        assert!(app.debug_adapter_config_popup.is_some());
        assert_eq!(app.notifications.len(), 1);
    }

    #[test]
    fn confirm_debug_adapter_config_with_no_detected_language_notifies_and_keeps_popup_open() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_adapter_config_popup = Some(DebugAdapterConfigPopupState {
            command: "codelldb".to_string(),
            args: String::new(),
            field: DebugConfigField::Command,
        });

        app.confirm_debug_adapter_config();

        assert!(app.debug_adapter_config_popup.is_some());
        assert_eq!(app.notifications.len(), 1);
    }

    #[test]
    fn confirm_debug_adapter_config_saves_and_updates_the_language() {
        let (dir, mut app) = rust_project_with_debug_adapter(None);
        app.toggle_debug_adapter_config_popup();
        {
            let state = app.debug_adapter_config_popup.as_mut().unwrap();
            state.command = "codelldb".to_string();
            state.args = "--port 1234".to_string();
        }

        app.confirm_debug_adapter_config();

        assert!(app.debug_adapter_config_popup.is_none());
        let lang = app.language.as_ref().unwrap();
        assert_eq!(lang.debug_adapter_command.as_deref(), Some("codelldb"));
        assert_eq!(lang.debug_adapter_args, vec!["--port", "1234"]);
        assert_eq!(
            app.debug_adapters.adapters.get("Rust"),
            Some(&DebugAdapterEntry {
                command: "codelldb".to_string(),
                args: vec!["--port".to_string(), "1234".to_string()],
            })
        );
        // Round-trips through the real persisted config file, same
        // real-filesystem convention `keymap::save`'s own tests accept.
        let reloaded = debug_config::load();
        assert_eq!(
            reloaded.adapters.get("Rust"),
            Some(&DebugAdapterEntry {
                command: "codelldb".to_string(),
                args: vec!["--port".to_string(), "1234".to_string()],
            })
        );
        let _ = dir; // keeps the tempdir alive for the duration of the test
    }

    #[test]
    fn handle_debug_adapter_config_key_tab_switches_field_and_edits_route_correctly() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_adapter_config_popup = Some(DebugAdapterConfigPopupState {
            command: String::new(),
            args: String::new(),
            field: DebugConfigField::Command,
        });

        app.handle_key(plain_key(KeyCode::Char('c')));
        app.handle_key(plain_key(KeyCode::Tab));
        app.handle_key(plain_key(KeyCode::Char('a')));

        let state = app.debug_adapter_config_popup.as_ref().unwrap();
        assert_eq!(state.command, "c");
        assert_eq!(state.args, "a");
        assert_eq!(state.field, DebugConfigField::Args);
    }

    #[test]
    fn handle_debug_adapter_config_key_esc_cancels() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug_adapter_config_popup = Some(DebugAdapterConfigPopupState {
            command: "x".to_string(),
            args: String::new(),
            field: DebugConfigField::Command,
        });

        app.handle_key(plain_key(KeyCode::Esc));

        assert!(app.debug_adapter_config_popup.is_none());
    }

    #[test]
    fn debug_action_bindings_reach_run_action() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.debug.threads = vec![ide_dap::ThreadInfo {
            id: 1,
            name: "main".to_string(),
        }];

        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::F(8))); // ToggleLineBreakpoint -- no active tab, so a documented no-op
        assert!(app.debug.breakpoints.is_empty());

        app.handle_key(plain_key(KeyCode::F(9))); // ResumeProgram, no session: no-op
        assert!(!app.debug.is_active());
    }

    // -- T31: Back/Forward navigation
    // (docs/features/tui-back-forward-navigation.md) --

    #[test]
    fn open_location_pushes_a_nav_entry_and_navigate_back_returns_to_the_prior_file() {
        let dir = sample_project(); // a.txt: "hello\nworld", b.txt: "second file"
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.handle_key(plain_key(KeyCode::Down)); // sub, a.txt, b.txt -- land on a.txt
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.active_buffer().unwrap().path, root.join("a.txt"));

        app.open_location(location(root.join("b.txt"), 0, 3));
        assert_eq!(app.active_buffer().unwrap().path, root.join("b.txt"));

        assert!(app.nav_history.can_go_back());
        app.run_action(Action::NavigateBack);
        assert_eq!(app.active_buffer().unwrap().path, root.join("a.txt"));

        assert!(app.nav_history.can_go_forward());
        app.run_action(Action::NavigateForward);
        assert_eq!(app.active_buffer().unwrap().path, root.join("b.txt"));
        assert_eq!(
            app.active_buffer()
                .unwrap()
                .buffer
                .text_buffer()
                .selections()
                .primary()
                .head,
            3
        );
    }

    #[test]
    fn navigate_back_at_the_oldest_entry_is_a_noop() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.push_nav_location(0);

        app.run_action(Action::NavigateBack);

        assert_eq!(app.active_buffer().unwrap().path, root.join("a.txt"));
    }

    #[test]
    fn navigate_forward_at_the_newest_entry_is_a_noop() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.push_nav_location(0);

        app.run_action(Action::NavigateForward);

        assert_eq!(app.active_buffer().unwrap().path, root.join("a.txt"));
    }

    #[test]
    fn navigate_back_and_forward_with_empty_history_do_not_panic() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.run_action(Action::NavigateBack);
        app.run_action(Action::NavigateForward);

        assert!(app.active_tab.is_none());
    }

    #[test]
    fn repeated_jumps_within_one_file_coalesce_into_a_single_nav_entry() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.push_nav_location(0);

        app.open_location(location(root.join("a.txt"), 1, 0)); // still a.txt

        assert!(!app.nav_history.can_go_back());
    }

    #[test]
    fn navigate_back_then_a_new_jump_truncates_the_forward_branch() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.push_nav_location(0);
        app.open_location(location(root.join("b.txt"), 0, 0));

        app.run_action(Action::NavigateBack); // back to a.txt
        app.open_search_result(root.join("sub/c.txt"), 0); // new jump from the middle

        assert!(!app.nav_history.can_go_forward());
    }

    #[test]
    fn confirm_recent_file_does_not_push_a_nav_entry() {
        let dir = sample_project();
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.open_or_focus_tab(root.join("a.txt")).unwrap();
        app.open_or_focus_tab(root.join("b.txt")).unwrap();
        app.active_tab = None;

        app.run_action(Action::RecentFiles);
        app.handle_key(plain_key(KeyCode::Enter));

        assert!(!app.nav_history.can_go_back());
    }

    #[test]
    fn toggling_a_directory_row_does_not_push_a_nav_entry() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.handle_key(plain_key(KeyCode::Enter)); // "sub" is the first row, a directory

        assert!(app.active_tab.is_none());
        assert!(!app.nav_history.can_go_back());
    }

    #[test]
    fn confirm_go_to_file_pushes_a_nav_entry() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.go_to_file = Some(GoToFileState::default());
        app.go_to_file.as_mut().unwrap().query = "a.txt".to_string();
        app.sync_go_to_file();
        wait_until(|| {
            app.poll_search();
            !app.files_search.searching
        });

        app.confirm_go_to_file();
        assert!(app.active_tab.is_some());

        // A lone push leaves nothing to go back to yet (single entry) --
        // confirmed instead by a subsequent different-file jump growing
        // history into two entries, which only happens if the first jump
        // was actually recorded.
        let root = dir.path().canonicalize().unwrap();
        app.open_location(location(root.join("b.txt"), 0, 0));
        assert!(app.nav_history.can_go_back());
    }

    #[test]
    fn confirm_new_scratch_file_pushes_a_nav_entry() {
        let name = "_ide_tui_test_t31_scratch_nav.txt";
        cleanup_scratch_file(name);
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::NewScratchFile);
        app.new_scratch_file.as_mut().unwrap().name = name.to_string();

        app.handle_key(plain_key(KeyCode::Enter));

        let root = dir.path().canonicalize().unwrap();
        app.open_location(location(root.join("a.txt"), 0, 0));
        assert!(app.nav_history.can_go_back());

        cleanup_scratch_file(name);
    }

    #[test]
    fn confirm_scratch_file_pushes_a_nav_entry() {
        let name = "_ide_tui_test_t31_confirm_scratch_nav.txt";
        cleanup_scratch_file(name);
        let path = scratch::new_scratch_path(name).unwrap().unwrap();
        std::fs::write(&path, "hello").unwrap();

        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.run_action(Action::ToggleScratchFiles);
        app.scratch_files.as_mut().unwrap().query = name.to_string();

        app.handle_key(plain_key(KeyCode::Enter));

        let root = dir.path().canonicalize().unwrap();
        app.open_location(location(root.join("a.txt"), 0, 0));
        assert!(app.nav_history.can_go_back());

        cleanup_scratch_file(name);
    }

    #[test]
    fn confirm_bookmark_jump_pushes_a_nav_entry_at_the_bookmarked_offset() {
        let dir = sample_project(); // a.txt: "hello\nworld"
        let mut app = App::new(dir.path().to_path_buf()).unwrap();
        app.handle_key(plain_key(KeyCode::Down)); // a.txt
        app.handle_key(plain_key(KeyCode::Enter)); // pushes a.txt @ offset 0
        app.active_buffer_mut()
            .unwrap()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(6)));
        app.run_action(Action::ToggleBookmark); // bookmarks line 1 ("world")
        app.active_tab = None;

        app.run_action(Action::ShowBookmarks);
        app.handle_key(plain_key(KeyCode::Enter)); // should coalesce-replace the
                                                   // entry above with offset 6

        let root = dir.path().canonicalize().unwrap();
        app.open_location(location(root.join("b.txt"), 0, 0)); // grows a 2nd entry
        app.run_action(Action::NavigateBack);

        let buf = app.active_buffer().unwrap();
        assert_eq!(buf.path, root.join("a.txt"));
        let offset = buf.buffer.text_buffer().selections().primary().head;
        assert_eq!(
            offset, 6,
            "confirm_bookmark_jump must push the resolved bookmark offset, not the stale 0 from the earlier tree click"
        );
    }

    #[test]
    fn push_nav_location_with_no_active_tab_is_a_noop() {
        let dir = sample_project();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.push_nav_location(0);

        assert!(!app.nav_history.can_go_back());
        assert!(!app.nav_history.can_go_forward());
    }

    #[test]
    fn reopening_an_already_open_tab_pushes_its_real_caret_not_zero() {
        // Regression test for the fix-round finding: `open_or_focus_tab`'s
        // refocus branch never touches a tab's live caret, so a jump site
        // that always pushed offset 0 could under-report where the caret
        // actually was.
        let dir = sample_project(); // a.txt: "hello\nworld"
        let root = dir.path().canonicalize().unwrap();
        let mut app = App::new(dir.path().to_path_buf()).unwrap();

        app.handle_key(plain_key(KeyCode::Down)); // select a.txt's tree row
        app.handle_key(plain_key(KeyCode::Enter)); // opens a.txt, pushes offset 0
        app.active_buffer_mut()
            .unwrap()
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(6)));

        app.open_location(location(root.join("b.txt"), 0, 0)); // switch away

        // Tree selection is still on a.txt's row (untouched by the above) --
        // pressing Enter again refocuses the already-open a.txt tab, whose
        // caret is still 6.
        app.handle_key(plain_key(KeyCode::Enter));
        assert_eq!(app.active_buffer().unwrap().path, root.join("a.txt"));

        app.run_action(Action::NavigateBack); // -> b.txt
        assert_eq!(app.active_buffer().unwrap().path, root.join("b.txt"));
        app.run_action(Action::NavigateForward); // -> back to a.txt

        let buf = app.active_buffer().unwrap();
        assert_eq!(buf.path, root.join("a.txt"));
        let offset = buf.buffer.text_buffer().selections().primary().head;
        assert_eq!(offset, 6, "must record the real caret, not a hardcoded 0");
    }
}
